//! macOS `vz` 後端的純函式：appliance（kernel+initramfs）查找、VM 資源計算、
//! kernel command line 組裝、guest 共享掛載標籤。
//!
//! 全部與作業系統無關，跨平台皆可編譯與測試（實際的 Virtualization.framework
//! 開機在 `vz.rs`，僅 macOS 編譯、僅能在實體 Mac 驗證）。

use std::path::{Path, PathBuf};

/// guest 內 virtiofs 共享標籤：bundle（唯讀）與 data（讀寫）。
/// initramfs 的 `/init` 依此標籤掛載。
pub const SHARE_TAG_BUNDLE: &str = "chefer-bundle";
pub const SHARE_TAG_DATA: &str = "chefer-data";

/// guest 內的掛載點（initramfs 約定）。
pub const GUEST_BUNDLE_DIR: &str = "/mnt/bundle";
pub const GUEST_DATA_DIR: &str = "/mnt/data";

/// VM 記憶體預設（MiB）與下限。
pub const DEFAULT_MEMORY_MIB: u64 = 1536;
pub const MIN_MEMORY_MIB: u64 = 512;
/// VM vCPU 數上限（避免在多核 host 上過度配置）。
pub const MAX_VCPUS: usize = 4;

/// macOS micro-VM appliance 的兩個檔案路徑。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliancePaths {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
}

/// 從已解壓 bundle 的 `vm/` 目錄找 appliance（kernel + initramfs 都在才回傳）。
pub fn appliance_in_bundle(bundle_dir: &Path, arch: &str) -> Option<AppliancePaths> {
    let vm = chefer_bundle::layout::vm_dir(bundle_dir);
    let kernel = vm.join(chefer_bundle::layout::kernel_name(arch));
    let initramfs = vm.join(chefer_bundle::layout::initramfs_name(arch));
    if kernel.is_file() && initramfs.is_file() {
        Some(AppliancePaths { kernel, initramfs })
    } else {
        None
    }
}

/// host 架構 → guest 架構（VZ 的 Linux guest 與 host 同架構）。
pub fn host_guest_arch(host_arch: &str) -> Option<&'static str> {
    match host_arch {
        "x86_64" => Some("x86_64"),
        "aarch64" => Some("aarch64"),
        _ => None,
    }
}

/// VM 資源配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmResources {
    pub cpu_count: usize,
    pub memory_mib: u64,
}

impl VmResources {
    /// 由 host 可用核心數與（選用的）記憶體覆寫值計算。
    /// cpu = clamp(host_cpus, 1..=MAX_VCPUS)；memory = max(override 或預設, 下限)。
    pub fn compute(host_cpus: usize, mem_override_mib: Option<u64>) -> Self {
        let cpu_count = host_cpus.clamp(1, MAX_VCPUS);
        let memory_mib = mem_override_mib
            .unwrap_or(DEFAULT_MEMORY_MIB)
            .max(MIN_MEMORY_MIB);
        VmResources {
            cpu_count,
            memory_mib,
        }
    }

    pub fn memory_bytes(&self) -> u64 {
        self.memory_mib * 1024 * 1024
    }
}

/// kernel command line：序列埠 console + 由 initramfs 解讀的 chefer 參數
/// （keep_rootfs 與共享標籤）。實際掛載由 initramfs `/init` 執行。
pub fn kernel_command_line(keep_rootfs: bool) -> String {
    let mut s = String::from("console=hvc0 quiet");
    s.push_str(&format!(
        " chefer.bundle_tag={SHARE_TAG_BUNDLE} chefer.data_tag={SHARE_TAG_DATA}"
    ));
    s.push_str(&format!(
        " chefer.bundle_dir={GUEST_BUNDLE_DIR} chefer.data_dir={GUEST_DATA_DIR}"
    ));
    if keep_rootfs {
        s.push_str(" chefer.keep_rootfs=1");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_mapping() {
        assert_eq!(host_guest_arch("aarch64"), Some("aarch64"));
        assert_eq!(host_guest_arch("x86_64"), Some("x86_64"));
        assert_eq!(host_guest_arch("riscv64"), None);
    }

    #[test]
    fn resources_clamp_and_floor() {
        // 多核 host → 夾在 MAX_VCPUS
        let r = VmResources::compute(16, None);
        assert_eq!(r.cpu_count, MAX_VCPUS);
        assert_eq!(r.memory_mib, DEFAULT_MEMORY_MIB);
        // 0 核 → 至少 1
        assert_eq!(VmResources::compute(0, None).cpu_count, 1);
        // 記憶體覆寫低於下限 → 拉到下限
        assert_eq!(
            VmResources::compute(2, Some(128)).memory_mib,
            MIN_MEMORY_MIB
        );
        // 合理覆寫保留
        assert_eq!(VmResources::compute(2, Some(4096)).memory_mib, 4096);
        assert_eq!(
            VmResources::compute(2, Some(4096)).memory_bytes(),
            4096 * 1024 * 1024
        );
    }

    #[test]
    fn cmdline_contains_tags_and_keep_flag() {
        let c = kernel_command_line(false);
        assert!(c.contains("console=hvc0"));
        assert!(c.contains(&format!("chefer.bundle_tag={SHARE_TAG_BUNDLE}")));
        assert!(c.contains(&format!("chefer.data_dir={GUEST_DATA_DIR}")));
        assert!(!c.contains("keep_rootfs"));
        assert!(kernel_command_line(true).contains("chefer.keep_rootfs=1"));
    }

    #[test]
    fn appliance_lookup_requires_both_files() {
        let tmp = std::env::temp_dir().join("chefer-vz-util-test");
        let bundle = tmp.join("bundle");
        let vm = chefer_bundle::layout::vm_dir(&bundle);
        std::fs::create_dir_all(&vm).unwrap();
        // 只有 kernel → None
        std::fs::write(vm.join(chefer_bundle::layout::kernel_name("aarch64")), b"k").unwrap();
        assert!(appliance_in_bundle(&bundle, "aarch64").is_none());
        // 補上 initramfs → Some
        std::fs::write(
            vm.join(chefer_bundle::layout::initramfs_name("aarch64")),
            b"i",
        )
        .unwrap();
        let found = appliance_in_bundle(&bundle, "aarch64").unwrap();
        assert!(found.kernel.ends_with("chefer-vmlinuz-aarch64"));
        assert!(found.initramfs.ends_with("chefer-initramfs-aarch64"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
