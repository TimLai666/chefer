//! image tar 的安全解壓（解到暫存目錄）。
//!
//! 安全規則（docs/DESIGN.md §0）：
//! - 拒絕絕對路徑、`..`、Windows 磁碟前綴。
//! - **直接拒絕 symlink 與 hardlink entry**。
//!
//! 為何拒絕而非「限制目標在根內」：純詞法的目標深度檢查可被
//! `d -> .` 這類「深度放大器」繞過——先建一個指向自身目錄的 symlink，
//! 再讓後續 entry 的路徑成分穿過它，落地時 `create_dir_all`/`File::create`
//! 會跟隨磁碟上已建立的 symlink 而寫到解壓根之外（build 機任意檔案寫入）。
//! 要正確防護需逐成分以 O_NOFOLLOW 重新解析（如 guest-agent::rootfs::secure_resolve）。
//! 但合法的 docker-archive / oci-archive 頂層只有 manifest.json / index.json /
//! blobs / oci-layout / repositories（皆為一般檔案與目錄），本就不含 symlink/hardlink；
//! 層內的 symlink 位於 blob 的巢狀 tar，由容器內的 rootfs 組裝處理。
//! 因此此處直接拒絕，攻擊面歸零且不影響任何合法輸入。

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
            // symlink / hardlink：合法 image archive 頂層不含這兩種，
            // 且純詞法目標檢查無法防 on-disk symlink 重導，故一律拒絕。
            EntryType::Symlink | EntryType::Link => {
                bail!(
                    "image archive 頂層不允許 symlink/hardlink（可能是惡意 image 的路徑逃逸嘗試）：{}",
                    rel.display()
                );
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

    /// 建一個含單一 entry 的最小 tar（手刻 header 以繞過 tar::Builder 的防護）。
    fn tar_with_entry(name: &[u8], entry_type: EntryType, link: Option<&[u8]>) -> Vec<u8> {
        let mut h = tar::Header::new_gnu();
        h.set_size(0);
        h.set_mode(0o644);
        h.set_entry_type(entry_type);
        h.as_old_mut().name[..name.len()].copy_from_slice(name);
        if let Some(l) = link {
            h.as_old_mut().linkname[..l.len()].copy_from_slice(l);
        }
        h.set_cksum();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(h.as_bytes());
        bytes.extend_from_slice(&[0u8; 1024]); // tar 結尾的兩個零區塊
        bytes
    }

    #[test]
    fn extract_rejects_symlink_entries() {
        // `d -> .` 深度放大器的第一步——直接被拒，逃逸不可能成立。
        let bytes = tar_with_entry(b"d", EntryType::Symlink, Some(b"."));
        let tmp = tempfile::tempdir().unwrap();
        let err = extract_tar_reader(bytes.as_slice(), tmp.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("不允許 symlink/hardlink"),
            "{err:#}"
        );
        assert!(!tmp.path().join("d").exists());
    }

    #[test]
    fn extract_rejects_hardlink_entries() {
        let bytes = tar_with_entry(b"h", EntryType::Link, Some(b"../../etc/passwd"));
        let tmp = tempfile::tempdir().unwrap();
        let err = extract_tar_reader(bytes.as_slice(), tmp.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("不允許 symlink/hardlink"),
            "{err:#}"
        );
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
