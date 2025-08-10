use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct AppCipe {
    pub version: String,
    pub name: String,

    #[serde(default)]
    pub app_version: Option<String>,

    #[serde(default)]
    pub old_names: Vec<String>,

    #[serde(default)]
    pub data_dir: Option<String>,

    #[serde(default)]
    pub crash: CrashPolicy,
    pub services: HashMap<String, Service>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Service {
    pub image: ImageSourceOrPath,

    #[serde(default)]
    pub cmd: Option<Cmd>,

    #[serde(default)]
    pub workdir:  Option<String>,

    #[serde(default)]
    pub env: HashMap<String, String>,

    #[serde(default)]
    pub persist_path: Option<String>,

    #[serde(default)]
    pub ports: Vec<String>,

    #[serde(default)]
    pub mounts: Vec<String>,

    #[serde(default)]
    pub interface_mode: InterfaceMode,

    #[serde(default)]
    pub depends_on: Vec<String>,
}


#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ImageSourceOrPath {
    TarPath(String),
    Full {
        source: ImageSourceType,
        file: String,

        #[serde(default)]
        format: ImageFormat,

        #[serde(default)]
        platform: ImagePlatform,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageSourceType {
    Tar,
    Dockerfile,
    Image, // 直接拉現有 image
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    Auto,
    DockerArchive,
    OciArchive,
}

impl Default for ImageFormat {
    fn default() -> Self {
        ImageFormat::Auto
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImagePlatform {
    #[serde(rename = "linux/amd64", alias = "linux_amd64", alias = "amd64", alias = "x86_64", alias = "x86-64")]
    LinuxAmd64,

    #[serde(rename = "linux/arm64", alias = "linux_arm64", alias = "arm64", alias = "aarch64")]
    LinuxArm64,

    // 如果要打包 Windows image，也支援 windows/amd64
    #[serde(rename = "windows/amd64", alias = "windows_amd64", alias = "win64", alias = "x86_64-windows")]
    WindowsAmd64,
}

impl Default for ImagePlatform {
    fn default() -> Self {
        ImagePlatform::LinuxAmd64
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashPolicy {
    FailFast,
}

impl Default for CrashPolicy {
    fn default() -> Self {
        CrashPolicy::FailFast
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Cmd {
    String(String),
    Array(Vec<String>),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceMode {
    Gui,
    Terminal,
    Both,
    None, // 如果要顯式表示沒有
}

impl Default for InterfaceMode {
    fn default() -> Self {
        InterfaceMode::None
    }
}
