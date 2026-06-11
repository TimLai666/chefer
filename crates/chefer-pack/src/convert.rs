//! appcipe 型別 → chefer_bundle manifest 型別的轉換。

use anyhow::{Result, bail};
use appcipe_spec::{Cmd, ImagePlatform, ImageSourceOrPath, ImageSourceType, Service};
use chefer_bundle::{CmdSpec, ImageConfig};

use crate::image::ImageConfigJson;

/// service 的目標平台字串；image 短寫（TarPath）預設 linux/amd64。
pub(crate) fn platform_of(svc: &Service) -> String {
    match &svc.image {
        ImageSourceOrPath::TarPath(_) => "linux/amd64".to_string(),
        ImageSourceOrPath::Full { platform, .. } => match platform {
            ImagePlatform::LinuxAmd64 => "linux/amd64",
            ImagePlatform::LinuxArm64 => "linux/arm64",
            ImagePlatform::WindowsAmd64 => "windows/amd64",
        }
        .to_string(),
    }
}

/// 取得 service 的 image tar 路徑；非 tar 來源回報「尚未支援」。
pub(crate) fn image_tar_path<'a>(name: &str, svc: &'a Service) -> Result<&'a str> {
    match &svc.image {
        ImageSourceOrPath::TarPath(p) => Ok(p),
        ImageSourceOrPath::Full { source, file, .. } => match source {
            ImageSourceType::Tar => Ok(file),
            ImageSourceType::Dockerfile => bail!(
                "service `{name}`：image.source=dockerfile 尚未支援；\
                 請先以 `docker build` + `docker save -o <file>` 匯出 tar 後改用 source=tar"
            ),
            ImageSourceType::Image => bail!(
                "service `{name}`：image.source=image 尚未支援；\
                 請先以 `docker save -o <file> <image>` 匯出 tar 後改用 source=tar"
            ),
        },
    }
}

/// appcipe 的 cmd 覆蓋 → manifest 的 CmdSpec。
pub(crate) fn to_cmd_spec(cmd: Option<&Cmd>) -> Option<CmdSpec> {
    match cmd {
        Some(Cmd::String(s)) => Some(CmdSpec::Shell(s.clone())),
        Some(Cmd::Array(v)) => Some(CmdSpec::Argv(v.clone())),
        None => None,
    }
}

pub(crate) fn to_interface_mode(m: &appcipe_spec::InterfaceMode) -> chefer_bundle::InterfaceMode {
    match m {
        appcipe_spec::InterfaceMode::Gui => chefer_bundle::InterfaceMode::Gui,
        appcipe_spec::InterfaceMode::Terminal => chefer_bundle::InterfaceMode::Terminal,
        appcipe_spec::InterfaceMode::Both => chefer_bundle::InterfaceMode::Both,
        appcipe_spec::InterfaceMode::None => chefer_bundle::InterfaceMode::None,
    }
}

/// 寬鬆解析後的 image config → manifest 的 ImageConfig。
/// 空字串的 WorkingDir / User 視為未設定。
pub(crate) fn to_image_config(cfg: &ImageConfigJson) -> ImageConfig {
    let rc = cfg.config.as_ref();
    let none_if_empty = |s: Option<&String>| s.filter(|v| !v.is_empty()).cloned();
    ImageConfig {
        entrypoint: rc.and_then(|c| c.entrypoint.clone()).unwrap_or_default(),
        cmd: rc.and_then(|c| c.cmd.clone()).unwrap_or_default(),
        env: rc.and_then(|c| c.env.clone()).unwrap_or_default(),
        working_dir: rc.and_then(|c| none_if_empty(c.working_dir.as_ref())),
        user: rc.and_then(|c| none_if_empty(c.user.as_ref())),
        exposed_ports: rc
            .and_then(|c| c.exposed_ports.as_ref())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::RuntimeConfigJson;

    #[test]
    fn image_config_conversion_handles_nulls_and_empty() {
        // 完全空的 config
        let cfg = ImageConfigJson::default();
        let ic = to_image_config(&cfg);
        assert!(ic.entrypoint.is_empty());
        assert!(ic.cmd.is_empty());
        assert!(ic.working_dir.is_none());
        assert!(ic.exposed_ports.is_empty());

        // 含 ExposedPorts 與空字串 WorkingDir
        let json = r#"{
            "architecture": "amd64", "os": "linux",
            "config": {
                "Env": ["A=1"], "Cmd": null, "Entrypoint": ["/e"],
                "WorkingDir": "", "User": "app",
                "ExposedPorts": {"80/tcp": {}, "53/udp": {}}
            },
            "unknown_field": 123
        }"#;
        let cfg: ImageConfigJson = serde_json::from_str(json).unwrap();
        let ic = to_image_config(&cfg);
        assert_eq!(ic.entrypoint, vec!["/e"]);
        assert!(ic.cmd.is_empty());
        assert_eq!(ic.env, vec!["A=1"]);
        assert!(ic.working_dir.is_none());
        assert_eq!(ic.user.as_deref(), Some("app"));
        // BTreeMap keys → 排序後輸出
        assert_eq!(ic.exposed_ports, vec!["53/udp", "80/tcp"]);
    }

    #[test]
    fn cmd_spec_conversion() {
        assert!(matches!(
            to_cmd_spec(Some(&Cmd::String("echo hi".into()))),
            Some(CmdSpec::Shell(_))
        ));
        assert!(matches!(
            to_cmd_spec(Some(&Cmd::Array(vec!["a".into()]))),
            Some(CmdSpec::Argv(_))
        ));
        assert!(to_cmd_spec(None).is_none());
    }

    #[test]
    fn runtime_config_is_lenient() {
        // 整個 config 為 null 也要能解析
        let cfg: ImageConfigJson =
            serde_json::from_str(r#"{"os":"linux","architecture":"amd64","config":null}"#).unwrap();
        assert!(cfg.config.is_none());
        let rc: RuntimeConfigJson = serde_json::from_str(r#"{"Env": null}"#).unwrap();
        assert!(rc.env.is_none());
    }
}
