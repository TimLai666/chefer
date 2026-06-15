//! vmm-backend — 依平台選擇執行後端，啟動 bundle 內的服務。
//!
//! 對應 docs/DESIGN.md §6「vmm-backend」節：
//! - Linux：`namespaces` 後端（in-process 呼叫 guest-agent lib）。
//! - Windows：`wsl2` 後端（chefer 專用 distro 內跑 bundle 內嵌的 musl guest-agent）。
//! - macOS：`vz` 後端（v1 骨架，回報明確未支援訊息）。
//!
//! 公開 API：[`Availability`]、[`RunOptions`]、[`AppRunContext`]、[`ExecBackend`]、
//! [`backends`]、[`run_app`]；另提供 [`cleanup_distros`] 供未來 CLI 清理 WSL distro。

#[cfg(target_os = "linux")]
mod namespaces;
#[cfg(target_os = "macos")]
mod vz;
#[cfg(target_os = "windows")]
mod wsl2;

// 純函式（路徑轉換、distro 命名、最小 rootfs tar 產生）；跨平台可編譯與測試。
// 非 Windows 平台僅供測試使用，允許 dead_code。
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
mod wsl_util;

// macOS vz 後端的純函式（appliance 查找、VM 資源、kernel cmdline）；跨平台可測。
// 一律 allow(dead_code)：非 macOS 上僅供測試；macOS 上部分（VmResources、
// kernel_command_line…）是預留給「待實機驗證」的 VZ 開機 shim、目前尚未接線，
// 但已被單元測試覆蓋。待 vz.rs 接上後可移除此 allow。
#[allow(dead_code)]
mod vz_util;

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
        vec![Box::new(wsl2::Wsl2Backend)]
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

/// 取第一個可用的後端執行 app；全部不可用時彙整每個後端的名稱與原因報錯。
pub fn run_app(ctx: &AppRunContext) -> Result<i32> {
    let list = backends();
    if list.is_empty() {
        anyhow::bail!(
            "No execution backend is available on this platform ({}). chefer single-file \
             apps currently support Linux (namespaces), Windows (WSL2), and macOS (planned).",
            std::env::consts::OS
        );
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

/// 清理所有由 chefer 建立的 WSL distro（`chefer-rt-` 前綴）；回傳已移除的 distro 名稱。
///
/// 供未來 CLI 子命令接線使用；非 Windows 平台回傳明確錯誤。
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
            assert_eq!(list.len(), 1, "本平台應有一個後端");
            #[cfg(target_os = "linux")]
            assert_eq!(list[0].name(), "namespaces");
            #[cfg(target_os = "windows")]
            assert_eq!(list[0].name(), "wsl2");
            #[cfg(target_os = "macos")]
            assert_eq!(list[0].name(), "vz");
        }
        let _ = list;
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn cleanup_distros_errors_off_windows() {
        let err = cleanup_distros().unwrap_err();
        assert!(format!("{err}").contains("only supported on Windows"));
    }
}
