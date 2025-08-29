// src/extract.rs
use anyhow::{Context, Result, bail};
use fs_err as fs;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};
use tar::Archive;
use tempfile::TempDir;

use crate::footer::Footer;

pub struct Extracted {
    pub tempdir: TempDir,
    pub bundle_dir: PathBuf,
}

pub fn extract_bundle(exe: &Path, ft: &Footer, keep_dir: Option<&Path>) -> Result<Extracted> {
    // 1) 讀出 bundle bytes
    let mut f = File::open(exe)?;
    f.seek(SeekFrom::Start(ft.offset))?;
    let mut bundle = vec![0u8; ft.length as usize];
    f.read_exact(&mut bundle)?;

    // 2) 驗證 sha256
    let mut hasher = Sha256::new();
    hasher.update(&bundle);
    let got = hasher.finalize();
    if got.as_slice() != &ft.sha256 {
        bail!("bundle sha256 mismatch");
    }

    // 3) 解壓到 temp
    let tempdir = match keep_dir {
        Some(dir) => {
            fs::create_dir_all(dir)?;
            TempDir::new_in(dir)?
        }
        None => tempfile::tempdir()?,
    };
    let out = tempdir.path().join("bundle");
    fs::create_dir_all(&out)?;

    // 支援：tar 或 zstd(tar)
    if ft.compressed_is_zstd() {
        let mut d = zstd::stream::read::Decoder::new(&bundle[..]).context("zstd decode")?;
        let mut tar_buf = Vec::new();
        d.read_to_end(&mut tar_buf)?;
        Archive::new(&tar_buf[..]).unpack(&out)?;
    } else {
        Archive::new(&bundle[..]).unpack(&out)?;
    }

    Ok(Extracted {
        tempdir,
        bundle_dir: out,
    })
}
