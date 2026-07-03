//! GPU passthrough（opt-in per-service `gpu: true`）——見 docs/DESIGN.md「GPU passthrough」節。
//!
//! 探測 host 既有的 GPU 裝置節點（`/dev/dxg`、`/dev/dri`、`/dev/nvidia*`）與 WSL2 的
//! host 驅動 userspace libs（`/usr/lib/wsl/lib`），把存在者 bind 進服務容器，讓容器內的
//! app 能做 GPU 計算/算繪。**只在能實際觸及 host GPU 的後端有意義**：原生 Linux
//! （namespaces）與 Windows WSL2。WHP / macOS VM 的 appliance host 沒有這些節點；
//! guest-agent 以 `gpu_host` 旗標把關（見 exec.rs / lib.rs），在那些後端回明確錯誤。
//!
//! **原生 NVIDIA userspace lib 注入**（`/proc/driver/nvidia/version` 存在時）：把 host 上與
//! driver 版本相符的 `libcuda`/`libnvidia-*`（`.so.<driver-version>`，如 NVIDIA Container
//! Toolkit）綁進容器暫存目錄並加進 `LD_LIBRARY_PATH`——container image 自帶的 lib 版本
//! 未必與 host kernel module 相符，故不能用 image 的。WSL2 則靠 `/usr/lib/wsl/lib`（WSL
//! 已備相符 libs）免此步。
//!
//! **soname**：每個 `libX.so.<ver>` 以多重 bind mount 綁到 versioned + `libX.so.1`（soname，
//! 執行期 DT_NEEDED 用）+ `libX.so`（dev/dlopen 用）三個容器路徑，取代在容器內建 symlink。
//! ABI major 假設為 1（涵蓋 CUDA/NVML/compute 常見情形；讀 DT_SONAME 精準化列後續）。

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

/// 原生 NVIDIA driver 版本檔（driver 已載入時存在）。
const NVIDIA_VERSION_PROC: &str = "/proc/driver/nvidia/version";
/// host NVIDIA userspace lib 的標準搜尋目錄。
const NVIDIA_LIB_DIRS: &[&str] = &[
    "/usr/lib/x86_64-linux-gnu",
    "/usr/lib64",
    "/usr/lib",
    "/lib/x86_64-linux-gnu",
    "/usr/lib/aarch64-linux-gnu",
];
/// 注入的 NVIDIA libs 在容器內的暫存目錄（加進 LD_LIBRARY_PATH）。
const NVIDIA_STAGE: &str = "/run/chefer-nvidia";

/// 以真實 host 路徑探測（`/dev` + `/usr/lib/wsl/lib` + 原生 NVIDIA userspace libs）。
pub fn collect() -> GpuPassthrough {
    let mut out = collect_from(Path::new("/dev"), Path::new("/usr/lib/wsl/lib"));
    // 原生 NVIDIA：driver 已載入（/proc 有版本）→ 注入版本相符的 userspace libs。
    if let Ok(text) = std::fs::read_to_string(NVIDIA_VERSION_PROC)
        && let Some(ver) = parse_nvidia_version(&text)
    {
        let dirs: Vec<PathBuf> = NVIDIA_LIB_DIRS.iter().map(PathBuf::from).collect();
        let libs = nvidia_lib_binds(&dirs, &ver, NVIDIA_STAGE);
        if !libs.is_empty() {
            out.binds.extend(libs);
            out.ld_library_paths.push(NVIDIA_STAGE.to_string());
        }
    }
    out
}

/// 從 `/proc/driver/nvidia/version` 內容解析 driver 版本（如 `535.183.01`）。
/// 取第一個形如 `\d+(\.\d+)+` 的 token。
fn parse_nvidia_version(proc_text: &str) -> Option<String> {
    for tok in proc_text.split_whitespace() {
        let mut parts = tok.split('.');
        let ok = tok.contains('.')
            && parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        if ok {
            return Some(tok.to_string());
        }
    }
    None
}

/// 找 host 上與 driver 版本相符的 NVIDIA userspace libs（`libcuda`/`libnvidia-*` 且以
/// `.so.<version>` 結尾），每個綁到容器 `stage` 下的 versioned + `.so.1` + `.so` 三個名字。
fn nvidia_lib_binds(lib_dirs: &[PathBuf], version: &str, stage: &str) -> Vec<GpuBind> {
    let suffix = format!(".so.{version}");
    let mut binds = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for dir in lib_dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in rd.flatten() {
            let fname = e.file_name().to_string_lossy().into_owned();
            if !(fname.starts_with("libcuda.so.") || fname.starts_with("libnvidia-")) {
                continue;
            }
            let Some(base) = fname.strip_suffix(&suffix) else {
                continue;
            };
            // base 不含更多 '.'（避免 libnvidia-ml.so.<ver>.extra 之類誤配）。
            if base.contains(".so") || !seen.insert(fname.clone()) {
                continue;
            }
            let host = e.path();
            for guest_name in [fname.clone(), format!("{base}.so.1"), format!("{base}.so")] {
                binds.push(GpuBind {
                    host: host.clone(),
                    guest: format!("{stage}/{guest_name}"),
                    read_only: true,
                    is_dir: false,
                });
            }
        }
    }
    binds
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

    #[test]
    fn parses_nvidia_driver_version() {
        let text = "NVRM version: NVIDIA UNIX x86_64 Kernel Module  535.183.01  Sun May 12 UTC 2024\n\
                    GCC version:  gcc version 12.3.0\n";
        assert_eq!(parse_nvidia_version(text).as_deref(), Some("535.183.01"));
        // 兩段式也接受。
        assert_eq!(
            parse_nvidia_version("Kernel Module 550.90 x").as_deref(),
            Some("550.90")
        );
        assert_eq!(parse_nvidia_version("no version here").as_deref(), None);
    }

    #[test]
    fn selects_version_matched_nvidia_libs() {
        let base = std::env::temp_dir().join(format!("chefer-nvlib-{}", std::process::id()));
        let d = base.join("lib");
        std::fs::create_dir_all(&d).unwrap();
        let ver = "535.183.01";
        for n in [
            "libcuda.so.535.183.01",
            "libnvidia-ml.so.535.183.01",
            "libnvidia-ptxjitcompiler.so.535.183.01",
        ] {
            std::fs::File::create(d.join(n)).unwrap();
        }
        // 不相符/非 nvidia/非 versioned：不選。
        std::fs::File::create(d.join("libcuda.so.470.1.2")).unwrap(); // 舊版本
        std::fs::File::create(d.join("libfoo.so.535.183.01")).unwrap(); // 非 nvidia
        std::fs::File::create(d.join("libnvidia-ml.so.1")).unwrap(); // soname 符號檔

        let binds = nvidia_lib_binds(std::slice::from_ref(&d), ver, "/run/chefer-nvidia");
        let guests: BTreeSet<String> = binds.iter().map(|b| b.guest.clone()).collect();
        // 每個相符 lib → versioned + .so.1 + .so 三個名字（同一 host 檔多重 bind）。
        assert!(guests.contains("/run/chefer-nvidia/libcuda.so.535.183.01"));
        assert!(guests.contains("/run/chefer-nvidia/libcuda.so.1"));
        assert!(guests.contains("/run/chefer-nvidia/libcuda.so"));
        assert!(guests.contains("/run/chefer-nvidia/libnvidia-ml.so.1"));
        assert!(guests.contains("/run/chefer-nvidia/libnvidia-ptxjitcompiler.so.1"));
        // 舊版本/非 nvidia 不選。
        assert!(
            !guests
                .iter()
                .any(|g| g.contains("470") || g.contains("libfoo"))
        );
        // 3 個相符 lib × 3 名 = 9；全唯讀、非目錄。
        assert_eq!(binds.len(), 9);
        assert!(binds.iter().all(|b| b.read_only && !b.is_dir));

        std::fs::remove_dir_all(&base).ok();
    }
}
