//! GPU passthrough（opt-in per-service `gpu: true`）——見 docs/DESIGN.md「GPU passthrough」節。
//!
//! 探測 host 既有的 GPU 裝置節點（`/dev/dxg`、`/dev/dri`、`/dev/nvidia*`）與 WSL2 的
//! host 驅動 userspace libs（`/usr/lib/wsl/lib`），把存在者 bind 進服務容器，讓容器內的
//! app 能做 GPU 計算/算繪。**只在能實際觸及 host GPU 的後端有意義**：原生 Linux
//! （namespaces）與 Windows WSL2。WHP / macOS VM 的 appliance host 沒有這些節點；
//! guest-agent 以 `gpu_host` 旗標把關（見 exec.rs / lib.rs），在那些後端回明確錯誤。
//!
//! 只做「節點綁定」——原生 Linux 的 host 相符 userspace lib 自動注入列後續階段（見 DESIGN）。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// 一筆 GPU 綁定：host 來源 + 容器內絕對路徑 + 屬性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuBind {
    /// host 端實體來源路徑。
    pub host: PathBuf,
    /// 容器內絕對路徑（正規、與 host 實際位置無關）。
    pub guest: String,
    /// 唯讀掛載（驅動 lib 目錄 = 唯讀；裝置節點 = 讀寫）。
    pub read_only: bool,
    /// 來源是否為目錄（決定綁定時建目錄或空檔案掛載點）。
    pub is_dir: bool,
}

/// 探測結果：要綁的節點/libs + 要追加到容器 `LD_LIBRARY_PATH` 的路徑。
#[derive(Debug, Default, Clone)]
pub struct GpuPassthrough {
    pub binds: Vec<GpuBind>,
    /// 追加到容器 `LD_LIBRARY_PATH` 的路徑（目前僅 WSL2 的 `/usr/lib/wsl/lib`）。
    pub ld_library_paths: Vec<String>,
}

impl GpuPassthrough {
    /// 一個節點都沒探到 → 呼叫端據此對 `gpu: true` 的服務回明確錯誤。
    pub fn is_empty(&self) -> bool {
        self.binds.is_empty()
    }
}

/// 固定候選節點名稱（`/dev/<name>`）——存在才綁；nvidia 數字節點於執行期列舉補上。
const DEV_NODES: &[&str] = &[
    "dxg",        // WSL2 GPU-PV
    "dri",        // DRM render/display（目錄）
    "nvidiactl",  // NVIDIA control
    "nvidia-uvm", // NVIDIA unified memory
    "nvidia-uvm-tools",
    "nvidia-modeset",
    "nvidia-caps", // NVIDIA capabilities（目錄）
];

/// 已知為目錄的節點（其餘視為 char device / 檔案掛載點）。
fn is_dir_node(name: &str) -> bool {
    matches!(name, "dri" | "nvidia-caps")
}

/// 以真實 host 路徑探測（`/dev` + `/usr/lib/wsl/lib`）。
pub fn collect() -> GpuPassthrough {
    collect_from(Path::new("/dev"), Path::new("/usr/lib/wsl/lib"))
}

/// 供測試注入 dev 目錄與 wsl lib 目錄；只依「存在性」判斷（不檢查 device 型別，
/// 便於單元測試以一般檔案模擬節點——真實 host 上這些本就是裝置節點）。
pub fn collect_from(dev_dir: &Path, wsl_lib_dir: &Path) -> GpuPassthrough {
    let mut out = GpuPassthrough::default();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for name in DEV_NODES {
        push_node(dev_dir, name, &mut out, &mut seen);
    }

    // nvidia 數字節點（nvidia0, nvidia1, …）——執行期列舉 `/dev`。
    if let Ok(rd) = std::fs::read_dir(dev_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(rest) = name.strip_prefix("nvidia")
                && !rest.is_empty()
                && rest.chars().all(|c| c.is_ascii_digit())
            {
                push_node(dev_dir, &name, &mut out, &mut seen);
            }
        }
    }

    // WSL2：host 驅動 userspace libs（版本與 host kernel module 相符 → CUDA/DirectML/…可用）。
    if wsl_lib_dir.is_dir() {
        out.binds.push(GpuBind {
            host: wsl_lib_dir.to_path_buf(),
            guest: "/usr/lib/wsl/lib".to_string(),
            read_only: true,
            is_dir: true,
        });
        out.ld_library_paths.push("/usr/lib/wsl/lib".to_string());
    }

    out
}

fn push_node(dev_dir: &Path, name: &str, out: &mut GpuPassthrough, seen: &mut BTreeSet<String>) {
    if !seen.insert(name.to_string()) {
        return;
    }
    let host = dev_dir.join(name);
    if !host.exists() {
        return;
    }
    out.binds.push(GpuBind {
        host,
        guest: format!("/dev/{name}"),
        read_only: false,
        is_dir: is_dir_node(name),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_existing_dev_nodes_and_wsl_libs() {
        let base = std::env::temp_dir().join(format!("chefer-gpu-test-{}", std::process::id()));
        let dev = base.join("dev");
        let dri = dev.join("dri");
        let wsl = base.join("wsl-lib");
        std::fs::create_dir_all(&dri).unwrap();
        std::fs::create_dir_all(&wsl).unwrap();
        // 以一般檔案模擬 char device 節點。
        for n in ["dxg", "nvidiactl", "nvidia-uvm", "nvidia0", "nvidia1"] {
            std::fs::File::create(dev.join(n)).unwrap();
        }

        let pt = collect_from(&dev, &wsl);
        let guests: BTreeSet<String> = pt.binds.iter().map(|b| b.guest.clone()).collect();
        assert!(guests.contains("/dev/dxg"));
        assert!(guests.contains("/dev/dri"));
        assert!(guests.contains("/dev/nvidiactl"));
        assert!(guests.contains("/dev/nvidia-uvm"));
        assert!(guests.contains("/dev/nvidia0"));
        assert!(guests.contains("/dev/nvidia1"));
        assert!(guests.contains("/usr/lib/wsl/lib"));
        // 不存在的節點不綁。
        assert!(!guests.contains("/dev/nvidia-modeset"));

        // dri 與 wsl lib 是目錄；dxg 是檔案節點；wsl lib 唯讀且進 LD path。
        let dri_bind = pt.binds.iter().find(|b| b.guest == "/dev/dri").unwrap();
        assert!(dri_bind.is_dir && !dri_bind.read_only);
        let dxg_bind = pt.binds.iter().find(|b| b.guest == "/dev/dxg").unwrap();
        assert!(!dxg_bind.is_dir && !dxg_bind.read_only);
        let wsl_bind = pt
            .binds
            .iter()
            .find(|b| b.guest == "/usr/lib/wsl/lib")
            .unwrap();
        assert!(wsl_bind.is_dir && wsl_bind.read_only);
        assert_eq!(pt.ld_library_paths, vec!["/usr/lib/wsl/lib".to_string()]);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn empty_when_no_gpu_present() {
        let base = std::env::temp_dir().join(format!("chefer-gpu-empty-{}", std::process::id()));
        let dev = base.join("dev");
        std::fs::create_dir_all(&dev).unwrap();
        let missing_wsl = base.join("nope");

        let pt = collect_from(&dev, &missing_wsl);
        assert!(pt.is_empty());
        assert!(pt.ld_library_paths.is_empty());

        std::fs::remove_dir_all(&base).ok();
    }
}
