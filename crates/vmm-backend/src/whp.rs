//! Windows `whp` 後端（Windows Hypervisor Platform）。
//!
//! 設計見 docs/DESIGN.md §6「whp 後端」：以 WHP 開機 bundle 內附的 Linux micro-VM
//! appliance（與 macOS `vz` 共用同一 kernel/initramfs/guest-agent），spawn 獨立的
//! `chefer-whp-helper` 做完整 VM boot，本後端串接其 stdout 解析 `CHEFER_GUEST_IP` /
//! `CHEFER_GUEST_EXIT` 標記，並依 guest IP 起 host→guest 埠轉發。

use std::ffi::c_void;
use std::io::{BufRead, BufReader};
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use chefer_bundle::PortProto;

use windows_sys::Win32::Foundation::{CloseHandle, FreeLibrary, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExA,
};

use crate::vz_util;
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
            _ => Availability::Unavailable(availability_reason_for(capability, Some(&bundle))),
        }
    }

    fn run(&self, ctx: &AppRunContext) -> Result<i32> {
        let host_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let mem_override = std::env::var("CHEFER_WHP_MEMORY_MIB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok());

        let mut invocation = whp_util::helper_invocation(
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

        // WHP 的 host→guest 埠轉發由 helper 自身持有（guest IP 為 smoltcp 虛擬位址、
        // host 無路由可達）。helper 把每個 guest 埠暴露在 host `[::1]:guest`（鏡像 WSL2
        // wslrelay），host≠guest 的對外 remap 仍由 runtime 既有的埠代理負責——故這裡只需
        // 把要暴露的 guest 埠（去重）傳給 helper，bind 用 guest 埠、不是使用者的 host 埠。
        let mut tcp_forwards: Vec<(u16, u16)> = Vec::new();
        let mut udp_forwards: Vec<(u16, u16)> = Vec::new();
        for f in vz_util::forward_ports(ctx.manifest) {
            let bucket = match f.proto {
                PortProto::Tcp => &mut tcp_forwards,
                PortProto::Udp => &mut udp_forwards,
            };
            if !bucket.iter().any(|(_, g)| *g == f.guest) {
                bucket.push((f.guest, f.guest));
            }
        }
        invocation.forwards = tcp_forwards;
        invocation.udp_forwards = udp_forwards;
        // app 有 gui/both 服務 → helper 掛 virtio-gpu/input 並開顯示視窗（M8-c-2）。
        invocation.gui = ctx
            .manifest
            .services
            .iter()
            .any(|s| s.interface_mode.wants_gui());
        // 顯示視窗標題用 app 名。
        invocation.app_name = ctx.manifest.app.name.clone();

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
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn the WHP helper: {}",
                    invocation.helper.display()
                )
            })?;

        // 防孤兒（DESIGN §6 whp「Helper 生命週期」）：runtime 的 Ctrl+C 處理仰賴 helper
        // 同 console 收到同一個 console 事件，但對 runtime 單殺（taskkill /PID，非 console
        // Ctrl+C）時 helper 收不到任何訊號，micro-VM 會殘留。雙保險：
        // ① Job Object（KILL_ON_JOB_CLOSE）：job handle 故意洩漏、隨本行程存亡——runtime
        //    無論怎麼死（taskkill /F、當機、正常結束）OS 都會關閉 handle → job 關閉 →
        //    helper 連同 VM 被系統終結。spawn 與掛入之間的微小空窗與掛入失敗由 ② 後援。
        if let Err(e) = assign_to_kill_on_close_job(&child) {
            eprintln!(
                "[chefer] warning: failed to put the WHP helper in a kill-on-close job: {e}; \
                 relying on the helper's stdin-EOF self-termination"
            );
        }

        // ② stdin liveness（與 vz 同契約）：helper 讀到 stdin EOF 即自我了結。寫端由本
        //    行程握住不寫——WHP 的 guest console 無輸入路徑（serial 僅 TX），無 vz 的
        //    terminal stdin 泵送需求——交由 crate 保管，寫端 handle 隨行程存亡，並讓
        //    中斷訊號路徑能提前關閉它（見 crate::close_liveness_handles）。
        let helper_stdin = child
            .stdin
            .take()
            .expect("child stdin was requested as piped");
        crate::hold_liveness_handle(helper_stdin);

        let stdout = child
            .stdout
            .take()
            .expect("child stdout was requested as piped");
        let mut guest_exit: Option<i32> = None;

        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            println!("{line}");
            if let Some(code) = vz_util::parse_guest_exit_code(&line) {
                guest_exit = Some(code);
            }
        }

        let status = child.wait().context("failed to wait for the WHP helper")?;
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

// ---------------------------------------------------------------------------
// Helper 防孤兒（Job Object）
// ---------------------------------------------------------------------------

/// 把 child 掛進 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 Job Object，回傳 job handle
///（防孤兒 ①，DESIGN §6 whp「Helper 生命週期」）。呼叫端**故意洩漏** handle（不
/// CloseHandle）——handle 隨本行程存亡，行程死亡（任何方式）時 OS 關閉它 → job 關閉
/// → child 連同其中的 micro-VM 被系統終結。Windows 8+ 支援巢狀 job：本行程已在別的
/// job（如 CI runner）也掛得進去。
fn assign_to_kill_on_close_job(child: &std::process::Child) -> std::io::Result<HANDLE> {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) != 0
            && AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) != 0;
        if !configured {
            let err = std::io::Error::last_os_error();
            CloseHandle(job);
            return Err(err);
        }
        Ok(job)
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

    /// 鎖住防孤兒 ① 的核心語意（DESIGN §6 whp「Helper 生命週期」）：關掉唯一的
    /// job handle → job 內的行程被系統終結（KILL_ON_JOB_CLOSE）。真 WHP VM 情境
    ///（runtime 被 taskkill）只能實機驗證；此測試以長命 dummy child 在 CI 的
    /// Windows runner 上鎖住機制本身。
    #[test]
    fn kill_on_close_job_terminates_child_when_handle_closes() {
        use std::time::Duration;

        // 長命 child（約 60 秒自我了結——測試失敗時不殘留）。
        let mut child = Command::new("ping")
            .args(["-n", "60", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the dummy child (ping)");

        let job = assign_to_kill_on_close_job(&child).expect("failed to create/assign the job");
        assert!(
            child.try_wait().expect("try_wait failed").is_none(),
            "the child died right after being assigned to the job"
        );

        // 關掉唯一的 handle：模擬 runtime 行程死亡時 OS 的行為。
        unsafe { CloseHandle(job) };

        let mut exited = false;
        for _ in 0..100 {
            if child.try_wait().expect("try_wait failed").is_some() {
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !exited {
            let _ = child.kill();
        }
        assert!(
            exited,
            "the child survived closing the job handle; KILL_ON_JOB_CLOSE did not take effect"
        );
    }
}
