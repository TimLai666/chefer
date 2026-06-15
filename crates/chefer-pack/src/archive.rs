//! image tar 的安全解壓（解到暫存目錄）。
//!
//! 安全規則（docs/DESIGN.md §0）：
//! - 拒絕絕對路徑、`..`、Windows 磁碟前綴。
//! - **拒絕 symlink entry**。
//! - **hardlink entry**：只允許目標經同樣越界檢查後落在解壓根內，並以「複製目標檔內容」
//!   實作（不在磁碟上留下連結）。
//!
//! 為何 symlink 一律拒絕：純詞法的目標深度檢查可被 `d -> .` 這類「深度放大器」繞過
//! ——先建一個指向自身目錄的 symlink，再讓後續 entry 的路徑成分穿過它，落地時
//! `create_dir_all`/`File::create` 會跟隨磁碟上已建立的 symlink 而寫到解壓根之外
//! （build 機任意檔案寫入）。要正確防護需逐成分以 O_NOFOLLOW 重新解析。
//!
//! 為何 hardlink 可放行：tar 的 hardlink 指向同一 archive 內「先前的」一般檔案 entry，
//! **不會在磁碟留下會重導後續路徑解析的連結**；我們把 linkname 當相對路徑做同樣的越界
//! 檢查、確認目標在根內，再把目標檔內容複製過來（內容等同）。**podman / buildah 的
//! docker-archive 會用 hardlink 去重相同的 `layer.tar`**，故不能像 symlink 一樣一律拒絕，
//! 否則 `source: dockerfile` 用 podman 建置時會解不開。docker 的 `save` 不用 hardlink，
//! 因此只有非-docker builder 會走到這條。

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
            // symlink：會在磁碟留下重導，後續 entry 的路徑成分可能穿過它寫到根外，故一律拒絕。
            EntryType::Symlink => {
                bail!(
                    "image archive 不允許 symlink（可能是惡意 image 的路徑逃逸嘗試）：{}",
                    rel.display()
                );
            }
            // hardlink：目標須經越界檢查且落在解壓根內；以複製目標檔內容實作（podman/buildah
            // 的 docker-archive 會以 hardlink 去重相同 layer.tar）。
            EntryType::Link => {
                let target = entry
                    .link_name()
                    .context("讀取 hardlink 目標失敗")?
                    .ok_or_else(|| anyhow::anyhow!("hardlink entry 缺少目標：{}", rel.display()))?
                    .into_owned();
                let safe_target = sanitize_rel_path(&target)
                    .with_context(|| format!("hardlink 目標不安全：{}", target.display()))?;
                let src = dest_root.join(&safe_target);
                // tar 的 hardlink 必指向先前已出現的 entry；不存在 → archive 損毀。
                if !src.is_file() {
                    bail!(
                        "hardlink 目標不存在或非一般檔案（image archive 可能損毀）：{} -> {}",
                        rel.display(),
                        safe_target.display()
                    );
                }
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&src, &dest).with_context(|| {
                    format!(
                        "複製 hardlink 目標失敗：{} -> {}",
                        src.display(),
                        dest.display()
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
        assert!(format!("{err:#}").contains("不允許 symlink"), "{err:#}");
        assert!(!tmp.path().join("d").exists());
    }

    #[test]
    fn extract_rejects_escaping_hardlink() {
        // 越界的 hardlink 目標（`..`）→ 仍被拒（目標越界檢查失敗）。
        let bytes = tar_with_entry(b"h", EntryType::Link, Some(b"../../etc/passwd"));
        let tmp = tempfile::tempdir().unwrap();
        let err = extract_tar_reader(bytes.as_slice(), tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("hardlink 目標不安全") || msg.contains("`..`"),
            "{msg}"
        );
        assert!(!tmp.path().join("h").exists());
    }

    #[test]
    fn extract_allows_in_root_hardlink_as_copy() {
        // podman/buildah 風格：layer.tar 以 hardlink 去重 → 應以複製內容解出。
        let mut b = tar::Builder::new(Vec::new());
        let mut h1 = tar::Header::new_gnu();
        h1.set_size(3);
        h1.set_mode(0o644);
        h1.set_entry_type(EntryType::Regular);
        h1.set_cksum();
        b.append_data(&mut h1, "a/layer.tar", &b"abc"[..]).unwrap();
        let mut h2 = tar::Header::new_gnu();
        h2.set_size(0);
        h2.set_mode(0o644);
        h2.set_entry_type(EntryType::Link);
        h2.set_cksum();
        b.append_link(&mut h2, "b/layer.tar", "a/layer.tar")
            .unwrap();
        let bytes = b.into_inner().unwrap();

        let tmp = tempfile::tempdir().unwrap();
        extract_tar_reader(bytes.as_slice(), tmp.path()).unwrap();
        assert_eq!(fs::read(tmp.path().join("b/layer.tar")).unwrap(), b"abc");
        assert_eq!(fs::read(tmp.path().join("a/layer.tar")).unwrap(), b"abc");
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
