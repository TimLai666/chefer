//! 連接埠映射 "host:guest[/proto]" 的解析與型別。

use std::fmt;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortProto {
    Tcp,
    Udp,
}

impl fmt::Display for PortProto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PortProto::Tcp => write!(f, "tcp"),
            PortProto::Udp => write!(f, "udp"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortSpec {
    pub host: u16,
    pub guest: u16,
    pub proto: PortProto,
}

impl PortSpec {
    /// 解析 "host:guest[/proto]"（proto 預設 tcp；埠號 1..=65535）。
    pub fn parse(s: &str) -> Result<Self> {
        let (mapping, proto) = match s.split_once('/') {
            Some((m, p)) => {
                let proto = match p.to_ascii_lowercase().as_str() {
                    "tcp" => PortProto::Tcp,
                    "udp" => PortProto::Udp,
                    other => {
                        bail!("unsupported protocol `{other}` (only tcp/udp are supported): {s}")
                    }
                };
                (m, proto)
            }
            None => (s, PortProto::Tcp),
        };
        let Some((h, g)) = mapping.split_once(':') else {
            bail!("invalid port mapping format, expected \"host:guest[/proto]\": {s}");
        };
        let host = parse_port(h, s)?;
        let guest = parse_port(g, s)?;
        Ok(PortSpec { host, guest, proto })
    }
}

fn parse_port(p: &str, whole: &str) -> Result<u16> {
    let n: u32 = p
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("port is not a number: `{p}` (in \"{whole}\")"))?;
    if n == 0 || n > 65535 {
        bail!("port out of range 1..=65535: `{p}` (in \"{whole}\")");
    }
    Ok(n as u16)
}

impl fmt::Display for PortSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}/{}", self.host, self.guest, self.proto)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let p = PortSpec::parse("5432:5432").unwrap();
        assert_eq!(p.host, 5432);
        assert_eq!(p.guest, 5432);
        assert_eq!(p.proto, PortProto::Tcp);
    }

    #[test]
    fn parse_udp() {
        let p = PortSpec::parse("9000:9001/udp").unwrap();
        assert_eq!(p.host, 9000);
        assert_eq!(p.guest, 9001);
        assert_eq!(p.proto, PortProto::Udp);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(PortSpec::parse("5432").is_err());
        assert!(PortSpec::parse("0:80").is_err());
        assert!(PortSpec::parse("80:99999").is_err());
        assert!(PortSpec::parse("80:80/icmp").is_err());
        assert!(PortSpec::parse("abc:80").is_err());
    }
}
