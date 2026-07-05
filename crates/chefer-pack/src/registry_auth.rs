//! 私有 registry 認證（build 時 `source: image` 拉取用；docs/DESIGN.md「image 來源」節）。
//!
//! 解析順序：
//! 1. **`CHEFER_REGISTRY_AUTH` 環境變數** — `user:pass`（第一個冒號後全為密碼），套用到
//!    本次 build 連到的**任何** registry（CI 打一組憑證最方便；多 registry 不同憑證請用
//!    docker config）。
//! 2. **Docker 設定檔**（`docker login` 產物）：`$DOCKER_CONFIG/config.json` 或
//!    `~/.docker/config.json`（Windows 為 `%USERPROFILE%\.docker\config.json`）的 `auths`
//!    條目——支援 `auth`（base64 的 `user:pass`）與明文 `username`/`password` 欄位；
//!    Docker Hub 的歷史別名鍵（`https://index.docker.io/v1/` 等）一併比對。
//!
//! **credsStore / credHelpers（外部 credential helper 程式）不支援**——那類條目的密碼存在
//! helper 的保管庫裡、config.json 沒有可讀的憑證，會被跳過（fallback 匿名；401 的錯誤訊息
//! 會提示改用上述兩種方式）。兩者皆無 → 匿名（公開 image 行為不變）。

use std::path::PathBuf;

/// 解析到的一組 Basic 憑證與其來源（來源只用於使用者可見的提示訊息，不含機密）。
pub(crate) struct ResolvedAuth {
    pub user: String,
    pub pass: String,
    /// "CHEFER_REGISTRY_AUTH" 或 "docker config"。
    pub source: &'static str,
}

/// 依 registry 主機名解析憑證；找不到（或格式不符）→ None（匿名）。
pub(crate) fn resolve(registry: &str) -> Option<ResolvedAuth> {
    if let Ok(v) = std::env::var("CHEFER_REGISTRY_AUTH") {
        if let Some((user, pass)) = parse_user_pass(&v) {
            return Some(ResolvedAuth {
                user,
                pass,
                source: "CHEFER_REGISTRY_AUTH",
            });
        }
        eprintln!(
            "[chefer] warning: CHEFER_REGISTRY_AUTH is set but not in `user:pass` form; ignoring it"
        );
    }
    let path = docker_config_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let (user, pass) = auth_from_docker_config(&text, registry)?;
    Some(ResolvedAuth {
        user,
        pass,
        source: "docker config",
    })
}

/// `user:pass` → (user, pass)。第一個冒號分隔（密碼可含冒號）；user 空 → None。
fn parse_user_pass(v: &str) -> Option<(String, String)> {
    let (user, pass) = v.split_once(':')?;
    if user.is_empty() {
        return None;
    }
    Some((user.to_string(), pass.to_string()))
}

/// Docker 設定檔路徑：`$DOCKER_CONFIG/config.json` 優先，否則 `~/.docker/config.json`
/// （HOME；Windows 退 USERPROFILE）。不存在 → None。
fn docker_config_path() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("DOCKER_CONFIG") {
        let p = PathBuf::from(d).join("config.json");
        return p.is_file().then_some(p);
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let p = PathBuf::from(home).join(".docker").join("config.json");
    p.is_file().then_some(p)
}

/// `auths` 內可能代表 `registry` 的鍵。一般 registry：裸主機名與 http(s):// 前綴形；
/// Docker Hub：官方 CLI 寫入的是 `https://index.docker.io/v1/`，而 image reference 的
/// registry 是 `docker.io`（oci-client 正規化）→ 把歷史別名全列入。
fn registry_key_candidates(registry: &str) -> Vec<String> {
    let mut v = vec![
        registry.to_string(),
        format!("https://{registry}"),
        format!("http://{registry}"),
    ];
    let hubish = matches!(
        registry,
        "docker.io" | "index.docker.io" | "registry-1.docker.io" | "registry.hub.docker.com"
    );
    if hubish {
        for k in [
            "docker.io",
            "index.docker.io",
            "registry-1.docker.io",
            "https://index.docker.io/v1/",
            "https://index.docker.io/v2/",
            "https://registry-1.docker.io",
        ] {
            if !v.iter().any(|x| x == k) {
                v.push(k.to_string());
            }
        }
    }
    v
}

/// 從 docker config.json 文字解析 `registry` 的 Basic 憑證（純函式，供單元測試）。
/// 支援 `username`+`password` 明文欄位與 `auth`（base64 `user:pass`）；`identitytoken`
/// 等無內嵌帳密的條目讀不出 → 視同無此條目。
fn auth_from_docker_config(json_text: &str, registry: &str) -> Option<(String, String)> {
    let root: serde_json::Value = serde_json::from_str(json_text).ok()?;
    let auths = root.get("auths")?.as_object()?;
    for key in registry_key_candidates(registry) {
        let Some(entry) = auths.get(&key) else {
            continue;
        };
        if let (Some(u), Some(p)) = (
            entry.get("username").and_then(|v| v.as_str()),
            entry.get("password").and_then(|v| v.as_str()),
        ) && !u.is_empty()
        {
            return Some((u.to_string(), p.to_string()));
        }
        if let Some(b64) = entry.get("auth").and_then(|v| v.as_str()) {
            use base64::Engine as _;
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64.trim())
                && let Ok(s) = String::from_utf8(bytes)
                && let Some(a) = parse_user_pass(&s)
            {
                return Some(a);
            }
        }
        // 鍵存在但讀不出帳密（credsStore/identitytoken/壞格式）→ 試下一個候選鍵。
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_pass_splits_on_first_colon_only() {
        assert_eq!(
            parse_user_pass("alice:s3cr3t"),
            Some(("alice".into(), "s3cr3t".into()))
        );
        // 密碼可含冒號（第一個冒號後全是密碼）。
        assert_eq!(
            parse_user_pass("alice:pa:ss:wd"),
            Some(("alice".into(), "pa:ss:wd".into()))
        );
        // 空密碼合法（少見但 registry 端才知道）；空 user / 無冒號 → None。
        assert_eq!(parse_user_pass("alice:"), Some(("alice".into(), "".into())));
        assert_eq!(parse_user_pass(":pass"), None);
        assert_eq!(parse_user_pass("nocolon"), None);
        assert_eq!(parse_user_pass(""), None);
    }

    #[test]
    fn docker_config_auth_field_base64() {
        // echo -n "bob:hunter2" | base64 → Ym9iOmh1bnRlcjI=
        let json = r#"{"auths": {"registry.example.com": {"auth": "Ym9iOmh1bnRlcjI="}}}"#;
        assert_eq!(
            auth_from_docker_config(json, "registry.example.com"),
            Some(("bob".into(), "hunter2".into()))
        );
        // 其他主機不誤配。
        assert_eq!(auth_from_docker_config(json, "ghcr.io"), None);
    }

    #[test]
    fn docker_config_username_password_fields() {
        let json = r#"{"auths": {"ghcr.io": {"username": "u", "password": "p"}}}"#;
        assert_eq!(
            auth_from_docker_config(json, "ghcr.io"),
            Some(("u".into(), "p".into()))
        );
    }

    #[test]
    fn docker_hub_alias_keys_match() {
        // docker login（無參數）寫入的是 v1 別名鍵；reference 的 registry 是 docker.io。
        let json = r#"{"auths": {"https://index.docker.io/v1/": {"auth": "Ym9iOmh1bnRlcjI="}}}"#;
        assert_eq!(
            auth_from_docker_config(json, "docker.io"),
            Some(("bob".into(), "hunter2".into()))
        );
        assert_eq!(
            auth_from_docker_config(json, "registry-1.docker.io"),
            Some(("bob".into(), "hunter2".into()))
        );
        // 非 Hub 主機不吃 Hub 別名。
        assert_eq!(auth_from_docker_config(json, "registry.example.com"), None);
    }

    #[test]
    fn https_prefixed_key_matches_bare_host() {
        let json = r#"{"auths": {"https://registry.example.com": {"auth": "Ym9iOmh1bnRlcjI="}}}"#;
        assert_eq!(
            auth_from_docker_config(json, "registry.example.com"),
            Some(("bob".into(), "hunter2".into()))
        );
    }

    #[test]
    fn unreadable_entries_fall_through_to_none() {
        // identitytoken-only（cred helper / OAuth）：config 裡沒有可讀帳密 → None（匿名）。
        let json = r#"{"auths": {"registry.example.com": {"identitytoken": "eyJ..."}}}"#;
        assert_eq!(auth_from_docker_config(json, "registry.example.com"), None);
        // auth 不是合法 base64 / 解出來沒有冒號 → None。
        let bad1 = r#"{"auths": {"r.example.com": {"auth": "!!!not-base64!!!"}}}"#;
        assert_eq!(auth_from_docker_config(bad1, "r.example.com"), None);
        let nocolon = r#"{"auths": {"r.example.com": {"auth": "bm9jb2xvbg=="}}}"#; // "nocolon"
        assert_eq!(auth_from_docker_config(nocolon, "r.example.com"), None);
        // 整份 JSON 壞掉 / 無 auths → None。
        assert_eq!(auth_from_docker_config("{not json", "x"), None);
        assert_eq!(auth_from_docker_config(r#"{"psst": 1}"#, "x"), None);
    }
}
