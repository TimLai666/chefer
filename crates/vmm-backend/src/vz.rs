//! macOS `vz` 後端（Apple Virtualization.framework）。
//!
//! 設計見 docs/DESIGN.md §6「macOS（vz）」：開一台輕量 Linux micro-VM，
//! 以 bundle 內附的 appliance（`vm/chefer-vmlinuz-<arch>` + `chefer-initramfs-<arch>`）
//! 開機，virtiofs 共享 bundle/data，guest 內 initramfs 執行 guest-agent。
//!
//! 開機由內附的 **Swift helper**（`agents/chefer-vz-helper-<arch>`）驅動：本後端只負責
//! spawn helper、把它的 stdout（接著 guest 序列 console）串接解析 `CHEFER_GUEST_IP` /
//! `CHEFER_GUEST_EXIT` 標記，並依 guest IP 起 host→guest 埠轉發。實際的
//! Virtualization.framework 呼叫全在 helper（Swift）內，僅能在實體 Mac 驗證 VM 真的開得起來。
//!
//! **誠實回報**：在實機驗證通過前 `availability()` 預設 `Unavailable`，需
//! `CHEFER_VZ_EXPERIMENTAL=1` 才啟用。

use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use chefer_bundle::PortProto;

use crate::vz_util::{self, PortForward};
use crate::{AppRunContext, Availability, ExecBackend};

/// macOS Virtualization.framework 後端。
pub struct VzBackend;

impl ExecBackend for VzBackend {
    fn name(&self) -> &'static str {
        "vz"
    }

    fn availability(&self, ctx: &AppRunContext) -> Availability {
        // 實機驗證前預設不可用；需 CHEFER_VZ_EXPERIMENTAL=1 才啟用（誠實不偽稱可執行）。
        if std::env::var("CHEFER_VZ_EXPERIMENTAL").ok().as_deref() != Some("1") {
            return Availability::Unavailable(
                "The macOS (vz) backend is experimental and not yet validated on real hardware. \
                 To try it on a Mac, set CHEFER_VZ_EXPERIMENTAL=1 (the app must embed a \
                 chefer-vz-helper signed with the com.apple.security.virtualization entitlement, \
                 or you can point CHEFER_VZ_HELPER at a locally built one)."
                    .to_string(),
            );
        }

        let host_arch = std::env::consts::ARCH;
        let Some(arch) = vz_util::host_guest_arch(host_arch) else {
            return Availability::Unavailable(format!(
                "unsupported macOS host architecture: {host_arch}; only x86_64 and aarch64 are supported"
            ));
        };
        if vz_util::appliance_in_bundle(ctx.bundle_dir, arch).is_none() {
            return Availability::Unavailable(format!(
                "This single-file app has no embedded macOS micro-VM appliance (missing vm/{} or vm/{}); \
                 rebuild with `chefer build --target *-apple-darwin` using a kit that includes the appliance",
                chefer_bundle::layout::kernel_name(arch),
                chefer_bundle::layout::initramfs_name(arch)
            ));
        }
        if resolve_helper(ctx.bundle_dir, host_arch).is_err() {
            return Availability::Unavailable(format!(
                "This single-file app has no embedded macOS VM helper (missing agents/{}); \
                 rebuild with a kit that includes it, or set CHEFER_VZ_HELPER to a locally built helper",
                chefer_bundle::layout::vz_helper_name(host_arch)
            ));
        }
        if let Some(reason) = macos_version_unsupported_reason() {
            return Availability::Unavailable(reason);
        }
        Availability::Available
    }

    fn run(&self, ctx: &AppRunContext) -> Result<i32> {
        let host_arch = std::env::consts::ARCH;
        let arch = vz_util::host_guest_arch(host_arch)
            .ok_or_else(|| anyhow::anyhow!("unsupported host architecture: {host_arch}"))?;
        let appliance = vz_util::appliance_in_bundle(ctx.bundle_dir, arch).ok_or_else(|| {
            anyhow::anyhow!(
                "This single-file app has no embedded macOS micro-VM appliance (missing vm/{} or vm/{})",
                chefer_bundle::layout::kernel_name(arch),
                chefer_bundle::layout::initramfs_name(arch)
            )
        })?;
        let helper = resolve_helper(ctx.bundle_dir, host_arch)?;

        std::fs::create_dir_all(ctx.data_dir).with_context(|| {
            format!(
                "failed to create data directory: {}",
                ctx.data_dir.display()
            )
        })?;

        let host_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let mem_override = std::env::var("CHEFER_VZ_MEMORY_MIB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok());
        let res = vz_util::VmResources::compute(host_cpus, mem_override);
        let cmdline = vz_util::kernel_command_line(ctx.opts.keep_tmp);
        // app 有 gui/both 服務 → helper 掛 virtio-gpu/鍵盤/指標並開 VZVirtualMachineView
        // 視窗（guest 側 cage overlay 與 WHP 完全共用；剪貼簿 token 由 helper 產生附進
        // cmdline，host 端經 NAT 直連 guest IP 同步）。
        let gui = ctx
            .manifest
            .services
            .iter()
            .any(|s| s.interface_mode.wants_gui());

        eprintln!(
            "[chefer] starting macOS micro-VM via {} (cpus={}, mem={}MiB{})",
            helper.display(),
            res.cpu_count,
            res.memory_mib,
            if gui { ", gui" } else { "" }
        );

        let args = vz_util::helper_args(
            &appliance.kernel,
            &appliance.initramfs,
            &cmdline,
            ctx.bundle_dir,
            ctx.data_dir,
            res.cpu_count,
            res.memory_mib,
            gui,
            &ctx.manifest.app.name,
        );
        let mut child = Command::new(&helper)
            .args(&args)
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!("failed to spawn the macOS VM helper: {}", helper.display())
            })?;

        let stdout = child
            .stdout
            .take()
            .expect("child stdout was requested as piped");
        let forwards = vz_util::forward_ports(ctx.manifest);
        let mut forwards_started = false;
        let mut guest_exit: Option<i32> = None;

        // 串接 helper（=guest console）的 stdout：直通給使用者，同時解析標記。
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            println!("{line}");
            if !forwards_started && let Some(ip) = vz_util::parse_guest_ip(&line) {
                start_port_forwards(ip, &forwards);
                forwards_started = true;
            }
            if let Some(code) = vz_util::parse_guest_exit_code(&line) {
                guest_exit = Some(code);
            }
        }

        let status = child
            .wait()
            .context("failed to wait for the macOS VM helper")?;
        if let Some(code) = guest_exit {
            return Ok(code);
        }
        if !status.success() {
            bail!(
                "the macOS VM helper exited with status {} before the guest reported an exit code; \
                 see the messages above for the Virtualization.framework error",
                status.code().unwrap_or(-1)
            );
        }
        // 乾淨結束卻沒看到 exit 標記（理論上不該發生）——保守回 0。
        Ok(0)
    }
}

/// 找開機 helper：`CHEFER_VZ_HELPER` env（供實機驗證直接指向自建 helper）優先，
/// 否則 bundle 內的 `agents/chefer-vz-helper-<host_arch>`。
fn resolve_helper(bundle_dir: &Path, host_arch: &str) -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("CHEFER_VZ_HELPER") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        bail!(
            "CHEFER_VZ_HELPER points to a file that does not exist: {}",
            p.display()
        );
    }
    let p = chefer_bundle::layout::agents_dir(bundle_dir)
        .join(chefer_bundle::layout::vz_helper_name(host_arch));
    if p.is_file() {
        return Ok(p);
    }
    bail!(
        "macOS VM helper not found: {}; rebuild with a kit that includes chefer-vz-helper-{host_arch}, \
         or set CHEFER_VZ_HELPER to a locally built helper",
        p.display()
    )
}

/// 取得 guest IP 後，對每個宣告的埠起 host→guest relay。best-effort：單一埠失敗只警告
/// （VM 已在跑），不中斷整個 app。
fn start_port_forwards(guest_ip: Ipv4Addr, forwards: &[PortForward]) {
    for f in forwards {
        match f.proto {
            PortProto::Tcp => match vz_util::spawn_tcp_forward(guest_ip, f.host, f.guest) {
                Ok(()) => eprintln!(
                    "[chefer] TCP port forward: 127.0.0.1:{} → {guest_ip}:{}",
                    f.host, f.guest
                ),
                Err(e) => eprintln!(
                    "[chefer] warning: could not forward TCP port {} (host bind failed: {e}); \
                     the service may be unreachable from the host",
                    f.host
                ),
            },
            PortProto::Udp => match UdpSocket::bind((Ipv4Addr::LOCALHOST, f.host)) {
                Ok(sock) => {
                    let target = SocketAddr::from((guest_ip, f.guest));
                    guest_agent::udp_bridge::spawn_udp_relay(sock, move || {
                        let up = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
                        up.connect(target)?;
                        Ok(up)
                    });
                    eprintln!(
                        "[chefer] UDP port forward: 127.0.0.1:{} → {guest_ip}:{}",
                        f.host, f.guest
                    );
                }
                Err(e) => eprintln!(
                    "[chefer] warning: could not forward UDP port {} (host bind failed: {e})",
                    f.host
                ),
            },
        }
    }
}

fn macos_version_unsupported_reason() -> Option<String> {
    let out = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    if !out.status.success() {
        return Some(
            "Could not determine the macOS version (`sw_vers -productVersion` failed); \
             make sure the system supports Apple Virtualization.framework"
                .to_string(),
        );
    }
    let version = String::from_utf8_lossy(&out.stdout);
    let mut parts = version.trim().split('.');
    let major = parts.next().and_then(|p| p.parse::<u32>().ok());
    let Some(major) = major else {
        return Some(format!(
            "could not parse the macOS version `{}`",
            version.trim()
        ));
    };
    if major < 13 {
        Some(format!(
            "macOS version is too old ({}); the Chefer vz backend requires macOS 13 or newer (for virtiofs support)",
            version.trim()
        ))
    } else {
        None
    }
}
