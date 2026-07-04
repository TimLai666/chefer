//! manifest.json 的 serde 型別（schema 見 docs/DESIGN.md §3）。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::mounts::MountSpec;
use crate::ports::PortSpec;

/// 目前的 manifest 格式版本。
pub const MANIFEST_FORMAT_VERSION: u32 = 1;

/// per-service GPU 請求：關 / 全部 / 指定 NVIDIA 卡索引（硬隔離）。
///
/// serde untagged，向後相容舊的布林寫法：`false` = 關、`true` = 全部 GPU、
/// `[0, 2]` = 只綁 `/dev/nvidia0`、`/dev/nvidia2`（其餘 nvidia 數字節點不綁）。
/// 舊 recipe/manifest 的 `gpu: true`/`false` 照樣解析成 `All(_)`，故 spec/manifest 版號不動。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GpuRequest {
    /// `false` = 關；`true` = 全部 GPU。
    All(bool),
    /// 指定的 host NVIDIA 卡索引（對應 `/dev/nvidia<N>`）；硬隔離，其餘 nvidia 節點不進容器。
    Devices(Vec<u32>),
}

impl Default for GpuRequest {
    fn default() -> Self {
        GpuRequest::All(false)
    }
}

impl GpuRequest {
    /// 是否要 GPU passthrough（`true` 或非空清單）。
    pub fn enabled(&self) -> bool {
        match self {
            GpuRequest::All(b) => *b,
            GpuRequest::Devices(d) => !d.is_empty(),
        }
    }

    /// 指定的卡索引（`None` = 全部；`Some(&[])` 不會出現，見 `enabled`）。
    pub fn devices(&self) -> Option<&[u32]> {
        match self {
            GpuRequest::Devices(d) => Some(d),
            GpuRequest::All(_) => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    pub app: AppMeta,
    /// 依 name 排序（輸出確定性）。
    pub services: Vec<ServiceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppMeta {
    pub name: String,
    #[serde(default)]
    pub app_version: Option<String>,
    pub spec_version: String,
    #[serde(default)]
    pub old_names: Vec<String>,
    #[serde(default)]
    pub data_dir_override: Option<String>,
    #[serde(default)]
    pub crash: CrashPolicy,
    /// app 網路模式（見 docs/DESIGN.md「網路隔離」節）。舊 bundle 無此欄位 → 預設 `shared`。
    #[serde(default)]
    pub network: NetworkMode,
    /// 共用主控台顯示策略（runtime 依此 + 介面模式決定是否隱藏 Windows console）。
    #[serde(default)]
    pub console: ConsoleMode,
    pub generated_at_utc: String,
    /// 打包此 app 的 chefer 版本（`chefer build` 寫入；供 `chefer inspect` 與
    /// runtime 顯示「這是哪一版 chefer 包的」）。舊 bundle 無此欄位 → 反序列化為空字串。
    #[serde(default)]
    pub builder_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceEntry {
    pub name: String,
    /// 例如 "linux/amd64"。
    pub platform: String,
    /// 依套用順序排列的 diff 層。
    pub layers: Vec<LayerRef>,
    pub image_config: ImageConfig,
    #[serde(default)]
    pub cmd_override: Option<CmdSpec>,
    /// appcipe 指定的環境變數（覆蓋 image env）。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub workdir_override: Option<String>,
    #[serde(default)]
    pub persist_path: Option<String>,
    #[serde(default)]
    pub ports: Vec<PortSpec>,
    #[serde(default)]
    pub mounts: Vec<MountSpec>,
    #[serde(default)]
    pub interface_mode: InterfaceMode,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// 選填健康檢查；驅動 depends_on 的 wait-until-ready（見 docs/DESIGN.md「健康檢查」）。
    #[serde(default)]
    pub healthcheck: Option<HealthCheck>,
    /// opt-in GPU passthrough（見 docs/DESIGN.md「GPU passthrough」節）。舊 bundle 無此欄位 →
    /// 反序列化為 `All(false)`。`true` = 全部 GPU、`[0,2]` = 只綁指定 NVIDIA 卡（硬隔離）。
    /// guest-agent 依此決定要把哪些 host GPU 節點 bind 進此服務容器
    /// （僅原生 Linux / WSL2 後端；WHP/vz 回明確錯誤）。
    #[serde(default)]
    pub gpu: GpuRequest,
}

/// 服務健康檢查（runtime 端解讀）。duration 一律已正規化為毫秒。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    /// 檢查命令：`shell` → `/bin/sh -c <s>`；`argv` → 直接 exec。
    pub test: CmdSpec,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub retries: u32,
    #[serde(default)]
    pub start_period_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerRef {
    /// 相對 bundle 根的路徑，例如 "services/db/layers/0000-abcdef123456.tar.zst"。
    pub rel_path: String,
    /// 未壓縮 layer tar 的 sha256（"sha256:<64hex>"）。
    pub diff_id: String,
    /// zstd 壓縮後的檔案大小（bytes）。
    #[serde(default)]
    pub size: Option<u64>,
}

/// 從 OCI image config 抽出的執行所需欄位。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageConfig {
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
    /// "KEY=VALUE" 形式（沿用 OCI 慣例）。
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    /// 例如 "5432/tcp"。
    #[serde(default)]
    pub exposed_ports: Vec<String>,
}

/// appcipe `cmd:` 的覆蓋語意：只取代 image 的 Cmd，不取代 Entrypoint。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmdSpec {
    /// 字串形式；guest 端轉為 ["/bin/sh", "-c", s]。
    Shell(String),
    /// 陣列形式；直接作為 argv。
    Argv(Vec<String>),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashPolicy {
    #[default]
    FailFast,
}

/// app 網路模式（見 docs/DESIGN.md「網路隔離」節）。
///
/// guest-agent 依此決定是否建立 per-app netns、是否起 pasta 出網。**預設為 `Bridge`**。
/// 注意：舊 bundle 的 manifest 無此欄位 → serde 反序列化時會套用此預設（`Bridge`），
/// 等同把舊 bundle 視為 bridge；如需沿用舊「共用 host 網路」行為請以新版重打包並寫
/// `network: shared`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    /// 共用 host/VM netns（不隔離；現況）。
    Shared,
    /// 自己的 app netns，只有 lo；宣告的 port 經 relay 對外；無對外網路。
    Internal,
    /// 同 Internal 再加 pasta 出網 NAT（Docker 預設 bridge 等價）。
    #[default]
    Bridge,
}

/// app 共用主控台顯示策略（runtime 端解讀；見 docs/DESIGN.md「主控台顯示」節）。
///
/// 舊 bundle 無此欄位 → 反序列化套用預設 `Auto`。runtime 依此 + 聚合介面模式決定是否
/// 隱藏 Windows console（其他平台不受影響）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleMode {
    /// 依介面模式自動：有 terminal/both → 顯示；只有 gui → 隱藏；全 none → 顯示。
    #[default]
    Auto,
    /// 一律顯示共用主控台。
    Shown,
    /// 隱藏共用主控台（建置期已驗證 app 另有 gui 或 terminal 可關閉）。
    Hidden,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceMode {
    Gui,
    Terminal,
    Both,
    #[default]
    None,
}

impl InterfaceMode {
    pub fn wants_gui(self) -> bool {
        matches!(self, InterfaceMode::Gui | InterfaceMode::Both)
    }
    pub fn wants_terminal(self) -> bool {
        matches!(self, InterfaceMode::Terminal | InterfaceMode::Both)
    }
}

impl ServiceEntry {
    /// 有效啟動命令 = entrypoint + (cmd_override 或 image cmd)。
    /// Shell 形式轉為 ["/bin/sh", "-c", s] 接在 entrypoint 後。
    pub fn effective_command(&self) -> Result<Vec<String>> {
        let mut argv = self.image_config.entrypoint.clone();
        match &self.cmd_override {
            Some(CmdSpec::Shell(s)) => {
                argv.extend(["/bin/sh".to_string(), "-c".to_string(), s.clone()]);
            }
            Some(CmdSpec::Argv(v)) => argv.extend(v.iter().cloned()),
            None => argv.extend(self.image_config.cmd.iter().cloned()),
        }
        if argv.is_empty() {
            bail!(
                "service `{}` has no command to run: the image defines no Entrypoint/Cmd and the appcipe specifies no cmd",
                self.name
            );
        }
        Ok(argv)
    }

    /// 合併後的環境變數：image env 為基底，appcipe env 覆蓋；
    /// 最終缺 PATH 時補上預設 PATH。
    pub fn effective_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        for kv in &self.image_config.env {
            if let Some((k, v)) = kv.split_once('=') {
                env.insert(k.to_string(), v.to_string());
            }
        }
        for (k, v) in &self.env {
            env.insert(k.clone(), v.clone());
        }
        env.entry("PATH".to_string()).or_insert_with(|| {
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string()
        });
        env
    }
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read manifest: {}", path.display()))?;
        let m: Manifest = serde_json::from_str(&s)
            .with_context(|| format!("failed to parse manifest: {}", path.display()))?;
        if m.format_version != MANIFEST_FORMAT_VERSION {
            bail!(
                "unsupported manifest format version {} (this build supports {}); repack with a matching chefer version",
                m.format_version,
                MANIFEST_FORMAT_VERSION
            );
        }
        Ok(m)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = serde_json::to_string_pretty(self)?;
        std::fs::write(path, s)
            .with_context(|| format!("failed to write manifest: {}", path.display()))?;
        Ok(())
    }

    pub fn service(&self, name: &str) -> Option<&ServiceEntry> {
        self.services.iter().find(|s| s.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str) -> ServiceEntry {
        ServiceEntry {
            name: name.into(),
            platform: "linux/amd64".into(),
            layers: vec![],
            image_config: ImageConfig::default(),
            cmd_override: None,
            env: BTreeMap::new(),
            workdir_override: None,
            persist_path: None,
            ports: vec![],
            mounts: vec![],
            interface_mode: InterfaceMode::None,
            depends_on: vec![],
            healthcheck: None,
            gpu: GpuRequest::All(false),
        }
    }

    #[test]
    fn effective_command_prefers_override() {
        let mut s = svc("a");
        s.image_config.entrypoint = vec!["ep".into()];
        s.image_config.cmd = vec!["default".into()];
        s.cmd_override = Some(CmdSpec::Argv(vec!["run".into()]));
        assert_eq!(s.effective_command().unwrap(), vec!["ep", "run"]);

        s.cmd_override = Some(CmdSpec::Shell("echo hi".into()));
        assert_eq!(
            s.effective_command().unwrap(),
            vec!["ep", "/bin/sh", "-c", "echo hi"]
        );

        s.cmd_override = None;
        assert_eq!(s.effective_command().unwrap(), vec!["ep", "default"]);
    }

    #[test]
    fn effective_command_empty_is_error() {
        let s = svc("a");
        assert!(s.effective_command().is_err());
    }

    #[test]
    fn effective_env_merges_and_adds_path() {
        let mut s = svc("a");
        s.image_config.env = vec!["A=1".into(), "B=2".into()];
        s.env.insert("B".into(), "3".into());
        let env = s.effective_env();
        assert_eq!(env["A"], "1");
        assert_eq!(env["B"], "3");
        assert!(env.contains_key("PATH"));
    }

    #[test]
    fn cmd_spec_serde_shape() {
        let shell: CmdSpec = serde_json::from_str(r#"{"shell": "echo hi"}"#).unwrap();
        assert!(matches!(shell, CmdSpec::Shell(_)));
        let argv: CmdSpec = serde_json::from_str(r#"{"argv": ["a", "b"]}"#).unwrap();
        assert!(matches!(argv, CmdSpec::Argv(_)));
    }

    #[test]
    fn manifest_roundtrip() {
        let m = Manifest {
            format_version: MANIFEST_FORMAT_VERSION,
            app: AppMeta {
                name: "App".into(),
                app_version: None,
                spec_version: "0.1".into(),
                old_names: vec![],
                data_dir_override: None,
                crash: CrashPolicy::FailFast,
                network: NetworkMode::Shared,
                console: ConsoleMode::Auto,
                generated_at_utc: "2026-01-01T00:00:00Z".into(),
                builder_version: "9.9.9".into(),
            },
            services: vec![svc("a")],
        };
        let dir = std::env::temp_dir().join("chefer-bundle-test-manifest");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("manifest.json");
        m.save(&p).unwrap();
        let m2 = Manifest::load(&p).unwrap();
        assert_eq!(m2.app.name, "App");
        assert_eq!(m2.services.len(), 1);
    }
}
