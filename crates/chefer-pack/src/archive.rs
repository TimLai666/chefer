//! image tar 的安全解壓（解到暫存目錄）。
//!
//! 安全規則（docs/DESIGN.md §0）：
//! - 拒絕絕對路徑、`..`、Windows 磁碟前綴。
//! - symlink/hardlink 目標必須限制在解壓根目錄內。

use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs_err as fs;
use tar::{Archive, EntryType};

/// 將 `tar_path` 的內容安全解壓到 `dest_root`。
pub(crate) fn extract_tar_to_dir(tar_path: &Path, dest_root: &Path) -> Result<()> {
    let file = fs::File::open(tar_path)
        .with_context(|| format!("開啟 image tar 失敗：{}", tar_path.display()))?;
    extract_tar_reader(file, dest_root)
}

/// 從任意 reader 逐 entry 解壓，並做路徑安全檢查。
pub(crate) fn extract_tar_reader<R: Read>(reader: R, dest_root: &Path) -> Result<()> {
    let mut ar = Archive::new(reader);
    for entry in ar
        .entries()
        .context("讀取 tar 內容失敗（檔案可能不是 tar 格式）")?
    {
        let mut entry = entry.context("讀取 tar entry 失敗")?;
        let raw = entry
            .path()
            .context("tar entry 的路徑無法解析")?
            .into_owned();
        let rel = sanitize_rel_path(&raw)
            .with_context(|| format!("tar 內含不安全路徑：{}", raw.display()))?;
        if rel.as_os_str().is_empty() {
            // 根目錄項（"./"），略過。
            continue;
        }
        let dest = dest_root.join(&rel);
        match entry.header().entry_type() {
            EntryType::Directory => {
                fs::create_dir_all(&dest)?;
            }
            EntryType::Symlink => {
                let Some(target) = entry.link_name()? else {
                    bail!("tar 內 symlink 缺少目標：{}", rel.display());
                };
                if !symlink_target_is_safe(&rel, &target) {
                    bail!(
                        "tar 內 symlink 目標越界（不在解壓根目錄內）：{} -> {}",
                        rel.display(),
                        target.display()
                    );
                }
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                create_symlink(&target, &dest).with_context(|| {
                    format!(
                        "建立 symlink 失敗：{} -> {}（Windows 上需要開發人員模式或系統管理員權限）",
                        dest.display(),
                        target.display()
                    )
                })?;
            }
            EntryType::Link => {
                let Some(target) = entry.link_name()? else {
                    bail!("tar 內 hardlink 缺少目標：{}", rel.display());
                };
                // hardlink 目標是相對 tar 根目錄的路徑，套用同一套安全檢查。
                let target_rel = sanitize_rel_path(&target)
                    .with_context(|| format!("tar 內 hardlink 目標不安全：{}", target.display()))?;
                let src = dest_root.join(&target_rel);
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::hard_link(&src, &dest).with_context(|| {
                    format!(
                        "建立 hardlink 失敗：{} -> {}",
                        dest.display(),
                        src.display()
                    )
                })?;
            }
            EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut out = fs::File::create(&dest)
                    .with_context(|| format!("建立檔案失敗：{}", dest.display()))?;
                io::copy(&mut entry, &mut out)
                    .with_context(|| format!("寫入檔案失敗：{}", dest.display()))?;
            }
            // fifo / char / block device 等特殊節點不該出現在 image archive 頂層，直接略過。
            _ => continue,
        }
    }
    Ok(())
}

/// 把 tar entry 路徑轉成「相對、無越界」的安全路徑。
/// 拒絕：絕對路徑、Windows 磁碟前綴、`..`、片段中含 `:` 或 `\`。
pub(crate) fn sanitize_rel_path(p: &Path) -> Result<PathBuf> {
    let mut buf = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                bail!("拒絕絕對路徑或 Windows 磁碟前綴：{}", p.display());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                bail!("拒絕含 `..` 的路徑：{}", p.display());
            }
            Component::Normal(seg) => {
                let s = seg.to_string_lossy();
                if s.contains(':') || s.contains('\\') {
                    bail!("路徑片段含非法字元（`:` 或 `\\`）：{s}");
                }
                buf.push(seg);
            }
        }
    }
    Ok(buf)
}

/// 檢查 symlink 目標（相對於 symlink 所在位置）是否仍在解壓根目錄內。
fn symlink_target_is_safe(link_rel: &Path, target: &Path) -> bool {
    // symlink 目標以 symlink 所在目錄為基準解析。
    let mut depth: i64 = link_rel
        .parent()
        .map(|p| p.components().count() as i64)
        .unwrap_or(0);
    for comp in target.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => return false,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Component::Normal(_) => depth += 1,
        }
    }
    true
}

#[cfg(unix)]
fn create_symlink(target: &Path, dest: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, dest)
}

#[cfg(windows)]
fn create_symlink(target: &Path, dest: &Path) -> io::Result<()> {
    // image archive 頂層極少出現 symlink；Windows 上需要特權，失敗時由呼叫端給出可行動訊息。
    std::os::windows::fs::symlink_file(target, dest)
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_target: &Path, _dest: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "此平台不支援建立 symlink",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_accepts_normal_paths() {
        assert_eq!(
            sanitize_rel_path(Path::new("./blobs/sha256/abc")).unwrap(),
            PathBuf::from("blobs/sha256/abc")
        );
        assert_eq!(
            sanitize_rel_path(Path::new("manifest.json")).unwrap(),
            PathBuf::from("manifest.json")
        );
    }

    #[test]
    fn sanitize_rejects_escapes() {
        assert!(sanitize_rel_path(Path::new("../evil")).is_err());
        assert!(sanitize_rel_path(Path::new("a/../../evil")).is_err());
        assert!(sanitize_rel_path(Path::new("/abs/path")).is_err());
        // Windows 磁碟前綴（在非 Windows 平台會被視為含 `:` 的片段，一樣被拒）
        assert!(sanitize_rel_path(Path::new("C:\\evil")).is_err());
    }

    #[test]
    fn symlink_target_depth_check() {
        // dir/link -> sibling（安全）
        assert!(symlink_target_is_safe(
            Path::new("dir/link"),
            Path::new("sibling")
        ));
        // dir/link -> ../top（安全：仍在根內）
        assert!(symlink_target_is_safe(
            Path::new("dir/link"),
            Path::new("../top")
        ));
        // link -> ../escape（越界）
        assert!(!symlink_target_is_safe(
            Path::new("link"),
            Path::new("../escape")
        ));
        // 絕對目標
        assert!(!symlink_target_is_safe(
            Path::new("dir/link"),
            Path::new("/etc/passwd")
        ));
    }

    #[test]
    fn extract_rejects_traversal_entries() {
        // tar::Builder 自己會擋 `..`，因此直接手刻惡意 header 來測我們的防線。
        let mut h = tar::Header::new_gnu();
        h.set_size(1);
        h.set_mode(0o644);
        h.set_entry_type(EntryType::Regular);
        let name = b"../evil.txt";
        h.as_old_mut().name[..name.len()].copy_from_slice(name);
        h.set_cksum();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(h.as_bytes());
        let mut data_block = [0u8; 512];
        data_block[0] = b'x';
        bytes.extend_from_slice(&data_block);
        bytes.extend_from_slice(&[0u8; 1024]); // tar 結尾的兩個零區塊

        let tmp = tempfile::tempdir().unwrap();
        let err = extract_tar_reader(bytes.as_slice(), tmp.path()).unwrap_err();
        assert!(format!("{err:#}").contains("不安全路徑"), "{err:#}");
        assert!(!tmp.path().parent().unwrap().join("evil.txt").exists());
    }
}
