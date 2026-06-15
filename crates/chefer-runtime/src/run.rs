//! 執行階段：載入 manifest → 解析 data dir（含 old_names 遷移）→
//! 啟動埠代理 → 註冊 Ctrl-C 處理 → 呼叫 vmm_backend::run_app 並透傳 exit code。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use chefer_bundle::{AppMeta, Manifest};
use vmm_backend::{AppRunContext, RunOptions};

use crate::proxy;

/// 執行整個 app；回傳 app 整體 exit code。
pub fn run(bundle_dir: &Path, keep_tmp: bool) -> Result<i32> {
    let manifest_path = chefer_bundle::layout::manifest_path(bundle_dir);
    let manifest = Manifest::load(&manifest_path)?;
    tracing::info!(
        "app：{}{}{}",
        manifest.app.name,
        manifest
            .app
            .app_version
            .as_deref()
            .map(|v| format!(" v{v}"))
            .unwrap_or_default(),
        if manifest.app.builder_version.is_empty() {
            String::new()
        } else {
            format!("（由 chefer {} 打包）", manifest.app.builder_version)
        }
    );

    // 主控台顯示策略：gui-only 隱藏共用 console（Windows 雙擊時），無介面 app 印 Ctrl+C 提示。
    crate::console::apply(&manifest);

    // data dir：override 優先，否則平台預設；遷移檢查必須在 create_dir_all 之前。
    let data_dir = resolve_data_dir(&manifest.app)?;
    migrate_old_names(&data_dir, &manifest.app.old_names)?;
    fs_err::create_dir_all(&data_dir)
        .with_context(|| format!("建立資料目錄失敗：{}", data_dir.display()))?;
    tracing::info!("資料目錄：{}", data_dir.display());

    // 埠代理（host != guest 才代理；bind 失敗會在啟動服務前整體報錯）。
    proxy::start_port_proxies(&manifest)?;

    // Ctrl-C：印訊息後等 run_app 自然返回（後端子行程共享 console，會收到
    // 同號訊號自行結束）；若 5 秒內未返回則以 130 強制退出。
    let finished = Arc::new(AtomicBool::new(false));
    {
        let finished = Arc::clone(&finished);
        ctrlc::set_handler(move || {
            tracing::info!("收到中斷訊號（Ctrl-C），正在等待服務結束…");
            for _ in 0..50 {
                if finished.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            tracing::warn!("服務在 5 秒內未結束，強制退出（exit code 130）");
            std::process::exit(130);
        })
        .context("註冊 Ctrl-C 處理器失敗")?;
    }

    let ctx = AppRunContext {
        bundle_dir,
        data_dir: &data_dir,
        manifest: &manifest,
        opts: RunOptions { keep_tmp },
    };
    let result = vmm_backend::run_app(&ctx);
    finished.store(true, Ordering::SeqCst);

    let code = result?;
    tracing::info!("app 結束，exit code = {code}");
    Ok(code)
}

/// 解析 app 資料目錄：`app.data_dir_override` 優先；否則平台預設目錄 + app 名稱。
pub(crate) fn resolve_data_dir(app: &AppMeta) -> Result<PathBuf> {
    if let Some(over) = &app.data_dir_override {
        return Ok(PathBuf::from(over));
    }
    Ok(platform_data_root()?.join(&app.name))
}

/// 平台預設的 app 資料根目錄：
/// - Windows：`%LOCALAPPDATA%`
/// - macOS：`~/Library/Application Support`
/// - Linux／其他 unix：`$XDG_DATA_HOME` 或 `~/.local/share`
fn platform_data_root() -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "環境變數 LOCALAPPDATA 未設定，無法決定資料目錄；\
                     請設定 LOCALAPPDATA，或在 appcipe.yml 以 data_dir 指定後重新打包"
                )
            })?;
        Ok(PathBuf::from(base))
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "環境變數 HOME 未設定，無法決定資料目錄；\
                     請設定 HOME，或在 appcipe.yml 以 data_dir 指定後重新打包"
                )
            })?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(xdg));
        }
        let home = std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "環境變數 XDG_DATA_HOME 與 HOME 皆未設定，無法決定資料目錄；\
                     請設定其一，或在 appcipe.yml 以 data_dir 指定後重新打包"
                )
            })?;
        Ok(PathBuf::from(home).join(".local").join("share"))
    }
    #[cfg(not(any(target_os = "windows", unix)))]
    {
        anyhow::bail!(
            "不支援的平台（{}）：無法決定預設資料目錄；請在 appcipe.yml 以 data_dir 指定",
            std::env::consts::OS
        )
    }
}

/// old_names 遷移：data dir 不存在時，依序檢查同一父目錄下各舊名目錄，
/// 將第一個存在者重新命名為新名（印 log）。
pub(crate) fn migrate_old_names(data_dir: &Path, old_names: &[String]) -> Result<()> {
    if old_names.is_empty() || data_dir.exists() {
        return Ok(());
    }
    let Some(parent) = data_dir.parent() else {
        return Ok(());
    };
    for old in old_names {
        // 縱深防禦：old_names 雖已於 appcipe-spec 驗證為單一目錄名，
        // 但此遷移在 host 端（沙箱外）執行，仍對每一項再做一次硬性檢查——
        // 拒絕含路徑分隔、`..`、絕對路徑或磁碟前綴者，確保 parent.join(old)
        // 必落在 parent 之下，杜絕任意 host 目錄改名（見 docs/DESIGN.md §0）。
        if !is_single_path_segment(old) {
            tracing::warn!("略過不安全的 old_names 項目（非單一目錄名）：{old}");
            continue;
        }
        let old_path = parent.join(old);
        if old_path.is_dir() {
            fs_err::rename(&old_path, data_dir).with_context(|| {
                format!(
                    "遷移舊資料目錄失敗：{} → {}；\
                     請確認目錄未被其他程式使用後重試",
                    old_path.display(),
                    data_dir.display()
                )
            })?;
            tracing::info!(
                "已將舊資料目錄 {} 遷移為 {}",
                old_path.display(),
                data_dir.display()
            );
            return Ok(());
        }
    }
    Ok(())
}

/// old 是否為「單一安全目錄名」：非空、不含任何路徑分隔成分、
/// 不是 `.`/`..`、不含磁碟前綴。用 std::path::Component 判定以涵蓋兩種分隔符。
fn is_single_path_segment(old: &str) -> bool {
    if old.is_empty() || old.contains('/') || old.contains('\\') || old.contains(':') {
        return false;
    }
    let mut comps = Path::new(old).components();
    matches!(
        (comps.next(), comps.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chefer_bundle::{CrashPolicy, NetworkMode};
    use std::fs;

    fn app_meta(name: &str, override_dir: Option<String>, old_names: Vec<String>) -> AppMeta {
        AppMeta {
            name: name.into(),
            app_version: None,
            spec_version: "0.1".into(),
            old_names,
            data_dir_override: override_dir,
            crash: CrashPolicy::FailFast,
            network: NetworkMode::Shared,
            console: chefer_bundle::ConsoleMode::Auto,
            generated_at_utc: "2026-01-01T00:00:00Z".into(),
            builder_version: String::new(),
        }
    }

    #[test]
    fn data_dir_override_wins() {
        let app = app_meta("MyApp", Some("D:/Apps/MyApp".into()), vec![]);
        let dir = resolve_data_dir(&app).unwrap();
        assert_eq!(dir, PathBuf::from("D:/Apps/MyApp"));
    }

    /// 唯一會改動環境變數的測試（避免與其他測試互踩）。
    /// edition 2024 的 set_var 為 unsafe：本測試自行承擔同步責任。
    #[test]
    #[cfg(target_os = "windows")]
    fn data_dir_platform_default_uses_localappdata() {
        let tmp = tempfile::tempdir().unwrap();
        let original = std::env::var_os("LOCALAPPDATA");
        unsafe { std::env::set_var("LOCALAPPDATA", tmp.path()) };

        let app = app_meta("MyApp", None, vec![]);
        let dir = resolve_data_dir(&app).unwrap();
        assert_eq!(dir, tmp.path().join("MyApp"));

        // 還原，避免影響同一行程內其他程式碼
        unsafe {
            match original {
                Some(v) => std::env::set_var("LOCALAPPDATA", v),
                None => std::env::remove_var("LOCALAPPDATA"),
            }
        }
    }

    #[test]
    fn migrates_first_existing_old_name() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        // old_names = ["Ancient", "Legacy"]；只有 Legacy 存在 → 改名為新名
        fs::create_dir(parent.join("Legacy")).unwrap();
        fs::write(parent.join("Legacy").join("marker.txt"), b"data").unwrap();

        let data_dir = parent.join("Modern");
        migrate_old_names(&data_dir, &["Ancient".into(), "Legacy".into()]).unwrap();

        assert!(data_dir.join("marker.txt").is_file(), "資料應隨改名搬移");
        assert!(!parent.join("Legacy").exists(), "舊目錄應已改名消失");
    }

    #[test]
    fn migration_prefers_first_match_and_keeps_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        fs::create_dir(parent.join("OldA")).unwrap();
        fs::create_dir(parent.join("OldB")).unwrap();

        let data_dir = parent.join("New");
        migrate_old_names(&data_dir, &["OldA".into(), "OldB".into()]).unwrap();

        assert!(data_dir.is_dir());
        assert!(!parent.join("OldA").exists(), "第一個舊名應被遷移");
        assert!(parent.join("OldB").exists(), "其餘舊名目錄不應被動到");
    }

    #[test]
    fn migration_noop_when_data_dir_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        fs::create_dir(parent.join("New")).unwrap();
        fs::create_dir(parent.join("Old")).unwrap();

        let data_dir = parent.join("New");
        migrate_old_names(&data_dir, &["Old".into()]).unwrap();

        assert!(parent.join("Old").exists(), "新目錄已存在時不應遷移");
    }

    #[test]
    fn migration_noop_without_old_names() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("New");
        migrate_old_names(&data_dir, &[]).unwrap();
        assert!(!data_dir.exists(), "無 old_names 時不應建立任何目錄");
    }

    #[test]
    fn single_segment_check_rejects_unsafe() {
        for ok in ["Studio", "Studio-Beta_2", "a"] {
            assert!(is_single_path_segment(ok), "{ok} 應為安全單一目錄名");
        }
        for bad in [
            "",
            "..",
            ".",
            "a/b",
            "a\\b",
            "/abs",
            "C:\\x",
            "D:victim",
            "../escape",
        ] {
            assert!(!is_single_path_segment(bad), "{bad} 應被拒");
        }
    }

    /// 縱深防禦：即使惡意 manifest 夾帶路徑穿越的 old_names，
    /// 遷移也絕不會去動父目錄以外的目錄。
    #[test]
    fn migration_ignores_unsafe_old_names() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        // 在「父目錄之外」放一個誘餌目錄（模擬受害者的敏感目錄）
        let outside = tmp.path().join("outside_victim");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"x").unwrap();

        let nested_parent = parent.join("approot");
        fs::create_dir(&nested_parent).unwrap();
        let data_dir = nested_parent.join("New");

        // old 指向 ../outside_victim：必須被忽略，誘餌目錄原封不動
        migrate_old_names(&data_dir, &["../outside_victim".into()]).unwrap();
        assert!(outside.join("secret.txt").is_file(), "沙箱外目錄不可被改名");
        assert!(!data_dir.exists(), "不安全項目被略過，不應產生 data_dir");
    }
}
