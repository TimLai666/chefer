//! image tar 的安全解壓（解到暫存目錄）。
//!
//! 安全規則（docs/DESIGN.md §0）：
//! - 拒絕絕對路徑、`..`、Windows 磁碟前綴。
//! - **symlink / hardlink entry 一律不在磁碟上建立連結**：把目標詞法解析、確認落在解壓根
//!   內後，**改成複製目標檔內容**。
//!
//! 為何「複製而非建連結」：純詞法的目標深度檢查可被 `d -> .` 這類「深度放大器」繞過
//! ——若把 symlink 真的建在磁碟上，後續 entry 的路徑成分可能穿過它，落地時
//! `create_dir_all`/`File::create` 會跟隨磁碟上的 symlink 寫到解壓根之外（build 機任意
//! 檔案寫入）。**只要不在磁碟留下任何連結**，這種「延後重導」攻擊就結構性不可能；同時
//! 我們仍把目標詞法解析並限制在解壓根內。
//!
//! 為何需要支援連結：**podman / buildah 的 docker-archive 會用 symlink/hardlink 去重相同的
//! `layer.tar`**，一律拒絕的話 `source: dockerfile` 用 podman 建置就解不開。docker 的
//! `save` 不用連結，故只有非-docker builder 會走到這條。
//!
//! 目標解析語意（依 tar 慣例）：**hardlink** 的 linkname 相對 archive 根；**symlink** 的
//! linkname 相對「連結自身所在目錄」（如同檔案系統 symlink，可含 `..`）。兩者解析後都必須
//! 落在解壓根內、且指向一般檔案，否則報錯。連結在主迴圈先收集、待一般檔案全數解出後再處理
//! （目標不分前後皆可）。

use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs_err as fs;
use tar::{Archive, EntryType};

/// 待處理的連結 entry（symlink/hardlink）：連結自身相對路徑、目標、是否為 symlink。
struct PendingLink {
    /// 連結自身相對解壓根的（已 sanitize）路徑。
    rel: PathBuf,
    /// linkname（symlink：相對連結所在目錄；hardlink：相對 archive 根）。
    target: PathBuf,
    is_symlink: bool,
}

/// 將 `tar_path` 的內容安全解壓到 `dest_root`。
pub(crate) fn extract_tar_to_dir(tar_path: &Path, dest_root: &Path) -> Result<()> {
    let file = fs::File::open(tar_path)
        .with_context(|| format!("開啟 image tar 失敗：{}", tar_path.display()))?;
    extract_tar_reader(file, dest_root)
}

/// 從任意 reader 逐 entry 解壓，並做路徑安全檢查。
pub(crate) fn extract_tar_reader<R: Read>(reader: R, dest_root: &Path) -> Result<()> {
    let mut ar = Archive::new(reader);
    // 連結 entry 先收集、待一般檔案全部解出後再處理（目標可能在連結之後才出現）。
    let mut links: Vec<PendingLink> = Vec::new();
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
            // symlink / hardlink：不在磁碟建連結；記下來，第二階段詞法解析目標後複製內容。
            ty @ (EntryType::Symlink | EntryType::Link) => {
                let target = entry
                    .link_name()
                    .context("讀取連結目標失敗")?
                    .ok_or_else(|| anyhow::anyhow!("連結 entry 缺少目標：{}", rel.display()))?
                    .into_owned();
                links.push(PendingLink {
                    rel,
                    target,
                    is_symlink: ty == EntryType::Symlink,
                });
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

    // 第二階段：把每個連結解析成「解壓根內的一般檔案」並複製內容（不建任何磁碟連結）。
    for link in &links {
        let resolved =
            resolve_link_target(&link.rel, &link.target, link.is_symlink).with_context(|| {
                format!(
                    "連結目標不安全：{} -> {}",
                    link.rel.display(),
                    link.target.display()
                )
            })?;
        let src = dest_root.join(&resolved);
        if !src.is_file() {
            bail!(
                "連結目標不存在或非一般檔案（image archive 可能損毀）：{} -> {}",
                link.rel.display(),
                resolved.display()
            );
        }
        let dest = dest_root.join(&link.rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&src, &dest).with_context(|| {
            format!("複製連結目標失敗：{} -> {}", src.display(), dest.display())
        })?;
    }
    Ok(())
}

/// 把連結目標詞法解析成「相對解壓根、不越界」的路徑。
///
/// - symlink：目標相對「連結所在目錄」（可含 `..`）。
/// - hardlink：目標相對 archive 根。
///
/// 任何越界（`..` 超出根）、絕對路徑、或含 `\` 的片段都報錯。
fn resolve_link_target(link_rel: &Path, target: &Path, is_symlink: bool) -> Result<PathBuf> {
    // 組出待正規化的「相對根」路徑。
    let combined = if is_symlink {
        match link_rel.parent() {
            Some(p) => p.join(target),
            None => target.to_path_buf(),
        }
    } else {
        target.to_path_buf()
    };
    let mut stack: Vec<OsString> = Vec::new();
    for comp in combined.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                bail!("連結目標為絕對路徑：{}", target.display());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if stack.pop().is_none() {
                    bail!("連結目標越界（`..` 超出解壓根）：{}", target.display());
                }
            }
            Component::Normal(seg) => {
                if seg.to_string_lossy().contains('\\') {
                    bail!("連結目標片段含非法字元（`\\`）：{}", target.display());
                }
                stack.push(seg.to_os_string());
            }
        }
    }
    if stack.is_empty() {
        bail!("連結目標為空或指向解壓根本身：{}", target.display());
    }
    Ok(stack.iter().collect())
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
    fn extract_never_creates_symlink_on_disk() {
        // `d -> .` 深度放大器：永遠不在磁碟建 symlink；目標 `.` 不是一般檔案 → 報錯。
        // 關鍵是磁碟上不會留下 `d` 這個 symlink（否則後續 entry 可穿過它逃逸）。
        let bytes = tar_with_entry(b"d", EntryType::Symlink, Some(b"."));
        let tmp = tempfile::tempdir().unwrap();
        let err = extract_tar_reader(bytes.as_slice(), tmp.path()).unwrap_err();
        assert!(format!("{err:#}").contains("連結目標"), "{err:#}");
        assert!(tmp.path().join("d").symlink_metadata().is_err());
    }

    #[test]
    fn extract_rejects_escaping_symlink() {
        // 指向解壓根外的 symlink（`../../etc/passwd`，相對連結所在目錄）→ 越界被拒。
        let bytes = tar_with_entry(b"sub/h", EntryType::Symlink, Some(b"../../../etc/passwd"));
        let tmp = tempfile::tempdir().unwrap();
        let err = extract_tar_reader(bytes.as_slice(), tmp.path()).unwrap_err();
        assert!(format!("{err:#}").contains("越界"), "{err:#}");
    }

    #[test]
    fn extract_rejects_escaping_hardlink() {
        // 越界的 hardlink 目標（root-relative，`..` 超出根）→ 被拒。
        let bytes = tar_with_entry(b"h", EntryType::Link, Some(b"../../etc/passwd"));
        let tmp = tempfile::tempdir().unwrap();
        let err = extract_tar_reader(bytes.as_slice(), tmp.path()).unwrap_err();
        assert!(format!("{err:#}").contains("越界"), "{err:#}");
        assert!(!tmp.path().join("h").exists());
    }

    #[test]
    fn extract_allows_in_root_symlink_as_copy() {
        // podman 風格：layer.tar 以 symlink（相對連結目錄）去重 → 應以複製內容解出，且磁碟上不留 symlink。
        let mut b = tar::Builder::new(Vec::new());
        let mut h1 = tar::Header::new_gnu();
        h1.set_size(3);
        h1.set_mode(0o644);
        h1.set_entry_type(EntryType::Regular);
        h1.set_cksum();
        b.append_data(&mut h1, "a/layer.tar", &b"abc"[..]).unwrap();
        let mut h2 = tar::Header::new_gnu();
        h2.set_size(0);
        h2.set_mode(0o777);
        h2.set_entry_type(EntryType::Symlink);
        h2.set_cksum();
        // b/layer.tar -> ../a/layer.tar（相對 b/）
        b.append_link(&mut h2, "b/layer.tar", "../a/layer.tar")
            .unwrap();
        let bytes = b.into_inner().unwrap();

        let tmp = tempfile::tempdir().unwrap();
        extract_tar_reader(bytes.as_slice(), tmp.path()).unwrap();
        assert_eq!(fs::read(tmp.path().join("b/layer.tar")).unwrap(), b"abc");
        // 磁碟上不可有 symlink。
        assert!(
            !tmp.path()
                .join("b/layer.tar")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
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
