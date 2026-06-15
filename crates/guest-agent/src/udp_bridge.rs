//! VM 內的 UDP 橋接：把 VM 對外網卡（eth0）上的 UDP 埠轉到服務監聽的 loopback。
//!
//! 背景：Windows 的 WSL2（與 macOS 的 VZ NAT）只會把 VM loopback 的 **TCP** 埠
//! 鏡射到 host（wslrelay）；UDP 不轉。chefer 的做法是讓 host 端 relay 直接打到
//! VM 的對外 IPv4（`<vm_ip>:guest`），再由本模組在 VM 內把 `<vm_ip>:guest` 橋回
//! 服務實際監聽的 `127.0.0.1:guest`——如此「服務只綁 loopback」也能從 host 連入，
//! 與 TCP 行為一致。
//!
//! 重要時序：本橋接**在服務啟動後**才綁 `<vm_ip>:guest`（給一段寬限期），原因是
//! 若服務綁的是 `0.0.0.0:guest`（涵蓋 eth0），它已可直接從 `<vm_ip>:guest` 命中，
//! 不需橋接；此時我們的 bind 會 EADDRINUSE → 直接略過。反過來若先綁了橋接、服務
//! 才綁 `0.0.0.0`，會害服務 bind 失敗，故寧可晚綁。
//!
//! 純 `std::net`，跨平台可編譯；實際只在 VM 後端（`--udp-bridge`）的 Linux 執行路徑被呼叫。
//! `detect_primary_ipv4` 另供 `guest-agent vmip` 子命令使用。

use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use chefer_bundle::{Manifest, PortProto};

/// 服務啟動到 VM 內橋接綁定之間的寬限期：讓綁 `0.0.0.0` 的服務先佔住埠，
/// 使我們的橋接 bind 以 EADDRINUSE 自然略過（避免與服務搶埠）。
const BRIDGE_GRACE: Duration = Duration::from_secs(3);

/// 偵測本機（VM）對外的主要 IPv4：用 UDP connect 技巧——connect 不送封包，
/// 只讓核心依路由表挑出 source IP（即 eth0 位址）。失敗或只有 loopback 時回 None。
pub fn detect_primary_ipv4() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // 203.0.113.0/24 為 TEST-NET-3（永不可路由）——僅用來觸發路由選擇，不會真的送出。
    sock.connect((Ipv4Addr::new(203, 0, 113, 1), 9)).ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => Some(v4),
        _ => None,
    }
}

/// 依 manifest 啟動所有 UDP 埠的 VM 內橋接（VM 後端專用）。
///
/// 非阻塞：spawn 一條背景執行緒，等 [`BRIDGE_GRACE`] 後再綁定各 `<vm_ip>:guest`，
/// 並對每個埠起 [`spawn_udp_relay`] 轉到 `127.0.0.1:guest`。橋接執行緒皆為 daemon
/// 性質，隨行程結束而消滅。
pub fn start_vm_udp_bridges(manifest: &Manifest) {
    let udp_guest_ports: BTreeSet<u16> = manifest
        .services
        .iter()
        .flat_map(|s| &s.ports)
        .filter(|p| p.proto == PortProto::Udp)
        .map(|p| p.guest)
        .collect();
    if udp_guest_ports.is_empty() {
        return;
    }

    thread::spawn(move || {
        // 等服務先綁好它們的 UDP 埠（綁 0.0.0.0 者會佔住 → 我們略過）。
        thread::sleep(BRIDGE_GRACE);

        let Some(vm_ip) = detect_primary_ipv4() else {
            eprintln!(
                "[guest-agent] could not find the VM's outbound IPv4, skipping UDP bridge; \
                 host-side UDP ports may be unreachable (if the service binds 0.0.0.0 it can still be hit via the VM IP)"
            );
            return;
        };

        for gp in udp_guest_ports {
            match UdpSocket::bind((vm_ip, gp)) {
                Ok(sock) => {
                    let target = SocketAddr::from((Ipv4Addr::LOCALHOST, gp));
                    spawn_udp_relay(sock, move || new_upstream(target));
                    eprintln!("[guest-agent] UDP bridge started: {vm_ip}:{gp} → 127.0.0.1:{gp}");
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                    // 服務已綁 0.0.0.0:gp（含 eth0）→ host 直接命中，不需橋接。
                    eprintln!(
                        "[guest-agent] UDP {gp} is already listening on eth0 (bound by the service directly), skipping bridge"
                    );
                }
                Err(e) => {
                    eprintln!("[guest-agent] UDP bridge failed to bind {vm_ip}:{gp}: {e}");
                }
            }
        }
    });
}

/// 通用的 per-client session UDP relay：`listen` 收到封包後，依來源位址各維護一條
/// 由 `make_upstream` 建立的 upstream socket，並各起一條回程執行緒把回應送回該來源。
///
/// `make_upstream` 抽象了「upstream 該在哪建立」：VM 橋接用 `new_upstream(target)`
/// （當前 netns）；per-app netns relay 則用會 `setns` 進 app netns 再 connect 的工廠
/// （見 `netns::relay`）——關鍵是 upstream socket 必須誕生在服務所在的 netns。
///
/// 比「只記最後來源」正確：多個 client 同時打同一埠時不會互蓋回程。閒置 session
/// 不主動回收（v1 可接受；來源量大時為輕微記憶體成長）。
pub fn spawn_udp_relay<F>(listen: UdpSocket, make_upstream: F)
where
    F: Fn() -> std::io::Result<UdpSocket> + Send + 'static,
{
    let listen = Arc::new(listen);
    let sessions: Arc<Mutex<HashMap<SocketAddr, Arc<UdpSocket>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    thread::spawn(move || {
        let mut buf = vec![0u8; 65535];
        loop {
            let (n, src) = match listen.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[udp-relay] receive failed: {e}");
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
            };

            // 取得（或建立）此來源的 upstream socket。
            let upstream = {
                let mut map = sessions.lock().unwrap();
                if let Some(up) = map.get(&src) {
                    Arc::clone(up)
                } else {
                    let up = match make_upstream() {
                        Ok(s) => Arc::new(s),
                        Err(e) => {
                            eprintln!("[udp-relay] failed to create upstream: {e}");
                            continue;
                        }
                    };
                    // 回程：upstream → listen.send_to(src)
                    let listen_back = Arc::clone(&listen);
                    let up_back = Arc::clone(&up);
                    thread::spawn(move || {
                        let mut rbuf = vec![0u8; 65535];
                        loop {
                            match up_back.recv(&mut rbuf) {
                                Ok(m) => {
                                    let _ = listen_back.send_to(&rbuf[..m], src);
                                }
                                Err(_) => return, // upstream 關閉 → 結束此 session 回程
                            }
                        }
                    });
                    map.insert(src, Arc::clone(&up));
                    up
                }
            };

            let _ = upstream.send(&buf[..n]);
        }
    });
}

/// 建立一條連到 `target` 的 upstream UDP socket（位址族對齊 target）。
fn new_upstream(target: SocketAddr) -> std::io::Result<UdpSocket> {
    let bind: SocketAddr = match target {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let sock = UdpSocket::bind(bind)?;
    sock.connect(target)?;
    Ok(sock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_primary_ipv4_is_non_loopback_or_none() {
        // 不同 CI 環境網路不一：有網卡應回非 loopback 的 v4，無則 None——兩者皆可接受。
        if let Some(ip) = detect_primary_ipv4() {
            assert!(!ip.is_loopback(), "偵測到的 IP 不應是 loopback：{ip}");
            assert!(!ip.is_unspecified(), "偵測到的 IP 不應是 0.0.0.0：{ip}");
        }
    }

    /// session relay：兩個不同來源同時打同一 relay，回程不應互蓋。
    #[test]
    fn udp_relay_keeps_per_client_return_path() {
        // UDP echo server
        let echo = UdpSocket::bind("127.0.0.1:0").unwrap();
        let echo_addr = echo.local_addr().unwrap();
        thread::spawn(move || {
            let mut buf = [0u8; 1500];
            loop {
                let Ok((n, src)) = echo.recv_from(&mut buf) else {
                    return;
                };
                let _ = echo.send_to(&buf[..n], src);
            }
        });

        let listen = UdpSocket::bind("127.0.0.1:0").unwrap();
        let relay_addr = listen.local_addr().unwrap();
        spawn_udp_relay(listen, move || new_upstream(echo_addr));

        for tag in [b"aaa".as_slice(), b"bbb".as_slice()] {
            let client = UdpSocket::bind("127.0.0.1:0").unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            client.send_to(tag, relay_addr).unwrap();
            let mut buf = [0u8; 16];
            let (n, _) = client.recv_from(&mut buf).unwrap();
            assert_eq!(&buf[..n], tag, "每個 client 應收回自己送出的內容");
        }
    }
}
