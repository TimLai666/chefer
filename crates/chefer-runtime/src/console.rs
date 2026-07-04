//! 共用主控台（彙整日誌 + Ctrl-C 的終端視窗）顯示策略。
//!
//! 見 docs/DESIGN.md「主控台顯示」。stdio 模型本身不變（互動終端最多一個、其餘服務
//! 以 `[svc]` 前綴彙整到共用主控台）；此模組只決定那個共用主控台「要不要顯示」，
//! 並在無介面 app 印出停止提示。實際隱藏只在 Windows 生效（雙擊啟動才會配 console）。

use chefer_bundle::{ConsoleMode, InterfaceMode, Manifest};

/// app 是否完全沒有 gui／terminal 介面 → 共用主控台是唯一的停止介面。
fn is_headless(manifest: &Manifest) -> bool {
    !manifest.services.iter().any(|s| {
        matches!(
            s.interface_mode,
            InterfaceMode::Gui | InterfaceMode::Terminal | InterfaceMode::Both
        )
    })
}

/// 依 manifest 計算「共用主控台是否應顯示」。呼叫前已排除 headless（見 [`apply`]）。
fn should_show_console(manifest: &Manifest) -> bool {
    let has_terminal = manifest.services.iter().any(|s| {
        matches!(
            s.interface_mode,
            InterfaceMode::Terminal | InterfaceMode::Both
        )
    });
    let has_gui = manifest
        .services
        .iter()
        .any(|s| matches!(s.interface_mode, InterfaceMode::Gui | InterfaceMode::Both));
    match manifest.app.console {
        ConsoleMode::Shown => true,
        // 建置期驗證已保證有 gui/terminal 可關閉（headless 已在 apply 提前返回）。
        ConsoleMode::Hidden => false,
        // 有互動終端 → 顯示；只有 gui（無終端）→ 隱藏。
        ConsoleMode::Auto => has_terminal || !has_gui,
    }
}

/// 啟動時套用主控台策略：無介面 app 印 Ctrl-C 提示且絕不隱藏；
/// 其餘情況依 [`should_show_console`] 決定是否隱藏 Windows console。
pub fn apply(manifest: &Manifest) {
    if is_headless(manifest) {
        // 全 none：共用主控台是唯一停止介面 → 一定保留，並提示如何停止。
        eprintln!(
            "This app has no GUI or terminal interface; press Ctrl+C in this window to stop it."
        );
        return;
    }
    if !should_show_console(manifest) {
        hide_console_if_owned();
    }
}

/// 隱藏目前行程的 console 視窗——但只在「本行程是 console 的唯一擁有者」時。
///
/// 雙擊執行檔時 Windows 會為它新配一個 console（process list 只有我們自己）；
/// 從既有 shell 啟動時 process list > 1，此時隱藏會連帶藏掉使用者自己的終端，故跳過。
#[cfg(target_os = "windows")]
fn hide_console_if_owned() {
    use windows_sys::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};
    unsafe {
        let mut buf = [0u32; 2];
        let n = GetConsoleProcessList(buf.as_mut_ptr(), buf.len() as u32);
        if n != 1 {
            return;
        }
        let hwnd = GetConsoleWindow();
        if !hwnd.is_null() {
            ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// 非 Windows 無「雙擊配 console」概念：終端由父行程繼承，不主動隱藏。
#[cfg(not(target_os = "windows"))]
fn hide_console_if_owned() {}

#[cfg(test)]
mod tests {
    use super::*;
    use chefer_bundle::{AppMeta, CrashPolicy, ImageConfig, NetworkMode, ServiceEntry};
    use std::collections::BTreeMap;

    fn manifest_with(console: ConsoleMode, modes: &[InterfaceMode]) -> Manifest {
        let services = modes
            .iter()
            .enumerate()
            .map(|(i, m)| ServiceEntry {
                name: format!("s{i}"),
                platform: "linux/amd64".into(),
                layers: vec![],
                image_config: ImageConfig::default(),
                cmd_override: None,
                env: BTreeMap::new(),
                workdir_override: None,
                persist_path: None,
                ports: vec![],
                mounts: vec![],
                interface_mode: *m,
                depends_on: vec![],
                healthcheck: None,
                gpu: Default::default(),
            })
            .collect();
        Manifest {
            format_version: chefer_bundle::MANIFEST_FORMAT_VERSION,
            app: AppMeta {
                name: "App".into(),
                app_version: None,
                spec_version: "0.1".into(),
                old_names: vec![],
                data_dir_override: None,
                crash: CrashPolicy::FailFast,
                network: NetworkMode::Shared,
                console,
                generated_at_utc: "2026-01-01T00:00:00Z".into(),
                builder_version: String::new(),
            },
            services,
        }
    }

    #[test]
    fn auto_hides_for_gui_only() {
        let m = manifest_with(
            ConsoleMode::Auto,
            &[InterfaceMode::Gui, InterfaceMode::None],
        );
        assert!(!should_show_console(&m));
        assert!(!is_headless(&m));
    }

    #[test]
    fn auto_shows_for_terminal() {
        let m = manifest_with(
            ConsoleMode::Auto,
            &[InterfaceMode::Terminal, InterfaceMode::None],
        );
        assert!(should_show_console(&m));
    }

    #[test]
    fn headless_detected() {
        let m = manifest_with(
            ConsoleMode::Auto,
            &[InterfaceMode::None, InterfaceMode::None],
        );
        assert!(is_headless(&m));
    }

    #[test]
    fn explicit_overrides_win() {
        let gui = &[InterfaceMode::Gui];
        assert!(should_show_console(&manifest_with(ConsoleMode::Shown, gui)));
        let term = &[InterfaceMode::Terminal];
        assert!(!should_show_console(&manifest_with(
            ConsoleMode::Hidden,
            term
        )));
    }
}
