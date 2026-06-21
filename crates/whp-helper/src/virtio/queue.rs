//! Split virtqueue 解析（virtio spec §2.7，跨平台純邏輯）。
//!
//! 從 guest 記憶體（[`super::GuestMemory`]）讀 descriptor table / avail ring / used ring，
//! 取出 driver 放進 avail ring 的 descriptor chain，並把處理結果寫回 used ring。
//! 不支援 indirect descriptor（VIRTQ_DESC_F_INDIRECT）——遇到回傳錯誤。

use super::GuestMemory;
use super::mmio::QueueConfig;

/// descriptor flag：鏈結到下一個 descriptor。
const VIRTQ_DESC_F_NEXT: u16 = 1;
/// descriptor flag：此 buffer 供裝置寫入（device-writable）。
const VIRTQ_DESC_F_WRITE: u16 = 2;
/// descriptor flag：indirect descriptor table（本實作不支援）。
const VIRTQ_DESC_F_INDIRECT: u16 = 4;

const DESC_SIZE: u64 = 16; // addr(8) + len(4) + flags(2) + next(2)

/// 一條 descriptor chain：拆成 driver→device 唯讀段與 device→driver 可寫段。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DescChain {
    /// avail ring 取出的 head descriptor index（push used 時要回填）。
    pub head: u16,
    /// 唯讀段（裝置讀取，如 blk request header / 寫入資料）：(gpa, len)。
    pub readable: Vec<(u64, u32)>,
    /// 可寫段（裝置寫入，如 blk 讀出資料 / status byte）：(gpa, len)。
    pub writable: Vec<(u64, u32)>,
}

/// 綁定一個 ready 的 virtqueue（設定 + guest 記憶體），可逐一取出 avail descriptor chain。
pub struct SplitQueue<'a, M: GuestMemory> {
    cfg: QueueConfig,
    mem: &'a mut M,
    last_avail_idx: u16,
}

impl<'a, M: GuestMemory> SplitQueue<'a, M> {
    /// `cfg` 須為 ready 且 num 非零的佇列。`last_avail_idx` 由呼叫端跨次保存。
    pub fn new(cfg: QueueConfig, mem: &'a mut M, last_avail_idx: u16) -> Self {
        Self {
            cfg,
            mem,
            last_avail_idx,
        }
    }

    /// 目前已消費到的 avail index（呼叫端應保存，下次 [`SplitQueue::new`] 帶回）。
    pub fn last_avail_idx(&self) -> u16 {
        self.last_avail_idx
    }

    /// avail ring 的 idx 欄位（driver 已發布的 descriptor 數，環繞計數）。
    fn avail_idx(&self) -> Result<u16, String> {
        // avail ring layout: flags(u16) @0, idx(u16) @2, ring[] @4
        self.mem.read_u16(self.cfg.avail + 2)
    }

    /// 取下一條 driver 發布的 descriptor chain；無新項目回傳 None。
    pub fn pop_avail(&mut self) -> Result<Option<DescChain>, String> {
        if !self.cfg.ready || self.cfg.num == 0 {
            return Ok(None);
        }
        let avail_idx = self.avail_idx()?;
        if avail_idx == self.last_avail_idx {
            return Ok(None);
        }
        let num = self.cfg.num;
        // avail.ring[] 是 u16 陣列，從 offset 4 起；以 queue size 取模環繞。
        let ring_slot = self.last_avail_idx % num;
        let head = self
            .mem
            .read_u16(self.cfg.avail + 4 + (ring_slot as u64) * 2)?;
        let chain = self.walk_chain(head, num)?;
        self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
        Ok(Some(chain))
    }

    /// 從 head 沿 NEXT 旗標走完整條 chain，依 WRITE 旗標分流到 readable/writable。
    fn walk_chain(&self, head: u16, num: u16) -> Result<DescChain, String> {
        let mut chain = DescChain {
            head,
            ..Default::default()
        };
        let mut idx = head;
        let mut count = 0u32;
        loop {
            if idx >= num {
                return Err(format!(
                    "descriptor index {idx} out of range (queue size {num})"
                ));
            }
            let desc = self.cfg.desc + (idx as u64) * DESC_SIZE;
            let addr = self.mem.read_u64(desc)?;
            let len = self.mem.read_u32(desc + 8)?;
            let flags = self.mem.read_u16(desc + 12)?;
            let next = self.mem.read_u16(desc + 14)?;

            if flags & VIRTQ_DESC_F_INDIRECT != 0 {
                return Err("indirect descriptors are not supported".to_string());
            }
            if flags & VIRTQ_DESC_F_WRITE != 0 {
                chain.writable.push((addr, len));
            } else {
                chain.readable.push((addr, len));
            }

            count += 1;
            if count > num as u32 {
                return Err("descriptor chain longer than queue size (loop?)".to_string());
            }
            if flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            idx = next;
        }
        Ok(chain)
    }

    /// 把一條已處理完的 chain 回填 used ring（`written` = 裝置寫入 writable 段的總 bytes）。
    pub fn push_used(&mut self, head: u16, written: u32) -> Result<(), String> {
        let num = self.cfg.num;
        // used ring layout: flags(u16) @0, idx(u16) @2, ring[]{id(u32) len(u32)} @4
        let used_idx = self.mem.read_u16(self.cfg.used + 2)?;
        let slot = used_idx % num;
        let elem = self.cfg.used + 4 + (slot as u64) * 8;
        self.mem.write_u32(elem, head as u32)?;
        self.mem.write_u32(elem + 4, written)?;
        // 先寫 elem 再 publish idx（驅動以 idx 為可見性屏障）。
        self.mem
            .write_u16(self.cfg.used + 2, used_idx.wrapping_add(1))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::SliceMem;

    // 在一塊 backing 上手工擺 desc table / avail ring / used ring，驗證解析。
    // 佈局：desc @0x100、avail @0x300、used @0x400，queue size = 8。
    const DESC: u64 = 0x100;
    const AVAIL: u64 = 0x300;
    const USED: u64 = 0x400;
    const QNUM: u16 = 8;

    fn cfg() -> QueueConfig {
        QueueConfig {
            num: QNUM,
            ready: true,
            desc: DESC,
            avail: AVAIL,
            used: USED,
        }
    }

    fn write_desc(mem: &mut SliceMem, idx: u64, addr: u64, len: u32, flags: u16, next: u16) {
        let d = DESC + idx * DESC_SIZE;
        mem.write(d, &addr.to_le_bytes()).unwrap();
        mem.write(d + 8, &len.to_le_bytes()).unwrap();
        mem.write(d + 12, &flags.to_le_bytes()).unwrap();
        mem.write(d + 14, &next.to_le_bytes()).unwrap();
    }

    fn publish_avail(mem: &mut SliceMem, slot: u64, head: u16, new_idx: u16) {
        mem.write_u16(AVAIL + 4 + slot * 2, head).unwrap();
        mem.write_u16(AVAIL + 2, new_idx).unwrap();
    }

    #[test]
    fn pop_single_descriptor() {
        let mut backing = vec![0u8; 0x1000];
        let mut mem = SliceMem::new(0, &mut backing);
        write_desc(&mut mem, 0, 0xDEAD, 512, VIRTQ_DESC_F_WRITE, 0);
        publish_avail(&mut mem, 0, 0, 1);

        let mut q = SplitQueue::new(cfg(), &mut mem, 0);
        let chain = q.pop_avail().unwrap().expect("one chain available");
        assert_eq!(chain.head, 0);
        assert_eq!(chain.writable, vec![(0xDEAD, 512)]);
        assert!(chain.readable.is_empty());
        // 消費後沒有第二條
        assert!(q.pop_avail().unwrap().is_none());
        assert_eq!(q.last_avail_idx(), 1);
    }

    #[test]
    fn pop_chained_readable_then_writable() {
        let mut backing = vec![0u8; 0x1000];
        let mut mem = SliceMem::new(0, &mut backing);
        // desc 0 (readable header) → desc 1 (writable data) → desc 2 (writable status)
        write_desc(&mut mem, 0, 0x1000, 16, VIRTQ_DESC_F_NEXT, 1);
        write_desc(
            &mut mem,
            1,
            0x2000,
            4096,
            VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
            2,
        );
        write_desc(&mut mem, 2, 0x3000, 1, VIRTQ_DESC_F_WRITE, 0);
        publish_avail(&mut mem, 0, 0, 1);

        let mut q = SplitQueue::new(cfg(), &mut mem, 0);
        let chain = q.pop_avail().unwrap().unwrap();
        assert_eq!(chain.readable, vec![(0x1000, 16)]);
        assert_eq!(chain.writable, vec![(0x2000, 4096), (0x3000, 1)]);
    }

    #[test]
    fn indirect_descriptor_rejected() {
        let mut backing = vec![0u8; 0x1000];
        let mut mem = SliceMem::new(0, &mut backing);
        write_desc(&mut mem, 0, 0x1000, 16, VIRTQ_DESC_F_INDIRECT, 0);
        publish_avail(&mut mem, 0, 0, 1);
        let mut q = SplitQueue::new(cfg(), &mut mem, 0);
        assert!(q.pop_avail().unwrap_err().contains("indirect"));
    }

    #[test]
    fn push_used_writes_ring_and_advances_idx() {
        let mut backing = vec![0u8; 0x1000];
        let mut mem = SliceMem::new(0, &mut backing);
        {
            let mut q = SplitQueue::new(cfg(), &mut mem, 0);
            q.push_used(3, 128).unwrap();
        }
        // used.ring[0] = {id=3, len=128}, used.idx = 1
        assert_eq!(mem.read_u32(USED + 4).unwrap(), 3);
        assert_eq!(mem.read_u32(USED + 8).unwrap(), 128);
        assert_eq!(mem.read_u16(USED + 2).unwrap(), 1);
    }

    #[test]
    fn avail_ring_wraps_at_queue_size() {
        let mut backing = vec![0u8; 0x1000];
        let mut mem = SliceMem::new(0, &mut backing);
        write_desc(&mut mem, 5, 0xBEEF, 64, VIRTQ_DESC_F_WRITE, 0);
        // last_avail_idx 從 QNUM 開始（環繞），ring slot = QNUM % QNUM = 0
        mem.write_u16(AVAIL + 4, 5).unwrap(); // ring[0] = desc 5
        mem.write_u16(AVAIL + 2, QNUM.wrapping_add(1)).unwrap(); // avail.idx = 9
        let mut q = SplitQueue::new(cfg(), &mut mem, QNUM);
        let chain = q.pop_avail().unwrap().unwrap();
        assert_eq!(chain.head, 5);
        assert_eq!(chain.writable, vec![(0xBEEF, 64)]);
    }
}
