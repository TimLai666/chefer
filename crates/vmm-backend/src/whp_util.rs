//! Windows `whp` 後端的純函式：appliance/helper 查找、VM 資源計算、helper CLI contract。
//!
//! 真正 Windows Hypervisor Platform VM boot 尚未實作；這裡先固定 runtime 與
//! `chefer-whp-helper` 之間的邊界，讓打包、診斷與 release kit 可以先被測試。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::vz_util::{
    GUEST_BUNDLE_DIR, GUEST_DATA_DIR, SHARE_TAG_BUNDLE, SHARE_TAG_DATA, VmResources,
};

/// Windows WHP appliance 的兩個檔案路徑。
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

/// host 架構 → WHP guest 架構。WHP 是虛擬化，不在這層做跨架構模擬。
pub fn host_guest_arch(host_arch: &str) -> Option<&'static str> {
    match host_arch {
        "x86_64" => Some("x86_64"),
        "aarch64" => Some("aarch64"),
        _ => None,
    }
}

/// Windows WHP helper 在 bundle 內的預期路徑。
pub fn helper_in_bundle(bundle_dir: &Path, host_arch: &str) -> PathBuf {
    chefer_bundle::layout::agents_dir(bundle_dir)
        .join(chefer_bundle::layout::whp_helper_name(host_arch))
}

/// kernel command line：序列埠 console + WHP 專屬參數 + chefer appliance 參數。
///
/// WHP 專屬：`nolapic`（LAPIC emulation 不完整，由 host 注入 timer）、
/// `lpj=1000000`（跳過 calibration）、`notsc clocksource=jiffies`（TSC 不可靠）。
pub fn kernel_command_line(keep_rootfs: bool) -> String {
    let mut s = String::from(
        "console=ttyS0 quiet ip=dhcp panic=-1 nolapic lpj=1000000 notsc clocksource=jiffies",
    );
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

/// Runtime 對 WHP helper 的完整呼叫契約。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperInvocation {
    pub helper: PathBuf,
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub cmdline: String,
    pub bundle_dir: PathBuf,
    pub data_dir: PathBuf,
    pub resources: VmResources,
    pub timeout_secs: u64,
}

impl HelperInvocation {
    /// 完整開機模式的 CLI 參數。
    pub fn args(&self) -> Vec<OsString> {
        let mut v = vec![
            "--kernel".into(),
            self.kernel.as_os_str().to_owned(),
            "--initramfs".into(),
            self.initramfs.as_os_str().to_owned(),
            "--cmdline".into(),
            self.cmdline.clone().into(),
            "--bundle-dir".into(),
            self.bundle_dir.as_os_str().to_owned(),
            "--data-dir".into(),
            self.data_dir.as_os_str().to_owned(),
            "--cpus".into(),
            self.resources.cpu_count.to_string().into(),
            "--memory-mib".into(),
            self.resources.memory_mib.to_string().into(),
        ];
        if self.timeout_secs != 300 {
            v.push("--timeout".into());
            v.push(self.timeout_secs.to_string().into());
        }
        v
    }

    /// `--preflight` 模式的 CLI 參數（用來在開機前驗證 WHP API partition 生命週期）。
    pub fn preflight_args(&self) -> Vec<OsString> {
        vec![
            "--preflight".into(),
            "--cpus".into(),
            self.resources.cpu_count.to_string().into(),
        ]
    }
}

/// 產生獨立的 preflight CLI 參數（不需要完整的 HelperInvocation）。
pub fn preflight_args(cpus: u32) -> Vec<OsString> {
    vec![
        "--preflight".into(),
        "--cpus".into(),
        cpus.to_string().into(),
    ]
}

/// Bundle 端的 WHP 執行前置檢查。即使回 `Ready`，目前 WHP backend 仍不可執行；
/// `Ready` 只代表 contract 所需檔案已在 bundle 內。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundlePreflight {
    Ready {
        helper: PathBuf,
        appliance: AppliancePaths,
    },
    UnsupportedHostArch {
        host_arch: String,
    },
    MissingAppliance {
        arch: &'static str,
    },
    MissingHelper {
        host_arch: String,
        helper_name: String,
    },
}

pub fn bundle_preflight(bundle_dir: &Path, host_arch: &str) -> BundlePreflight {
    let Some(arch) = host_guest_arch(host_arch) else {
        return BundlePreflight::UnsupportedHostArch {
            host_arch: host_arch.to_string(),
        };
    };
    let Some(appliance) = appliance_in_bundle(bundle_dir, arch) else {
        return BundlePreflight::MissingAppliance { arch };
    };
    let helper = helper_in_bundle(bundle_dir, host_arch);
    if !helper.is_file() {
        return BundlePreflight::MissingHelper {
            host_arch: host_arch.to_string(),
            helper_name: chefer_bundle::layout::whp_helper_name(host_arch),
        };
    }
    BundlePreflight::Ready { helper, appliance }
}

pub fn helper_invocation(
    bundle_dir: &Path,
    data_dir: &Path,
    keep_tmp: bool,
    host_arch: &str,
    host_cpus: usize,
    mem_override_mib: Option<u64>,
) -> Result<HelperInvocation, BundlePreflight> {
    let timeout_secs = std::env::var("CHEFER_WHP_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    match bundle_preflight(bundle_dir, host_arch) {
        BundlePreflight::Ready { helper, appliance } => Ok(HelperInvocation {
            helper,
            kernel: appliance.kernel,
            initramfs: appliance.initramfs,
            cmdline: kernel_command_line(keep_tmp),
            bundle_dir: bundle_dir.to_path_buf(),
            data_dir: data_dir.to_path_buf(),
            resources: VmResources::compute(host_cpus, mem_override_mib),
            timeout_secs,
        }),
        other => Err(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_mapping_is_virtualization_only() {
        assert_eq!(host_guest_arch("x86_64"), Some("x86_64"));
        assert_eq!(host_guest_arch("aarch64"), Some("aarch64"));
        assert_eq!(host_guest_arch("arm"), None);
    }

    #[test]
    fn cmdline_contains_shared_appliance_contract() {
        let cmdline = kernel_command_line(true);

        assert!(cmdline.contains("console=ttyS0"));
        assert!(cmdline.contains("nolapic"));
        assert!(cmdline.contains("lpj=1000000"));
        assert!(cmdline.contains("notsc"));
        assert!(cmdline.contains("clocksource=jiffies"));
        assert!(cmdline.contains("chefer.bundle_tag=chefer-bundle"));
        assert!(cmdline.contains("chefer.data_dir=/mnt/data"));
        assert!(cmdline.contains("chefer.keep_rootfs=1"));
    }

    #[test]
    fn bundle_preflight_requires_appliance_and_helper() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let vm = chefer_bundle::layout::vm_dir(&bundle);
        let agents = chefer_bundle::layout::agents_dir(&bundle);
        std::fs::create_dir_all(&vm).unwrap();
        std::fs::create_dir_all(&agents).unwrap();

        assert_eq!(
            bundle_preflight(&bundle, "x86_64"),
            BundlePreflight::MissingAppliance { arch: "x86_64" }
        );

        std::fs::write(vm.join(chefer_bundle::layout::kernel_name("x86_64")), b"k").unwrap();
        std::fs::write(
            vm.join(chefer_bundle::layout::initramfs_name("x86_64")),
            b"i",
        )
        .unwrap();
        assert_eq!(
            bundle_preflight(&bundle, "x86_64"),
            BundlePreflight::MissingHelper {
                host_arch: "x86_64".to_string(),
                helper_name: "chefer-whp-helper-x86_64.exe".to_string(),
            }
        );

        std::fs::write(
            agents.join(chefer_bundle::layout::whp_helper_name("x86_64")),
            b"h",
        )
        .unwrap();
        assert!(matches!(
            bundle_preflight(&bundle, "x86_64"),
            BundlePreflight::Ready { .. }
        ));
    }

    #[test]
    fn helper_invocation_pins_cli_args() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let data = tmp.path().join("data");
        let vm = chefer_bundle::layout::vm_dir(&bundle);
        let agents = chefer_bundle::layout::agents_dir(&bundle);
        std::fs::create_dir_all(&vm).unwrap();
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(vm.join(chefer_bundle::layout::kernel_name("x86_64")), b"k").unwrap();
        std::fs::write(
            vm.join(chefer_bundle::layout::initramfs_name("x86_64")),
            b"i",
        )
        .unwrap();
        std::fs::write(
            agents.join(chefer_bundle::layout::whp_helper_name("x86_64")),
            b"h",
        )
        .unwrap();

        let invocation =
            helper_invocation(&bundle, &data, false, "x86_64", 16, Some(2048)).unwrap();
        let args = invocation
            .args()
            .into_iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();

        assert_eq!(invocation.resources.cpu_count, 4);
        assert_eq!(invocation.resources.memory_mib, 2048);
        assert!(has_arg_pair(
            &args,
            "--kernel",
            invocation.kernel.to_string_lossy().as_ref()
        ));
        assert!(has_arg_pair(&args, "--cpus", "4"));
        assert!(has_arg_pair(&args, "--memory-mib", "2048"));
    }

    #[test]
    fn preflight_args_contain_flag_and_cpus() {
        let args = preflight_args(4)
            .into_iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(args, vec!["--preflight", "--cpus", "4"]);
    }

    #[test]
    fn invocation_preflight_args_use_computed_cpus() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("bundle");
        let data = tmp.path().join("data");
        let vm = chefer_bundle::layout::vm_dir(&bundle);
        let agents = chefer_bundle::layout::agents_dir(&bundle);
        std::fs::create_dir_all(&vm).unwrap();
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(vm.join(chefer_bundle::layout::kernel_name("x86_64")), b"k").unwrap();
        std::fs::write(
            vm.join(chefer_bundle::layout::initramfs_name("x86_64")),
            b"i",
        )
        .unwrap();
        std::fs::write(
            agents.join(chefer_bundle::layout::whp_helper_name("x86_64")),
            b"h",
        )
        .unwrap();

        let invocation = helper_invocation(&bundle, &data, false, "x86_64", 16, None).unwrap();
        let args = invocation
            .preflight_args()
            .into_iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(args[0], "--preflight");
        assert!(has_arg_pair(
            &args,
            "--cpus",
            &invocation.resources.cpu_count.to_string()
        ));
    }

    fn has_arg_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2)
            .any(|pair| pair[0] == flag && pair[1] == value)
    }
}
