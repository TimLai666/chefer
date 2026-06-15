//! Runtime kit（預編譯二進位）探索（規格見 docs/DESIGN.md §5）。
//! pack（找 guest-agent）、assembler/cli（找 runtime）共用。

use std::path::PathBuf;

/// 依優先序回傳 kit 搜尋目錄：
/// `CHEFER_KIT_DIR` > `<exe 所在目錄>/kit` > `<exe 所在目錄>`。
/// （CLI 的 `--kit-dir` 由呼叫端 prepend。）
pub fn default_kit_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(d) = std::env::var("CHEFER_KIT_DIR")
        && !d.trim().is_empty()
    {
        dirs.push(PathBuf::from(d));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        dirs.push(exe_dir.join("kit"));
        dirs.push(exe_dir.to_path_buf());
    }
    dirs
}

/// runtime 執行檔的期望檔名（含目標平台副檔名）。
pub fn runtime_file_name(target: &str) -> String {
    if target.contains("windows") {
        format!("chefer-runtime-{target}.exe")
    } else {
        format!("chefer-runtime-{target}")
    }
}

/// 尋找指定 target 的 chefer-runtime。
/// `allow_plain` 為 true 時（target == host）也接受不帶 triple 的
/// `chefer-runtime[.exe]`（方便本機開發直接用 cargo build 產物）。
pub fn find_runtime(kit_dirs: &[PathBuf], target: &str, allow_plain: bool) -> Option<PathBuf> {
    let mut names = vec![runtime_file_name(target)];
    if allow_plain {
        if cfg!(windows) {
            names.push("chefer-runtime.exe".to_string());
        } else {
            names.push("chefer-runtime".to_string());
        }
    }
    find_first(kit_dirs, &names)
}

/// 尋找指定 guest 架構（"x86_64"/"aarch64"）的靜態 musl guest-agent。
pub fn find_guest_agent(kit_dirs: &[PathBuf], arch: &str) -> Option<PathBuf> {
    let names = [crate::layout::guest_agent_name(arch)];
    find_first(kit_dirs, &names)
}

/// 尋找指定 guest 架構的靜態 musl pasta（bridge 出網用；可選——缺少時 bridge 退化為 internal）。
pub fn find_pasta(kit_dirs: &[PathBuf], arch: &str) -> Option<PathBuf> {
    let names = [crate::layout::pasta_name(arch)];
    find_first(kit_dirs, &names)
}

/// 尋找 macOS micro-VM appliance（kernel + initramfs）；兩者皆存在才回傳。
/// 供 chefer-pack 在打包 macOS 目標時內嵌進 bundle 的 `vm/`。
pub fn find_appliance(kit_dirs: &[PathBuf], arch: &str) -> Option<(PathBuf, PathBuf)> {
    let kernel = find_first(kit_dirs, &[crate::layout::kernel_name(arch)])?;
    let initramfs = find_first(kit_dirs, &[crate::layout::initramfs_name(arch)])?;
    Some((kernel, initramfs))
}

fn find_first<S: AsRef<str>>(dirs: &[PathBuf], names: &[S]) -> Option<PathBuf> {
    for d in dirs {
        for n in names {
            let p = d.join(n.as_ref());
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// 組出「找不到」時的友善錯誤說明（列出搜尋過的目錄與期望檔名）。
pub fn not_found_help(kit_dirs: &[PathBuf], expected: &str) -> String {
    let searched: Vec<String> = kit_dirs.iter().map(|d| d.display().to_string()).collect();
    format!(
        "`{expected}` not found. Searched: {}.\n\
         Fix: 1) download the runtime kit from GitHub Releases and place it in a kit/ directory next to chefer; \
         2) or build it yourself with `cargo build --release -p chefer-runtime` (plus the guest-agent for musl targets), \
         then point --kit-dir or CHEFER_KIT_DIR at the directory containing it.",
        if searched.is_empty() {
            "(no directories to search)".to_string()
        } else {
            searched.join(", ")
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_name_has_exe_for_windows_targets() {
        assert_eq!(
            runtime_file_name("x86_64-pc-windows-msvc"),
            "chefer-runtime-x86_64-pc-windows-msvc.exe"
        );
        assert_eq!(
            runtime_file_name("x86_64-unknown-linux-gnu"),
            "chefer-runtime-x86_64-unknown-linux-gnu"
        );
    }

    #[test]
    fn finds_file_in_order() {
        let dir = std::env::temp_dir().join("chefer-bundle-test-kit");
        let sub = dir.join("kit");
        std::fs::create_dir_all(&sub).unwrap();
        let name = runtime_file_name("x86_64-unknown-linux-gnu");
        let p = sub.join(&name);
        std::fs::write(&p, b"fake").unwrap();
        let found = find_runtime(
            &[dir.clone(), sub.clone()],
            "x86_64-unknown-linux-gnu",
            false,
        )
        .unwrap();
        assert_eq!(found, p);
        // 找不到的情況
        assert!(find_runtime(&[dir], "aarch64-unknown-linux-gnu", false).is_none());
    }
}
