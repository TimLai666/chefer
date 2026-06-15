//! macOS `vz` 後端（Apple Virtualization.framework）。
//!
//! 設計見 docs/DESIGN.md §6「macOS（vz）」：開一台輕量 Linux micro-VM，
//! 以 bundle 內附的 appliance（`vm/chefer-vmlinuz-<arch>` + `chefer-initramfs-<arch>`）
//! 開機，virtiofs 共享 bundle/data，guest 內 initramfs 執行 guest-agent。
//!
//! 狀態：契約與跨平台純邏輯（[`crate::vz_util`]）已完成並測試；實際的
//! Virtualization.framework 開機 shim 尚待在實體 Mac 上實作與驗證；
//! Linux appliance 由 scripts/build-appliance.sh 與 scripts/qemu-e2e.sh 驗證。
//! **在 VZ 實機驗證通過前 `availability()` 一律回報
//! `Unavailable`，絕不偽稱可執行。**

use anyhow::Result;

use crate::vz_util;
use crate::{AppRunContext, Availability, ExecBackend};

/// macOS Virtualization.framework 後端。
pub struct VzBackend;

impl ExecBackend for VzBackend {
    fn name(&self) -> &'static str {
        "vz"
    }

    fn availability(&self, ctx: &AppRunContext) -> Availability {
        let host_arch = std::env::consts::ARCH;
        let Some(arch) = vz_util::host_guest_arch(host_arch) else {
            return Availability::Unavailable(format!(
                "unsupported macOS host architecture: {host_arch}; only x86_64 and aarch64 are supported"
            ));
        };
        if vz_util::appliance_in_bundle(ctx.bundle_dir, arch).is_none() {
            return Availability::Unavailable(format!(
                "This single-file app has no embedded macOS micro-VM appliance (missing vm/{} or vm/{}); \
                 rebuild with `chefer build --target *-apple-darwin` using a kit that includes the appliance",
                chefer_bundle::layout::kernel_name(arch),
                chefer_bundle::layout::initramfs_name(arch)
            ));
        }
        if let Some(reason) = macos_version_unsupported_reason() {
            return Availability::Unavailable(reason);
        }
        // VZ 開機 shim 尚未實機驗證前仍誠實回報不可用。Linux+QEMU appliance
        // E2E 通過後，下一步才可把這裡接到真正的 Virtualization.framework helper。
        Availability::Unavailable(
            "The macOS appliance is embedded and the host architecture is supported, but the \
             Virtualization.framework boot shim has not yet been validated on a physical Mac. \
             For now, run this app on Linux or Windows, or enable the vz backend after \
             validating the vz helper on a physical Apple Silicon Mac."
                .to_string(),
        )
    }

    fn run(&self, ctx: &AppRunContext) -> Result<i32> {
        // 即使尚未能開機，也先跑通「定位 appliance + 計算資源」這條純邏輯，
        // 讓錯誤訊息帶上實際診斷（哪個 arch、appliance 是否內嵌）。
        let host_arch = std::env::consts::ARCH;
        let arch = vz_util::host_guest_arch(host_arch)
            .ok_or_else(|| anyhow::anyhow!("unsupported host architecture: {host_arch}"))?;

        match vz_util::appliance_in_bundle(ctx.bundle_dir, arch) {
            Some(ap) => anyhow::bail!(
                "The macOS execution backend is not implemented yet (embedded appliance found: {}, {}). \
                 The VM boot shim still needs to be completed on a physical Mac; for now, run this app on Linux or Windows.",
                ap.kernel.display(),
                ap.initramfs.display()
            ),
            None => anyhow::bail!(
                "This single-file app has no embedded macOS micro-VM appliance (missing vm/chefer-vmlinuz-{arch} or \
                 chefer-initramfs-{arch}), and the macOS execution backend is not implemented yet; \
                 for now, run this app on Linux or Windows."
            ),
        }
    }
}

fn macos_version_unsupported_reason() -> Option<String> {
    let out = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    if !out.status.success() {
        return Some(
            "Could not determine the macOS version (`sw_vers -productVersion` failed); \
             make sure the system supports Apple Virtualization.framework"
                .to_string(),
        );
    }
    let version = String::from_utf8_lossy(&out.stdout);
    let mut parts = version.trim().split('.');
    let major = parts.next().and_then(|p| p.parse::<u32>().ok());
    let Some(major) = major else {
        return Some(format!(
            "could not parse the macOS version `{}`",
            version.trim()
        ));
    };
    if major < 13 {
        Some(format!(
            "macOS version is too old ({}); the Chefer vz backend requires macOS 13 or newer (for virtiofs support)",
            version.trim()
        ))
    } else {
        None
    }
}
