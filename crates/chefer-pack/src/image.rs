use anyhow::{Result, Context, bail};
use fs_err as fs;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use appcipe_spec::{Service, ImageSourceOrPath, ImageSourceType};
use tar::{Archive, EntryType};
use flate2::read::GzDecoder;

use crate::bundle::Layout;

/// 針對單一 service 解出 rootfs 到 services/<name>/rootfs
pub fn extract_rootfs(layout: &Layout, name: &str, svc: &Service) -> Result<()> {
    let out = layout.svc_rootfs_dir(name);
    fs::create_dir_all(&out)?;
    match &svc.image {
        ImageSourceOrPath::TarPath(p) => unpack_tar_auto(p, &out)
            .with_context(|| format!("service `{name}` unpack {:?}", p)),
        ImageSourceOrPath::Full { source, file, .. } => match source {
            ImageSourceType::Tar => unpack_tar_auto(file, &out)
                .with_context(|| format!("service `{name}` unpack {:?}", file)),
            _ => bail!("MVP only supports image.source=tar for service `{name}`"),
        }
    }
}

/// 自動判斷 .tar / .tar.gz / .tgz
fn unpack_tar_auto(path: &str, out_dir: &Path) -> Result<()> {
    let p = Path::new(path);
    let file = File::open(p).with_context(|| format!("open tar {:?}", p))?;
    let is_gz = p.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.ends_with(".tar.gz") || s.ends_with(".tgz"))
        .unwrap_or(false);

    if is_gz {
        let gz = GzDecoder::new(file);
        unpack_tar_from_reader(gz, out_dir)
    } else {
        unpack_tar_from_reader(file, out_dir)
    }
}

/// 逐 entry 解包，並做路徑安全檢查
fn unpack_tar_from_reader<R: Read>(reader: R, out_dir: &Path) -> Result<()> {
    let mut ar = Archive::new(reader);
    for entry in ar.entries()? {
        let mut entry = entry?;
        let raw_path = entry.path()?;
        let safe_rel = sanitize_rel_path(&raw_path)
            .with_context(|| format!("unsafe path in tar: {:?}", raw_path))?;
        let dest = out_dir.join(&safe_rel);

        // 確保父目錄存在
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }

        match entry.header().entry_type() {
            EntryType::Directory => {
                fs::create_dir_all(&dest)?;
            }
            _ => {
                entry.unpack(&dest)?;
            }
        }
    }
    Ok(())
}

/// 把 tar entry 的路徑轉成「相對、無越界」的安全路徑
fn sanitize_rel_path(p: &Path) -> Result<PathBuf> {
    use std::path::Component;
    let mut buf = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                // 去掉 Windows 前綴與絕對根
                continue;
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                // 阻擋走逸
                bail!("parent dir not allowed");
            }
            Component::Normal(seg) => buf.push(seg),
        }
    }
    Ok(buf)
}
