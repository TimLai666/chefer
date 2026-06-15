//! 綁定掛載 "<host>:<guest>" 的解析與型別。
//! 解析時從右往左切一次 ':'，以相容 Windows 磁碟代號（"C:\dir:/data"）。

use std::fmt;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountSpec {
    /// Host 端路徑（打包前已絕對化）。
    pub host: String,
    /// 容器內路徑（必須以 '/' 開頭）。
    pub guest: String,
    #[serde(default)]
    pub read_only: bool,
}

impl MountSpec {
    /// 解析 "<host>:<guest>"。
    pub fn parse(s: &str) -> Result<Self> {
        let mut it = s.rsplitn(2, ':');
        let guest = it.next().unwrap_or_default();
        let Some(host) = it.next() else {
            bail!("invalid mount format, expected \"<host_path>:<container_path>\": {s}");
        };
        if host.is_empty() {
            bail!("mount host path must not be empty: {s}");
        }
        if !guest.starts_with('/') {
            bail!("mount container path must start with '/': {s}");
        }
        Ok(MountSpec {
            host: host.to_string(),
            guest: guest.to_string(),
            read_only: false,
        })
    }
}

impl fmt::Display for MountSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.guest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_unix_host() {
        let m = MountSpec::parse("/bigdata:/mnt/data").unwrap();
        assert_eq!(m.host, "/bigdata");
        assert_eq!(m.guest, "/mnt/data");
    }

    #[test]
    fn parse_windows_drive_host() {
        let m = MountSpec::parse(r"C:\data\presets:/app/presets").unwrap();
        assert_eq!(m.host, r"C:\data\presets");
        assert_eq!(m.guest, "/app/presets");
    }

    #[test]
    fn rejects_bad_input() {
        assert!(MountSpec::parse("no-colon").is_err());
        assert!(MountSpec::parse("/host:relative/guest").is_err());
        assert!(MountSpec::parse(":/guest").is_err());
    }
}
