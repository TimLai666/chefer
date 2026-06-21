//! bundle/data 目錄 ↔ virtio-blk raw image 轉換（host 端，跨平台）。
//!
//! WHP 沒有 virtiofs，bundle（唯讀）與 data（讀寫）改以 virtio-blk 傳遞。本模組把 host
//! 目錄打包成一個 **sector(512) 對齊的 tar image** 當 [`super::blk::BlkDevice`] 的 backing；
//! 關機後再把 data image 解回 host 目錄（持久化回寫）。
//!
//! 選 tar 而非檔案系統 image：純 Rust、跨平台可產生、guest 端 busybox `tar` 即可展開，
//! 不需在 host（尤其 Windows）備妥 mke2fs/mksquashfs。**已知取捨**：guest 端需把 image
//! 展開到 tmpfs（佔 RAM）；對 data（小）合適，bundle（大）若 RAM 吃緊，後續可換唯讀
//! squashfs image（見 docs/DESIGN.md §6 whp / virtio-mmio）。

use std::io;
use std::path::Path;

use super::blk::SECTOR_SIZE;

/// 把 buffer 補零到 sector(512) 對齊（raw block device 大小須為 sector 整數倍）。
pub fn pad_to_sectors(mut buf: Vec<u8>) -> Vec<u8> {
    let sector = SECTOR_SIZE as usize;
    let rem = buf.len() % sector;
    if rem != 0 {
        buf.resize(buf.len() + (sector - rem), 0);
    }
    buf
}

/// 把 `dir` 整個目錄樹打包成 sector 對齊的 tar image（供 virtio-blk backing）。
pub fn pack_dir(dir: &Path) -> io::Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    // append_dir_all 以 "." 為 archive 根，遞迴納入子目錄與檔案。
    builder.append_dir_all(".", dir)?;
    builder.finish()?;
    let inner = builder.into_inner()?;
    Ok(pad_to_sectors(inner))
}

/// 把 tar image 解回 `dir`（關機回寫 data；尾端 sector padding 為 tar EOF 後的零塊，忽略）。
pub fn unpack_image(image: &[u8], dir: &Path) -> io::Result<()> {
    let mut archive = tar::Archive::new(image);
    archive.unpack(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn pad_rounds_up_to_sector() {
        assert_eq!(pad_to_sectors(vec![1, 2, 3]).len(), 512);
        assert_eq!(pad_to_sectors(vec![0u8; 512]).len(), 512); // 已對齊不變
        assert_eq!(pad_to_sectors(vec![0u8; 513]).len(), 1024);
        assert!(pad_to_sectors(Vec::new()).is_empty());
    }

    #[test]
    fn pad_preserves_existing_bytes() {
        let padded = pad_to_sectors(vec![0xAB, 0xCD]);
        assert_eq!(&padded[..2], &[0xAB, 0xCD]);
        assert!(padded[2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn pack_output_is_sector_aligned() {
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("manifest.json"), b"{}").unwrap();
        let image = pack_dir(src.path()).unwrap();
        assert_eq!(image.len() % SECTOR_SIZE as usize, 0);
        assert!(!image.is_empty());
    }

    #[test]
    fn pack_unpack_roundtrip_preserves_tree() {
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("manifest.json"), b"{\"v\":1}").unwrap();
        fs::create_dir(src.path().join("agents")).unwrap();
        fs::write(src.path().join("agents/guest-agent-x86_64"), b"ELFBINARY").unwrap();
        fs::create_dir(src.path().join("layers")).unwrap();
        fs::write(src.path().join("layers/0.tar.zst"), [0u8, 1, 2, 3, 4]).unwrap();

        let image = pack_dir(src.path()).unwrap();

        let dst = tempfile::tempdir().unwrap();
        unpack_image(&image, dst.path()).unwrap();

        assert_eq!(
            fs::read(dst.path().join("manifest.json")).unwrap(),
            b"{\"v\":1}"
        );
        assert_eq!(
            fs::read(dst.path().join("agents/guest-agent-x86_64")).unwrap(),
            b"ELFBINARY"
        );
        assert_eq!(
            fs::read(dst.path().join("layers/0.tar.zst")).unwrap(),
            &[0u8, 1, 2, 3, 4]
        );
    }

    #[test]
    fn unpack_tolerates_sector_padding() {
        // pack 後的 image 尾端有 padding（tar EOF 零塊 + sector 補零），unpack 應正常結束。
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("f"), b"data").unwrap();
        let mut image = pack_dir(src.path()).unwrap();
        image.extend(std::iter::repeat_n(0u8, 512)); // 再多塞一個零 sector
        let dst = tempfile::tempdir().unwrap();
        unpack_image(&image, dst.path()).unwrap();
        assert_eq!(fs::read(dst.path().join("f")).unwrap(), b"data");
    }
}
