//! appcipe 驗證規則（DESIGN.md §6 appcipe-spec 節）。
//!
//! `validate()` 會**收集所有錯誤一次回報**（多行訊息、每行一條），
//! 不會只回傳第一個錯誤；訊息順序具確定性（service 依名稱排序）。

use std::collections::{BTreeMap, HashMap, VecDeque};

use chefer_bundle::{MountSpec, PortSpec};

use crate::types::*;

/// 目前唯一支援的 spec 版本。
const SUPPORTED_VERSION: &str = "0.1";
/// app name 最大長度。
const MAX_APP_NAME_LEN: usize = 64;
/// service 名稱最大長度。
const MAX_SERVICE_NAME_LEN: usize = 32;

impl AppCipe {
    /// 驗證整份 appcipe 設定；回傳所有錯誤的彙整（多行），而非只有第一個。
    pub fn validate(&self) -> Result<(), String> {
        let mut errs: Vec<String> = Vec::new();

        // --- version ---
        if self.version != SUPPORTED_VERSION {
            errs.push(format!(
                "unsupported version: `{}` (only \"{SUPPORTED_VERSION}\" is supported; please change version to \"{SUPPORTED_VERSION}\")",
                self.version
            ));
        }

        // --- app name ---
        if let Err(e) = check_app_name(&self.name) {
            errs.push(e);
        }

        // --- data_dir：若指定則不可為空白（否則正規化後等同沿用 base 目錄，行為令人意外）---
        if let Some(d) = &self.data_dir
            && d.trim().is_empty()
        {
            errs.push(
                "data_dir must not be an empty string; remove the field to use the platform default, or provide a valid path".to_string(),
            );
        }

        // --- old_names：每一項都必須是「單一目錄名」(同 app name 規則) ---
        // 安全關鍵：執行期 runtime 會在 host 端（沙箱外）把 parent.join(old)
        // rename 成 data_dir。若放任 old 含路徑分隔/`..`/絕對路徑，惡意 appcipe
        // 可讓終端使用者首次執行時改名任意 host 目錄（DESIGN §0 路徑安全）。
        for old in &self.old_names {
            if let Err(e) = check_app_name(old) {
                errs.push(format!(
                    "old_names `{old}` is invalid: must be a single directory name (same rules as name, \
                     no path separators, `..`, or absolute paths); underlying cause: {e}"
                ));
            }
        }

        // service 依名稱排序，確保錯誤訊息順序確定
        let mut names: Vec<&String> = self.services.keys().collect();
        names.sort();

        // host port 全 app 不得重複：port -> 第一個使用者的描述
        let mut host_ports: BTreeMap<u16, String> = BTreeMap::new();
        // interface_mode 為 terminal/both 的服務（全 app 最多一個）
        let mut terminal_svcs: Vec<&str> = Vec::new();

        for name in names {
            let svc = &self.services[name];

            // --- service 名稱 ---
            if let Err(e) = check_service_name(name) {
                errs.push(e);
            }

            // --- image.source：tar / image / dockerfile ---
            match &svc.image {
                ImageSourceOrPath::TarPath(p) => {
                    if p.trim().is_empty() {
                        errs.push(format!(
                            "service `{name}`: image path must not be empty (provide the path to the image tar file)"
                        ));
                    }
                }
                ImageSourceOrPath::Full { source, file, .. } => match source {
                    ImageSourceType::Tar => {
                        if file.trim().is_empty() {
                            errs.push(format!(
                                "service `{name}`: image.file must not be empty (provide the path to the image tar file)"
                            ));
                        }
                    }
                    ImageSourceType::Dockerfile => {
                        if file.trim().is_empty() {
                            errs.push(format!(
                                "service `{name}`: image.file must not be empty (provide the path to the Dockerfile)"
                            ));
                        }
                    }
                    ImageSourceType::Image => {
                        if let Err(e) = check_image_reference(file) {
                            errs.push(format!(
                                "service `{name}` image ref `{file}` is invalid: {e}"
                            ));
                        }
                    }
                },
            }

            // --- ports ---
            for p in &svc.ports {
                match PortSpec::parse(p) {
                    Ok(spec) => {
                        if let Some(prev) = host_ports.get(&spec.host) {
                            errs.push(format!(
                                "service `{name}` ports \"{p}\": host port {} duplicates {prev} \
                                 (host ports must be unique within an app; use a different host port)",
                                spec.host
                            ));
                        } else {
                            host_ports.insert(spec.host, format!("service `{name}` \"{p}\""));
                        }
                    }
                    Err(e) => errs.push(format!("service `{name}` ports \"{p}\" is invalid: {e}")),
                }
            }

            // --- mounts ---
            for m in &svc.mounts {
                if let Err(e) = MountSpec::parse(m) {
                    errs.push(format!("service `{name}` mounts \"{m}\" is invalid: {e}"));
                }
            }

            // --- persist_path ---
            if let Some(pp) = &svc.persist_path
                && !pp.starts_with('/')
            {
                errs.push(format!(
                    "service `{name}` persist_path `{pp}` is invalid: must start with '/' (absolute path inside the container)"
                ));
            }

            // --- workdir：若指定須為容器內絕對路徑（相對路徑多半是誤填）---
            if let Some(wd) = &svc.workdir
                && !wd.starts_with('/')
            {
                errs.push(format!(
                    "service `{name}` workdir `{wd}` is invalid: must start with '/' (absolute path inside the container)"
                ));
            }

            // --- env key ---
            let mut keys: Vec<&String> = svc.env.keys().collect();
            keys.sort();
            for k in keys {
                if !is_valid_env_key(k) {
                    errs.push(format!(
                        "service `{name}` env variable name `{k}` is invalid: must match [A-Za-z_][A-Za-z0-9_]* \
                         (start with a letter or underscore, contain only letters, digits, and underscores)"
                    ));
                }
            }

            // --- depends_on：必須存在、不得自指 ---
            for d in &svc.depends_on {
                if d == name {
                    errs.push(format!(
                        "service `{name}` depends_on must not depend on itself"
                    ));
                } else if !self.services.contains_key(d) {
                    errs.push(format!(
                        "service `{name}` depends on a non-existent service `{d}` (make sure that name is defined under services)"
                    ));
                }
            }

            // --- healthcheck ---
            if let Some(hc) = &svc.healthcheck {
                if hc.test.to_cmd().is_none() {
                    errs.push(format!(
                        "service `{name}` healthcheck.test must not be empty (provide a command string or [\"CMD\", ...]/[\"CMD-SHELL\", \"...\"])"
                    ));
                }
                match hc.interval() {
                    Ok(d) if d.is_zero() => {
                        errs.push(format!("service `{name}` healthcheck.interval must be > 0"));
                    }
                    Err(e) => errs.push(format!("service `{name}` healthcheck.interval {e}")),
                    _ => {}
                }
                match hc.timeout() {
                    Ok(d) if d.is_zero() => {
                        errs.push(format!("service `{name}` healthcheck.timeout must be > 0"));
                    }
                    Err(e) => errs.push(format!("service `{name}` healthcheck.timeout {e}")),
                    _ => {}
                }
                if let Err(e) = hc.start_period() {
                    errs.push(format!("service `{name}` healthcheck.start_period {e}"));
                }
                if hc.retries == Some(0) {
                    errs.push(format!("service `{name}` healthcheck.retries must be >= 1"));
                }
            }

            // --- interface_mode 統計 ---
            if matches!(
                svc.interface_mode,
                InterfaceMode::Terminal | InterfaceMode::Both
            ) {
                terminal_svcs.push(name.as_str());
            }
        }

        // --- depends_on：不得有循環（缺失/自指邊已各自報錯，這裡忽略它們）---
        if let Some(stuck) = find_cycle(&self.services) {
            errs.push(format!(
                "depends_on has a dependency cycle, so the startup order cannot be determined: {stuck} (remove one of the dependencies in the cycle)"
            ));
        }

        // --- terminal/both 全 app 最多一個 ---
        if terminal_svcs.len() > 1 {
            errs.push(format!(
                "an app may have at most one service with interface_mode terminal/both, but there are {}: {} \
                 (change the rest to gui or none)",
                terminal_svcs.len(),
                terminal_svcs.join(", ")
            ));
        }

        // --- console: hidden 需有可關閉的介面（gui 或 terminal/both），否則 app 無從停止 ---
        if self.console == ConsoleMode::Hidden {
            let has_gui = self
                .services
                .values()
                .any(|s| matches!(s.interface_mode, InterfaceMode::Gui | InterfaceMode::Both));
            let has_terminal = !terminal_svcs.is_empty(); // terminal 或 both
            if !has_gui && !has_terminal {
                errs.push(
                    "console: hidden requires at least one gui or terminal/both service (otherwise, once the app starts \
                     there is no way to stop it—the shared console is the only stop interface); use auto/shown instead, or add a service with an interface"
                        .to_string(),
                );
            }
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs
                .iter()
                .map(|e| format!("- {e}"))
                .collect::<Vec<_>>()
                .join("\n"))
        }
    }
}

/// app name 規則：[A-Za-z][A-Za-z0-9_-]*，長度 ≤ 64。
fn check_app_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !first_ok || !rest_ok {
        return Err(format!(
            "name `{name}` is invalid: must match [A-Za-z][A-Za-z0-9_-]* \
             (start with a letter; contain only letters, digits, underscores, and hyphens; no spaces)"
        ));
    }
    let len = name.chars().count();
    if len > MAX_APP_NAME_LEN {
        return Err(format!(
            "name `{name}` is too long: at most {MAX_APP_NAME_LEN} characters (currently {len})"
        ));
    }
    Ok(())
}

/// service 名稱規則：[a-z][a-z0-9_]*，長度 ≤ 32。
fn check_service_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !first_ok || !rest_ok {
        return Err(format!(
            "service name `{name}` is invalid: must match [a-z][a-z0-9_]* \
             (start with a lowercase letter; contain only lowercase letters, digits, and underscores)"
        ));
    }
    let len = name.chars().count();
    if len > MAX_SERVICE_NAME_LEN {
        return Err(format!(
            "service name `{name}` is too long: at most {MAX_SERVICE_NAME_LEN} characters (currently {len})"
        ));
    }
    Ok(())
}

/// 驗證 `source: image` 的 registry reference。
///
/// 為了**可重現**，必須釘住版本：要嘛帶 `@<algo>:<hex>` digest（最穩），要嘛帶明確且
/// 非 `latest` 的 tag。未標記（會被當成 `latest`）或顯式 `:latest` 一律拒絕。
fn check_image_reference(r: &str) -> Result<(), String> {
    let r = r.trim();
    if r.is_empty() {
        return Err("must not be empty".into());
    }
    // digest 形式：<name>@<algo>:<hex> —— 釘到內容，最可重現，直接接受。
    if let Some((name, digest)) = r.split_once('@') {
        if name.is_empty() {
            return Err("the image name before `@` must not be empty".into());
        }
        let Some((algo, hex)) = digest.split_once(':') else {
            return Err(format!(
                "digest `{digest}` is malformed (expected <algo>:<hex>, e.g. sha256:…)"
            ));
        };
        if algo.is_empty() || hex.len() < 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "digest `{digest}` is malformed (hex too short or contains non-hexadecimal characters)"
            ));
        }
        return Ok(());
    }
    // tag 形式：tag 在「最後一段路徑」的 ':' 之後（避免把 registry 埠 `host:5000/…` 誤判為 tag）。
    let last_segment = r.rsplit('/').next().unwrap_or(r);
    match last_segment.rsplit_once(':') {
        Some((name, tag)) => {
            if name.is_empty() {
                return Err("image name must not be empty".into());
            }
            if tag.is_empty() {
                return Err("tag must not be empty".into());
            }
            if tag == "latest" {
                return Err(
                    "`latest` is not accepted: for reproducibility, specify an explicit version (e.g. redis:7.2-alpine) or an @sha256 digest"
                        .into(),
                );
            }
            Ok(())
        }
        None => Err(
            "no tag specified (would be treated as latest): for reproducibility, specify an explicit version (e.g. redis:7.2-alpine) or an @sha256 digest"
                .into(),
        ),
    }
}

/// env key 規則：[A-Za-z_][A-Za-z0-9_]*。
fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    first_ok && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 以 Kahn 演算法偵測 depends_on 循環；忽略缺失依賴與自指（它們有獨立錯誤）。
/// 回傳卡在循環中的 service 名稱清單（依名稱排序），無循環則回 None。
fn find_cycle(services: &HashMap<String, Service>) -> Option<String> {
    let mut indeg: BTreeMap<&str, usize> = services.keys().map(|n| (n.as_str(), 0)).collect();
    let mut rdeps: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (name, svc) in services {
        for d in &svc.depends_on {
            if d != name && services.contains_key(d) {
                *indeg.get_mut(name.as_str()).unwrap() += 1;
                rdeps.entry(d.as_str()).or_default().push(name.as_str());
            }
        }
    }

    let mut queue: VecDeque<&str> = indeg
        .iter()
        .filter(|&(_, &d)| d == 0)
        .map(|(&n, _)| n)
        .collect();
    let mut done = 0usize;
    while let Some(n) = queue.pop_front() {
        done += 1;
        if let Some(ms) = rdeps.get(n) {
            for &m in ms {
                let d = indeg.get_mut(m).unwrap();
                *d -= 1;
                if *d == 0 {
                    queue.push_back(m);
                }
            }
        }
    }

    if done == services.len() {
        None
    } else {
        let stuck: Vec<&str> = indeg
            .iter()
            .filter(|&(_, &d)| d > 0)
            .map(|(&n, _)| n)
            .collect();
        Some(stuck.join(", "))
    }
}

#[cfg(test)]
mod tests {
    /// 解析 YAML（不經 validate），回傳 AppCipe 供直接呼叫 validate() 測試。
    fn parse_raw(yaml: &str) -> crate::AppCipe {
        serde_yaml::from_str(yaml).expect("測試 YAML 應可解析")
    }

    /// 驗證並取出彙整後的錯誤訊息。
    fn validate_err(yaml: &str) -> String {
        parse_raw(yaml).validate().expect_err("預期驗證失敗")
    }

    // ---------- 通過案例 ----------

    #[test]
    fn full_valid_config_passes() {
        let app = parse_raw(
            r#"
version: "0.1"
name: Studio-Pro_2
app_version: "2.3.1"
old_names: [Studio]
data_dir: D:/Apps/StudioPro
crash: fail_fast
services:
  db:
    image:
      source: tar
      file: ./db.tar
      platform: linux/amd64
    env:
      POSTGRES_PASSWORD: pw
      _PRIVATE: "1"
    persist_path: /var/lib/postgresql/data
    ports:
      - "5432:5432"
  app1:
    image: ./app.tar
    ports:
      - "8080:80/tcp"
      - "9000:9000/udp"
    mounts:
      - ./data:/mnt/data
    interface_mode: terminal
    depends_on: [db]
"#,
        );
        assert!(app.validate().is_ok(), "{:?}", app.validate());
    }

    // ---------- version ----------

    #[test]
    fn rejects_unsafe_old_names() {
        // 路徑穿越（相對）、絕對路徑、Windows 磁碟、含分隔符——全部要拒
        for bad in [
            "../../etc",
            "..\\\\..\\\\windows",
            "/etc/passwd",
            "D:\\\\victim",
            "a/b",
            "name with space",
        ] {
            let e = validate_err(&format!(
                "version: \"0.1\"\nname: App\nold_names: [\"{bad}\"]\nservices:\n  db: {{ image: ./db.tar }}\n"
            ));
            assert!(e.contains("old_names"), "old_names `{bad}` 應被拒：{e}");
        }
    }

    #[test]
    fn accepts_safe_old_names() {
        let app = parse_raw(
            "version: \"0.1\"\nname: App\nold_names: [Studio, Studio-Beta_2]\nservices:\n  db: {{ image: ./db.tar }}\n"
                .replace("{{", "{")
                .replace("}}", "}")
                .as_str(),
        );
        assert!(app.validate().is_ok(), "{:?}", app.validate());
    }

    #[test]
    fn rejects_unsupported_version() {
        let e = validate_err(
            r#"
version: "9.9"
name: App
services:
  db: { image: ./db.tar }
"#,
        );
        assert!(e.contains("unsupported version"), "{e}");
    }

    // ---------- app name ----------

    #[test]
    fn app_name_rules() {
        let ok = ["App", "a", "App-Name_01", "Z9"];
        for n in ok {
            let app = parse_raw(&format!(
                "version: \"0.1\"\nname: {n}\nservices:\n  db: {{ image: ./db.tar }}\n"
            ));
            assert!(app.validate().is_ok(), "name `{n}` 應通過");
        }
        let bad = ["1app", "_app", "-app", "app name", "中文名"];
        for n in bad {
            let e = validate_err(&format!(
                "version: \"0.1\"\nname: \"{n}\"\nservices:\n  db: {{ image: ./db.tar }}\n"
            ));
            assert!(e.contains("name"), "name `{n}` 應被拒：{e}");
        }
    }

    #[test]
    fn app_name_length_limit() {
        let ok = format!("A{}", "x".repeat(63)); // 64 字元：通過
        let app = parse_raw(&format!(
            "version: \"0.1\"\nname: {ok}\nservices:\n  db: {{ image: ./db.tar }}\n"
        ));
        assert!(app.validate().is_ok());

        let too_long = format!("A{}", "x".repeat(64)); // 65 字元：拒絕
        let e = validate_err(&format!(
            "version: \"0.1\"\nname: {too_long}\nservices:\n  db: {{ image: ./db.tar }}\n"
        ));
        assert!(e.contains("is too long"), "{e}");
    }

    // ---------- service 名稱 ----------

    #[test]
    fn service_name_rules() {
        let ok = ["db", "web_1", "a0_z"];
        for n in ok {
            let app = parse_raw(&format!(
                "version: \"0.1\"\nname: App\nservices:\n  {n}: {{ image: ./db.tar }}\n"
            ));
            assert!(app.validate().is_ok(), "service `{n}` 應通過");
        }
        let bad = ["Db", "1db", "db-x", "_db"];
        for n in bad {
            let e = validate_err(&format!(
                "version: \"0.1\"\nname: App\nservices:\n  \"{n}\": {{ image: ./db.tar }}\n"
            ));
            assert!(e.contains("service name"), "service `{n}` 應被拒：{e}");
        }
    }

    #[test]
    fn service_name_length_limit() {
        let ok = "a".repeat(32);
        let app = parse_raw(&format!(
            "version: \"0.1\"\nname: App\nservices:\n  {ok}: {{ image: ./db.tar }}\n"
        ));
        assert!(app.validate().is_ok());

        let too_long = "a".repeat(33);
        let e = validate_err(&format!(
            "version: \"0.1\"\nname: App\nservices:\n  {too_long}: {{ image: ./db.tar }}\n"
        ));
        assert!(e.contains("is too long"), "{e}");
    }

    // ---------- ports ----------

    #[test]
    fn rejects_invalid_port_format() {
        let e = validate_err(
            r#"
version: "0.1"
name: App
services:
  db:
    image: ./db.tar
    ports: ["abc:80", "0:80", "5432"]
"#,
        );
        assert!(e.contains("ports"), "{e}");
        // 三條 port 錯誤都要被收集
        assert_eq!(e.lines().count(), 3, "{e}");
    }

    #[test]
    fn rejects_duplicate_host_ports_across_services() {
        let e = validate_err(
            r#"
version: "0.1"
name: App
services:
  db:
    image: ./db.tar
    ports: ["8080:5432"]
  web:
    image: ./web.tar
    ports: ["8080:80"]
"#,
        );
        assert!(
            e.contains("host port 8080") && e.contains("duplicates"),
            "{e}"
        );
    }

    #[test]
    fn rejects_duplicate_host_ports_within_one_service() {
        let e = validate_err(
            r#"
version: "0.1"
name: App
services:
  db:
    image: ./db.tar
    ports: ["8080:80", "8080:81"]
"#,
        );
        assert!(e.contains("host port 8080"), "{e}");
    }

    // ---------- mounts ----------

    #[test]
    fn rejects_invalid_mounts() {
        let e = validate_err(
            r#"
version: "0.1"
name: App
services:
  db:
    image: ./db.tar
    mounts: ["no-colon", "/host:relative/guest"]
"#,
        );
        assert!(e.contains("mounts"), "{e}");
        assert_eq!(e.lines().count(), 2, "{e}");
    }

    #[test]
    fn accepts_windows_drive_mount() {
        let app = parse_raw(
            "version: \"0.1\"\nname: App\nservices:\n  db:\n    image: ./db.tar\n    mounts: [\"C:\\\\data:/mnt/data\"]\n",
        );
        assert!(app.validate().is_ok(), "{:?}", app.validate());
    }

    // ---------- persist_path ----------

    #[test]
    fn rejects_relative_persist_path() {
        let e = validate_err(
            r#"
version: "0.1"
name: App
services:
  db:
    image: ./db.tar
    persist_path: var/lib/data
"#,
        );
        assert!(e.contains("persist_path"), "{e}");
    }

    // ---------- depends_on ----------

    #[test]
    fn rejects_missing_dependency() {
        let e = validate_err(
            r#"
version: "0.1"
name: App
services:
  db:
    image: ./db.tar
    depends_on: [nope]
"#,
        );
        assert!(e.contains("non-existent service `nope`"), "{e}");
    }

    #[test]
    fn rejects_self_dependency() {
        let e = validate_err(
            r#"
version: "0.1"
name: App
services:
  db:
    image: ./db.tar
    depends_on: [db]
"#,
        );
        assert!(e.contains("must not depend on itself"), "{e}");
    }

    #[test]
    fn rejects_dependency_cycle() {
        let e = validate_err(
            r#"
version: "0.1"
name: App
services:
  a:
    image: ./a.tar
    depends_on: [b]
  b:
    image: ./b.tar
    depends_on: [c]
  c:
    image: ./c.tar
    depends_on: [a]
"#,
        );
        assert!(e.contains("dependency cycle"), "{e}");
    }

    // ---------- env key ----------

    #[test]
    fn rejects_invalid_env_keys() {
        let e = validate_err(
            r#"
version: "0.1"
name: App
services:
  db:
    image: ./db.tar
    env:
      1BAD: x
      BAD-KEY: y
      GOOD_KEY: z
"#,
        );
        assert!(e.contains("`1BAD`"), "{e}");
        assert!(e.contains("`BAD-KEY`"), "{e}");
        assert!(!e.contains("GOOD_KEY"), "{e}");
    }

    // ---------- image.source ----------

    #[test]
    fn accepts_dockerfile_source() {
        // source: dockerfile 現已支援（建置在打包機上進行）。
        let app = parse_raw(
            r#"
version: "0.1"
name: App
services:
  a:
    image:
      source: dockerfile
      file: ./Dockerfile
"#,
        );
        assert!(app.validate().is_ok(), "{:?}", app.validate());
    }

    #[test]
    fn rejects_dockerfile_with_empty_file() {
        let e = validate_err(
            r#"
version: "0.1"
name: App
services:
  a:
    image:
      source: dockerfile
      file: ""
"#,
        );
        assert!(e.contains("image.file"), "{e}");
    }

    #[test]
    fn accepts_pinned_image_reference() {
        // source: image 帶明確 tag / digest 應通過（不再一律拒絕）。
        for r in [
            "postgres:16",
            "redis:7.2-alpine",
            "ghcr.io/owner/app:1.2.3",
            "localhost:5000/team/svc:v2",
            "alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ] {
            let app = parse_raw(&format!(
                "version: \"0.1\"\nname: App\nservices:\n  a:\n    image:\n      source: image\n      file: \"{r}\"\n"
            ));
            assert!(
                app.validate().is_ok(),
                "image ref `{r}` 應通過：{:?}",
                app.validate()
            );
        }
    }

    #[test]
    fn rejects_unpinned_or_latest_image_reference() {
        // 未標記 / :latest / 空 tag 一律拒絕（不可重現）。
        for r in [
            "redis",
            "redis:latest",
            "ghcr.io/owner/app",
            "localhost:5000/team/svc",
        ] {
            let e = validate_err(&format!(
                "version: \"0.1\"\nname: App\nservices:\n  a:\n    image:\n      source: image\n      file: \"{r}\"\n"
            ));
            assert!(e.contains("image ref"), "image ref `{r}` 應被拒：{e}");
        }
    }

    #[test]
    fn accepts_image_format_hyphen_and_underscore() {
        // 連字號（skopeo 慣用、文件與 E2E 腳本所用）與底線兩種都要能解析。
        for fmt in [
            "docker-archive",
            "docker_archive",
            "oci-archive",
            "oci_archive",
            "auto",
        ] {
            let app = parse_raw(&format!(
                "version: \"0.1\"\nname: App\nservices:\n  db:\n    image:\n      source: tar\n      file: ./db.tar\n      format: {fmt}\n"
            ));
            assert!(
                app.validate().is_ok(),
                "format `{fmt}` 應可解析並通過：{:?}",
                app.validate()
            );
        }
    }

    #[test]
    fn rejects_empty_image_file() {
        let e = validate_err(
            r#"
version: "0.1"
name: App
services:
  db:
    image: ""
"#,
        );
        assert!(e.contains("must not be empty"), "{e}");
    }

    // ---------- interface_mode ----------

    #[test]
    fn rejects_multiple_terminal_services() {
        let e = validate_err(
            r#"
version: "0.1"
name: App
services:
  a:
    image: ./a.tar
    interface_mode: terminal
  b:
    image: ./b.tar
    interface_mode: both
"#,
        );
        assert!(e.contains("at most one"), "{e}");
        assert!(e.contains("a, b"), "{e}");
    }

    #[test]
    fn single_terminal_service_is_ok() {
        let app = parse_raw(
            r#"
version: "0.1"
name: App
services:
  a:
    image: ./a.tar
    interface_mode: both
  b:
    image: ./b.tar
    interface_mode: gui
"#,
        );
        assert!(app.validate().is_ok());
    }

    // ---------- network ----------

    #[test]
    fn network_defaults_to_bridge() {
        // 省略 network: 時預設為 bridge（隔離 + 出網，對齊 Docker 預設）。
        let app = parse_raw("version: \"0.1\"\nname: App\nservices:\n  db: { image: ./db.tar }\n");
        assert_eq!(app.network, crate::NetworkMode::Bridge);
        assert!(app.validate().is_ok());
    }

    #[test]
    fn network_parses_all_modes() {
        for (s, want) in [
            ("shared", crate::NetworkMode::Shared),
            ("internal", crate::NetworkMode::Internal),
            ("bridge", crate::NetworkMode::Bridge),
        ] {
            let app = parse_raw(&format!(
                "version: \"0.1\"\nname: App\nnetwork: {s}\nservices:\n  db: {{ image: ./db.tar }}\n"
            ));
            assert_eq!(app.network, want, "network `{s}` 應解析為 {want:?}");
            assert!(app.validate().is_ok(), "network `{s}` 應通過驗證");
        }
    }

    #[test]
    fn network_rejects_unknown_mode() {
        let bad: Result<crate::AppCipe, _> = serde_yaml::from_str(
            "version: \"0.1\"\nname: App\nnetwork: wormhole\nservices:\n  db: { image: ./db.tar }\n",
        );
        assert!(bad.is_err(), "未知 network 模式應解析失敗");
    }

    // ---------- data_dir / workdir ----------

    #[test]
    fn rejects_empty_data_dir() {
        let e = validate_err(
            "version: \"0.1\"\nname: App\ndata_dir: \"\"\nservices:\n  db: { image: ./db.tar }\n",
        );
        assert!(e.contains("data_dir"), "{e}");
    }

    #[test]
    fn rejects_relative_workdir() {
        let e = validate_err(
            "version: \"0.1\"\nname: App\nservices:\n  db: { image: ./db.tar, workdir: rel/dir }\n",
        );
        assert!(e.contains("workdir"), "{e}");
    }

    // ---------- 未知欄位（打錯名稱）----------

    #[test]
    fn rejects_misspelled_service_field() {
        // `port`（少了 s）是常見打錯；deny_unknown_fields 應在解析期擋下，而非靜默忽略。
        let e = serde_yaml::from_str::<crate::AppCipe>(
            "version: \"0.1\"\nname: App\nservices:\n  db: { image: ./db.tar, port: [\"5432:5432\"] }\n",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("port") || e.contains("unknown field"), "{e}");
    }

    #[test]
    fn rejects_misspelled_top_level_field() {
        let e = serde_yaml::from_str::<crate::AppCipe>(
            "version: \"0.1\"\nname: App\nservcies:\n  db: { image: ./db.tar }\n",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("servcies") || e.contains("unknown field"), "{e}");
    }

    // ---------- healthcheck ----------

    #[test]
    fn parse_duration_forms() {
        use crate::parse_duration;
        use std::time::Duration;
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("1m").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("3").unwrap(), Duration::from_secs(3));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("2x").is_err());
    }

    #[test]
    fn health_test_normalizes_docker_forms() {
        use crate::{HealthTest, TestCmd};
        assert_eq!(
            HealthTest::Shell("redis-cli ping".into()).to_cmd(),
            Some(TestCmd::Shell("redis-cli ping".into()))
        );
        assert_eq!(
            HealthTest::Args(vec!["CMD".into(), "pg_isready".into()]).to_cmd(),
            Some(TestCmd::Argv(vec!["pg_isready".into()]))
        );
        assert_eq!(
            HealthTest::Args(vec!["CMD-SHELL".into(), "redis-cli ping".into()]).to_cmd(),
            Some(TestCmd::Shell("redis-cli ping".into()))
        );
        // 無前綴 → 整段 argv
        assert_eq!(
            HealthTest::Args(vec!["true".into()]).to_cmd(),
            Some(TestCmd::Argv(vec!["true".into()]))
        );
        // 空 / NONE → None
        assert_eq!(HealthTest::Args(vec!["NONE".into()]).to_cmd(), None);
        assert_eq!(HealthTest::Args(vec![]).to_cmd(), None);
        assert_eq!(HealthTest::Shell("  ".into()).to_cmd(), None);
    }

    #[test]
    fn healthcheck_valid_passes() {
        let app = parse_raw(
            "version: \"0.1\"\nname: App\nservices:\n  db:\n    image: ./db.tar\n    healthcheck:\n      test: [\"CMD\", \"redis-cli\", \"ping\"]\n      interval: 2s\n      timeout: 5s\n      retries: 5\n",
        );
        assert!(app.validate().is_ok(), "{:?}", app.validate());
    }

    #[test]
    fn healthcheck_rejects_empty_test_and_bad_fields() {
        let e = validate_err(
            "version: \"0.1\"\nname: App\nservices:\n  db:\n    image: ./db.tar\n    healthcheck:\n      test: [\"NONE\"]\n",
        );
        assert!(e.contains("healthcheck.test"), "{e}");

        let e = validate_err(
            "version: \"0.1\"\nname: App\nservices:\n  db:\n    image: ./db.tar\n    healthcheck:\n      test: \"x\"\n      interval: 0s\n",
        );
        assert!(e.contains("healthcheck.interval"), "{e}");

        let e = validate_err(
            "version: \"0.1\"\nname: App\nservices:\n  db:\n    image: ./db.tar\n    healthcheck:\n      test: \"x\"\n      retries: 0\n",
        );
        assert!(e.contains("healthcheck.retries"), "{e}");

        let e = validate_err(
            "version: \"0.1\"\nname: App\nservices:\n  db:\n    image: ./db.tar\n    healthcheck:\n      test: \"x\"\n      timeout: bogus\n",
        );
        assert!(e.contains("healthcheck.timeout"), "{e}");
    }

    // ---------- console ----------

    #[test]
    fn console_defaults_to_auto() {
        let app = parse_raw("version: \"0.1\"\nname: App\nservices:\n  db: { image: ./db.tar }\n");
        assert_eq!(app.console, crate::ConsoleMode::Auto);
    }

    #[test]
    fn console_hidden_rejected_when_no_interface() {
        // 全 none + console: hidden → 無從停止 → 拒絕。
        let e = validate_err(
            "version: \"0.1\"\nname: App\nconsole: hidden\nservices:\n  db: { image: ./db.tar }\n",
        );
        assert!(e.contains("console: hidden"), "{e}");
    }

    #[test]
    fn console_hidden_ok_with_gui_or_terminal() {
        for mode in ["gui", "terminal", "both"] {
            let app = parse_raw(&format!(
                "version: \"0.1\"\nname: App\nconsole: hidden\nservices:\n  ui: {{ image: ./u.tar, interface_mode: {mode} }}\n"
            ));
            assert!(
                app.validate().is_ok(),
                "console: hidden + {mode} 應通過：{:?}",
                app.validate()
            );
        }
    }

    // ---------- 錯誤彙整 ----------

    #[test]
    fn collects_all_errors_at_once() {
        let e = validate_err(
            r#"
version: "9.9"
name: "1bad name"
services:
  Db:
    image: ./db.tar
    ports: ["abc:80"]
    persist_path: not/absolute
"#,
        );
        assert!(e.contains("unsupported version"), "{e}");
        assert!(e.contains("name"), "{e}");
        assert!(e.contains("service name"), "{e}");
        assert!(e.contains("ports"), "{e}");
        assert!(e.contains("persist_path"), "{e}");
        assert!(e.lines().count() >= 5, "應收集所有錯誤：{e}");
    }
}
