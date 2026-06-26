//! virtio-net 裝置邏輯（virtio spec §5.1，跨平台純邏輯）。
//!
//! 只處理 descriptor chain ↔ ethernet frame 的搬運：
//! - **tx**（queue 1）：從 guest 放進 chain 的 buffer 取出要送出的 ethernet frame
//!   （去掉前置的 virtio_net_hdr）。
//! - **rx**（queue 0）：把 host 收到的 ethernet frame 填回 guest 提供的 buffer
//!   （前面補一個零值 virtio_net_hdr）。
//!
//! 不含 host 端網路 backend（NAT/DHCP/ARP/TCP-UDP 與真實網路進出、host→guest 埠轉發）
//! ——那是 cfg(windows) 的接線層，會把 tx frame 餵給 backend、把 backend 收到的 frame
//! 經 rx 填回 guest。

use super::GuestMemory;
use super::queue::DescChain;

/// virtio_net_hdr 長度（modern / VIRTIO_F_VERSION_1：含 num_buffers 欄位，共 12 bytes）。
pub const VIRTIO_NET_HDR_LEN: usize = 12;

/// config status：VIRTIO_NET_S_LINK_UP（bit 0）。
const VIRTIO_NET_S_LINK_UP: u16 = 1;

/// virtio-net 裝置：保存 MAC，提供 tx/rx frame 搬運與 config space。
pub struct NetDevice {
    mac: [u8; 6],
}

impl NetDevice {
    pub fn new(mac: [u8; 6]) -> Self {
        Self { mac }
    }

    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// virtio-net config space（小端，len 1/2/4）：mac[0..6] + status(u16 @6, LINK_UP)。
    pub fn config_read(&self, offset: u64, len: u8) -> u32 {
        let mut cfg = [0u8; 8];
        cfg[..6].copy_from_slice(&self.mac);
        cfg[6..8].copy_from_slice(&VIRTIO_NET_S_LINK_UP.to_le_bytes());
        let mut out = [0u8; 4];
        for (i, slot) in out.iter_mut().enumerate().take(len as usize) {
            *slot = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
        u32::from_le_bytes(out)
    }

    /// 從 tx chain 的 readable 段取出 ethernet frame：串接所有 readable bytes，去掉前
    /// `VIRTIO_NET_HDR_LEN` 的 virtio_net_hdr。
    pub fn read_tx_frame<M: GuestMemory>(
        &self,
        chain: &DescChain,
        mem: &M,
    ) -> Result<Vec<u8>, String> {
        let mut buf = Vec::new();
        for &(gpa, len) in &chain.readable {
            let mut seg = vec![0u8; len as usize];
            mem.read(gpa, &mut seg)?;
            buf.extend_from_slice(&seg);
        }
        if buf.len() < VIRTIO_NET_HDR_LEN {
            return Err(format!(
                "virtio-net tx chain shorter than header: {} bytes",
                buf.len()
            ));
        }
        Ok(buf.split_off(VIRTIO_NET_HDR_LEN))
    }

    /// 把 ethernet frame 填入 rx chain 的 writable 段：前 12 bytes 為零值 virtio_net_hdr
    /// （num_buffers=1），其後接 frame。回傳寫入 guest 的總 bytes（hdr + frame）。
    /// 不採用 mergeable rx buffers，故要求單一 chain 容量足夠。
    pub fn write_rx_frame<M: GuestMemory>(
        &self,
        chain: &DescChain,
        mem: &mut M,
        frame: &[u8],
    ) -> Result<u32, String> {
        let total = VIRTIO_NET_HDR_LEN + frame.len();
        let capacity: usize = chain.writable.iter().map(|&(_, l)| l as usize).sum();
        if capacity < total {
            return Err(format!(
                "virtio-net rx buffer too small: need {total}, have {capacity}"
            ));
        }
        let mut payload = vec![0u8; VIRTIO_NET_HDR_LEN];
        payload[10..12].copy_from_slice(&1u16.to_le_bytes()); // num_buffers = 1
        payload.extend_from_slice(frame);
        let mut off = 0usize;
        for &(gpa, len) in &chain.writable {
            if off >= payload.len() {
                break;
            }
            let n = (len as usize).min(payload.len() - off);
            mem.write(gpa, &payload[off..off + n])?;
            off += n;
        }
        Ok(total as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::SliceMem;

    const HDR: u64 = 0x100; // virtio_net_hdr
    const DATA: u64 = 0x200; // frame

    fn dev() -> NetDevice {
        NetDevice::new([0x52, 0x54, 0x00, 0x12, 0x34, 0x56])
    }

    #[test]
    fn config_exposes_mac_and_link_up() {
        let d = dev();
        // mac 低 4 bytes（offset 0, len 4）
        assert_eq!(
            d.config_read(0, 4),
            u32::from_le_bytes([0x52, 0x54, 0x00, 0x12])
        );
        // status @6 = LINK_UP
        assert_eq!(d.config_read(6, 2) as u16, VIRTIO_NET_S_LINK_UP);
    }

    #[test]
    fn tx_strips_header_returns_frame() {
        let mut backing = vec![0u8; 0x1000];
        let mut mem = SliceMem::new(0, &mut backing);
        // hdr 12 bytes（任意）+ frame 4 bytes
        mem.write(HDR, &[0u8; VIRTIO_NET_HDR_LEN]).unwrap();
        mem.write(HDR + VIRTIO_NET_HDR_LEN as u64, &[0xAA, 0xBB, 0xCC, 0xDD])
            .unwrap();
        // 單一 readable 段含 hdr+frame
        let chain = DescChain {
            head: 0,
            readable: vec![(HDR, VIRTIO_NET_HDR_LEN as u32 + 4)],
            writable: vec![],
        };
        let frame = dev().read_tx_frame(&chain, &mem).unwrap();
        assert_eq!(frame, vec![0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn tx_too_short_errors() {
        let mut backing = vec![0u8; 0x1000];
        let mem = SliceMem::new(0, &mut backing);
        let chain = DescChain {
            head: 0,
            readable: vec![(HDR, 4)], // < 12
            writable: vec![],
        };
        assert!(
            dev()
                .read_tx_frame(&chain, &mem)
                .unwrap_err()
                .contains("shorter")
        );
    }

    #[test]
    fn rx_writes_header_then_frame() {
        let mut backing = vec![0u8; 0x1000];
        let mut mem = SliceMem::new(0, &mut backing);
        let chain = DescChain {
            head: 0,
            readable: vec![],
            writable: vec![(DATA, 2048)],
        };
        let frame = [0x11, 0x22, 0x33];
        let written = dev().write_rx_frame(&chain, &mut mem, &frame).unwrap();
        assert_eq!(written, VIRTIO_NET_HDR_LEN as u32 + 3);
        // hdr：num_buffers (offset 10) = 1
        assert_eq!(mem.read_u16(DATA + 10).unwrap(), 1);
        // frame 緊接 hdr 後
        let mut got = [0u8; 3];
        mem.read(DATA + VIRTIO_NET_HDR_LEN as u64, &mut got)
            .unwrap();
        assert_eq!(got, frame);
    }

    #[test]
    fn rx_buffer_too_small_errors() {
        let mut backing = vec![0u8; 0x1000];
        let mut mem = SliceMem::new(0, &mut backing);
        let chain = DescChain {
            head: 0,
            readable: vec![],
            writable: vec![(DATA, 8)], // < 12 + frame
        };
        let err = dev()
            .write_rx_frame(&chain, &mut mem, &[1, 2, 3, 4])
            .unwrap_err();
        assert!(err.contains("too small"));
    }
}
