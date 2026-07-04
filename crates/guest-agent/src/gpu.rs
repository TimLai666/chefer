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
//! **soname**：每個 `libX.so.<ver>` 以多重 bind mount 綁到 versioned + 真實 **DT_SONAME**
//! （直接解析 ELF 讀出，如 `libcuda.so.1`；讀不到才退回 `libX.so.1` 假設）+ `libX.so`
//! （dev/dlopen 用）三個容器路徑，取代在容器內建 symlink。

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

/// 是否為 NVIDIA 驅動的 userspace lib（`base` 已去掉 `.so.<version>` 尾巴）。對齊
/// `nvidia-container-cli list` 的庫集：CUDA/NVML/PTX-JIT/NVVM/OpenCL（`libcuda*`/`libnvidia-*`）、
/// NVENC/NVDEC（`libnvcuvid`）、OptiX（`libnvoptix`）、GLVND 廠商庫
/// （`libGLX_nvidia`/`libEGL_nvidia`/`libGLESv*_nvidia`）。搭配 `.so.<driver-version>` 尾巴的
/// 前置過濾，只會命中驅動版號的庫（`libcudart` 等 toolkit 庫版號不同、不會誤中）。
fn is_nvidia_driver_lib(base: &str) -> bool {
    base.starts_with("libcuda")        // libcuda, libcudadebugger
        || base.starts_with("libnvidia-")
        || base.starts_with("libnvcuvid") // NVENC/NVDEC
        || base.starts_with("libnvoptix") // OptiX
        || base.ends_with("_nvidia") // libGLX_nvidia / libEGL_nvidia / libGLESv*_nvidia
}

/// 找 host 上與 driver 版本相符的 NVIDIA userspace libs（`is_nvidia_driver_lib` 認定，且以
/// `.so.<version>` 結尾），每個綁到容器 `stage` 下的 versioned + soname + `.so`（dev）名字。
/// soname 優先讀 ELF 的 DT_SONAME（精確），讀不到才退回 `libX.so.1`（ABI major 假設 1）。
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
            let Some(base) = fname.strip_suffix(&suffix) else {
                continue;
            };
            // base 不含更多 '.'（避免 libnvidia-ml.so.<ver>.extra 之類誤配）；
            // 且須為 NVIDIA 驅動庫（對齊 nvidia-container-cli list）。
            if base.contains(".so") || !is_nvidia_driver_lib(base) || !seen.insert(fname.clone()) {
                continue;
            }
            let host = e.path();
            // 真實 DT_SONAME（如 libcuda.so.1）；讀不到（空檔/非 ELF）則退回 .so.1 假設。
            let soname = read_soname(&host).unwrap_or_else(|| format!("{base}.so.1"));
            // versioned + soname + dev(.so)；去重（soname 可能等於 versioned）。
            let mut names: BTreeSet<String> = BTreeSet::new();
            names.insert(fname.clone());
            names.insert(soname);
            names.insert(format!("{base}.so"));
            for guest_name in names {
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

fn u16_le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn u32_le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn u64_le(b: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}

/// 讀 ELF64-LE 共享物件的 `DT_SONAME`（如 `libcuda.so.1`）。非 ELF64-LE / 無 SONAME → None。
/// 只 seek+read 需要的區段（不整檔載入；NVIDIA libs 可達數十 MB）。
fn read_soname(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let mut hdr = [0u8; 64]; // ELF64 header
    f.read_exact(&mut hdr).ok()?;
    if &hdr[0..4] != b"\x7fELF" || hdr[4] != 2 || hdr[5] != 1 {
        return None; // 只支援 ELF64 little-endian（x86_64 / aarch64 皆是）
    }
    let e_phoff = u64_le(&hdr, 32);
    let e_phentsize = u16_le(&hdr, 54) as usize;
    let e_phnum = u16_le(&hdr, 56) as usize;
    if e_phentsize < 56 || e_phnum == 0 || e_phnum > 4096 {
        return None;
    }
    let mut ph = vec![0u8; e_phentsize * e_phnum];
    f.seek(SeekFrom::Start(e_phoff)).ok()?;
    f.read_exact(&mut ph).ok()?;
    let mut dynamic: Option<(u64, u64)> = None; // (offset, filesz)
    let mut loads: Vec<(u64, u64, u64)> = Vec::new(); // (vaddr, offset, filesz)
    for i in 0..e_phnum {
        let b = &ph[i * e_phentsize..];
        match u32_le(b, 0) {
            2 => dynamic = Some((u64_le(b, 8), u64_le(b, 32))), // PT_DYNAMIC
            1 => loads.push((u64_le(b, 16), u64_le(b, 8), u64_le(b, 32))), // PT_LOAD
            _ => {}
        }
    }
    let (doff, dsize) = dynamic?;
    if dsize == 0 || dsize > (1 << 20) {
        return None;
    }
    let mut dyn_buf = vec![0u8; dsize as usize];
    f.seek(SeekFrom::Start(doff)).ok()?;
    f.read_exact(&mut dyn_buf).ok()?;
    let mut soname_off: Option<u64> = None;
    let mut strtab_vaddr: Option<u64> = None;
    let mut i = 0;
    while i + 16 <= dyn_buf.len() {
        let d_tag = i64::from_le_bytes(dyn_buf[i..i + 8].try_into().ok()?);
        let d_val = u64_le(&dyn_buf, i + 8);
        match d_tag {
            0 => break,                      // DT_NULL
            14 => soname_off = Some(d_val),  // DT_SONAME（strtab 內偏移）
            5 => strtab_vaddr = Some(d_val), // DT_STRTAB（虛擬位址）
            _ => {}
        }
        i += 16;
    }
    let soname_off = soname_off?;
    let strtab_vaddr = strtab_vaddr?;
    // strtab 虛擬位址 → 檔內偏移（找涵蓋它的 PT_LOAD）。
    let strtab_off = loads.iter().find_map(|(v, o, sz)| {
        (strtab_vaddr >= *v && strtab_vaddr < v.checked_add(*sz)?).then(|| o + (strtab_vaddr - v))
    })?;
    let name_off = strtab_off.checked_add(soname_off)?;
    f.seek(SeekFrom::Start(name_off)).ok()?;
    let mut nbuf = [0u8; 256];
    let n = f.read(&mut nbuf).ok()?;
    let end = nbuf[..n].iter().position(|&b| b == 0).unwrap_or(n);
    if end == 0 {
        return None;
    }
    std::str::from_utf8(&nbuf[..end]).ok().map(str::to_string)
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
        // `/usr/lib/wsl/lib` 內的 `libcuda`/`libnvidia-ml` 其實是 loader shim，會 dlopen
        // 真正的驅動實作（`/usr/lib/wsl/drivers/<inf>/…`，來自 Windows DriverStore，經 WSL
        // 以 9p 掛在 `/usr/lib/wsl/drivers`）。只綁 lib 而不綁這個 drivers store，容器內
        // 的 CUDA/nvidia-smi 會回「couldn't communicate with the NVIDIA driver / Driver Not
        // Loaded」。一般 WSL distro 兩者皆由 WSL 自動掛載；容器換了 mount namespace，須把
        // drivers store 一併帶入（nvidia-container-toolkit 的 WSL 模式亦綁此目錄）。
        if let Some(drivers) = wsl_lib_dir.parent().map(|p| p.join("drivers"))
            && drivers.is_dir()
        {
            out.binds.push(GpuBind {
                host: drivers,
                guest: "/usr/lib/wsl/drivers".to_string(),
                read_only: true,
                is_dir: true,
            });
        }
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
        let drivers = base.join("drivers"); // wsl_lib_dir 的兄弟目錄 → /usr/lib/wsl/drivers
        std::fs::create_dir_all(&dri).unwrap();
        std::fs::create_dir_all(&wsl).unwrap();
        std::fs::create_dir_all(&drivers).unwrap();
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
        // WSL 驅動 store（loader shim dlopen 真驅動之處）也要一併綁進容器。
        let drv_bind = pt
            .binds
            .iter()
            .find(|b| b.guest == "/usr/lib/wsl/drivers")
            .expect("WSL drivers store should be bound alongside /usr/lib/wsl/lib");
        assert!(drv_bind.is_dir && drv_bind.read_only);

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
        // 涵蓋 nvidia-container-cli list 的庫集：CUDA/NVML/PTX-JIT + NVENC(libnvcuvid) +
        // OptiX(libnvoptix) + GL 廠商庫(libGLX_nvidia) + libcuda* 家族(libcudadebugger)。
        let matched = [
            "libcuda",
            "libcudadebugger",
            "libnvidia-ml",
            "libnvidia-ptxjitcompiler",
            "libnvcuvid",
            "libnvoptix",
            "libGLX_nvidia",
        ];
        for m in matched {
            std::fs::File::create(d.join(format!("{m}.so.535.183.01"))).unwrap();
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
        // 擴充庫集（此前被漏掉的）：NVENC/OptiX/GL/libcuda* 家族皆入選。
        assert!(guests.contains("/run/chefer-nvidia/libnvcuvid.so"));
        assert!(guests.contains("/run/chefer-nvidia/libnvoptix.so"));
        assert!(guests.contains("/run/chefer-nvidia/libGLX_nvidia.so"));
        assert!(guests.contains("/run/chefer-nvidia/libcudadebugger.so"));
        // 舊版本/非 nvidia 不選。
        assert!(
            !guests
                .iter()
                .any(|g| g.contains("470") || g.contains("libfoo"))
        );
        // 7 個相符 lib × 3 名 = 21；全唯讀、非目錄。
        assert_eq!(binds.len(), 21);
        assert!(binds.iter().all(|b| b.read_only && !b.is_dir));

        std::fs::remove_dir_all(&base).ok();
    }

    /// 造一個最小 ELF64-LE 共享物件，其 `DT_SONAME` = `soname`（PT_LOAD 讓 vaddr==offset）。
    fn crafted_elf_with_soname(soname: &str) -> Vec<u8> {
        let dyn_off = 176u64;
        let strtab_off = 224u64;
        let mut strtab = vec![0u8]; // 前導 NUL
        strtab.extend_from_slice(soname.as_bytes());
        strtab.push(0);
        let total = strtab_off as usize + strtab.len();
        let mut buf = vec![0u8; total];
        buf[0..4].copy_from_slice(b"\x7fELF");
        buf[4] = 2; // ELF64
        buf[5] = 1; // little-endian
        buf[16..18].copy_from_slice(&3u16.to_le_bytes()); // e_type ET_DYN
        buf[18..20].copy_from_slice(&0x3eu16.to_le_bytes()); // e_machine x86_64
        buf[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
        buf[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        buf[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
        buf[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        buf[56..58].copy_from_slice(&2u16.to_le_bytes()); // e_phnum
        // PH0 PT_LOAD @64：offset 0 / vaddr 0 / filesz total → vaddr==檔內偏移。
        let p0 = 64usize;
        buf[p0..p0 + 4].copy_from_slice(&1u32.to_le_bytes());
        buf[p0 + 4..p0 + 8].copy_from_slice(&5u32.to_le_bytes());
        buf[p0 + 32..p0 + 40].copy_from_slice(&(total as u64).to_le_bytes());
        buf[p0 + 40..p0 + 48].copy_from_slice(&(total as u64).to_le_bytes());
        // PH1 PT_DYNAMIC @120。
        let p1 = 120usize;
        buf[p1..p1 + 4].copy_from_slice(&2u32.to_le_bytes());
        buf[p1 + 8..p1 + 16].copy_from_slice(&dyn_off.to_le_bytes());
        buf[p1 + 16..p1 + 24].copy_from_slice(&dyn_off.to_le_bytes());
        buf[p1 + 32..p1 + 40].copy_from_slice(&48u64.to_le_bytes());
        buf[p1 + 40..p1 + 48].copy_from_slice(&48u64.to_le_bytes());
        // dynamic @176：DT_SONAME(14, off=1), DT_STRTAB(5, vaddr=224), DT_NULL(0,0)。
        let put = |buf: &mut [u8], idx: usize, tag: i64, val: u64| {
            let o = dyn_off as usize + idx * 16;
            buf[o..o + 8].copy_from_slice(&tag.to_le_bytes());
            buf[o + 8..o + 16].copy_from_slice(&val.to_le_bytes());
        };
        put(&mut buf, 0, 14, 1);
        put(&mut buf, 1, 5, strtab_off);
        put(&mut buf, 2, 0, 0);
        buf[strtab_off as usize..].copy_from_slice(&strtab);
        buf
    }

    #[test]
    fn reads_dt_soname_from_elf() {
        let base = std::env::temp_dir().join(format!("chefer-soname-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let p = base.join("libcuda.so.535.183.01");
        std::fs::write(&p, crafted_elf_with_soname("libcuda.so.1")).unwrap();
        assert_eq!(read_soname(&p).as_deref(), Some("libcuda.so.1"));
        // 非 ELF / 空檔 → None（呼叫端 fallback 到 .so.1）。
        let q = base.join("notelf");
        std::fs::write(&q, b"not an elf").unwrap();
        assert_eq!(read_soname(&q), None);
        std::fs::remove_dir_all(&base).ok();
    }
}
