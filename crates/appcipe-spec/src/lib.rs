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
    pub crash_policy: Option<String>,
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
    pub interface_mode: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ImageSourceOrPath {
    TarPath(String),
    Full {
        source: String,
        file: String,

        #[serde(default)]
        format: Option<String>,

        #[serde(default)]
        platform: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Cmd {
    String(String),
    Array(Vec<String>),
}

use std::fs;

pub fn parse_appcipe_from_file(path: &str) -> Result<AppCipe, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;
    let appcipe: AppCipe = serde_yaml::from_str(&content)?;
    Ok(appcipe)
}
