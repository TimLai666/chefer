//! virtio-blk 裝置邏輯（virtio spec §5.2，跨平台純邏輯）。
//!
//! 處理一條 descriptor chain 形式的 block request：解析 header（type + sector）、對
//! in-memory backing image 做讀/寫，寫回 status byte，回傳寫入 guest 的 bytes 數（供
//! used ring 的 len 欄位）。不碰 WHP／virtqueue 取得本身——chain 由 [`super::queue`]
//! 取出後傳入。M2：bundle 唯讀、data 讀寫，皆以 [`BlkDevice`] 表示。

use super::GuestMemory;
use super::queue::DescChain;
use std::sync::atomic::{AtomicU64, Ordering};

/// 區塊大小（virtio-blk 固定 512 bytes/sector）。
pub const SECTOR_SIZE: u64 = 512;

// ── request type（virtio_blk_req.type，spec §5.2.6）──
const VIRTIO_BLK_T_IN: u32 = 0; // 讀（device → guest buffer）
const VIRTIO_BLK_T_OUT: u32 = 1; // 寫（guest buffer → device）
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_T_GET_ID: u32 = 8;

// ── status byte（virtio spec §5.2.6）──
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

const HDR_LEN: u32 = 16; // type(4) + reserved(4) + sector(8)
const BLK_ID_BYTES: usize = 20; // GET_ID 回傳的裝置 id 長度
static BLK_TRACE_SEQ: AtomicU64 = AtomicU64::new(0);

fn trace_blk(req: &str, sector: u64, bytes: u64, segs: usize) {
    if std::env::var_os("CHEFER_WHP_TRACE_BLK").is_none() {
        return;
    }
    let seq = BLK_TRACE_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    if seq > 128 {
        return;
    }
    eprintln!("[virtio-blk {seq}] {req}: sector={sector} bytes={bytes} segs={segs}");
}

/// virtio-blk 裝置：以 `Vec<u8>` 為 backing image。
pub struct BlkDevice {
    backing: Vec<u8>,
    read_only: bool,
    id: [u8; BLK_ID_BYTES],
}

impl BlkDevice {
    /// 以既有 image bytes 建立裝置；`read_only` 對應 bundle（true）vs data（false）。
    /// `id` 為 GET_ID 回傳的識別字串（截斷/補零到 20 bytes）。
    pub fn new(backing: Vec<u8>, read_only: bool, id: &str) -> Self {
        let mut id_buf = [0u8; BLK_ID_BYTES];
        let bytes = id.as_bytes();
        let n = bytes.len().min(BLK_ID_BYTES);
        id_buf[..n].copy_from_slice(&bytes[..n]);
        Self {
            backing,
            read_only,
            id: id_buf,
        }
    }

    /// 容量（sector 數），供 config space（capacity @ offset 0）與 driver 讀取。
    pub fn capacity_sectors(&self) -> u64 {
        self.backing.len() as u64 / SECTOR_SIZE
    }

    /// 取出 backing（接線層在關機時回寫 host data image 用）。
    pub fn into_backing(self) -> Vec<u8> {
        self.backing
    }

    /// virtio-blk config space 讀取（小端，len 1/2/4）。目前僅 capacity（u64 @ 0）。
    pub fn config_read(&self, offset: u64, len: u8) -> u32 {
        let cap = self.capacity_sectors().to_le_bytes();
        let mut out = [0u8; 4];
        for (i, slot) in out.iter_mut().enumerate().take(len as usize) {
            *slot = cap.get(offset as usize + i).copied().unwrap_or(0);
        }
        u32::from_le_bytes(out)
    }

    /// 處理一條 request chain，回傳寫回 guest 的 bytes 數（used ring len）。
    ///
    /// chain 慣例：readable[0]=16-byte header；IN 時 data 在 writable[..last]、OUT 時
    /// data 在 readable[1..]；writable.last()=1-byte status。
    pub fn process_chain<M: GuestMemory>(
        &mut self,
        chain: &DescChain,
        mem: &mut M,
    ) -> Result<u32, String> {
        // status byte 必為最後一段 writable、長度 1。
        let Some(&(status_gpa, status_len)) = chain.writable.last() else {
            return Err("virtio-blk chain has no writable status byte".to_string());
        };
        if status_len != 1 {
            return Err(format!(
                "virtio-blk status descriptor len {status_len} != 1"
            ));
        }
        let Some(&(hdr_gpa, hdr_len)) = chain.readable.first() else {
            return Err("virtio-blk chain has no readable header".to_string());
        };
        if hdr_len < HDR_LEN {
            return Err(format!("virtio-blk header len {hdr_len} < {HDR_LEN}"));
        }

        let req_type = mem.read_u32(hdr_gpa)?;
        let sector = mem.read_u64(hdr_gpa + 8)?;

        let (status, written) = match req_type {
            VIRTIO_BLK_T_IN => self.handle_in(chain, mem, sector)?,
            VIRTIO_BLK_T_OUT => self.handle_out(chain, mem, sector)?,
            VIRTIO_BLK_T_FLUSH => (VIRTIO_BLK_S_OK, 0), // in-memory backing：flush 即 ok
            VIRTIO_BLK_T_GET_ID => self.handle_get_id(chain, mem)?,
            _ => (VIRTIO_BLK_S_UNSUPP, 0),
        };

        mem.write(status_gpa, &[status])?;
        // used len = 寫回 guest 的 data bytes + 1（status）。
        Ok(written + 1)
    }

    /// 讀（device → guest）：把 backing[sector..] 依序填入 writable data 段（扣掉 status）。
    fn handle_in<M: GuestMemory>(
        &self,
        chain: &DescChain,
        mem: &mut M,
        sector: u64,
    ) -> Result<(u8, u32), String> {
        let data_segs = &chain.writable[..chain.writable.len() - 1];
        let total: u64 = data_segs.iter().map(|&(_, l)| l as u64).sum();
        trace_blk("in", sector, total, data_segs.len());
        let mut off = match sector.checked_mul(SECTOR_SIZE) {
            Some(v) => v as usize,
            None => return Ok((VIRTIO_BLK_S_IOERR, 0)),
        };
        if off as u64 + total > self.backing.len() as u64 {
            return Ok((VIRTIO_BLK_S_IOERR, 0));
        }
        let mut written = 0u32;
        for &(gpa, len) in data_segs {
            let len = len as usize;
            mem.write(gpa, &self.backing[off..off + len])?;
            off += len;
            written += len as u32;
        }
        Ok((VIRTIO_BLK_S_OK, written))
    }

    /// 寫（guest → device）：把 readable data 段（扣掉 header）依序寫入 backing[sector..]。
    fn handle_out<M: GuestMemory>(
        &mut self,
        chain: &DescChain,
        mem: &mut M,
        sector: u64,
    ) -> Result<(u8, u32), String> {
        if self.read_only {
            return Ok((VIRTIO_BLK_S_IOERR, 0));
        }
        let data_segs = &chain.readable[1..];
        let total: u64 = data_segs.iter().map(|&(_, l)| l as u64).sum();
        trace_blk("out", sector, total, data_segs.len());
        let mut off = match sector.checked_mul(SECTOR_SIZE) {
            Some(v) => v as usize,
            None => return Ok((VIRTIO_BLK_S_IOERR, 0)),
        };
        if off as u64 + total > self.backing.len() as u64 {
            return Ok((VIRTIO_BLK_S_IOERR, 0));
        }
        for &(gpa, len) in data_segs {
            let len = len as usize;
            let mut buf = vec![0u8; len];
            mem.read(gpa, &mut buf)?;
            self.backing[off..off + len].copy_from_slice(&buf);
            off += len;
        }
        // OUT 不寫資料回 guest，used data len = 0。
        Ok((VIRTIO_BLK_S_OK, 0))
    }

    /// GET_ID：把裝置 id 寫進第一個 writable data 段（最多 20 bytes）。
    fn handle_get_id<M: GuestMemory>(
        &self,
        chain: &DescChain,
        mem: &mut M,
    ) -> Result<(u8, u32), String> {
        let Some(&(gpa, len)) = chain.writable.first() else {
            return Ok((VIRTIO_BLK_S_UNSUPP, 0));
        };
        let n = (len as usize).min(BLK_ID_BYTES);
        mem.write(gpa, &self.id[..n])?;
        Ok((VIRTIO_BLK_S_OK, n as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::SliceMem;

    // GPA 佈局：header @0x100、data @0x200、status @0x800。
    const HDR: u64 = 0x100;
    const DATA: u64 = 0x200;
    const STATUS: u64 = 0x800;

    fn write_hdr(mem: &mut SliceMem, req_type: u32, sector: u64) {
        mem.write_u32(HDR, req_type).unwrap();
        mem.write_u32(HDR + 4, 0).unwrap(); // reserved
        mem.write(HDR + 8, &sector.to_le_bytes()).unwrap();
    }

    fn read_chain(data_len: u32) -> DescChain {
        DescChain {
            head: 0,
            readable: vec![(HDR, HDR_LEN)],
            writable: vec![(DATA, data_len), (STATUS, 1)],
        }
    }

    fn write_chain(data_len: u32) -> DescChain {
        DescChain {
            head: 0,
            readable: vec![(HDR, HDR_LEN), (DATA, data_len)],
            writable: vec![(STATUS, 1)],
        }
    }

    #[test]
    fn capacity_from_backing() {
        let dev = BlkDevice::new(vec![0u8; 4096], true, "x");
        assert_eq!(dev.capacity_sectors(), 8); // 4096 / 512
        // config: capacity u64 @0，低字 = 8
        assert_eq!(dev.config_read(0, 4), 8);
    }

    #[test]
    fn read_in_returns_backing_data() {
        let mut backing = vec![0u8; 1024];
        backing[512..516].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // sector 1
        let mut dev = BlkDevice::new(backing, true, "bundle");

        let mut store = vec![0u8; 0x1000];
        let mut mem = SliceMem::new(0, &mut store);
        write_hdr(&mut mem, VIRTIO_BLK_T_IN, 1); // 讀 sector 1
        let chain = read_chain(512);

        let used = dev.process_chain(&chain, &mut mem).unwrap();
        assert_eq!(used, 512 + 1); // data + status
        let mut got = [0u8; 4];
        mem.read(DATA, &mut got).unwrap();
        assert_eq!(got, [0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(mem.read_u16(STATUS).unwrap() as u8, VIRTIO_BLK_S_OK);
    }

    #[test]
    fn write_out_updates_backing() {
        let mut dev = BlkDevice::new(vec![0u8; 1024], false, "data");
        let mut store = vec![0u8; 0x1000];
        let mut mem = SliceMem::new(0, &mut store);
        write_hdr(&mut mem, VIRTIO_BLK_T_OUT, 1);
        mem.write(DATA, &[1, 2, 3, 4]).unwrap();
        let chain = write_chain(4);

        let used = dev.process_chain(&chain, &mut mem).unwrap();
        assert_eq!(used, 1); // OUT：只有 status
        assert_eq!(mem.read_u16(STATUS).unwrap() as u8, VIRTIO_BLK_S_OK);
        let backing = dev.into_backing();
        assert_eq!(&backing[512..516], &[1, 2, 3, 4]);
    }

    #[test]
    fn write_to_readonly_is_ioerr() {
        let mut dev = BlkDevice::new(vec![0u8; 1024], true, "bundle");
        let mut store = vec![0u8; 0x1000];
        let mut mem = SliceMem::new(0, &mut store);
        write_hdr(&mut mem, VIRTIO_BLK_T_OUT, 0);
        let chain = write_chain(4);
        dev.process_chain(&chain, &mut mem).unwrap();
        assert_eq!(mem.read_u16(STATUS).unwrap() as u8, VIRTIO_BLK_S_IOERR);
    }

    #[test]
    fn read_past_end_is_ioerr() {
        let mut dev = BlkDevice::new(vec![0u8; 512], true, "x"); // 1 sector
        let mut store = vec![0u8; 0x1000];
        let mut mem = SliceMem::new(0, &mut store);
        write_hdr(&mut mem, VIRTIO_BLK_T_IN, 5); // 超出範圍
        let chain = read_chain(512);
        dev.process_chain(&chain, &mut mem).unwrap();
        assert_eq!(mem.read_u16(STATUS).unwrap() as u8, VIRTIO_BLK_S_IOERR);
    }

    #[test]
    fn get_id_returns_device_id() {
        let mut dev = BlkDevice::new(vec![0u8; 512], true, "chefer-bundle");
        let mut store = vec![0u8; 0x1000];
        let mut mem = SliceMem::new(0, &mut store);
        write_hdr(&mut mem, VIRTIO_BLK_T_GET_ID, 0);
        let chain = read_chain(BLK_ID_BYTES as u32);
        dev.process_chain(&chain, &mut mem).unwrap();
        let mut got = [0u8; 13];
        mem.read(DATA, &mut got).unwrap();
        assert_eq!(&got, b"chefer-bundle");
        assert_eq!(mem.read_u16(STATUS).unwrap() as u8, VIRTIO_BLK_S_OK);
    }

    #[test]
    fn unsupported_type_is_unsupp() {
        let mut dev = BlkDevice::new(vec![0u8; 512], true, "x");
        let mut store = vec![0u8; 0x1000];
        let mut mem = SliceMem::new(0, &mut store);
        write_hdr(&mut mem, 99, 0);
        let chain = read_chain(8);
        dev.process_chain(&chain, &mut mem).unwrap();
        assert_eq!(mem.read_u16(STATUS).unwrap() as u8, VIRTIO_BLK_S_UNSUPP);
    }

    #[test]
    fn flush_is_ok() {
        let mut dev = BlkDevice::new(vec![0u8; 512], false, "x");
        let mut store = vec![0u8; 0x1000];
        let mut mem = SliceMem::new(0, &mut store);
        write_hdr(&mut mem, VIRTIO_BLK_T_FLUSH, 0);
        let chain = DescChain {
            head: 0,
            readable: vec![(HDR, HDR_LEN)],
            writable: vec![(STATUS, 1)],
        };
        let used = dev.process_chain(&chain, &mut mem).unwrap();
        assert_eq!(used, 1);
        assert_eq!(mem.read_u16(STATUS).unwrap() as u8, VIRTIO_BLK_S_OK);
    }
}
