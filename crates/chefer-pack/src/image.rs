// crates/chefer-pack/src/image.rs
use anyhow::{Result, Context, bail};
use fs_err as fs;
use std::fs::File;
use std::path::Path;
use appcipe_spec::{Service, ImageSourceOrPath, ImageSourceType};
use tar::Archive;
use flate2::read::GzDecoder;

use crate::bundle::Layout;

pub fn extract_rootfs(layout: &Layout, name: &str, svc: &Service) -> Result<()> {
    let out = layout.svc_rootfs_dir(name);
    fs::create_dir_all(&out)?;
    match &svc.image {
        ImageSourceOrPath::TarPath(p) => unpack_tar_auto(p, &out),
        ImageSourceOrPath::Full { source, file, .. } => match source {
            ImageSourceType::Tar => unpack_tar_auto(file, &out),
            _ => bail!("MVP only supports image.source=tar for service `{name}`"),
        }
    }
}

fn unpack_tar_auto(path: &str, out_dir: &Path) -> Result<()> {
    let p = Path::new(path);
    let file = File::open(p).with_context(|| format!("open tar {:?}", p))?;
    let is_gz = p
        .file_name().and_then(|n| n.to_str())
        .map(|s| s.ends_with(".tar.gz") || s.ends_with(".tgz"))
        .unwrap_or(false);
    if is_gz {
        let gz = GzDecoder::new(file);
        Archive::new(gz).unpack(out_dir)?;
    } else {
        Archive::new(file).unpack(out_dir)?;
    }
    Ok(())
}
