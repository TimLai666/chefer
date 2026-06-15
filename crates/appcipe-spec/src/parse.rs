//! appcipe.yml 的解析入口：解析 + 驗證。
//!
//! 注意：此處**不**做任何路徑正規化（host 路徑絕對化在 `appcipe-normalize`）。
//! CLI / pack 等正式流程一律走 `appcipe_normalize::load`。

use std::path::Path;

use anyhow::Context;

use crate::types::AppCipe;

/// 讀取檔案並解析 appcipe.yml，隨後立即執行 `validate()`。
pub fn from_file<P: AsRef<Path>>(path: P) -> anyhow::Result<AppCipe> {
    let path = path.as_ref();
    let s = std::fs::read_to_string(path).with_context(|| {
        format!(
            "cannot read config file {} (make sure the file exists and is readable)",
            path.display()
        )
    })?;
    from_str(&s)
}

/// 解析 appcipe.yml 字串，隨後立即執行 `validate()`。
pub fn from_str(yaml: &str) -> anyhow::Result<AppCipe> {
    let app: AppCipe = serde_yaml::from_str(yaml)
        .context("failed to parse appcipe.yml (invalid YAML syntax or wrong field type)")?;
    app.validate()
        .map_err(|e| anyhow::anyhow!("appcipe.yml validation failed:\n{e}"))?;
    Ok(app)
}

#[cfg(test)]
mod tests {
    use crate::types::*;

    #[test]
    fn from_str_parses_and_validates() {
        let app = crate::from_str(
            r#"
version: "0.1"
name: MyApp
services:
  db:
    image: ./db.tar
"#,
        )
        .unwrap();
        assert_eq!(app.name, "MyApp");
        assert_eq!(app.services.len(), 1);
    }

    #[test]
    fn from_str_does_not_normalize_paths() {
        // 解析層不做路徑絕對化：相對路徑必須原樣保留
        let app = crate::from_str(
            r#"
version: "0.1"
name: MyApp
data_dir: ./data
services:
  db:
    image: ./db.tar
    mounts:
      - ./host:/mnt/host
"#,
        )
        .unwrap();
        assert_eq!(app.data_dir.as_deref(), Some("./data"));
        let svc = &app.services["db"];
        match &svc.image {
            ImageSourceOrPath::TarPath(p) => assert_eq!(p, "./db.tar"),
            other => panic!("非預期的 image 形式：{other:?}"),
        }
        assert_eq!(svc.mounts[0], "./host:/mnt/host");
    }

    #[test]
    fn crash_policy_alias_is_accepted() {
        // 舊欄位名 crash_policy 必須能映射到 crash
        let app = crate::from_str(
            r#"
version: "0.1"
name: MyApp
crash_policy: fail_fast
services:
  db:
    image: ./db.tar
"#,
        )
        .unwrap();
        assert_eq!(app.crash, CrashPolicy::FailFast);
    }

    #[test]
    fn from_str_rejects_invalid_spec() {
        let err = crate::from_str(
            r#"
version: "9.9"
name: MyApp
services:
  db:
    image: ./db.tar
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("validation failed"));
    }

    #[test]
    fn from_file_reads_and_parses() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("appcipe.yml");
        std::fs::write(
            &p,
            "version: \"0.1\"\nname: MyApp\nservices:\n  db:\n    image: ./db.tar\n",
        )
        .unwrap();
        let app = crate::from_file(&p).unwrap();
        assert_eq!(app.name, "MyApp");
    }

    #[test]
    fn from_file_missing_file_gives_actionable_error() {
        let err = crate::from_file("Z:/definitely/not/here/appcipe.yml").unwrap_err();
        assert!(err.to_string().contains("cannot read config file"));
    }
}
