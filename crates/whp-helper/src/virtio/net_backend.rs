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
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address};

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

/// host 端網路 gateway + host→guest 埠轉發：以 smoltcp 當 guest 的 gateway，
/// 對每個宣告的埠在 host 開一個 TCP listener，accept 後以 smoltcp TCP socket 連到
/// guest 的服務埠，雙向搬 bytes。接線層在每次 virtio-net notify / timer tick 呼叫
/// [`NetBackend::poll`]，由它驅動 smoltcp（讀 phy.rx 產 phy.tx）與連線搬運。
pub struct NetBackend {
    iface: Interface,
    sockets: SocketSet<'static>,
    guest_ip: Ipv4Address,
    forwards: Vec<(TcpListener, u16)>,
    conns: Vec<Conn>,
    next_local_port: u16,
}

struct Conn {
    host: TcpStream,
    handle: SocketHandle,
}

impl NetBackend {
    /// 以 gateway MAC/IP 與 guest IP 建立 smoltcp interface（綁在給定 phy 上）。
    pub fn new(
        gateway_mac: [u8; 6],
        gateway_ip: Ipv4Address,
        guest_ip: Ipv4Address,
        phy: &mut VirtioNetPhy,
    ) -> Self {
        let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(gateway_mac)));
        let mut iface = Interface::new(config, phy, Instant::from_millis(0));
        iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::new(gateway_ip.into(), 24));
        });
        Self {
            iface,
            sockets: SocketSet::new(vec![]),
            guest_ip,
            forwards: Vec::new(),
            conns: Vec::new(),
            next_local_port: 49152,
        }
    }

    /// 新增 host→guest 埠轉發：host `127.0.0.1:host_port` → guest `:guest_port`。
    /// 回傳實際綁定的 host port（傳 0 由 OS 配，方便測試）。
    pub fn add_forward(&mut self, host_port: u16, guest_port: u16) -> std::io::Result<u16> {
        let listener = TcpListener::bind(("127.0.0.1", host_port))?;
        listener.set_nonblocking(true)?;
        let actual = listener.local_addr()?.port();
        self.forwards.push((listener, guest_port));
        Ok(actual)
    }

    fn alloc_local_port(&mut self) -> u16 {
        let p = self.next_local_port;
        self.next_local_port = if p >= 65534 { 49152 } else { p + 1 };
        p
    }

    /// 驅動 smoltcp + 搬 host↔guest bytes。`phy` 為與 virtio-net 橋接的 device
    /// （guest 送來的 frame 已 push 進去；產出的 frame 之後由接線層 pop 給 guest）。
    pub fn poll(&mut self, phy: &mut VirtioNetPhy, now: Instant) {
        self.iface.poll(now, phy, &mut self.sockets);
        self.accept_new();
        self.pump();
        self.iface.poll(now, phy, &mut self.sockets);
    }

    fn accept_new(&mut self) {
        // 先收集 accepted（釋放 forwards 借用）再開 smoltcp socket。
        let mut accepted: Vec<(TcpStream, u16)> = Vec::new();
        for (listener, guest_port) in &self.forwards {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => accepted.push((stream, *guest_port)),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
        for (stream, guest_port) in accepted {
            let _ = stream.set_nonblocking(true);
            let local = self.alloc_local_port();
            let mut sock = tcp::Socket::new(
                tcp::SocketBuffer::new(vec![0u8; 65536]),
                tcp::SocketBuffer::new(vec![0u8; 65536]),
            );
            let connected = sock
                .connect(
                    self.iface.context(),
                    (IpAddress::Ipv4(self.guest_ip), guest_port),
                    local,
                )
                .is_ok();
            if connected {
                let handle = self.sockets.add(sock);
                self.conns.push(Conn {
                    host: stream,
                    handle,
                });
            }
        }
    }

    fn pump(&mut self) {
        let mut closed = Vec::new();
        for (i, conn) in self.conns.iter_mut().enumerate() {
            let sock = self.sockets.get_mut::<tcp::Socket>(conn.handle);
            // guest → host：把 guest 回應寫到 host socket。
            if sock.can_recv() {
                let _ = sock.recv(|data| {
                    let n = conn.host.write(data).unwrap_or(0);
                    (n, ())
                });
            }
            // host → guest：把 host 來的 bytes 送進 guest。
            if sock.can_send() {
                let mut buf = [0u8; 4096];
                match conn.host.read(&mut buf) {
                    Ok(0) => sock.close(), // host 端關閉
                    Ok(n) => {
                        let _ = sock.send_slice(&buf[..n]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => sock.close(),
                }
            }
            if sock.state() == tcp::State::Closed {
                closed.push(i);
            }
        }
        for i in closed.into_iter().rev() {
            let conn = self.conns.remove(i);
            self.sockets.remove(conn.handle);
        }
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

    #[test]
    fn forward_initiates_connection_to_guest() {
        // host 連到轉發埠後，NetBackend 應對 guest 發起連線（先 ARP 找 guest MAC）——
        // 證明 host→guest 埠轉發的發起端可行（完整握手需 guest 回應，留實機驗證）。
        let mut phy = VirtioNetPhy::new();
        let gw_ip = Ipv4Address::new(10, 0, 2, 2);
        let guest_ip = Ipv4Address::new(10, 0, 2, 15);
        let mut backend = NetBackend::new(
            [0x52, 0x54, 0x00, 0x00, 0x00, 0x02],
            gw_ip,
            guest_ip,
            &mut phy,
        );
        let host_port = backend.add_forward(0, 6379).unwrap();

        // host 端連線（loopback，立即進 listener backlog）。
        let _client = std::net::TcpStream::connect(("127.0.0.1", host_port)).unwrap();
        for ms in 0..50 {
            backend.poll(&mut phy, Instant::from_millis(ms));
        }

        // device tx 應有一個 ARP request：who-has guest_ip（target protocol addr @ 38..42）。
        let mut found = false;
        while let Some(frame) = phy.pop_to_guest() {
            if frame.len() >= 42
                && frame[12..14] == [0x08, 0x06]
                && frame[38..42] == guest_ip.octets()
            {
                found = true;
            }
        }
        assert!(found, "NetBackend 應對 guest 發 ARP（埠轉發發起）");
    }
}
