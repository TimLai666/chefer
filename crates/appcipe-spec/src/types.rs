use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// appcipe.yml 的頂層結構。
///
/// 注意：本型別只負責「解析 + 驗證」；host 路徑絕對化等正規化邏輯
/// 位於 `appcipe-normalize` crate（正式載入入口為 `appcipe_normalize::load`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppCipe {
    pub version: String,
    pub name: String,

    #[serde(default)]
    pub app_version: Option<String>,

    #[serde(default)]
    pub old_names: Vec<String>,

    #[serde(default)]
    pub data_dir: Option<String>,

    /// 當機策略；舊欄位名 `crash_policy` 以 serde alias 相容。
    #[serde(default, alias = "crash_policy")]
    pub crash: CrashPolicy,

    /// app 的網路模式（見 docs/DESIGN.md「網路隔離」節）。
    #[serde(default)]
    pub network: NetworkMode,
    pub services: HashMap<String, Service>,
}

/// app 級網路模式。
///
/// **目標預設是 `bridge`**（對齊 Docker：app 有自己的私有網路、可出網、只開宣告的 port）；
/// 但在 `bridge` 於各後端做完並通過 E2E 前，**`#[default]` 暫時維持 `Shared`**，避免 ship
/// 出「預設模式尚未實作」的壞版本。待 P3 完成再把預設旗標翻成 `Bridge`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    /// 共用 host/VM 的 netns；`ports:` 直接生效但未宣告的 port 仍可從 host 連到（不隔離）。
    #[default]
    Shared,
    /// 自己的 app netns，只有 `lo`：服務間以 127.0.0.1 互通、宣告的 port 經 relay 對外，
    /// **無對外網路**（對齊 Docker compose 的 `internal: true`）。
    Internal,
    /// 同 `Internal` 再加 bundled pasta 提供出網 NAT（對齊 Docker 預設 bridge 網路）。
    Bridge,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Service {
    pub image: ImageSourceOrPath,

    #[serde(default)]
    pub cmd: Option<Cmd>,

    #[serde(default)]
    pub workdir: Option<String>,

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageSourceType {
    Tar,
    Dockerfile,
    Image, // 直接拉現有 image
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ImageFormat {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    // 標準（skopeo/containers）寫法為連字號；同時接受底線形式以相容。
    #[serde(rename = "docker-archive", alias = "docker_archive")]
    DockerArchive,
    #[serde(rename = "oci-archive", alias = "oci_archive")]
    OciArchive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ImagePlatform {
    #[serde(
        rename = "linux/amd64",
        alias = "linux_amd64",
        alias = "amd64",
        alias = "x86_64",
        alias = "x86-64"
    )]
    #[default]
    LinuxAmd64,

    #[serde(
        rename = "linux/arm64",
        alias = "linux_arm64",
        alias = "arm64",
        alias = "aarch64"
    )]
    LinuxArm64,

    // 如果要打包 Windows image，也支援 windows/amd64
    #[serde(
        rename = "windows/amd64",
        alias = "windows_amd64",
        alias = "win64",
        alias = "x86_64-windows"
    )]
    WindowsAmd64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum CrashPolicy {
    #[default]
    FailFast,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Cmd {
    String(String),
    Array(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum InterfaceMode {
    Gui,
    Terminal,
    Both,
    #[default]
    None, // 如果要顯式表示沒有
}
