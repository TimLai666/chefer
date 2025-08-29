// src/footer.rs
use anyhow::{Context, Result, bail};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

pub const FOOTER_LEN: u64 = 80;
const MAGIC: &[u8; 8] = b"CHEFER\0\0";

#[derive(Debug, Clone, Copy)]
pub struct Footer {
    pub version: u8,
    pub flags: u8,
    pub offset: u64,
    pub length: u64,
    pub sha256: [u8; 32],
}

impl Footer {
    pub fn read_from_exe(exe: &Path) -> Result<Self> {
        let mut f = File::open(exe).with_context(|| format!("open exe {:?}", exe))?;
        let size = f.metadata()?.len();
        if size < FOOTER_LEN {
            bail!("file too small, no footer");
        }
        f.seek(SeekFrom::End(-(FOOTER_LEN as i64)))?;
        let mut buf = [0u8; FOOTER_LEN as usize];
        f.read_exact(&mut buf)?;

        // 解析
        if &buf[0..8] != MAGIC {
            bail!("bad magic");
        }
        let version = buf[8];
        let flags = buf[9];
        // 10..16 reserved
        let offset = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        let length = u64::from_le_bytes(buf[24..32].try_into().unwrap());
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(&buf[32..64]);
        // 64..80 reserved

        if version != 1 {
            bail!("unsupported footer version: {}", version);
        }
        // 基本檢查
        if offset
            .checked_add(length)
            .filter(|&end| end <= size)
            .is_none()
        {
            bail!(
                "footer offset/length out of range (offset={}, len={}, file_size={})",
                offset,
                length,
                size
            );
        }
        Ok(Footer {
            version,
            flags,
            offset,
            length,
            sha256,
        })
    }

    pub fn compressed_is_zstd(&self) -> bool {
        (self.flags & 0b0000_0001) != 0
    }
}
