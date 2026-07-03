use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// appcipe.yml 的頂層結構。
///
/// 注意：本型別只負責「解析 + 驗證」；host 路徑絕對化等正規化邏輯
/// 位於 `appcipe-normalize` crate（正式載入入口為 `appcipe_normalize::load`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)] // 擋打錯的頂層欄位名（否則 serde 靜默忽略 → app 行為悄悄消失）
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

    /// app 共用主控台（彙整日誌 + Ctrl+C 的終端視窗）的顯示策略。
    #[serde(default)]
    pub console: ConsoleMode,
    pub services: HashMap<String, Service>,
}

/// app 共用主控台的顯示策略（主要影響 Windows 雙擊啟動時那個 console 視窗）。
///
/// 不影響 stdio 模型本身：互動終端仍最多一個（terminal/both 服務），其餘服務輸出仍以
/// `[svc]` 前綴彙整到共用主控台。此設定只決定那個共用主控台「要不要顯示」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleMode {
    /// 依介面模式自動：有 terminal/both → 顯示；只有 gui → 隱藏（關窗即停）；全 none → 顯示（保留停止介面）。
    #[default]
    Auto,
    /// 一律顯示共用主控台。
    Shown,
    /// 隱藏共用主控台（**僅當 app 另有 gui 或 terminal/both 可關閉時允許**——否則 app 將無從停止）。
    Hidden,
}

/// app 級網路模式。
///
/// **預設為 `bridge`**（對齊 Docker：app 有自己的私有網路、可出網、只開宣告的 port）。
/// 自 native Linux + WSL2 驗證通過後正式採用（見 docs/DESIGN.md「網路隔離」）。
/// 省略 `network:` 的舊 appcipe 因此會從「共用 host 網路」改為隔離——這是 pre-1.0 的
/// 刻意行為變更；需要原本「未宣告的 port 也可從 host 連到」者請顯式寫 `network: shared`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    /// 共用 host/VM 的 netns；`ports:` 直接生效但未宣告的 port 仍可從 host 連到（不隔離）。
    Shared,
    /// 自己的 app netns，只有 `lo`：服務間以 127.0.0.1 互通、宣告的 port 經 relay 對外，
    /// **無對外網路**（對齊 Docker compose 的 `internal: true`）。
    Internal,
    /// 同 `Internal` 再加 bundled pasta 提供出網 NAT（對齊 Docker 預設 bridge 網路）。
    #[default]
    Bridge,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)] // 擋打錯的欄位名，例如 port→ports、command→cmd、volumes→mounts
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

    /// 選填的健康檢查（對齊 Docker HEALTHCHECK）；驅動 depends_on 的 wait-until-ready。
    #[serde(default)]
    pub healthcheck: Option<HealthCheck>,

    /// opt-in GPU passthrough（見 docs/DESIGN.md「GPU passthrough」節）。預設關；
    /// 開啟後 guest-agent 把 host GPU 裝置節點（+ WSL2 的驅動 libs）bind 進此服務容器。
    /// **僅原生 Linux 與 Windows WSL2 後端可行**（WHP/vz VM 會於啟動時回明確錯誤）。
    #[serde(default)]
    pub gpu: bool,
}

/// 服務健康檢查（見 docs/DESIGN.md「健康檢查」節）。duration 欄位接受 `<n>(ms|s|m)`
/// 或裸整數（= 秒）；省略時套用 [`HealthCheck`] 的 `DEFAULT_*`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheck {
    /// 檢查命令；字串 = `sh -c`，陣列比照 Docker（`CMD`/`CMD-SHELL`/`NONE` 前綴）。
    pub test: HealthTest,
    #[serde(default)]
    pub interval: Option<String>,
    #[serde(default)]
    pub timeout: Option<String>,
    #[serde(default)]
    pub retries: Option<u32>,
    #[serde(default)]
    pub start_period: Option<String>,
}

/// `healthcheck.test`：字串（→ `sh -c`）或字串陣列（Docker 風格）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HealthTest {
    Shell(String),
    Args(Vec<String>),
}

/// `test` 正規化後的可執行形式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestCmd {
    /// 經 `/bin/sh -c <字串>` 執行。
    Shell(String),
    /// 直接作為 argv 執行。
    Argv(Vec<String>),
}

impl HealthTest {
    /// 正規化為可執行命令；`None` 代表「空 / NONE」（無效，驗證期會擋）。
    pub fn to_cmd(&self) -> Option<TestCmd> {
        match self {
            HealthTest::Shell(s) if !s.trim().is_empty() => Some(TestCmd::Shell(s.clone())),
            HealthTest::Shell(_) => None,
            HealthTest::Args(v) => match v.split_first() {
                None => None,
                Some((head, rest)) => match head.as_str() {
                    "NONE" => None,
                    "CMD-SHELL" => {
                        let s = rest.join(" ");
                        (!s.trim().is_empty()).then_some(TestCmd::Shell(s))
                    }
                    "CMD" => (!rest.is_empty()).then(|| TestCmd::Argv(rest.to_vec())),
                    // 無已知前綴 → 整段視為 argv。
                    _ => Some(TestCmd::Argv(v.clone())),
                },
            },
        }
    }
}

impl HealthCheck {
    pub const DEFAULT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
    pub const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    pub const DEFAULT_RETRIES: u32 = 10;
    pub const DEFAULT_START_PERIOD: std::time::Duration = std::time::Duration::ZERO;

    /// 解析後的 interval（省略 → 預設）。
    pub fn interval(&self) -> Result<std::time::Duration, String> {
        opt_duration(&self.interval, Self::DEFAULT_INTERVAL)
    }
    pub fn timeout(&self) -> Result<std::time::Duration, String> {
        opt_duration(&self.timeout, Self::DEFAULT_TIMEOUT)
    }
    pub fn start_period(&self) -> Result<std::time::Duration, String> {
        opt_duration(&self.start_period, Self::DEFAULT_START_PERIOD)
    }
    pub fn retries(&self) -> u32 {
        self.retries.unwrap_or(Self::DEFAULT_RETRIES)
    }
}

fn opt_duration(
    raw: &Option<String>,
    default: std::time::Duration,
) -> Result<std::time::Duration, String> {
    match raw {
        None => Ok(default),
        Some(s) => parse_duration(s),
    }
}

/// 解析時間長度：`<n>ms` / `<n>s` / `<n>m`，或裸整數（= 秒）。
pub fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("duration must not be empty".to_string());
    }
    let (num, unit_ms): (&str, u64) = if let Some(n) = t.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = t.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = t.strip_suffix('m') {
        (n, 60_000)
    } else {
        (t, 1000) // 裸數字 = 秒
    };
    let num = num.trim();
    let value: u64 = num.parse().map_err(|_| {
        format!(
            "cannot parse duration `{s}` (accepts <n>ms/<n>s/<n>m or a bare integer in seconds)"
        )
    })?;
    value
        .checked_mul(unit_ms)
        .map(std::time::Duration::from_millis)
        .ok_or_else(|| format!("duration `{s}` overflows"))
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

        /// 僅 `source: dockerfile` 使用：build context 目錄；省略 → Dockerfile 所在目錄。
        #[serde(default)]
        context: Option<String>,

        /// 僅 `source: dockerfile` 使用：傳給 builder 的 `--build-arg`。
        #[serde(default)]
        build_args: HashMap<String, String>,
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
