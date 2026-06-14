//! appcipe-normalize — appcipe 設定的正規化（host 路徑絕對化）與一站式載入入口。
//!
//! 流程（DESIGN.md §6）：讀檔 → serde 解析 → `normalize()`（路徑絕對化）→ `validate()`。
//! CLI 與 pack 一律走 `load()`，不要直接用 `appcipe_spec::from_file`
//! （後者只做解析 + 驗證，不做路徑正規化）。
//!
//! 舊欄位遷移（`crash_policy` → `crash`）由 appcipe-spec 的 serde alias 在解析層處理，
//! 本 crate 的 `load()` 自然涵蓋。

use std::path::Path;

use anyhow::Context;
use appcipe_spec::{AppCipe, ImageFormat, ImagePlatform, ImageSourceOrPath, ImageSourceType};

/// 將 appcipe 中的 host 路徑以 `base` 為基準絕對化：
/// - `image.file`（僅 source = tar；TarPath 簡寫形式亦同）
/// - `mounts` 的左半（host 路徑；guest 路徑不動）
/// - `data_dir`
///
/// 容器內路徑（`persist_path`、mounts 右半、`workdir`）一律不動。
/// `base` 建議傳絕對路徑（`load()` 會自動處理）。
pub fn normalize(app: &mut AppCipe, base: &Path) -> anyhow::Result<()> {
    // data_dir（Host 路徑）
    if let Some(dir) = &app.data_dir {
        app.data_dir = Some(to_abs(base, dir));
    }

    for (_name, svc) in app.services.iter_mut() {
        // 簡寫 `image: <字串>`：若看起來是 registry reference（非路徑、帶 tag/digest、檔案不存在），
        // 轉成 `source: image`（compose 風格的 `image: redis:7.2-alpine`）；否則維持 tar 路徑。
        if let ImageSourceOrPath::TarPath(p) = &svc.image
            && looks_like_image_ref(p, base)
        {
            let reference = p.clone();
            svc.image = ImageSourceOrPath::Full {
                source: ImageSourceType::Image,
                file: reference,
                format: ImageFormat::Auto,
                platform: ImagePlatform::LinuxAmd64,
            };
        }

        // image.file（Host 路徑；僅 source=tar 或 TarPath 簡寫）
        match &mut svc.image {
            ImageSourceOrPath::TarPath(p) => {
                *p = to_abs(base, p);
            }
            ImageSourceOrPath::Full { source, file, .. } => {
                if matches!(source, ImageSourceType::Tar) {
                    *file = to_abs(base, file);
                }
            }
        }

        // mounts：只轉左半邊（Host 路徑）。用 rsplitn(2, ':') 以相容 Windows 的 "C:\"
        // 形如 "host_path:container_path"；切不開的留給 validate() 報錯
        for m in &mut svc.mounts {
            if let Some((host, guest)) = split_mount(m) {
                let new_host = to_abs(base, host);
                *m = format!("{new_host}:{guest}");
            }
        }

        // 注意：persist_path 是容器內路徑，不轉！
    }
    Ok(())
}

/// 一站式載入入口：讀檔 → serde 解析 → normalize（路徑絕對化）→ validate。
/// 注意順序：**先 normalize 再 validate**。
pub fn load(path: &Path) -> anyhow::Result<AppCipe> {
    let s = std::fs::read_to_string(path)
        .with_context(|| format!("無法讀取設定檔 {}（請確認檔案存在且可讀）", path.display()))?;
    let mut app: AppCipe =
        serde_yaml::from_str(&s).context("appcipe.yml 解析失敗（YAML 格式或欄位型別錯誤）")?;

    // base = 設定檔所在目錄（絕對化；相對的設定檔路徑以目前工作目錄補全）
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let base = if parent.as_os_str().is_empty() {
        std::env::current_dir().context("無法取得目前工作目錄")?
    } else if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()
            .context("無法取得目前工作目錄")?
            .join(parent)
    };

    normalize(&mut app, &base)?;
    app.validate()
        .map_err(|e| anyhow::anyhow!("appcipe.yml 驗證失敗：\n{e}"))?;
    Ok(app)
}

/// 判斷簡寫 `image: <字串>` 看起來是不是 registry reference（而非本機 tar 路徑）。
///
/// 保守判定，避免把既有的 tar 路徑誤判為 ref：明確的路徑徵兆（`.`/`/`/`\`/`~` 開頭、
/// Windows 磁碟代號、`.tar`/`.tgz`/`.tar.gz` 副檔名、或檔案實際存在）一律當路徑；
/// 其餘**帶 `@digest` 或最後一段含 `:tag`** 的才視為 ref。tag 是否合法（禁 latest 等）
/// 留給 `validate()`。
fn looks_like_image_ref(s: &str, base: &Path) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    let first = s.as_bytes()[0];
    if matches!(first, b'.' | b'/' | b'\\' | b'~') {
        return false;
    }
    // Windows 磁碟代號：X:\ 或 X:/
    let b = s.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/') {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    if lower.ends_with(".tar") || lower.ends_with(".tgz") || lower.ends_with(".tar.gz") {
        return false;
    }
    // 確實存在的檔案 → 當 tar 路徑。
    if Path::new(s).is_file() || base.join(s).is_file() {
        return false;
    }
    // 釘版徵兆：有 @digest，或「最後一段路徑」含 ':'（tag）。
    if s.contains('@') {
        return true;
    }
    let last_segment = s.rsplit('/').next().unwrap_or(s);
    last_segment.contains(':')
}

/// 相對路徑以 base 補全；絕對路徑原樣保留。
fn to_abs(base: &Path, p: &str) -> String {
    let pb = Path::new(p);
    if pb.is_absolute() {
        p.to_string()
    } else {
        base.join(pb).to_string_lossy().to_string()
    }
}

/// 從右往左找一次 ':'，避免誤切 Windows 磁碟代號（"C:\"）。
fn split_mount(s: &str) -> Option<(&str, &str)> {
    let (left, right) = s.rsplit_once(':')?;

    Some((left, right))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 解析 YAML（不經 validate / normalize）。
    fn parse_raw(yaml: &str) -> AppCipe {
        serde_yaml::from_str(yaml).expect("測試 YAML 應可解析")
    }

    /// 取得本平台的絕對 base 路徑（測試用）。
    fn abs_base() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\proj\demo")
        } else {
            PathBuf::from("/proj/demo")
        }
    }

    // ---------- normalize：相對路徑絕對化 ----------

    #[test]
    fn normalize_absolutizes_relative_host_paths() {
        let base = abs_base();
        let mut app = parse_raw(
            r#"
version: "0.1"
name: App
data_dir: ./data
services:
  db:
    image: ./db.tar
    mounts:
      - ./host:/mnt/host
"#,
        );
        normalize(&mut app, &base).unwrap();

        let expect_data = base.join("./data").to_string_lossy().to_string();
        assert_eq!(app.data_dir.as_deref(), Some(expect_data.as_str()));

        let svc = &app.services["db"];
        let expect_img = base.join("./db.tar").to_string_lossy().to_string();
        match &svc.image {
            ImageSourceOrPath::TarPath(p) => assert_eq!(p, &expect_img),
            other => panic!("非預期的 image 形式：{other:?}"),
        }

        let expect_host = base.join("./host").to_string_lossy().to_string();
        assert_eq!(svc.mounts[0], format!("{expect_host}:/mnt/host"));
        assert!(svc.mounts[0].ends_with(":/mnt/host"), "guest 半邊不可變動");
    }

    #[test]
    fn normalize_absolutizes_full_image_form_for_tar_source() {
        let base = abs_base();
        let mut app = parse_raw(
            r#"
version: "0.1"
name: App
services:
  db:
    image:
      source: tar
      file: images/db.tar
"#,
        );
        normalize(&mut app, &base).unwrap();
        match &app.services["db"].image {
            ImageSourceOrPath::Full { file, .. } => {
                assert_eq!(
                    file,
                    &base.join("images/db.tar").to_string_lossy().to_string()
                );
            }
            other => panic!("非預期的 image 形式：{other:?}"),
        }
    }

    #[test]
    fn normalize_skips_non_tar_image_sources() {
        // dockerfile 來源的 file 不是 tar host 路徑語意，不做絕對化
        let base = abs_base();
        let mut app = parse_raw(
            r#"
version: "0.1"
name: App
services:
  db:
    image:
      source: dockerfile
      file: ./Dockerfile
"#,
        );
        normalize(&mut app, &base).unwrap();
        match &app.services["db"].image {
            ImageSourceOrPath::Full { file, .. } => assert_eq!(file, "./Dockerfile"),
            other => panic!("非預期的 image 形式：{other:?}"),
        }
    }

    #[test]
    fn normalize_keeps_absolute_paths_unchanged() {
        let base = abs_base();
        let abs_tar = if cfg!(windows) {
            r"D:\imgs\db.tar"
        } else {
            "/imgs/db.tar"
        };
        let abs_dir = if cfg!(windows) {
            r"D:\appdata"
        } else {
            "/appdata"
        };
        let yaml = format!(
            "version: \"0.1\"\nname: App\ndata_dir: \"{}\"\nservices:\n  db:\n    image: \"{}\"\n",
            abs_dir.replace('\\', "\\\\"),
            abs_tar.replace('\\', "\\\\"),
        );
        let mut app = parse_raw(&yaml);
        normalize(&mut app, &base).unwrap();
        assert_eq!(app.data_dir.as_deref(), Some(abs_dir));
        match &app.services["db"].image {
            ImageSourceOrPath::TarPath(p) => assert_eq!(p, abs_tar),
            other => panic!("非預期的 image 形式：{other:?}"),
        }
    }

    #[test]
    fn normalize_does_not_touch_persist_path_or_guest_side() {
        let base = abs_base();
        let mut app = parse_raw(
            r#"
version: "0.1"
name: App
services:
  db:
    image: ./db.tar
    persist_path: /var/lib/data
    mounts:
      - ./x:/guest/path
"#,
        );
        normalize(&mut app, &base).unwrap();
        let svc = &app.services["db"];
        assert_eq!(svc.persist_path.as_deref(), Some("/var/lib/data"));
        assert!(svc.mounts[0].ends_with(":/guest/path"));
    }

    /// Windows 磁碟代號開頭的 mount："C:\data:/mnt/data" 的左半是絕對路徑，必須原樣保留，
    /// 且 rsplitn 切法不可把 "C" 誤當 host。
    #[cfg(windows)]
    #[test]
    fn normalize_windows_drive_mount_host_is_preserved() {
        let base = PathBuf::from(r"C:\proj\demo");
        let mut app = parse_raw(
            "version: \"0.1\"\nname: App\nservices:\n  db:\n    image: ./db.tar\n    mounts: [\"C:\\\\data\\\\presets:/app/presets\"]\n",
        );
        normalize(&mut app, &base).unwrap();
        assert_eq!(
            app.services["db"].mounts[0],
            r"C:\data\presets:/app/presets"
        );
    }

    #[test]
    fn normalize_leaves_unsplittable_mounts_for_validate() {
        // 切不開（沒有 ':'）的 mount 不在 normalize 動，由 validate() 報錯
        let base = abs_base();
        let mut app = parse_raw(
            r#"
version: "0.1"
name: App
services:
  db:
    image: ./db.tar
    mounts: ["no-colon"]
"#,
        );
        normalize(&mut app, &base).unwrap();
        assert_eq!(app.services["db"].mounts[0], "no-colon");
        assert!(app.validate().is_err());
    }

    // ---------- 簡寫 image ref 偵測 ----------

    #[test]
    fn normalize_detects_short_form_registry_ref() {
        let base = abs_base();
        let mut app = parse_raw(
            "version: \"0.1\"\nname: App\nservices:\n  db: { image: \"redis:7.2-alpine\" }\n  web: { image: \"ghcr.io/o/i:1.0\" }\n",
        );
        normalize(&mut app, &base).unwrap();
        for (svc, want) in [("db", "redis:7.2-alpine"), ("web", "ghcr.io/o/i:1.0")] {
            match &app.services[svc].image {
                ImageSourceOrPath::Full {
                    source: ImageSourceType::Image,
                    file,
                    ..
                } => assert_eq!(file, want, "{svc} 應轉成 image ref 且不絕對化"),
                other => panic!("{svc} 應為 source:image，實為 {other:?}"),
            }
        }
    }

    #[test]
    fn normalize_keeps_tar_paths_as_tar() {
        let base = abs_base();
        // 各種「明顯是路徑」的簡寫都不可被誤判為 ref。
        let app_yaml = "version: \"0.1\"\nname: App\nservices:\n  a: { image: \"./db.tar\" }\n  b: { image: \"images/x.tar\" }\n  c: { image: \"plainname\" }\n";
        let mut app = parse_raw(app_yaml);
        normalize(&mut app, &base).unwrap();
        for svc in ["a", "b", "c"] {
            match &app.services[svc].image {
                ImageSourceOrPath::TarPath(p) => {
                    assert!(Path::new(p).is_absolute(), "{svc} tar 路徑應被絕對化：{p}");
                }
                other => panic!("{svc} 應維持 tar 路徑，實為 {other:?}"),
            }
        }
    }

    // ---------- load：端到端 ----------

    #[test]
    fn load_end_to_end_normalizes_then_validates() {
        let dir = tempfile::tempdir().unwrap();
        let yml = dir.path().join("appcipe.yml");
        std::fs::write(
            &yml,
            r#"
version: "0.1"
name: App
data_dir: ./data
services:
  db:
    image: ./db.tar
    persist_path: /var/lib/data
    ports: ["5432:5432"]
    mounts:
      - ./host:/mnt/host
"#,
        )
        .unwrap();

        let app = load(&yml).unwrap();

        // 所有 host 路徑都應是絕對路徑（以設定檔所在目錄為基準）
        let base = dir.path();
        assert_eq!(
            app.data_dir.as_deref(),
            Some(base.join("./data").to_string_lossy().to_string().as_str())
        );
        match &app.services["db"].image {
            ImageSourceOrPath::TarPath(p) => {
                assert!(Path::new(p).is_absolute(), "image 路徑應為絕對：{p}");
            }
            other => panic!("非預期的 image 形式：{other:?}"),
        }
        let m = &app.services["db"].mounts[0];
        let (host, guest) = split_mount(m).unwrap();
        assert!(Path::new(host).is_absolute(), "mount host 應為絕對：{m}");
        assert_eq!(guest, "/mnt/host");
        // 容器內路徑不動
        assert_eq!(
            app.services["db"].persist_path.as_deref(),
            Some("/var/lib/data")
        );
    }

    #[test]
    fn load_rejects_invalid_spec_after_normalize() {
        let dir = tempfile::tempdir().unwrap();
        let yml = dir.path().join("appcipe.yml");
        std::fs::write(
            &yml,
            r#"
version: "9.9"
name: App
services:
  db:
    image: ./db.tar
    persist_path: not/absolute
"#,
        )
        .unwrap();
        let err = load(&yml).unwrap_err().to_string();
        assert!(err.contains("驗證失敗"), "{err}");
        assert!(err.contains("不支援的 version"), "{err}");
        assert!(err.contains("persist_path"), "{err}");
    }

    #[test]
    fn load_migrates_crash_policy_alias() {
        let dir = tempfile::tempdir().unwrap();
        let yml = dir.path().join("appcipe.yml");
        std::fs::write(
            &yml,
            r#"
version: "0.1"
name: App
crash_policy: fail_fast
services:
  db:
    image: ./db.tar
"#,
        )
        .unwrap();
        let app = load(&yml).unwrap();
        assert_eq!(app.crash, appcipe_spec::CrashPolicy::FailFast);
    }

    #[test]
    fn load_missing_file_gives_actionable_error() {
        let err = load(Path::new("Z:/definitely/not/here/appcipe.yml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("無法讀取設定檔"), "{err}");
    }

    #[test]
    fn load_rejects_bad_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let yml = dir.path().join("appcipe.yml");
        std::fs::write(&yml, "version: [this is: not valid\n").unwrap();
        let err = load(&yml).unwrap_err().to_string();
        assert!(err.contains("解析失敗"), "{err}");
    }
}
