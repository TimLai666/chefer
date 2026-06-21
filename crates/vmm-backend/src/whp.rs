//! Windows `whp` 後端（Windows Hypervisor Platform）。
//!
//! 設計見 docs/DESIGN.md §6「whp 後端」：以 WHP 開機 bundle 內附的 Linux micro-VM
//! appliance（與 macOS `vz` 共用同一 kernel/initramfs/guest-agent），spawn 獨立的
//! `chefer-whp-helper` 做完整 VM boot，本後端串接其 stdout 解析 `CHEFER_GUEST_IP` /
//! `CHEFER_GUEST_EXIT` 標記，並依 guest IP 起 host→guest 埠轉發。

use std::ffi::c_void;
use std::io::{BufRead, BufReader};
use std::mem::size_of;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use chefer_bundle::PortProto;

use windows_sys::Win32::Foundation::FreeLibrary;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExA,
};

use crate::vz_util::{self, PortForward};
use crate::{AppRunContext, Availability, ExecBackend, whp_util};

const WIN_HV_PLATFORM_DLL: &[u8] = b"WinHvPlatform.dll\0";
const WHV_GET_CAPABILITY: &[u8] = b"WHvGetCapability\0";
const WHV_CAPABILITY_CODE_HYPERVISOR_PRESENT: u32 = 0;

type WhvGetCapability = unsafe extern "system" fn(
    capability_code: u32,
    capability_buffer: *mut c_void,
    capability_buffer_size_in_bytes: u32,
    written_size_in_bytes: *mut u32,
) -> i32;

pub struct WhpBackend;

impl ExecBackend for WhpBackend {
    fn name(&self) -> &'static str {
        "whp"
    }

    fn availability(&self, ctx: &AppRunContext) -> Availability {
        let capability = probe_host_capability();
        let bundle = whp_util::bundle_preflight(ctx.bundle_dir, std::env::consts::ARCH);

        match (&capability, &bundle) {
            (WhpHostCapability::HypervisorPresent, whp_util::BundlePreflight::Ready { .. }) => {
                Availability::Available
            }
            _ => Availability::Unavailable(availability_reason_for(
                capability,
                Some(&bundle),
            )),
        }
    }

    fn run(&self, ctx: &AppRunContext) -> Result<i32> {
        let host_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let mem_override = std::env::var("CHEFER_WHP_MEMORY_MIB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok());

        let invocation = whp_util::helper_invocation(
            ctx.bundle_dir,
            ctx.data_dir,
            ctx.opts.keep_tmp,
            std::env::consts::ARCH,
            host_cpus,
            mem_override,
        )
        .map_err(|pf| {
            anyhow::anyhow!(
                "WHP bundle preflight failed: {}",
                bundle_preflight_reason(&pf)
            )
        })?;

        std::fs::create_dir_all(ctx.data_dir).with_context(|| {
            format!(
                "failed to create data directory: {}",
                ctx.data_dir.display()
            )
        })?;

        eprintln!(
            "[chefer] starting WHP micro-VM via {} (cpus={}, mem={}MiB)",
            invocation.helper.display(),
            invocation.resources.cpu_count,
            invocation.resources.memory_mib
        );

        let mut child = Command::new(&invocation.helper)
            .args(invocation.args())
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn the WHP helper: {}",
                    invocation.helper.display()
                )
            })?;

        let stdout = child
            .stdout
            .take()
            .expect("child stdout was requested as piped");
        let forwards = vz_util::forward_ports(ctx.manifest);
        let mut forwards_started = false;
        let mut guest_exit: Option<i32> = None;

        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            println!("{line}");
            if !forwards_started
                && let Some(ip) = vz_util::parse_guest_ip(&line)
            {
                start_port_forwards(ip, &forwards);
                forwards_started = true;
            }
            if let Some(code) = vz_util::parse_guest_exit_code(&line) {
                guest_exit = Some(code);
            }
        }

        let status = child
            .wait()
            .context("failed to wait for the WHP helper")?;
        if let Some(code) = guest_exit {
            return Ok(code);
        }
        if !status.success() {
            bail!(
                "the WHP helper exited with status {} before the guest reported an exit code; \
                 check the messages above for the VM error",
                status.code().unwrap_or(-1)
            );
        }
        Ok(0)
    }
}

fn start_port_forwards(guest_ip: Ipv4Addr, forwards: &[PortForward]) {
    for f in forwards {
        match f.proto {
            PortProto::Tcp => match vz_util::spawn_tcp_forward(guest_ip, f.host, f.guest) {
                Ok(_) => eprintln!(
                    "[chefer] TCP port forward 127.0.0.1:{} → {}:{}",
                    f.host, guest_ip, f.guest
                ),
                Err(e) => eprintln!(
                    "[chefer] warning: TCP port forward 127.0.0.1:{} → {}:{}: {e}",
                    f.host, guest_ip, f.guest
                ),
            },
            PortProto::Udp => {
                let dest = SocketAddr::from((guest_ip, f.guest));
                match UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], f.host))) {
                    Ok(sock) => {
                        eprintln!(
                            "[chefer] UDP port forward 127.0.0.1:{} → {}:{}",
                            f.host, guest_ip, f.guest
                        );
                        std::thread::spawn(move || {
                            guest_agent::udp_bridge::spawn_udp_relay(sock, move || {
                                let s = UdpSocket::bind("0.0.0.0:0")?;
                                s.connect(dest)?;
                                Ok(s)
                            });
                        });
                    }
                    Err(e) => eprintln!(
                        "[chefer] warning: UDP port forward 127.0.0.1:{}: {e}",
                        f.host
                    ),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Host capability probing
// ---------------------------------------------------------------------------

pub(crate) fn availability() -> Availability {
    let cap = probe_host_capability();
    if cap == WhpHostCapability::HypervisorPresent {
        Availability::Available
    } else {
        Availability::Unavailable(availability_reason_for(cap, None))
    }
}

pub(crate) fn spawn_preflight(helper: &std::path::Path, cpus: u32) -> String {
    let args = whp_util::preflight_args(cpus);
    match std::process::Command::new(helper)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let line = stdout.trim();
            if line.is_empty() {
                "OK".to_string()
            } else {
                line.to_string()
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            format!(
                "FAILED (exit {}): {}",
                output.status.code().unwrap_or(-1),
                stderr.trim()
            )
        }
        Err(e) => format!("could not run helper: {e}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WhpHostCapability {
    ApiUnavailable,
    ProbeUnavailable,
    ProbeFailed(i32),
    HypervisorAbsent,
    HypervisorPresent,
}

fn availability_reason_for(
    capability: WhpHostCapability,
    bundle: Option<&whp_util::BundlePreflight>,
) -> String {
    let preflight = match capability {
        WhpHostCapability::ApiUnavailable => {
            "The Windows Hypervisor Platform (WHP) API is not available \
             (WinHvPlatform.dll could not be loaded); enable the Windows Hypervisor \
             Platform optional feature and reboot."
                .to_string()
        }
        WhpHostCapability::ProbeUnavailable => {
            "The Windows Hypervisor Platform (WHP) API is present, but \
             WHvGetCapability could not be found; update Windows or use WSL2."
                .to_string()
        }
        WhpHostCapability::ProbeFailed(hr) => format!(
            "The Windows Hypervisor Platform (WHP) API probe failed \
             (WHvGetCapability HypervisorPresent returned HRESULT 0x{:08X}).",
            hr as u32
        ),
        WhpHostCapability::HypervisorAbsent => {
            "The Windows Hypervisor Platform (WHP) API is installed, but \
             WHvGetCapability reports no active hypervisor; enable hardware \
             virtualization and the WHP feature, then reboot."
                .to_string()
        }
        WhpHostCapability::HypervisorPresent => {
            "The Windows Hypervisor Platform (WHP) API is available and an active \
             hypervisor is present."
                .to_string()
        }
    };

    let bundle = bundle.map_or_else(String::new, |status| match status {
        whp_util::BundlePreflight::Ready { .. } => {
            " This bundle contains the WHP helper and appliance.".to_string()
        }
        _ => format!(" {}", bundle_preflight_reason(status)),
    });

    format!("{preflight}{bundle}")
}

fn bundle_preflight_reason(status: &whp_util::BundlePreflight) -> String {
    match status {
        whp_util::BundlePreflight::Ready { .. } => String::new(),
        whp_util::BundlePreflight::UnsupportedHostArch { host_arch } => format!(
            "The WHP helper contract does not support this Windows host architecture ({host_arch}); \
             only x86_64 and aarch64 are supported."
        ),
        whp_util::BundlePreflight::MissingAppliance { arch } => format!(
            "This single-file app has no embedded WHP micro-VM appliance (missing vm/{} or vm/{}); \
             rebuild the Windows target with a kit that includes the appliance.",
            chefer_bundle::layout::kernel_name(arch),
            chefer_bundle::layout::initramfs_name(arch)
        ),
        whp_util::BundlePreflight::MissingHelper {
            host_arch: _,
            helper_name,
        } => format!(
            "This single-file app has no embedded WHP helper (missing agents/{helper_name}); \
             rebuild the Windows target with a kit that includes it."
        ),
    }
}

fn probe_host_capability() -> WhpHostCapability {
    let module = unsafe {
        LoadLibraryExA(
            WIN_HV_PLATFORM_DLL.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if module.is_null() {
        return WhpHostCapability::ApiUnavailable;
    }

    let proc = unsafe { GetProcAddress(module, WHV_GET_CAPABILITY.as_ptr()) };
    let Some(proc) = proc else {
        unsafe {
            FreeLibrary(module);
        }
        return WhpHostCapability::ProbeUnavailable;
    };

    let whv_get_capability: WhvGetCapability = unsafe { std::mem::transmute(proc) };
    let mut hypervisor_present = 0i32;
    let mut written = 0u32;
    let hr = unsafe {
        whv_get_capability(
            WHV_CAPABILITY_CODE_HYPERVISOR_PRESENT,
            (&mut hypervisor_present as *mut i32).cast::<c_void>(),
            size_of::<i32>() as u32,
            &mut written,
        )
    };
    unsafe {
        FreeLibrary(module);
    }

    if hr < 0 {
        return WhpHostCapability::ProbeFailed(hr);
    }

    if hypervisor_present == 0 {
        WhpHostCapability::HypervisorAbsent
    } else {
        WhpHostCapability::HypervisorPresent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_missing_whp_api_with_install_hint() {
        let reason = availability_reason_for(WhpHostCapability::ApiUnavailable, None);

        assert!(reason.contains("WinHvPlatform.dll"));
        assert!(reason.contains("enable"));
    }

    #[test]
    fn formats_hypervisor_absent_with_reboot_hint() {
        let reason = availability_reason_for(WhpHostCapability::HypervisorAbsent, None);

        assert!(reason.contains("no active hypervisor"));
        assert!(reason.contains("reboot"));
    }

    #[test]
    fn formats_hypervisor_present_as_available() {
        let reason = availability_reason_for(WhpHostCapability::HypervisorPresent, None);

        assert!(reason.contains("active hypervisor is present"));
        assert!(!reason.contains("not implemented"));
    }

    #[test]
    fn formats_probe_hresult() {
        let reason = availability_reason_for(WhpHostCapability::ProbeFailed(-1), None);

        assert!(reason.contains("HRESULT 0xFFFFFFFF"));
    }

    #[test]
    fn formats_bundle_missing_helper() {
        let status = whp_util::BundlePreflight::MissingHelper {
            host_arch: "x86_64".to_string(),
            helper_name: chefer_bundle::layout::whp_helper_name("x86_64"),
        };
        let reason = availability_reason_for(WhpHostCapability::HypervisorPresent, Some(&status));

        assert!(reason.contains("missing agents/chefer-whp-helper-x86_64.exe"));
    }
}
