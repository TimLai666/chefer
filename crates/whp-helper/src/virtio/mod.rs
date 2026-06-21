//! virtio-over-MMIO 裝置模型（WHP 後端用，跨平台純邏輯）。
//!
//! 設計見 docs/DESIGN.md §6「whp 後端 / virtio-mmio 裝置模型」。本模組只含**跨平台
//! 純邏輯**：MMIO transport register state machine（[`mmio`]）與 split virtqueue 解析
//! （[`queue`]）。真正接上 WHP `EXIT_MEM_ACCESS` / 指令模擬器、以及具體裝置（virtio-blk
//! /net）的 I/O，屬於 cfg(windows) 的接線層，之後再補。
//!
//! guest 實體記憶體以 [`GuestMemory`] trait 抽象：boot loop 端用 host-mapped 的
//! VirtualAlloc 區（[`SliceMem`]）實作；單元測試用同一個 [`SliceMem`] 包一塊 Vec。

pub mod blk;
pub mod image;
pub mod mmio;
pub mod queue;

/// virtio device type（virtio spec §5；DeviceID register 0x008）。
pub const DEVICE_ID_NET: u32 = 1;
pub const DEVICE_ID_BLK: u32 = 2;

/// virtio-mmio MagicValue（"virt" little-endian，register 0x000）。
pub const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;
/// virtio-mmio 版本（modern, register 0x004）。
pub const VIRTIO_MMIO_VERSION: u32 = 2;

/// feature bit：VIRTIO_F_VERSION_1（bit 32，modern 介面必備）。
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// Status register（0x070）bit 旗標（virtio spec §2.1）。
pub const STATUS_ACKNOWLEDGE: u32 = 1;
pub const STATUS_DRIVER: u32 = 2;
pub const STATUS_DRIVER_OK: u32 = 4;
pub const STATUS_FEATURES_OK: u32 = 8;
pub const STATUS_DEVICE_NEEDS_RESET: u32 = 64;
pub const STATUS_FAILED: u32 = 128;

/// 讀寫 guest 實體記憶體的抽象（GPA 定址，little-endian helpers）。
pub trait GuestMemory {
    /// 從 `gpa` 起讀 `buf.len()` bytes；越界回傳明確錯誤。
    fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<(), String>;
    /// 從 `gpa` 起寫入 `buf`；越界回傳明確錯誤。
    fn write(&mut self, gpa: u64, buf: &[u8]) -> Result<(), String>;

    fn read_u16(&self, gpa: u64) -> Result<u16, String> {
        let mut b = [0u8; 2];
        self.read(gpa, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }
    fn read_u32(&self, gpa: u64) -> Result<u32, String> {
        let mut b = [0u8; 4];
        self.read(gpa, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }
    fn read_u64(&self, gpa: u64) -> Result<u64, String> {
        let mut b = [0u8; 8];
        self.read(gpa, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }
    fn write_u16(&mut self, gpa: u64, v: u16) -> Result<(), String> {
        self.write(gpa, &v.to_le_bytes())
    }
    fn write_u32(&mut self, gpa: u64, v: u32) -> Result<(), String> {
        self.write(gpa, &v.to_le_bytes())
    }
}

/// 以一塊連續位元組 slice 為 backing 的 [`GuestMemory`]（`base` = 此 slice 對應的 GPA 起點）。
///
/// boot loop 端：`base = 0`、`data` = VirtualAlloc 的整塊 guest RAM。
/// 測試端：`base` 任意、`data` 為一塊 `Vec<u8>` 的 slice。
pub struct SliceMem<'a> {
    pub base: u64,
    pub data: &'a mut [u8],
}

impl<'a> SliceMem<'a> {
    pub fn new(base: u64, data: &'a mut [u8]) -> Self {
        Self { base, data }
    }

    /// 把 GPA 換算成 slice 內 offset；越界回傳 None。
    fn offset(&self, gpa: u64, len: usize) -> Option<usize> {
        let off = gpa.checked_sub(self.base)? as usize;
        let end = off.checked_add(len)?;
        if end <= self.data.len() {
            Some(off)
        } else {
            None
        }
    }
}

impl GuestMemory for SliceMem<'_> {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<(), String> {
        let off = self.offset(gpa, buf.len()).ok_or_else(|| {
            format!(
                "guest memory read out of range: gpa=0x{gpa:X} len={} (base=0x{:X} size={})",
                buf.len(),
                self.base,
                self.data.len()
            )
        })?;
        buf.copy_from_slice(&self.data[off..off + buf.len()]);
        Ok(())
    }

    fn write(&mut self, gpa: u64, buf: &[u8]) -> Result<(), String> {
        let off = self.offset(gpa, buf.len()).ok_or_else(|| {
            format!(
                "guest memory write out of range: gpa=0x{gpa:X} len={} (base=0x{:X} size={})",
                buf.len(),
                self.base,
                self.data.len()
            )
        })?;
        self.data[off..off + buf.len()].copy_from_slice(buf);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slicemem_roundtrip_le() {
        let mut backing = vec![0u8; 64];
        let mut mem = SliceMem::new(0x1000, &mut backing);
        mem.write_u32(0x1000, 0xDEAD_BEEF).unwrap();
        mem.write_u16(0x1010, 0x1234).unwrap();
        assert_eq!(mem.read_u32(0x1000).unwrap(), 0xDEAD_BEEF);
        assert_eq!(mem.read_u16(0x1010).unwrap(), 0x1234);
        // little-endian 落地檢查
        assert_eq!(&backing[0..4], &[0xEF, 0xBE, 0xAD, 0xDE]);
    }

    #[test]
    fn slicemem_rejects_out_of_range() {
        let mut backing = vec![0u8; 16];
        let mem = SliceMem::new(0x2000, &mut backing);
        assert!(mem.read_u32(0x1FFF).is_err()); // 低於 base
        assert!(mem.read_u32(0x200E).is_err()); // 跨越尾端（offset 14 + 4 > 16）
        assert!(mem.read_u32(0x200C).is_ok()); // offset 12 + 4 == 16，剛好
    }
}
