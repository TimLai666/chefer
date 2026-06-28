//! WHP 出網 NAT（outbound）純邏輯：解析 guest 送往**外部**的 IPv4/UDP frame，
//! 合成外部→guest 的回程 frame。跨平台、無 I/O——host 端 UDP socket 與 NAT 表的接線
//! 屬 cfg(windows) 的 net_backend 層（之後接上）。
//!
//! 為何 UDP 走「手工解析/合成」而非 smoltcp：smoltcp 是 endpoint stack，不做封包轉發/
//! NAT；要它處理 guest→任意外部 dst 需動態把每個 dst IP 加進 interface，繁瑣。UDP 無
//! 連線狀態（無 seq/ack），且 IPv4 下 UDP checksum 可為 0，手工合成簡單可靠。TCP（有狀態）
//! 才需另以 smoltcp 動態 dst 處理（後續階段）。
//!
//! 路由前提：guest 對 off-subnet 的外部 IP 會把封包送到 **gateway MAC**（下一跳），dst IP
//! 仍是外部位址。故 outbound frame 的 dst MAC = gateway MAC、dst IP = 外部；回程則 src MAC
//! = gateway MAC、src IP = 外部、dst = guest。

const ETH_HDR: usize = 14;
const ETHERTYPE_IPV4: u16 = 0x0800;
const IPPROTO_UDP: u8 = 17;
const IPPROTO_TCP: u8 = 6;

/// 一個 guest 送出的 outbound UDP 封包（已從 ethernet/IPv4/UDP header 解析出五元組 + payload）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundUdp {
    pub src_mac: [u8; 6],
    pub src_ip: [u8; 4],
    pub src_port: u16,
    pub dst_ip: [u8; 4],
    pub dst_port: u16,
    pub payload: Vec<u8>,
}

/// 解析一個 guest ethernet frame；若為 IPv4/UDP 回傳 [`OutboundUdp`]，否則 None
/// （非 IPv4/UDP、表頭不全、或長度不符）。**不**判斷 dst 是否為「外部」——由呼叫端
/// 依路由規則（排除 gateway / 同網段 / broadcast / multicast）決定是否 NAT。
pub fn parse_udp(frame: &[u8]) -> Option<OutboundUdp> {
    if frame.len() < ETH_HDR + 20 + 8 {
        return None;
    }
    if u16::from_be_bytes([frame[12], frame[13]]) != ETHERTYPE_IPV4 {
        return None;
    }
    let src_mac = [frame[6], frame[7], frame[8], frame[9], frame[10], frame[11]];
    let ip = &frame[ETH_HDR..];
    let version = ip[0] >> 4;
    let ihl = (ip[0] & 0x0f) as usize * 4;
    if version != 4 || ihl < 20 || ip.len() < ihl + 8 {
        return None;
    }
    if ip[9] != IPPROTO_UDP {
        return None;
    }
    let total_len = u16::from_be_bytes([ip[2], ip[3]]) as usize;
    let src_ip = [ip[12], ip[13], ip[14], ip[15]];
    let dst_ip = [ip[16], ip[17], ip[18], ip[19]];
    let udp = &ip[ihl..];
    let src_port = u16::from_be_bytes([udp[0], udp[1]]);
    let dst_port = u16::from_be_bytes([udp[2], udp[3]]);
    let udp_len = u16::from_be_bytes([udp[4], udp[5]]) as usize;
    // payload 長度以 UDP length 為準，但夾在可用 bytes 內（容忍尾端填充/截斷）。
    // 同時用 IP total_len 收斂（避免吃到 ethernet padding）。
    let avail = udp.len();
    let by_udp = udp_len.clamp(8, avail);
    let by_ip = total_len.saturating_sub(ihl).clamp(8, avail);
    let end = by_udp.min(by_ip);
    let payload = udp[8..end].to_vec();
    Some(OutboundUdp {
        src_mac,
        src_ip,
        src_port,
        dst_ip,
        dst_port,
        payload,
    })
}

/// 合成一個外部→guest 的回程 ethernet+IPv4+UDP frame。
///
/// - `gateway_mac` → `guest_mac`（回程的 L2）。
/// - IPv4：src = `ext_ip`（外部回應方）、dst = `guest_ip`；ttl=64、IHL=5、計算 header checksum。
/// - UDP：src = `ext_port`、dst = `guest_port`、**checksum = 0**（IPv4 下合法，省去 pseudo-header）。
#[allow(clippy::too_many_arguments)]
pub fn build_udp_reply(
    gateway_mac: [u8; 6],
    guest_mac: [u8; 6],
    ext_ip: [u8; 4],
    ext_port: u16,
    guest_ip: [u8; 4],
    guest_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let udp_len = 8 + payload.len();
    let ip_total = 20 + udp_len;
    let mut frame = Vec::with_capacity(ETH_HDR + ip_total);

    // ── Ethernet ──
    frame.extend_from_slice(&guest_mac); // dst
    frame.extend_from_slice(&gateway_mac); // src
    frame.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());

    // ── IPv4 (20 bytes, no options) ──
    let ip_start = frame.len();
    frame.push(0x45); // version 4, IHL 5
    frame.push(0x00); // DSCP/ECN
    frame.extend_from_slice(&(ip_total as u16).to_be_bytes());
    frame.extend_from_slice(&0u16.to_be_bytes()); // identification
    frame.extend_from_slice(&0x4000u16.to_be_bytes()); // flags=DF, offset=0
    frame.push(64); // TTL
    frame.push(IPPROTO_UDP);
    frame.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    frame.extend_from_slice(&ext_ip); // src
    frame.extend_from_slice(&guest_ip); // dst
    let checksum = ipv4_checksum(&frame[ip_start..ip_start + 20]);
    frame[ip_start + 10..ip_start + 12].copy_from_slice(&checksum.to_be_bytes());

    // ── UDP ──
    frame.extend_from_slice(&ext_port.to_be_bytes()); // src
    frame.extend_from_slice(&guest_port.to_be_bytes()); // dst
    frame.extend_from_slice(&(udp_len as u16).to_be_bytes());
    frame.extend_from_slice(&0u16.to_be_bytes()); // checksum = 0（IPv4 合法）
    frame.extend_from_slice(payload);

    frame
}

/// 一個 guest 送出的 outbound TCP 封包的關鍵欄位（4-tuple + 旗標）。出網 TCP 由 smoltcp
/// 動態 dst 處理（非手工合成）；本結構供接線層分流／管理 NAT flow 與 iface 位址用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboundTcp {
    pub src_mac: [u8; 6],
    pub src_ip: [u8; 4],
    pub src_port: u16,
    pub dst_ip: [u8; 4],
    pub dst_port: u16,
    pub syn: bool,
    pub ack: bool,
    pub fin: bool,
    pub rst: bool,
}

/// 解析一個 guest ethernet frame；若為 IPv4/TCP 回傳 [`OutboundTcp`]（4-tuple + 旗標），
/// 否則 None。**不**判斷 dst 是否「外部」——由呼叫端依路由規則決定是否 NAT。
/// 只解析到 TCP 固定 20-byte header 的旗標與埠，不碰 options/payload。
pub fn parse_tcp(frame: &[u8]) -> Option<OutboundTcp> {
    if frame.len() < ETH_HDR + 20 + 20 {
        return None;
    }
    if u16::from_be_bytes([frame[12], frame[13]]) != ETHERTYPE_IPV4 {
        return None;
    }
    let src_mac = [frame[6], frame[7], frame[8], frame[9], frame[10], frame[11]];
    let ip = &frame[ETH_HDR..];
    let version = ip[0] >> 4;
    let ihl = (ip[0] & 0x0f) as usize * 4;
    if version != 4 || ihl < 20 || ip.len() < ihl + 20 {
        return None;
    }
    if ip[9] != IPPROTO_TCP {
        return None;
    }
    let src_ip = [ip[12], ip[13], ip[14], ip[15]];
    let dst_ip = [ip[16], ip[17], ip[18], ip[19]];
    let tcp = &ip[ihl..];
    let src_port = u16::from_be_bytes([tcp[0], tcp[1]]);
    let dst_port = u16::from_be_bytes([tcp[2], tcp[3]]);
    let flags = tcp[13]; // data-offset(4)+rsvd 在 [12]，flags 在 [13]
    Some(OutboundTcp {
        src_mac,
        src_ip,
        src_port,
        dst_ip,
        dst_port,
        fin: flags & 0x01 != 0,
        syn: flags & 0x02 != 0,
        rst: flags & 0x04 != 0,
        ack: flags & 0x10 != 0,
    })
}

/// IPv4 header checksum（RFC 1071 的 one's complement 和；輸入須為 20-byte header，
/// checksum 欄位已置 0）。
fn ipv4_checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < header.len() {
        sum += u16::from_be_bytes([header[i], header[i + 1]]) as u32;
        i += 2;
    }
    if i < header.len() {
        sum += (header[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GW_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x00, 0x00, 0x02];
    const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
    const EXT_IP: [u8; 4] = [1, 1, 1, 1];

    /// 用 build_udp_reply 造一個「外部→guest」frame，再用 parse_udp 解回——欄位需一致。
    /// （parse 對方向不敏感，可雙用：驗證合成 frame 的結構正確。）
    #[test]
    fn build_then_parse_roundtrip() {
        let payload = b"\x12\x34\x81\x80 dns-ish payload";
        let frame = build_udp_reply(GW_MAC, GUEST_MAC, EXT_IP, 53, GUEST_IP, 40000, payload);
        let p = parse_udp(&frame).expect("synth frame should parse as IPv4/UDP");
        assert_eq!(p.src_ip, EXT_IP);
        assert_eq!(p.src_port, 53);
        assert_eq!(p.dst_ip, GUEST_IP);
        assert_eq!(p.dst_port, 40000);
        assert_eq!(p.payload, payload);
        assert_eq!(p.src_mac, GW_MAC); // 回程 L2 src = gateway
    }

    #[test]
    fn build_sets_valid_ipv4_checksum() {
        let frame = build_udp_reply(GW_MAC, GUEST_MAC, EXT_IP, 53, GUEST_IP, 40000, b"x");
        // 對含 checksum 的 header 重算 one's complement 和應為 0（合法）。
        let ip = &frame[ETH_HDR..ETH_HDR + 20];
        let mut sum: u32 = 0;
        for c in ip.chunks(2) {
            sum += u16::from_be_bytes([c[0], c[1]]) as u32;
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        assert_eq!(sum as u16, 0xffff, "IPv4 header checksum 應自洽");
        // ethertype、ttl、proto sanity
        assert_eq!(&frame[12..14], &[0x08, 0x00]);
        assert_eq!(frame[ETH_HDR + 8], 64); // TTL
        assert_eq!(frame[ETH_HDR + 9], IPPROTO_UDP);
    }

    #[test]
    fn parse_real_outbound_udp_frame() {
        // 組一個 guest→外部 的 DNS query frame（guest 10.0.2.15:40000 → 1.1.1.1:53）。
        let payload = b"hello-dns";
        // 直接重用 build_udp_reply 的合成器造一個 L3 等價 frame，但方向相反（src=guest）。
        let frame = build_udp_reply(GUEST_MAC, GW_MAC, GUEST_IP, 40000, EXT_IP, 53, payload);
        let p = parse_udp(&frame).unwrap();
        assert_eq!(p.src_ip, GUEST_IP);
        assert_eq!(p.src_port, 40000);
        assert_eq!(p.dst_ip, EXT_IP);
        assert_eq!(p.dst_port, 53);
        assert_eq!(p.payload, payload);
        assert_eq!(p.src_mac, GUEST_MAC);
    }

    #[test]
    fn parse_rejects_non_udp_and_short() {
        // 太短
        assert!(parse_udp(&[0u8; 20]).is_none());
        // ARP（ethertype 0x0806）
        let mut arp = vec![0u8; 60];
        arp[12] = 0x08;
        arp[13] = 0x06;
        assert!(parse_udp(&arp).is_none());
        // IPv4 但 proto=TCP(6)
        let mut tcp = build_udp_reply(GW_MAC, GUEST_MAC, EXT_IP, 1, GUEST_IP, 2, b"abcd");
        tcp[ETH_HDR + 9] = 6; // proto → TCP
        assert!(parse_udp(&tcp).is_none());
    }

    #[test]
    fn parse_clamps_payload_to_udp_length_ignoring_padding() {
        // 合成一個 payload=3 bytes 的 frame，再人工在尾端補 ethernet padding，
        // parse 應只取 UDP length 內的 3 bytes（不含 padding）。
        let mut frame = build_udp_reply(GUEST_MAC, GW_MAC, GUEST_IP, 5, EXT_IP, 6, b"abc");
        frame.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]); // padding
        let p = parse_udp(&frame).unwrap();
        assert_eq!(p.payload, b"abc");
    }

    /// 測試用：合成一個 guest→外部 的 ethernet+IPv4+TCP frame（20-byte TCP header，無 options）。
    fn build_tcp_frame(
        src_ip: [u8; 4],
        src_port: u16,
        dst_ip: [u8; 4],
        dst_port: u16,
        flags: u8,
    ) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&GW_MAC); // dst mac（任意）
        f.extend_from_slice(&GUEST_MAC); // src mac
        f.extend_from_slice(&[0x08, 0x00]); // ethertype IPv4
        // IPv4 header (20)
        f.push(0x45);
        f.push(0x00);
        f.extend_from_slice(&40u16.to_be_bytes()); // total len 20+20
        f.extend_from_slice(&0u16.to_be_bytes());
        f.extend_from_slice(&0x4000u16.to_be_bytes());
        f.push(64);
        f.push(IPPROTO_TCP);
        f.extend_from_slice(&0u16.to_be_bytes()); // header checksum（parse 不驗）
        f.extend_from_slice(&src_ip);
        f.extend_from_slice(&dst_ip);
        // TCP header (20)
        f.extend_from_slice(&src_port.to_be_bytes());
        f.extend_from_slice(&dst_port.to_be_bytes());
        f.extend_from_slice(&[0, 0, 0, 0]); // seq
        f.extend_from_slice(&[0, 0, 0, 0]); // ack
        f.push(0x50); // data offset 5 (20 bytes)
        f.push(flags);
        f.extend_from_slice(&0xffffu16.to_be_bytes()); // window
        f.extend_from_slice(&0u16.to_be_bytes()); // checksum
        f.extend_from_slice(&0u16.to_be_bytes()); // urgent ptr
        f
    }

    #[test]
    fn parse_tcp_extracts_tuple_and_syn() {
        // guest 10.0.2.15:50000 → 1.1.1.1:443，純 SYN。
        let frame = build_tcp_frame(GUEST_IP, 50000, EXT_IP, 443, 0x02);
        let t = parse_tcp(&frame).unwrap();
        assert_eq!(t.src_mac, GUEST_MAC);
        assert_eq!(t.src_ip, GUEST_IP);
        assert_eq!(t.src_port, 50000);
        assert_eq!(t.dst_ip, EXT_IP);
        assert_eq!(t.dst_port, 443);
        assert!(t.syn && !t.ack && !t.fin && !t.rst);
    }

    #[test]
    fn parse_tcp_flag_combinations() {
        // SYN+ACK
        let sa = parse_tcp(&build_tcp_frame(GUEST_IP, 1, EXT_IP, 2, 0x12)).unwrap();
        assert!(sa.syn && sa.ack && !sa.fin && !sa.rst);
        // FIN+ACK
        let fa = parse_tcp(&build_tcp_frame(GUEST_IP, 1, EXT_IP, 2, 0x11)).unwrap();
        assert!(fa.fin && fa.ack && !fa.syn);
        // RST
        let r = parse_tcp(&build_tcp_frame(GUEST_IP, 1, EXT_IP, 2, 0x04)).unwrap();
        assert!(r.rst && !r.syn && !r.ack);
    }

    #[test]
    fn parse_tcp_rejects_udp_and_short() {
        // UDP frame → parse_tcp None
        let udp = build_udp_reply(GUEST_MAC, GW_MAC, GUEST_IP, 1, EXT_IP, 2, b"abcd");
        assert!(parse_tcp(&udp).is_none());
        // 太短
        assert!(parse_tcp(&[0u8; 40]).is_none());
        // parse_udp 不應把 TCP frame 當 UDP
        let tcp = build_tcp_frame(GUEST_IP, 1, EXT_IP, 2, 0x02);
        assert!(parse_udp(&tcp).is_none());
    }
}
