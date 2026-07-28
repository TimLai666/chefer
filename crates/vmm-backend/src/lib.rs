//! vmm-backend — 依平台選擇執行後端，啟動 bundle 內的服務。
//!
//! 對應 docs/DESIGN.md §6「vmm-backend」節：
//! - Linux：`namespaces` 後端（in-process 呼叫 guest-agent lib）。
//! - Windows：`wsl2` 後端（chefer 專用 distro 內跑 bundle 內嵌的 musl guest-agent）。
//! - macOS：`vz` 後端（內附 Swift helper 驅動 Virtualization.framework；已於 Apple Silicon
//!   實機驗證，`availability()` 依真實前提決定可用性，無實驗性 env 鎖）。
//!
//! 公開 API：[`Availability`]、[`RunOptions`]、[`AppRunContext`]、[`ExecBackend`]、
//! [`backends`]、[`run_app`]；另提供 [`cleanup_distros`] 供清理舊版殘留 WSL distro。

#[cfg(target_os = "linux")]
mod namespaces;
#[cfg(target_os = "macos")]
mod vz;
#[cfg(target_os = "windows")]
mod whp;
#[cfg(target_os = "windows")]
mod wsl2;

// 純函式（路徑轉換、distro 命名、最小 rootfs tar 產生）；跨平台可編譯與測試。
// 非 Windows 平台僅供測試使用，允許 dead_code。
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
mod wsl_util;

// macOS vz 後端的純函式（appliance 查找、VM 資源、kernel cmdline、helper CLI contract）；
// 跨平台可測。vz.rs（僅 macOS 編譯）已全數接上——allow(dead_code) 是給非 macOS 平台
// （vz.rs 不編譯、函式只被單元測試引用）。
#[allow(dead_code)]
mod vz_util;

// Windows WHP 後端的純函式（appliance/helper 查找、helper CLI contract、資源計算）；
// 跨平台可測。whp.rs（僅 Windows 編譯）已接上——allow(dead_code) 是給非 Windows 平台
// （函式只被單元測試引用）。
#[allow(dead_code)]
mod whp_util;

use std::process::ChildStdin;
use std::sync::Mutex;

use anyhow::Result;

/// 後端可用性檢查結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// 後端可用，可呼叫 [`ExecBackend::run`]。
    Available,
    /// 後端不可用；附上原因與補救方式（可行動的訊息）。
    Unavailable(String),
}

/// 執行選項（由 chefer-runtime 傳入）。
#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    /// 是否保留暫存資料（對應 guest-agent 的 keep_rootfs）。
    pub keep_tmp: bool,
}

/// 一次 app 執行所需的完整情境。
#[derive(Debug, Clone)]
pub struct AppRunContext<'a> {
    /// 解壓後的 bundle 目錄（內含 manifest.json）。
    pub bundle_dir: &'a std::path::Path,
    /// app 資料目錄（persist 資料的根）。
    pub data_dir: &'a std::path::Path,
    /// 已載入的 manifest（由 chefer-bundle 解析）。
    pub manifest: &'a chefer_bundle::Manifest,
    /// 執行選項。
    pub opts: RunOptions,
}

/// 執行後端的共同介面。
pub trait ExecBackend {
    /// 後端名稱（"namespaces" / "wsl2" / "vz"）。
    fn name(&self) -> &'static str;
    /// 檢查此後端於目前環境與本次 bundle 是否可用。
    fn availability(&self, ctx: &AppRunContext) -> Availability;
    /// 執行整個 app，回傳 app 整體 exit code。
    fn run(&self, ctx: &AppRunContext) -> Result<i32>;
}

/// 依目前平台回傳候選後端清單（依優先順序排序）。
pub fn backends() -> Vec<Box<dyn ExecBackend>> {
    #[cfg(target_os = "linux")]
    {
        vec![Box::new(namespaces::NamespacesBackend)]
    }
    #[cfg(target_os = "windows")]
    {
        vec![Box::new(wsl2::Wsl2Backend), Box::new(whp::WhpBackend)]
    }
    #[cfg(target_os = "macos")]
    {
        vec![Box::new(vz::VzBackend)]
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        Vec::new()
    }
}

/// Report the host-level WHP capability (no bundle context).
///
/// Returns `Available` when the WHP API and hypervisor are present.
/// [`ExecBackend::availability`] adds bundle-specific checks (helper + appliance).
pub fn whp_availability() -> Availability {
    #[cfg(target_os = "windows")]
    {
        whp::availability()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Availability::Unavailable(format!(
            "the Windows Hypervisor Platform backend only applies on Windows; this host is {}",
            std::env::consts::OS
        ))
    }
}

/// helper 的 stdin liveness 寫端（見 DESIGN §6 vz/whp 的「Helper 生命週期」）。
///
/// helper 讀到 stdin EOF 即自我了結，所以寫端只要活著 helper 就活著。這裡收下寫端相當於
/// 舊的 `mem::forget`（fd 隨 runtime 行程存亡，OS 保底），另外多一條「訊號路徑主動關閉」的
/// 快捷路徑——見 [`close_liveness_handles`]。
static LIVENESS_HANDLES: Mutex<Vec<ChildStdin>> = Mutex::new(Vec::new());

/// 交出 helper stdin 寫端由本模組保管，直到行程結束或 [`close_liveness_handles`] 被呼叫。
///
/// 只有會 spawn helper 的 VM 後端（macOS `vz`、Windows `whp`）用得到；Linux 的 `namespaces`
/// 後端是 in-process 呼叫 guest-agent，沒有 helper 行程可登記——故該平台只有單元測試會用到。
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
pub(crate) fn hold_liveness_handle(handle: ChildStdin) {
    lock_liveness().push(handle);
}

/// 中斷訊號路徑呼叫：立刻關掉所有 helper liveness 寫端，讓 helper 讀到 EOF 自我了結。
///
/// 沒有這條路徑時，helper 要等到 runtime 行程真的死亡（Ctrl-C handler 的 5 秒寬限跑完、
/// `exit(130)`）才會收到 EOF——對 runtime **單發** SIGINT 時實測約 6 秒才收攤。呼叫本函式
/// 讓 helper 立刻結束，`run_app` 也就立刻返回、走正常結束路徑。
pub fn close_liveness_handles() {
    lock_liveness().clear();
}

/// 取鎖；中毒時照樣取用內部值——這是訊號路徑，不能因為別處 panic 過就放棄收攤。
fn lock_liveness() -> std::sync::MutexGuard<'static, Vec<ChildStdin>> {
    LIVENESS_HANDLES.lock().unwrap_or_else(|e| e.into_inner())
}

/// 取第一個可用的後端執行 app；全部不可用時彙整每個後端的名稱與原因報錯。
///
/// `CHEFER_BACKEND` 環境變數可強制只用某個後端（例如 `CHEFER_BACKEND=whp`）：用於在
/// 同時裝了 WSL2 的機器上驗證 `whp` 路徑（否則 wsl2 排在前面會先被選中）。值需對應某個
/// 本平台候選後端名（namespaces/wsl2/whp/vz）；不認得的值或該後端在本平台不存在 → 明確錯誤。
pub fn run_app(ctx: &AppRunContext) -> Result<i32> {
    let list = backends();
    if list.is_empty() {
        anyhow::bail!(
            "No execution backend is available on this platform ({}). chefer single-file \
             apps currently support Linux (namespaces), Windows (WSL2), and macOS (planned).",
            std::env::consts::OS
        );
    }

    // 後端覆寫：只試指定的後端，方便在有 WSL 的機器上驗證 whp 等替代後端。
    let forced = std::env::var("CHEFER_BACKEND")
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(name) = forced.as_deref() {
        let backend = resolve_forced_backend(&list, name)?;
        return match backend.availability(ctx) {
            Availability::Available => backend.run(ctx),
            Availability::Unavailable(reason) => anyhow::bail!(
                "CHEFER_BACKEND forced the `{name}` backend, but it is unavailable: {reason}"
            ),
        };
    }

    let mut reasons: Vec<String> = Vec::new();
    for backend in &list {
        match backend.availability(ctx) {
            Availability::Available => return backend.run(ctx),
            Availability::Unavailable(reason) => {
                reasons.push(format!("  - {}: {}", backend.name(), reason));
            }
        }
    }
    anyhow::bail!(
        "No execution backend is available. Tried the following:\n{}",
        reasons.join("\n")
    )
}

/// 依名稱在候選後端清單中找出指定後端；找不到時回傳列出可用名稱的可行動錯誤。
fn resolve_forced_backend<'a>(
    list: &'a [Box<dyn ExecBackend>],
    name: &str,
) -> Result<&'a dyn ExecBackend> {
    list.iter()
        .find(|b| b.name() == name)
        .map(|b| b.as_ref())
        .ok_or_else(|| {
            let names: Vec<&str> = list.iter().map(|b| b.name()).collect();
            anyhow::anyhow!(
                "CHEFER_BACKEND=\"{name}\" is not a valid backend on this platform ({}). \
                 Available: {}.",
                std::env::consts::OS,
                names.join(", ")
            )
        })
}

/// Spawn a WHP helper binary with `--preflight` and return a one-line summary.
///
/// Doctor calls this with a helper binary found in the kit to validate
/// that the WHP partition lifecycle works on this host.
pub fn whp_preflight_with_helper(helper: &std::path::Path, cpus: u32) -> String {
    #[cfg(target_os = "windows")]
    {
        whp::spawn_preflight(helper, cpus)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (helper, cpus);
        "skipped (not on Windows)".to_string()
    }
}

/// 清理所有由 chefer 建立的 WSL distro（`chefer-rt-` 前綴）；回傳已移除的 distro 名稱。
///
/// 新版 Windows runtime 會在 app 結束時自動清理當次使用的 distro；此 API 主要保留給
/// 舊版殘留或維運工具使用。非 Windows 平台回傳明確錯誤。
pub fn cleanup_distros() -> Result<Vec<String>> {
    #[cfg(target_os = "windows")]
    {
        wsl2::cleanup_distros_impl()
    }
    #[cfg(not(target_os = "windows"))]
    {
        anyhow::bail!(
            "cleanup_distros is only supported on Windows (WSL2 backend); this platform ({}) \
             has no chefer-dedicated WSL distros to clean up",
            std::env::consts::OS
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backends_nonempty_on_supported_platforms() {
        let list = backends();
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        {
            #[cfg(target_os = "linux")]
            assert_eq!(backend_names(&list), vec!["namespaces"]);
            #[cfg(target_os = "windows")]
            assert_eq!(backend_names(&list), vec!["wsl2", "whp"]);
            #[cfg(target_os = "macos")]
            assert_eq!(backend_names(&list), vec!["vz"]);
        }
        let _ = list;
    }

    #[test]
    fn resolve_forced_backend_rejects_unknown_name() {
        let list = backends();
        // 空清單平台（非 linux/windows/macos）此測試無意義，跳過。
        if list.is_empty() {
            return;
        }
        // 不能用 unwrap_err()：Ok 型別是 &dyn ExecBackend（trait object 無 Debug）。
        let err = match resolve_forced_backend(&list, "bogus-backend") {
            Ok(_) => panic!("bogus name should not resolve"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(msg.contains("is not a valid backend"), "got: {msg}");
        // 錯誤訊息要列出真實可用後端名，方便使用者修正。
        assert!(msg.contains(list[0].name()), "got: {msg}");
    }

    #[test]
    fn resolve_forced_backend_accepts_real_name() {
        let list = backends();
        if list.is_empty() {
            return;
        }
        let name = list[0].name();
        let picked = resolve_forced_backend(&list, name).expect("real name should resolve");
        assert_eq!(picked.name(), name);
    }

    #[test]
    fn whp_availability_reports_host_state() {
        match whp_availability() {
            Availability::Available => {
                // WHP API + hypervisor 可用——合理（Windows + 虛擬化已啟用）
            }
            Availability::Unavailable(reason) => {
                #[cfg(target_os = "windows")]
                assert!(reason.contains("WHP"));
                #[cfg(not(target_os = "windows"))]
                assert!(reason.contains("only applies on Windows"));
            }
        }
    }

    fn backend_names(list: &[Box<dyn ExecBackend>]) -> Vec<&'static str> {
        list.iter().map(|backend| backend.name()).collect()
    }

    /// helper 的防孤兒契約：liveness 寫端一被 [`close_liveness_handles`] 關掉，讀端就拿到
    /// EOF 並自我了結——不必等 runtime 行程死亡。這裡用 `cat`（讀 stdin 到 EOF 就結束）代打
    /// helper，直接驗「關閉 → 子行程結束」這條因果。
    #[cfg(unix)]
    #[test]
    fn closing_liveness_handles_gives_the_child_eof() {
        use std::process::{Command, Stdio};

        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn cat");
        hold_liveness_handle(child.stdin.take().expect("piped stdin"));

        // 寫端還握著 → cat 應該還在等輸入。
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "child exited while the liveness handle was still held"
        );

        close_liveness_handles();
        let status = child.wait().expect("wait");
        assert!(status.success(), "child exited abnormally: {status:?}");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn cleanup_distros_errors_off_windows() {
        let err = cleanup_distros().unwrap_err();
        assert!(format!("{err}").contains("only supported on Windows"));
    }
}
