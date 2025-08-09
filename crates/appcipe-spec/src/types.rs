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
    pub crash: Option<CrashPolicy>,
    pub services: HashMap<String, Service>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Service {
    pub image: ImageSourceOrPath,

    #[serde(default)]
    pub cmd: Option<Cmd>,

    #[serde(default)]
    pub workdir: Option<String>,

    #[serde(default)]
    pub env: Option<HashMap<String, String>>,

    #[serde(default)]
    pub persist_path: Option<String>,

    #[serde(default)]
    pub ports: Option<Vec<String>>,

    #[serde(default)]
    pub mounts: Option<Vec<String>>,

    #[serde(default)]
    pub interface_mode: Option<InterfaceMode>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ImageSourceOrPath {
    TarPath(String),
    Full {
        source: ImageSourceType,
        file: String,

        #[serde(default)]
        format: Option<String>,

        #[serde(default)]
        platform: Option<String>,
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
pub enum CrashPolicy {
    FailFast,
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
