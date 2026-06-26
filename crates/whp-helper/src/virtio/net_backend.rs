//! WHP virtio-net 的 host 端 user-mode 網路 backend（smoltcp gateway）。
//!
//! WHP 不像 QEMU 有內建 slirp、不像 VZ 有 VZNAT，host 端要自己當 guest 的 gateway。
//! 本模組以 smoltcp 提供純 Rust 的 user-space 網路堆疊：
//! - [`VirtioNetPhy`] 是 smoltcp 的 phy device，用兩個 frame 佇列橋接 virtio-net——
//!   guest 經 virtio-net tx 送出的 ethernet frame 餵進 `rx`（待 smoltcp 收），smoltcp
//!   產生的 frame 進 `tx`（待經 virtio-net rx 回填 guest）。
//! - 之後在此 device 上跑 smoltcp `Interface`（gateway IP），處理 ARP/IP；host→guest
//!   埠轉發以 smoltcp TCP socket 橋接到 host 真 socket。
//!
//! 接線層（cfg windows boot loop）負責：把 virtio-net tx queue 取出的 frame 呼叫
//! [`VirtioNetPhy::push_from_guest`]、把 [`VirtioNetPhy::pop_to_guest`] 的 frame 經
//! virtio-net rx queue 填回 guest。

use std::collections::VecDeque;

use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

/// 乙太網 MTU（virtio-net 預設）。
pub const MTU: usize = 1500;

/// smoltcp phy device：以兩個 frame 佇列橋接 WHP 的 virtio-net。
#[derive(Default)]
pub struct VirtioNetPhy {
    /// guest → host：virtio-net tx 來的 frame，待 smoltcp `receive`。
    rx: VecDeque<Vec<u8>>,
    /// host → guest：smoltcp `transmit` 產的 frame，待經 virtio-net rx 給 guest。
    tx: VecDeque<Vec<u8>>,
}

impl VirtioNetPhy {
    pub fn new() -> Self {
        Self::default()
    }

    /// guest 經 virtio-net tx 送來一個 ethernet frame（接線層呼叫）。
    pub fn push_from_guest(&mut self, frame: Vec<u8>) {
        self.rx.push_back(frame);
    }

    /// 取出一個要給 guest（經 virtio-net rx）的 ethernet frame；無則 None。
    pub fn pop_to_guest(&mut self) -> Option<Vec<u8>> {
        self.tx.pop_front()
    }

    /// 是否有待送給 guest 的 frame（接線層據此決定是否注入 rx + IRQ）。
    pub fn has_guest_frames(&self) -> bool {
        !self.tx.is_empty()
    }
}

/// smoltcp 收封包用的 token：持有一個 guest 送來的 frame。
pub struct PhyRxToken(Vec<u8>);

/// smoltcp 送封包用的 token：consume 時把 frame 推進 tx 佇列。
pub struct PhyTxToken<'a>(&'a mut VecDeque<Vec<u8>>);

impl RxToken for PhyRxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl TxToken for PhyTxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        self.0.push_back(buf);
        r
    }
}

impl Device for VirtioNetPhy {
    type RxToken<'a> = PhyRxToken;
    type TxToken<'a> = PhyTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(PhyRxToken, PhyTxToken<'_>)> {
        let frame = self.rx.pop_front()?;
        Some((PhyRxToken(frame), PhyTxToken(&mut self.tx)))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<PhyTxToken<'_>> {
        Some(PhyTxToken(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = MTU;
        caps.medium = Medium::Ethernet;
        caps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::iface::{Config, Interface};
    use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, Ipv4Address};

    #[test]
    fn phy_frame_roundtrip() {
        let mut phy = VirtioNetPhy::new();
        assert!(phy.pop_to_guest().is_none());
        // guest 送一個 frame → smoltcp receive 拿得到
        phy.push_from_guest(vec![1, 2, 3, 4]);
        let got = phy
            .receive(Instant::from_millis(0))
            .map(|(rx, _tx)| rx.consume(|f| f.to_vec()));
        assert_eq!(got, Some(vec![1, 2, 3, 4]));
        // smoltcp transmit 一個 frame → pop_to_guest 拿得到
        let tx = phy.transmit(Instant::from_millis(0)).unwrap();
        tx.consume(3, |buf| buf.copy_from_slice(&[9, 8, 7]));
        assert!(phy.has_guest_frames());
        assert_eq!(phy.pop_to_guest(), Some(vec![9, 8, 7]));
    }

    #[test]
    fn smoltcp_gateway_replies_to_guest_arp() {
        // 驗證 smoltcp 整合可行：以 VirtioNetPhy 建一個 gateway interface，
        // 餵 guest 的 ARP request（who-has gateway），poll 後 device tx 應有 ARP reply。
        let mut phy = VirtioNetPhy::new();
        let gw_mac = EthernetAddress([0x52, 0x54, 0x00, 0x00, 0x00, 0x02]);
        let gw_ip = Ipv4Address::new(10, 0, 2, 2);
        let guest_mac = EthernetAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        let guest_ip = Ipv4Address::new(10, 0, 2, 15);

        let config = Config::new(HardwareAddress::Ethernet(gw_mac));
        let mut iface = Interface::new(config, &mut phy, Instant::from_millis(0));
        iface.update_ip_addrs(|addrs| {
            addrs.push(IpCidr::new(gw_ip.into(), 24)).unwrap();
        });

        // 組一個 ARP request：guest 問「誰是 10.0.2.2」。
        let mut arp = Vec::new();
        arp.extend_from_slice(&gw_mac.0); // 目的 MAC（broadcast 也可，但用 gw 簡化）
        arp.extend_from_slice(&guest_mac.0); // 來源 MAC
        arp.extend_from_slice(&[0x08, 0x06]); // ethertype ARP
        arp.extend_from_slice(&[0x00, 0x01, 0x08, 0x00, 0x06, 0x04, 0x00, 0x01]); // htype/ptype/hlen/plen/op=request
        arp.extend_from_slice(&guest_mac.0); // sender HW
        arp.extend_from_slice(&guest_ip.octets()); // sender IP
        arp.extend_from_slice(&[0u8; 6]); // target HW（未知）
        arp.extend_from_slice(&gw_ip.octets()); // target IP
        phy.push_from_guest(arp);

        let mut sockets = smoltcp::iface::SocketSet::new(vec![]);
        iface.poll(Instant::from_millis(0), &mut phy, &mut sockets);

        // device tx 應有一個 ARP reply（gateway 回「我是 10.0.2.2，MAC 是 ...」）。
        let reply = phy.pop_to_guest().expect("gateway should reply to ARP");
        assert_eq!(&reply[12..14], &[0x08, 0x06], "回應應為 ARP");
        // ARP opcode @ ethernet(14)+6 = offset 20..22 應為 reply(2)
        assert_eq!(&reply[20..22], &[0x00, 0x02], "ARP opcode 應為 reply");
    }
}
