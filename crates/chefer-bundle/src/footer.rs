//! 單檔尾端 footer（80 bytes）的統一讀寫（格式見 docs/DESIGN.md §4）。
//! runtime 與 assembler 都必須用這裡的實作，不得自行手刻位元組。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail};

pub const FOOTER_LEN: u64 = 80;
pub const MAGIC: &[u8; 8] = b"CHEFER\0\0";
/// bit0：payload 為 zstd 壓縮的 tar。
pub const FLAG_ZSTD: u8 = 0b0000_0001;

pub const FOOTER_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footer {
    pub version: u8,
    pub flags: u8,
    /// payload 在檔案中的起始位移。
    pub offset: u64,
    /// payload 長度（bytes）。
    pub length: u64,
    /// payload（壓縮後 bytes）的 SHA-256。
    pub sha256: [u8; 32],
}

impl Footer {
    pub fn new_zstd(offset: u64, length: u64, sha256: [u8; 32]) -> Self {
        Footer {
            version: FOOTER_VERSION,
            flags: FLAG_ZSTD,
            offset,
            length,
            sha256,
        }
    }

    pub fn is_zstd(&self) -> bool {
        (self.flags & FLAG_ZSTD) != 0
    }

    /// 序列化為 80 bytes。
    pub fn to_bytes(&self) -> [u8; FOOTER_LEN as usize] {
        let mut buf = [0u8; FOOTER_LEN as usize];
        buf[0..8].copy_from_slice(MAGIC);
        buf[8] = self.version;
        buf[9] = self.flags;
        // 10..16 保留
        buf[16..24].copy_from_slice(&self.offset.to_le_bytes());
        buf[24..32].copy_from_slice(&self.length.to_le_bytes());
        buf[32..64].copy_from_slice(&self.sha256);
        // 64..80 保留
        buf
    }

    /// 從 80 bytes 解析（不含檔案大小邊界檢查）。
    pub fn from_bytes(buf: &[u8; FOOTER_LEN as usize]) -> Result<Self> {
        if &buf[0..8] != MAGIC {
            bail!("不是 Chefer 單檔（magic 不符）");
        }
        let version = buf[8];
        if version != FOOTER_VERSION {
            bail!(
                "不支援的 footer 版本 {}（本版支援 {}）；請更新 Chefer 或以相同版本重新打包",
                version,
                FOOTER_VERSION
            );
        }
        let flags = buf[9];
        let offset = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        let length = u64::from_le_bytes(buf[24..32].try_into().unwrap());
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(&buf[32..64]);
        Ok(Footer {
            version,
            flags,
            offset,
            length,
            sha256,
        })
    }

    /// 從檔案尾端讀出 footer，並檢查 offset/length 是否落在檔案範圍內。
    pub fn read_from_file(path: &Path) -> Result<Self> {
        let mut f =
            File::open(path).with_context(|| format!("開啟檔案失敗：{}", path.display()))?;
        let size = f.metadata()?.len();
        if size < FOOTER_LEN {
            bail!("檔案過小，沒有 Chefer footer：{}", path.display());
        }
        f.seek(SeekFrom::End(-(FOOTER_LEN as i64)))?;
        let mut buf = [0u8; FOOTER_LEN as usize];
        f.read_exact(&mut buf)?;
        let ft = Self::from_bytes(&buf)?;
        let payload_end = ft
            .offset
            .checked_add(ft.length)
            .filter(|&end| end <= size.saturating_sub(FOOTER_LEN));
        if payload_end.is_none() {
            bail!(
                "footer 的 offset/length 超出檔案範圍（offset={}, len={}, file_size={}）",
                ft.offset,
                ft.length,
                size
            );
        }
        Ok(ft)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn roundtrip_bytes() {
        let ft = Footer::new_zstd(123, 456, [7u8; 32]);
        let bytes = ft.to_bytes();
        let ft2 = Footer::from_bytes(&bytes).unwrap();
        assert_eq!(ft, ft2);
        assert!(ft2.is_zstd());
    }

    #[test]
    fn bad_magic_rejected() {
        let mut bytes = Footer::new_zstd(0, 0, [0u8; 32]).to_bytes();
        bytes[0] = b'X';
        assert!(Footer::from_bytes(&bytes).is_err());
    }

    #[test]
    fn bad_version_rejected() {
        let mut bytes = Footer::new_zstd(0, 0, [0u8; 32]).to_bytes();
        bytes[8] = 99;
        assert!(Footer::from_bytes(&bytes).is_err());
    }

    #[test]
    fn read_from_file_checks_bounds() {
        let dir = std::env::temp_dir().join("chefer-bundle-test-footer");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("fake.bin");

        // payload 10 bytes + footer，offset=0, length=10 → 合法
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(&[0u8; 10]).unwrap();
        f.write_all(&Footer::new_zstd(0, 10, [1u8; 32]).to_bytes())
            .unwrap();
        drop(f);
        assert!(Footer::read_from_file(&p).is_ok());

        // length 超出 → 拒絕
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(&[0u8; 10]).unwrap();
        f.write_all(&Footer::new_zstd(0, 11, [1u8; 32]).to_bytes())
            .unwrap();
        drop(f);
        assert!(Footer::read_from_file(&p).is_err());
    }
}
