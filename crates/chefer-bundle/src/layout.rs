//! bundle 目錄佈局的路徑輔助（佈局見 docs/DESIGN.md §2）。

use std::path::{Path, PathBuf};

pub fn manifest_path(bundle_dir: &Path) -> PathBuf {
    bundle_dir.join("manifest.json")
}

pub fn appcipe_out_path(bundle_dir: &Path) -> PathBuf {
    bundle_dir.join("appcipe.yml")
}

pub fn services_dir(bundle_dir: &Path) -> PathBuf {
    bundle_dir.join("services")
}

pub fn service_layers_dir(bundle_dir: &Path, svc: &str) -> PathBuf {
    services_dir(bundle_dir).join(svc).join("layers")
}

pub fn agents_dir(bundle_dir: &Path) -> PathBuf {
    bundle_dir.join("agents")
}

/// 內嵌 guest-agent 的檔名（arch ∈ "x86_64", "aarch64"）。
pub fn guest_agent_name(arch: &str) -> String {
    format!("guest-agent-{arch}")
}

/// 內嵌 pasta（bridge 出網用，靜態 musl）的檔名（arch ∈ "x86_64", "aarch64"）。
/// 與 guest-agent 同放 `agents/`；bridge 模式時 guest-agent 會找它來提供出網 NAT。
pub fn pasta_name(arch: &str) -> String {
    format!("pasta-{arch}")
}

/// macOS micro-VM appliance（kernel + initramfs）目錄。
pub fn vm_dir(bundle_dir: &Path) -> PathBuf {
    bundle_dir.join("vm")
}

/// 預編 Linux kernel 的檔名（arch ∈ "x86_64", "aarch64"）。
pub fn kernel_name(arch: &str) -> String {
    format!("chefer-vmlinuz-{arch}")
}

/// 最小 initramfs 的檔名（arch ∈ "x86_64", "aarch64"）。
pub fn initramfs_name(arch: &str) -> String {
    format!("chefer-initramfs-{arch}")
}

/// 由 image platform 字串（如 "linux/amd64"）取得 guest 架構名。
/// 回傳 None 表示非 Linux 平台（目前不支援執行）。
pub fn platform_to_arch(platform: &str) -> Option<&'static str> {
    match platform {
        "linux/amd64" => Some("x86_64"),
        "linux/arm64" => Some("aarch64"),
        _ => None,
    }
}

/// 層檔名：`{序號:04}-{diff_id 去掉 "sha256:" 後前 12 碼}.tar.zst`。
pub fn layer_file_name(index: usize, diff_id: &str) -> String {
    let hex = diff_id.strip_prefix("sha256:").unwrap_or(diff_id);
    let short = &hex[..hex.len().min(12)];
    format!("{index:04}-{short}.tar.zst")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_name_format() {
        let n = layer_file_name(3, "sha256:abcdef0123456789ff");
        assert_eq!(n, "0003-abcdef012345.tar.zst");
    }

    #[test]
    fn arch_mapping() {
        assert_eq!(platform_to_arch("linux/amd64"), Some("x86_64"));
        assert_eq!(platform_to_arch("linux/arm64"), Some("aarch64"));
        assert_eq!(platform_to_arch("windows/amd64"), None);
    }

    #[test]
    fn appliance_names() {
        assert_eq!(kernel_name("aarch64"), "chefer-vmlinuz-aarch64");
        assert_eq!(initramfs_name("x86_64"), "chefer-initramfs-x86_64");
        assert!(vm_dir(Path::new("/b")).ends_with("vm"));
    }
}
