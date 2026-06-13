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
                "不支援的 macOS host 架構：{host_arch}；目前僅支援 x86_64 與 aarch64"
            ));
        };
        if vz_util::appliance_in_bundle(ctx.bundle_dir, arch).is_none() {
            return Availability::Unavailable(format!(
                "此單檔未內嵌 macOS micro-VM appliance（缺 vm/{} 或 vm/{}）；\
                 請使用含 appliance 的 kit 重新 `chefer build --target *-apple-darwin`",
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
            "macOS appliance 已內嵌，且 host 架構可支援；但 Virtualization.framework \
             開機 shim 尚待實體 Mac 驗證完成。目前請於 Linux 或 Windows 執行，\
             或在實體 Apple Silicon Mac 上完成 vz helper 驗證後再啟用。"
                .to_string(),
        )
    }

    fn run(&self, ctx: &AppRunContext) -> Result<i32> {
        // 即使尚未能開機，也先跑通「定位 appliance + 計算資源」這條純邏輯，
        // 讓錯誤訊息帶上實際診斷（哪個 arch、appliance 是否內嵌）。
        let host_arch = std::env::consts::ARCH;
        let arch = vz_util::host_guest_arch(host_arch)
            .ok_or_else(|| anyhow::anyhow!("不支援的 host 架構：{host_arch}"))?;

        match vz_util::appliance_in_bundle(ctx.bundle_dir, arch) {
            Some(ap) => anyhow::bail!(
                "macOS 執行後端尚未實作（已找到內嵌 appliance：{}、{}）。\
                 VM 開機 shim 待在實體 Mac 上完成；目前請於 Linux 或 Windows 執行。",
                ap.kernel.display(),
                ap.initramfs.display()
            ),
            None => anyhow::bail!(
                "此單檔未內嵌 macOS micro-VM appliance（缺 vm/chefer-vmlinuz-{arch} 或 \
                 chefer-initramfs-{arch}），且 macOS 執行後端尚未實作；\
                 目前請於 Linux 或 Windows 執行。"
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
            "無法確認 macOS 版本（`sw_vers -productVersion` 失敗）；\
             請確認系統支援 Apple Virtualization.framework"
                .to_string(),
        );
    }
    let version = String::from_utf8_lossy(&out.stdout);
    let mut parts = version.trim().split('.');
    let major = parts.next().and_then(|p| p.parse::<u32>().ok());
    let Some(major) = major else {
        return Some(format!("無法解析 macOS 版本 `{}`", version.trim()));
    };
    if major < 13 {
        Some(format!(
            "macOS 版本過舊（{}）；Chefer vz 後端需要支援 virtiofs 的 macOS 13 以上",
            version.trim()
        ))
    } else {
        None
    }
}
