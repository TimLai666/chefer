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
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::mpsc;
use std::time::Duration;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpListenEndpoint, Ipv4Address,
};

/// 出網 host TCP connect 的逾時（背景執行緒，不阻塞 VM run loop）。
const NAT_TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 「約定 DNS」位址（slirp 慣例 `10.0.2.3`；QEMU 路徑由 slirp 原生在同位址提供 DNS，
/// WHP 與之對齊）。guest 的 resolv.conf（kernel cmdline `ip=` 的 dns0 欄，見
/// `vmm-backend::whp_util::kernel_command_line`）指向這裡；接線層（main.rs drain_tx）把
/// 送到此位址 UDP/TCP:53 的查詢分流給 [`NetBackend::nat_outbound_dns`] /
/// [`NetBackend::nat_tcp_dns_syn`] 轉發到 host 設定的 DNS 上游。
pub const NAT_DNS_IP: [u8; 4] = [10, 0, 2, 3];

/// DNS 上游（host 設定的 DNS server）快取壽命：網路切換（VPN 連上/斷開）會改 host DNS，
/// 長駐 app 不能只在啟動時探一次；也不宜每個查詢都呼叫 Windows API。
const DNS_UPSTREAM_TTL: Duration = Duration::from_secs(60);

/// 乙太網 MTU（virtio-net 預設）。
pub const MTU: usize = 1500;

/// 是否開啟 net 封包追蹤（環境變數 `CHEFER_WHP_NET_TRACE` 非空）。診斷 host↔guest
/// 封包流用：印 smoltcp connect/狀態轉移、guest tx/rx frame——接線層（main.rs）共用。
pub fn net_trace_enabled() -> bool {
    use std::sync::OnceLock;
    static NET_TRACE: OnceLock<bool> = OnceLock::new();
    *NET_TRACE.get_or_init(|| std::env::var_os("CHEFER_WHP_NET_TRACE").is_some())
}

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

    /// 直接把一個 ethernet frame 排進 guest-bound 佇列（出網 NAT 合成的回程 frame 用，
    /// 不經 smoltcp）。與 smoltcp `transmit` 產的 frame 走同一佇列、由 fill_rx 送 guest。
    pub fn push_to_guest(&mut self, frame: Vec<u8>) {
        self.tx.push_back(frame);
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
    /// host UDP listener（綁 `[::1]:listen`）+ 對應 guest 埠。
    udp_forwards: Vec<(UdpSocket, u16)>,
    /// per-client UDP session：用 unique local port demux guest 的回程。
    udp_sessions: Vec<UdpSession>,
    /// 出網 NAT：gateway 的 MAC（合成回程 frame 的 L2 src）。
    gateway_mac: [u8; 6],
    /// 出網 UDP NAT：per (guest_ip, guest_port) flow 的 host UDP socket。
    nat_flows: Vec<NatFlow>,
    /// 出網 TCP NAT：per 4-tuple flow（smoltcp socket ↔ host TcpStream）。
    tcp_nat: Vec<TcpNatFlow>,
    next_local_port: u16,
    /// `10.0.2.3:53` DNS pivot 的上游（host 設定的 DNS server）；lazy 探測 + TTL 快取。
    dns_upstreams: Vec<SocketAddr>,
    dns_upstreams_at: Option<std::time::Instant>,
}

struct Conn {
    host: TcpStream,
    handle: SocketHandle,
    last_state: tcp::State,
}

/// 一個 host UDP client 的轉發 session：smoltcp UDP socket（綁 unique local port）+
/// 來源 client 位址 + 所屬 udp_forwards 索引（回程用其 listener send_to client）。
struct UdpSession {
    handle: SocketHandle,
    client: SocketAddr,
    forward_idx: usize,
}

/// 出網 UDP NAT 的一條 flow：以 guest 的 `(ip, port, dns)` 為鍵，持有一個 host UDP socket
/// 對外送/收。一般 flow 回程以 socket `recv_from` 得到的外部位址當合成 frame 的 src；
/// DNS pivot flow（guest → `10.0.2.3:53`）回程只收 `upstreams` 內的來源，且 src 一律
/// masquerade 成 `NAT_DNS_IP:53`——guest 端 resolver 多用 connected UDP socket，回程
/// src 不是它查的位址會被 kernel 丟掉。
struct NatFlow {
    guest_ip: [u8; 4],
    guest_port: u16,
    guest_mac: [u8; 6],
    sock: UdpSocket,
    /// DNS pivot flow；與一般 flow 分鍵（同一 guest src port 可能同時查外部 DNS 與 10.0.2.3）。
    dns: bool,
    /// DNS pivot 的 fanout 上游（一般 flow 為空）。
    upstreams: Vec<SocketAddr>,
}

/// 出網 TCP NAT 的 host 端連線狀態。
enum TcpConnState {
    /// 背景執行緒 connect 中；收到結果前 guest 側資料先緩在 smoltcp socket。
    Connecting(mpsc::Receiver<std::io::Result<TcpStream>>),
    /// 已連上外部 host。
    Up(TcpStream),
}

/// 出網 TCP NAT 的一條 flow（以 guest 4-tuple 為鍵）：guest 側由 smoltcp socket（在 dst
/// 位址上 listen→accept）承接，host 側為連到外部的 TcpStream。dst IP 已動態加進 iface。
struct TcpNatFlow {
    guest_ip: [u8; 4],
    guest_port: u16,
    dst_ip: [u8; 4],
    dst_port: u16,
    handle: SocketHandle,
    host: TcpConnState,
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
            // 約定 DNS 位址（同網段、on-link）：guest／pasta 送查詢前會對它 ARP，而 smoltcp
            // 只回答自己持有的位址——掛上 /32 讓 ARP 有回應。實際的 :53 流量不進 smoltcp
            // socket，由接線層 drain_tx 分流到 nat_outbound_dns / nat_tcp_dns_syn。
            let dns = Ipv4Address::new(NAT_DNS_IP[0], NAT_DNS_IP[1], NAT_DNS_IP[2], NAT_DNS_IP[3]);
            let _ = addrs.push(IpCidr::new(IpAddress::Ipv4(dns), 32));
        });
        Self {
            iface,
            sockets: SocketSet::new(vec![]),
            guest_ip,
            forwards: Vec::new(),
            conns: Vec::new(),
            udp_forwards: Vec::new(),
            udp_sessions: Vec::new(),
            gateway_mac,
            nat_flows: Vec::new(),
            tcp_nat: Vec::new(),
            next_local_port: 49152,
            dns_upstreams: Vec::new(),
            dns_upstreams_at: None,
        }
    }

    /// 新增 host→guest 埠轉發：host `[::1]:listen_port` → guest `:guest_port`。
    /// 回傳實際綁定的 listen port（傳 0 由 OS 配，方便測試）。
    ///
    /// 綁 **IPv6 loopback `[::1]`** 是刻意的：chefer-runtime 既有的 host≠guest 埠代理
    /// （`proxy.rs`）按 WSL2 wslrelay 慣例假設「後端把 guest 埠暴露在 localhost」，且其
    /// host==guest 的 Windows 補橋會佔 `127.0.0.1:guest`。helper 綁 `[::1]:guest` 正好
    /// 鏡像 wslrelay，使 runtime 的 v4→v6 fallback 與補橋都接得上、互不搶埠。
    pub fn add_forward(&mut self, listen_port: u16, guest_port: u16) -> std::io::Result<u16> {
        let listener = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, listen_port))?;
        listener.set_nonblocking(true)?;
        let actual = listener.local_addr()?.port();
        self.forwards.push((listener, guest_port));
        Ok(actual)
    }

    /// 新增 host→guest **UDP** 埠轉發：host `[::1]:listen_port` → guest `:guest_port`。
    /// 綁 `[::1]` 理由同 [`Self::add_forward`]（鏡像 wslrelay、與 runtime proxy 分層）。
    /// 回傳實際綁定的 listen port（傳 0 由 OS 配，方便測試）。
    pub fn add_udp_forward(&mut self, listen_port: u16, guest_port: u16) -> std::io::Result<u16> {
        let listener = UdpSocket::bind((std::net::Ipv6Addr::LOCALHOST, listen_port))?;
        listener.set_nonblocking(true)?;
        let actual = listener.local_addr()?.port();
        self.udp_forwards.push((listener, guest_port));
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
        self.tcp_nat_pump();
        self.udp_host_to_guest();
        self.iface.poll(now, phy, &mut self.sockets);
        self.udp_guest_to_host();
        // 出網 NAT：把 host UDP socket 收到的外部回應合成 frame 注入 guest rx。
        self.nat_poll(phy);
    }

    /// 出網 TCP SYN（接線層 drain_tx 分流後、push frame 給 smoltcp 前呼叫）：為此 4-tuple
    /// 預註冊——dst IP 動態加進 iface、建 smoltcp listen socket(dst,port)、背景 connect host。
    /// 重傳 SYN（同 4-tuple 已有 flow）則略過。
    pub fn nat_tcp_syn(&mut self, t: super::nat::OutboundTcp) {
        self.nat_tcp_syn_to(t, SocketAddr::from((t.dst_ip, t.dst_port)));
    }

    /// DNS pivot 的 TCP fallback（guest → `10.0.2.3:53` 的 SYN）：resolver 收到截斷（TC）
    /// 回應時會改走 TCP:53（glibc/node/go 都會；musl 不會）。guest 側照常對 10.0.2.3:53
    /// 完成 smoltcp 握手，host 側改連第一個 DNS 上游。
    pub fn nat_tcp_dns_syn(&mut self, t: super::nat::OutboundTcp) {
        let Some(up) = self.dns_upstreams().first().copied() else {
            return;
        };
        self.nat_tcp_syn_to(t, up);
    }

    /// [`Self::nat_tcp_syn`] 的實體：guest 可見的連線對象是 `t.dst`（smoltcp 在該位址
    /// listen），host 端實際連 `connect_to`（一般出網兩者相同；DNS pivot 時 dst=10.0.2.3
    /// 而 connect_to=真正的 DNS 上游）。
    fn nat_tcp_syn_to(&mut self, t: super::nat::OutboundTcp, connect_to: SocketAddr) {
        if self.tcp_nat.iter().any(|f| {
            f.guest_ip == t.src_ip
                && f.guest_port == t.src_port
                && f.dst_ip == t.dst_ip
                && f.dst_port == t.dst_port
        }) {
            return;
        }
        if !self.ensure_dst_addr(t.dst_ip) {
            if net_trace_enabled() {
                eprintln!("[whp-net-trace] nat-tcp: iface addr table full, dropping new dst");
            }
            return;
        }
        let dst_addr = Ipv4Address::new(t.dst_ip[0], t.dst_ip[1], t.dst_ip[2], t.dst_ip[3]);
        let mut sock = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; 65536]),
            tcp::SocketBuffer::new(vec![0u8; 65536]),
        );
        let ep = IpListenEndpoint {
            addr: Some(IpAddress::Ipv4(dst_addr)),
            port: t.dst_port,
        };
        if sock.listen(ep).is_err() {
            self.release_dst_addr(t.dst_ip);
            return;
        }
        let handle = self.sockets.add(sock);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(TcpStream::connect_timeout(
                &connect_to,
                NAT_TCP_CONNECT_TIMEOUT,
            ));
        });
        if net_trace_enabled() {
            let d = t.dst_ip;
            eprintln!(
                "[whp-net-trace] nat-tcp: new flow guest:{} -> {}.{}.{}.{}:{}",
                t.src_port, d[0], d[1], d[2], d[3], t.dst_port
            );
        }
        self.tcp_nat.push(TcpNatFlow {
            guest_ip: t.src_ip,
            guest_port: t.src_port,
            dst_ip: t.dst_ip,
            dst_port: t.dst_port,
            handle,
            host: TcpConnState::Connecting(rx),
        });
    }

    /// 確保 dst IP 在 iface ip_addrs（讓 smoltcp 接受 guest 對該 dst 的連線）；滿了則嘗試
    /// 驅逐一個無 active flow 的 /32 dst 再加。成功回 true。
    fn ensure_dst_addr(&mut self, dst_ip: [u8; 4]) -> bool {
        let x = Ipv4Address::new(dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3]);
        let cidr = IpCidr::new(IpAddress::Ipv4(x), 32);
        if self.iface.ip_addrs().contains(&cidr) {
            return true;
        }
        let mut ok = false;
        self.iface
            .update_ip_addrs(|addrs| ok = addrs.push(cidr).is_ok());
        if ok {
            return true;
        }
        // 滿了：找一個目前無 flow 使用的 /32 dst 驅逐（約定 DNS 位址是常駐的，永不驅逐）。
        let evict = self.iface.ip_addrs().iter().copied().find(|c| {
            c.prefix_len() == 32
                && match c.address() {
                    IpAddress::Ipv4(a) => {
                        a.octets() != NAT_DNS_IP
                            && !self.tcp_nat.iter().any(|f| f.dst_ip == a.octets())
                    }
                    #[allow(unreachable_patterns)]
                    _ => false,
                }
        });
        if let Some(e) = evict {
            self.iface.update_ip_addrs(|addrs| {
                addrs.retain(|c| *c != e);
                let _ = addrs.push(cidr);
            });
            return true;
        }
        false
    }

    /// flow 關閉後釋放其 dst IP（若無其他 flow 再用該 dst）。須在 flow 已自 tcp_nat 移除後呼叫。
    /// 約定 DNS 位址常駐（ARP 應答依賴它），即使是 DNS TCP fallback 的 flow 關閉也不釋放。
    fn release_dst_addr(&mut self, dst_ip: [u8; 4]) {
        if dst_ip == NAT_DNS_IP || self.tcp_nat.iter().any(|f| f.dst_ip == dst_ip) {
            return;
        }
        let x = Ipv4Address::new(dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3]);
        let cidr = IpCidr::new(IpAddress::Ipv4(x), 32);
        self.iface
            .update_ip_addrs(|addrs| addrs.retain(|c| *c != cidr));
    }

    /// 驅動出網 TCP flow：完成 host connect、雙向橋接 smoltcp socket ↔ host TcpStream、
    /// 清理已關閉的 flow。
    fn tcp_nat_pump(&mut self) {
        let mut closed: Vec<usize> = Vec::new();
        for (i, flow) in self.tcp_nat.iter_mut().enumerate() {
            // 1. 完成 host connect。
            if let TcpConnState::Connecting(rx) = &flow.host {
                match rx.try_recv() {
                    Ok(Ok(stream)) => {
                        let _ = stream.set_nonblocking(true);
                        flow.host = TcpConnState::Up(stream);
                    }
                    Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                        self.sockets.get_mut::<tcp::Socket>(flow.handle).abort();
                        closed.push(i);
                        continue;
                    }
                    Err(mpsc::TryRecvError::Empty) => continue, // 仍在連，下一輪再看
                }
            }
            // 2. Established：橋接 bytes。
            let sock = self.sockets.get_mut::<tcp::Socket>(flow.handle);
            if let TcpConnState::Up(host) = &mut flow.host {
                if sock.can_recv() {
                    let _ = sock.recv(|data| {
                        let n = host.write(data).unwrap_or(0);
                        (n, ())
                    });
                }
                if sock.can_send() {
                    let mut buf = [0u8; 4096];
                    match host.read(&mut buf) {
                        Ok(0) => sock.close(),
                        Ok(n) => {
                            let _ = sock.send_slice(&buf[..n]);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => sock.close(),
                    }
                }
            }
            if sock.state() == tcp::State::Closed {
                closed.push(i);
            }
        }
        for i in closed.into_iter().rev() {
            let flow = self.tcp_nat.remove(i);
            self.sockets.remove(flow.handle);
            self.release_dst_addr(flow.dst_ip);
        }
    }

    /// 處理一個 guest 出網 UDP 封包（接線層 drain_tx 分流後呼叫）：找/建對應 flow 的
    /// host UDP socket，`send_to(dst)`。
    pub fn nat_outbound(&mut self, udp: super::nat::OutboundUdp) {
        let idx = match self
            .nat_flows
            .iter()
            .position(|f| !f.dns && f.guest_ip == udp.src_ip && f.guest_port == udp.src_port)
        {
            Some(i) => i,
            None => {
                let Some(i) = self.new_nat_flow(&udp, false) else {
                    return;
                };
                i
            }
        };
        let dst = SocketAddr::from((udp.dst_ip, udp.dst_port));
        let _ = self.nat_flows[idx].sock.send_to(&udp.payload, dst);
    }

    /// 處理 guest 送往「約定 DNS」`10.0.2.3:53` 的查詢（接線層 drain_tx 分流後呼叫）：
    /// 把 payload 原樣 fanout 到 host 設定的所有 DNS 上游（首個回應者勝出——resolver 只取
    /// 第一個答案，多餘回應無害）。回程由 [`Self::nat_poll`] masquerade 成 `10.0.2.3:53`。
    pub fn nat_outbound_dns(&mut self, udp: super::nat::OutboundUdp) {
        let upstreams = self.dns_upstreams();
        let idx = match self
            .nat_flows
            .iter()
            .position(|f| f.dns && f.guest_ip == udp.src_ip && f.guest_port == udp.src_port)
        {
            Some(i) => i,
            None => {
                let Some(i) = self.new_nat_flow(&udp, true) else {
                    return;
                };
                i
            }
        };
        for up in &upstreams {
            let _ = self.nat_flows[idx].sock.send_to(&udp.payload, up);
        }
        // 上游快取可能已刷新（VPN 切換）；flow 的回程過濾清單跟著更新。
        self.nat_flows[idx].upstreams = upstreams;
    }

    /// 建一條出網 UDP NAT flow（bind host socket）；bind 失敗回 None（該封包丟棄）。
    fn new_nat_flow(&mut self, udp: &super::nat::OutboundUdp, dns: bool) -> Option<usize> {
        let sock = match UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)) {
            Ok(s) => s,
            Err(e) => {
                if net_trace_enabled() {
                    eprintln!("[whp-net-trace] nat: host udp bind failed: {e}");
                }
                return None;
            }
        };
        let _ = sock.set_nonblocking(true);
        if net_trace_enabled() {
            let s = udp.src_ip;
            let kind = if dns { "dns pivot" } else { "outbound" };
            eprintln!(
                "[whp-net-trace] nat: new {kind} flow {}.{}.{}.{}:{}",
                s[0], s[1], s[2], s[3], udp.src_port
            );
        }
        self.nat_flows.push(NatFlow {
            guest_ip: udp.src_ip,
            guest_port: udp.src_port,
            guest_mac: udp.src_mac,
            sock,
            dns,
            upstreams: Vec::new(),
        });
        Some(self.nat_flows.len() - 1)
    }

    /// DNS 上游（lazy 探測 + TTL 快取；見 [`discover_dns_upstreams`]）。
    fn dns_upstreams(&mut self) -> Vec<SocketAddr> {
        let stale = self
            .dns_upstreams_at
            .is_none_or(|t| t.elapsed() >= DNS_UPSTREAM_TTL);
        if stale {
            self.dns_upstreams = discover_dns_upstreams();
            self.dns_upstreams_at = Some(std::time::Instant::now());
            if net_trace_enabled() {
                eprintln!("[whp-net-trace] dns: upstreams = {:?}", self.dns_upstreams);
            }
        }
        self.dns_upstreams.clone()
    }

    /// 測試用：直接設定 DNS 上游（並標記為新鮮，跳過探測）。
    #[cfg(test)]
    fn set_dns_upstreams_for_test(&mut self, ups: Vec<SocketAddr>) {
        self.dns_upstreams = ups;
        self.dns_upstreams_at = Some(std::time::Instant::now());
    }

    /// 把各 NAT flow 的 host socket 收到的外部回應合成 外部→guest 的 frame 注入 guest rx。
    /// DNS pivot flow：只收 fanout 過的上游來源（其餘丟棄），src masquerade 成
    /// `NAT_DNS_IP:53`（guest resolver 的 connected socket 才收得下）。
    fn nat_poll(&mut self, phy: &mut VirtioNetPhy) {
        let mut buf = [0u8; 65535];
        for f in &self.nat_flows {
            loop {
                match f.sock.recv_from(&mut buf) {
                    Ok((n, SocketAddr::V4(from))) => {
                        let (ext_ip, ext_port) = if f.dns {
                            if !f.upstreams.contains(&SocketAddr::V4(from)) {
                                continue; // 非我們查詢的上游，丟棄
                            }
                            (NAT_DNS_IP, 53)
                        } else {
                            (from.ip().octets(), from.port())
                        };
                        let frame = super::nat::build_udp_reply(
                            self.gateway_mac,
                            f.guest_mac,
                            ext_ip,
                            ext_port,
                            f.guest_ip,
                            f.guest_port,
                            &buf[..n],
                        );
                        phy.push_to_guest(frame);
                    }
                    Ok((_, SocketAddr::V6(_))) => {} // 只合成 v4 回程
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
    }

    /// host UDP listener 收到的 datagram → 經 per-client smoltcp UDP socket 送到 guest。
    fn udp_host_to_guest(&mut self) {
        let mut inbound: Vec<(usize, SocketAddr, u16, Vec<u8>)> = Vec::new();
        for (idx, (listener, gp)) in self.udp_forwards.iter().enumerate() {
            let mut buf = [0u8; 65535];
            loop {
                match listener.recv_from(&mut buf) {
                    Ok((n, client)) => inbound.push((idx, client, *gp, buf[..n].to_vec())),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
        for (idx, client, gp, data) in inbound {
            let handle = match self
                .udp_sessions
                .iter()
                .find(|s| s.forward_idx == idx && s.client == client)
            {
                Some(s) => s.handle,
                None => {
                    let local = self.alloc_local_port();
                    let mut sock = udp::Socket::new(
                        udp::PacketBuffer::new(
                            vec![udp::PacketMetadata::EMPTY; 16],
                            vec![0u8; 65536],
                        ),
                        udp::PacketBuffer::new(
                            vec![udp::PacketMetadata::EMPTY; 16],
                            vec![0u8; 65536],
                        ),
                    );
                    if sock.bind(local).is_err() {
                        continue;
                    }
                    let handle = self.sockets.add(sock);
                    if net_trace_enabled() {
                        eprintln!(
                            "[whp-net-trace] new UDP session client={client} -> {}:{gp} local={local}",
                            self.guest_ip
                        );
                    }
                    self.udp_sessions.push(UdpSession {
                        handle,
                        client,
                        forward_idx: idx,
                    });
                    handle
                }
            };
            let sock = self.sockets.get_mut::<udp::Socket>(handle);
            let _ = sock.send_slice(&data, (IpAddress::Ipv4(self.guest_ip), gp));
        }
    }

    /// per-client smoltcp UDP socket 收到的 guest 回應 → send_to 原 host client。
    fn udp_guest_to_host(&mut self) {
        let mut outbound: Vec<(usize, SocketAddr, Vec<u8>)> = Vec::new();
        for s in &self.udp_sessions {
            let sock = self.sockets.get_mut::<udp::Socket>(s.handle);
            while sock.can_recv() {
                match sock.recv() {
                    Ok((data, _meta)) => outbound.push((s.forward_idx, s.client, data.to_vec())),
                    Err(_) => break,
                }
            }
        }
        for (idx, client, data) in outbound {
            if let Some((listener, _)) = self.udp_forwards.get(idx) {
                let _ = listener.send_to(&data, client);
            }
        }
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
            if net_trace_enabled() {
                eprintln!(
                    "[whp-net-trace] host conn accepted -> smoltcp connect {}:{guest_port} local={local} ok={connected}",
                    self.guest_ip
                );
            }
            if connected {
                let handle = self.sockets.add(sock);
                self.conns.push(Conn {
                    host: stream,
                    handle,
                    last_state: tcp::State::Closed,
                });
            }
        }
    }

    fn pump(&mut self) {
        let mut closed = Vec::new();
        for (i, conn) in self.conns.iter_mut().enumerate() {
            let sock = self.sockets.get_mut::<tcp::Socket>(conn.handle);
            let state = sock.state();
            if state != conn.last_state {
                if net_trace_enabled() {
                    eprintln!(
                        "[whp-net-trace] smoltcp socket {state:?} (was {:?})",
                        conn.last_state
                    );
                }
                conn.last_state = state;
            }
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

/// 探測 DNS 上游，依序：`CHEFER_WHP_DNS`（逗號分隔 IPv4；測試/受限網路的明確覆寫）→
/// Windows host 目前設定的 DNS（`GetNetworkParams`；跟著 VPN/公司網路走，等價 Docker
/// 讀 host resolv.conf）→ 都沒有才 fallback 公共 DNS（1.1.1.1/8.8.8.8，Docker 同款
/// 行為）。fallback 不與 host DNS 並用：host DNS 存在時查詢不外流到第三方。
fn discover_dns_upstreams() -> Vec<SocketAddr> {
    if let Ok(v) = std::env::var("CHEFER_WHP_DNS") {
        let list = parse_dns_override(&v);
        if !list.is_empty() {
            return list;
        }
        // 設了但解析不出任何位址 → 視同未設定，繼續探測。
    }
    let mut list = host_dns_servers();
    // 保序去重 + 上限（GetNetworkParams 可能列多網卡重複項；fanout 不宜太寬）。
    let mut seen = Vec::new();
    list.retain(|a| {
        let dup = seen.contains(a);
        seen.push(*a);
        !dup
    });
    list.truncate(3);
    if list.is_empty() {
        list = vec![
            SocketAddr::from(([1, 1, 1, 1], 53)),
            SocketAddr::from(([8, 8, 8, 8], 53)),
        ];
    }
    list
}

/// 解析 `CHEFER_WHP_DNS`：逗號分隔的 IPv4（一律用 :53；無效項忽略）。
fn parse_dns_override(v: &str) -> Vec<SocketAddr> {
    v.split(',')
        .filter_map(|s| s.trim().parse::<std::net::Ipv4Addr>().ok())
        .filter(|ip| !ip.is_unspecified())
        .map(|ip| SocketAddr::from((ip, 53)))
        .collect()
}

/// Windows host 目前設定的 DNS server（IPv4）：IP Helper `GetNetworkParams` 的
/// `DnsServerList`（系統層級、跨網卡的解析順序，正是「host 自己解名字會問誰」）。
/// loopback 上游（本機跑 AdGuard 之類）也保留——helper 的 socket 在 host 端，連得到。
#[cfg(windows)]
fn host_dns_servers() -> Vec<SocketAddr> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{FIXED_INFO_W2KSP1, GetNetworkParams};
    unsafe {
        let mut len: u32 = 0;
        // 第一次呼叫拿所需 buffer 大小（預期回 ERROR_BUFFER_OVERFLOW）。
        let _ = GetNetworkParams(std::ptr::null_mut(), &mut len);
        if len == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u8; len as usize];
        if GetNetworkParams(buf.as_mut_ptr().cast::<FIXED_INFO_W2KSP1>(), &mut len) != 0 {
            return Vec::new();
        }
        let info = &*buf.as_ptr().cast::<FIXED_INFO_W2KSP1>();
        let mut out = Vec::new();
        let mut node = std::ptr::addr_of!(info.DnsServerList);
        while !node.is_null() {
            // IP_ADDRESS_STRING 是以 NUL 結尾的點分十進位 ANSI 字串。
            let raw =
                std::slice::from_raw_parts((*node).IpAddress.String.as_ptr().cast::<u8>(), 16);
            let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
            if let Ok(txt) = std::str::from_utf8(&raw[..end])
                && let Ok(ip) = txt.parse::<std::net::Ipv4Addr>()
                && !ip.is_unspecified()
            {
                out.push(SocketAddr::from((ip, 53)));
            }
            node = (*node).Next;
        }
        out
    }
}

/// 非 Windows（本 crate 的跨平台單元測試環境）：無 host DNS 探測，交給上層 fallback。
#[cfg(not(windows))]
fn host_dns_servers() -> Vec<SocketAddr> {
    Vec::new()
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
        let listen_port = backend.add_forward(0, 6379).unwrap();

        // host 端連線（IPv6 loopback，立即進 listener backlog）。
        let _client =
            std::net::TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, listen_port)).unwrap();
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

    #[test]
    fn udp_forward_initiates_datagram_to_guest() {
        // host client 對 UDP 轉發埠送一個 datagram 後，NetBackend 應對 guest 發起送出
        // （先 ARP 找 guest MAC）——證明 host→guest UDP 轉發的發起端可行（回程需 guest 回應，
        // 留實機驗證）。
        let mut phy = VirtioNetPhy::new();
        let gw_ip = Ipv4Address::new(10, 0, 2, 2);
        let guest_ip = Ipv4Address::new(10, 0, 2, 15);
        let mut backend = NetBackend::new(
            [0x52, 0x54, 0x00, 0x00, 0x00, 0x02],
            gw_ip,
            guest_ip,
            &mut phy,
        );
        let listen_port = backend.add_udp_forward(0, 53).unwrap();

        // host client 送一個 datagram 到轉發埠（IPv6 loopback）。
        let client = UdpSocket::bind((std::net::Ipv6Addr::LOCALHOST, 0)).unwrap();
        client
            .send_to(b"hello", (std::net::Ipv6Addr::LOCALHOST, listen_port))
            .unwrap();
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
        assert!(found, "NetBackend 應對 guest 發 ARP（UDP 埠轉發發起）");
    }

    const GW_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x00, 0x00, 0x02];
    const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const GUEST_IP: [u8; 4] = [10, 0, 2, 15];

    fn test_backend(phy: &mut VirtioNetPhy) -> NetBackend {
        NetBackend::new(
            GW_MAC,
            Ipv4Address::new(10, 0, 2, 2),
            Ipv4Address::new(10, 0, 2, 15),
            phy,
        )
    }

    /// 組一個 guest → 10.0.2.3:53 的查詢並解析成 OutboundUdp（重用 nat 的合成器）。
    fn dns_query_from_guest(src_port: u16, payload: &[u8]) -> super::super::nat::OutboundUdp {
        let frame = super::super::nat::build_udp_reply(
            GUEST_MAC, GW_MAC, GUEST_IP, src_port, NAT_DNS_IP, 53, payload,
        );
        super::super::nat::parse_udp(&frame).unwrap()
    }

    #[test]
    fn smoltcp_answers_arp_for_nat_dns_ip() {
        // guest（或 pasta）送查詢前會對 on-link 的 10.0.2.3 ARP；backend 的 iface 掛了
        // /32，應回 ARP reply——否則查詢連 L2 都出不去。
        let mut phy = VirtioNetPhy::new();
        let mut backend = test_backend(&mut phy);

        let mut arp = Vec::new();
        arp.extend_from_slice(&[0xff; 6]); // broadcast
        arp.extend_from_slice(&GUEST_MAC);
        arp.extend_from_slice(&[0x08, 0x06]);
        arp.extend_from_slice(&[0x00, 0x01, 0x08, 0x00, 0x06, 0x04, 0x00, 0x01]);
        arp.extend_from_slice(&GUEST_MAC);
        arp.extend_from_slice(&GUEST_IP);
        arp.extend_from_slice(&[0u8; 6]);
        arp.extend_from_slice(&NAT_DNS_IP); // who-has 10.0.2.3
        phy.push_from_guest(arp);

        backend.poll(&mut phy, Instant::from_millis(0));
        let reply = phy
            .pop_to_guest()
            .expect("應回 ARP reply（10.0.2.3 由 iface 持有）");
        assert_eq!(&reply[12..14], &[0x08, 0x06]);
        assert_eq!(&reply[20..22], &[0x00, 0x02], "opcode 應為 reply");
        assert_eq!(&reply[28..32], &NAT_DNS_IP, "sender IP 應為 10.0.2.3");
    }

    #[test]
    fn dns_pivot_forwards_to_upstream_and_masquerades_reply() {
        // 完整 UDP pivot 鏈（host 端）：guest 查 10.0.2.3:53 → 上游收到原 payload →
        // 上游回應 → 合成回 guest 的 frame src 必須是 10.0.2.3:53（masquerade）。
        let mut phy = VirtioNetPhy::new();
        let mut backend = test_backend(&mut phy);

        let upstream = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        upstream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        backend.set_dns_upstreams_for_test(vec![upstream.local_addr().unwrap()]);

        backend.nat_outbound_dns(dns_query_from_guest(40000, b"query-bytes"));

        let mut buf = [0u8; 512];
        let (n, flow_addr) = upstream.recv_from(&mut buf).expect("上游應收到查詢");
        assert_eq!(&buf[..n], b"query-bytes");
        upstream.send_to(b"answer-bytes", flow_addr).unwrap();

        // 回程是真 OS socket，非同步；輪詢至 frame 出現。
        let mut reply = None;
        for ms in 0..200 {
            backend.poll(&mut phy, Instant::from_millis(ms));
            if let Some(f) = phy.pop_to_guest() {
                reply = Some(f);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let p = super::super::nat::parse_udp(&reply.expect("應合成回 guest 的 DNS 回應"))
            .expect("回程應為 IPv4/UDP");
        assert_eq!(p.src_ip, NAT_DNS_IP, "src 應 masquerade 成 10.0.2.3");
        assert_eq!(p.src_port, 53);
        assert_eq!(p.dst_ip, GUEST_IP);
        assert_eq!(p.dst_port, 40000);
        assert_eq!(p.payload, b"answer-bytes");
    }

    #[test]
    fn dns_pivot_drops_reply_from_non_upstream() {
        // 只有 fanout 過的上游能回；其他來源（spoof）一律丟棄。
        let mut phy = VirtioNetPhy::new();
        let mut backend = test_backend(&mut phy);

        let upstream = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        upstream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        backend.set_dns_upstreams_for_test(vec![upstream.local_addr().unwrap()]);

        backend.nat_outbound_dns(dns_query_from_guest(40001, b"q"));
        let mut buf = [0u8; 512];
        let (_, flow_addr) = upstream.recv_from(&mut buf).expect("上游應收到查詢");

        // 惡意來源先送，正牌上游後送——只有正牌 payload 能到 guest。
        let spoofer = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        spoofer.send_to(b"spoofed", flow_addr).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        upstream.send_to(b"legit", flow_addr).unwrap();

        let mut frames = Vec::new();
        for ms in 0..200 {
            backend.poll(&mut phy, Instant::from_millis(ms));
            while let Some(f) = phy.pop_to_guest() {
                frames.push(f);
            }
            if !frames.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(frames.len(), 1, "spoof 應被丟棄、只留正牌回應");
        let p = super::super::nat::parse_udp(&frames[0]).unwrap();
        assert_eq!(p.payload, b"legit");
        assert_eq!(p.src_ip, NAT_DNS_IP);
    }

    #[test]
    fn dns_flow_keyed_separately_from_normal_flow() {
        // 同一 guest src port 同時查外部 DNS（一般 NAT）與 10.0.2.3（pivot）：
        // 兩者不能共用 flow，否則 masquerade 會錯掛到一般回程上。
        let mut phy = VirtioNetPhy::new();
        let mut backend = test_backend(&mut phy);
        backend.set_dns_upstreams_for_test(vec![SocketAddr::from(([127, 0, 0, 1], 9))]);

        // 一般出網（dst 用 loopback discard 埠，不真的期待回應）。
        let ext_frame = super::super::nat::build_udp_reply(
            GUEST_MAC,
            GW_MAC,
            GUEST_IP,
            40002,
            [127, 0, 0, 1],
            9,
            b"x",
        );
        backend.nat_outbound(super::super::nat::parse_udp(&ext_frame).unwrap());
        backend.nat_outbound_dns(dns_query_from_guest(40002, b"y"));

        assert_eq!(backend.nat_flows.len(), 2, "一般與 DNS pivot 應各一條 flow");
        assert!(backend.nat_flows.iter().any(|f| !f.dns));
        assert!(backend.nat_flows.iter().any(|f| f.dns));
    }

    #[test]
    fn nat_dns_addr_is_never_evicted_or_released() {
        // TCP NAT 動態 dst 塞滿 iface 位址表時，常駐的 10.0.2.3 不可被驅逐；
        // release_dst_addr(10.0.2.3) 也必須是 no-op（ARP 應答依賴它）。
        let mut phy = VirtioNetPhy::new();
        let mut backend = test_backend(&mut phy);
        let dns_cidr = IpCidr::new(IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 3)), 32);
        assert!(backend.iface.ip_addrs().contains(&dns_cidr));

        // 塞爆位址表（SMOLTCP_IFACE_MAX_ADDR_COUNT=64），迫使驅逐路徑運轉。
        for i in 0..100u32 {
            let b = i.to_be_bytes();
            let _ = backend.ensure_dst_addr([1, b[1], b[2], b[3].max(1)]);
        }
        assert!(
            backend.iface.ip_addrs().contains(&dns_cidr),
            "10.0.2.3 不可被驅逐"
        );

        backend.release_dst_addr(NAT_DNS_IP);
        assert!(
            backend.iface.ip_addrs().contains(&dns_cidr),
            "release 對 10.0.2.3 應為 no-op"
        );
    }

    #[test]
    fn dns_override_parsing() {
        assert_eq!(
            parse_dns_override("1.1.1.1, 8.8.8.8"),
            vec![
                SocketAddr::from(([1, 1, 1, 1], 53)),
                SocketAddr::from(([8, 8, 8, 8], 53)),
            ]
        );
        // 無效項忽略；0.0.0.0 排除；全空 → 空清單（呼叫端視同未設定）。
        assert_eq!(
            parse_dns_override("junk, 0.0.0.0, 9.9.9.9"),
            vec![SocketAddr::from(([9, 9, 9, 9], 53))]
        );
        assert!(parse_dns_override("").is_empty());
    }
}
