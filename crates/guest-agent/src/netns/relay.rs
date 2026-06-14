//! per-app netns 的 inbound port relay（`internal` / `bridge` 共用）。
//!
//! supervisor 在 **parent netns** 為每個宣告的 `ports:` 開 listener；連入的流量轉到
//! **app netns** 內的 `127.0.0.1:guest`。因 netns 是 per-thread，本模組起一條常駐的
//! **app-netns socket factory** 執行緒（`setns` 進 app netns 後不再離開），負責建立
//! 連到 `127.0.0.1:guest` 的 upstream socket；listener / 位元組搬運則留在 parent netns。
//! socket fd 一旦建立即與 netns 無關，可跨執行緒自由讀寫。
//!
//! 未宣告的 port 不開 listener → parent netns 無對應入口 → host 連不到（隔離核心）。

use std::collections::BTreeSet;
use std::io;
use std::net::{Ipv4Addr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::mpsc::{Sender, SyncSender, channel, sync_channel};
use std::thread;

use chefer_bundle::{Manifest, PortProto};

use crate::netns;
use crate::udp_bridge::spawn_udp_relay;

/// 對 app-netns socket factory 的請求：建立連到 `127.0.0.1:port` 的 upstream，
/// 透過 reply channel 回傳建立好的 fd。
enum DialReq {
    Tcp(u16, SyncSender<io::Result<OwnedFd>>),
    Udp(u16, SyncSender<io::Result<OwnedFd>>),
}

/// app-netns socket factory 的把手（可 clone，供多個 relay listener 共用）。
#[derive(Clone)]
struct Dialer {
    tx: Sender<DialReq>,
}

impl Dialer {
    fn tcp(&self, port: u16) -> io::Result<TcpStream> {
        let (rtx, rrx) = sync_channel(0);
        self.tx
            .send(DialReq::Tcp(port, rtx))
            .map_err(|_| io::Error::other("app-netns dialer 已結束"))?;
        let fd = rrx
            .recv()
            .map_err(|_| io::Error::other("app-netns dialer 無回應"))??;
        // SAFETY: fd 由 factory 以 IntoRawFd 釋出，所有權移交給此 TcpStream。
        Ok(unsafe { TcpStream::from_raw_fd(fd.into_raw_fd()) })
    }

    fn udp(&self, port: u16) -> io::Result<UdpSocket> {
        let (rtx, rrx) = sync_channel(0);
        self.tx
            .send(DialReq::Udp(port, rtx))
            .map_err(|_| io::Error::other("app-netns dialer 已結束"))?;
        let fd = rrx
            .recv()
            .map_err(|_| io::Error::other("app-netns dialer 無回應"))??;
        Ok(unsafe { UdpSocket::from_raw_fd(fd.into_raw_fd()) })
    }
}

/// 啟動 app-netns socket factory：常駐執行緒先 `setns` 進 app netns，再依請求建立 upstream。
/// 回傳 `Dialer`；若 factory 無法進入 netns，回 None（呼叫端據此放棄起 relay）。
fn start_dialer(net_fd: RawFd) -> Option<Dialer> {
    let (tx, rx) = channel::<DialReq>();
    let (ready_tx, ready_rx) = sync_channel::<bool>(0);
    thread::spawn(move || {
        if let Err(e) = netns::enter_netns(net_fd) {
            eprintln!("[guest-agent] inbound relay factory 無法進入 app netns：{e:#}");
            let _ = ready_tx.send(false);
            return;
        }
        let _ = ready_tx.send(true);
        // 此執行緒自此常駐於 app netns，逐一回應建立 upstream 的請求。
        while let Ok(req) = rx.recv() {
            match req {
                DialReq::Tcp(port, reply) => {
                    let r = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).map(OwnedFd::from);
                    let _ = reply.send(r);
                }
                DialReq::Udp(port, reply) => {
                    let r = connect_udp(port).map(OwnedFd::from);
                    let _ = reply.send(r);
                }
            }
        }
    });
    match ready_rx.recv() {
        Ok(true) => Some(Dialer { tx }),
        _ => None,
    }
}

/// 在「當前 netns」建立一條連到 `127.0.0.1:port` 的 UDP socket。
fn connect_udp(port: u16) -> io::Result<UdpSocket> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    sock.connect((Ipv4Addr::LOCALHOST, port))?;
    Ok(sock)
}

/// 依 manifest 為所有宣告的 inbound port 起跨 netns relay（parent netns ↔ app netns）。
///
/// `net_fd` 為 app netns 的 net ns fd。relay 執行緒皆為 daemon 性質，隨行程結束消滅。
/// listener 綁 `127.0.0.1:guest`（parent netns）——與 `shared` 模式下服務自己綁 loopback
/// 等效，故 chefer-runtime 既有的 host→`127.0.0.1:guest` 代理無需改動。
pub fn start_inbound_relays(manifest: &Manifest, net_fd: RawFd) {
    let tcp_ports: BTreeSet<u16> = manifest
        .services
        .iter()
        .flat_map(|s| &s.ports)
        .filter(|p| p.proto == PortProto::Tcp)
        .map(|p| p.guest)
        .collect();
    let udp_ports: BTreeSet<u16> = manifest
        .services
        .iter()
        .flat_map(|s| &s.ports)
        .filter(|p| p.proto == PortProto::Udp)
        .map(|p| p.guest)
        .collect();

    if tcp_ports.is_empty() && udp_ports.is_empty() {
        return; // 無宣告 port → 完全不開對外入口（純內部 app）。
    }

    let Some(dialer) = start_dialer(net_fd) else {
        eprintln!(
            "[guest-agent] 無法啟動 inbound relay（app netns 進入失敗）；\
             宣告的 port 將無法從 host 連入"
        );
        return;
    };

    for gp in tcp_ports {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, gp)) {
            Ok(listener) => {
                let dialer = dialer.clone();
                thread::spawn(move || tcp_listener_loop(listener, dialer, gp));
                eprintln!(
                    "[guest-agent] inbound relay（TCP）已啟動：127.0.0.1:{gp} → app-netns 127.0.0.1:{gp}"
                );
            }
            Err(e) => eprintln!("[guest-agent] inbound relay bind 127.0.0.1:{gp}/tcp 失敗：{e}"),
        }
    }

    for gp in udp_ports {
        match UdpSocket::bind((Ipv4Addr::LOCALHOST, gp)) {
            Ok(listen) => {
                let dialer = dialer.clone();
                spawn_udp_relay(listen, move || dialer.udp(gp));
                eprintln!(
                    "[guest-agent] inbound relay（UDP）已啟動：127.0.0.1:{gp} → app-netns 127.0.0.1:{gp}"
                );
            }
            Err(e) => eprintln!("[guest-agent] inbound relay bind 127.0.0.1:{gp}/udp 失敗：{e}"),
        }
    }
}

/// TCP listener 迴圈（parent netns）：每個連線向 dialer 取一條 app-netns upstream 後雙向搬運。
fn tcp_listener_loop(listener: TcpListener, dialer: Dialer, port: u16) {
    for conn in listener.incoming() {
        let client = match conn {
            Ok(c) => c,
            Err(_) => continue,
        };
        let dialer = dialer.clone();
        thread::spawn(move || {
            let upstream = match dialer.tcp(port) {
                Ok(u) => u,
                Err(_) => return, // 服務尚未就緒/已關 → 關閉此連線（host client 自會重試）
            };
            bidir_copy(client, upstream);
        });
    }
}

/// 雙向搬運兩條 TCP stream 的位元組，一端 EOF 即半關對端寫方向。
fn bidir_copy(client: TcpStream, upstream: TcpStream) {
    let (Ok(mut c_rd), Ok(mut u_wr), Ok(mut u_rd), Ok(mut c_wr)) = (
        client.try_clone(),
        upstream.try_clone(),
        upstream.try_clone(),
        client.try_clone(),
    ) else {
        return;
    };
    let h = thread::spawn(move || {
        let _ = io::copy(&mut c_rd, &mut u_wr);
        let _ = u_wr.shutdown(std::net::Shutdown::Write);
    });
    let _ = io::copy(&mut u_rd, &mut c_wr);
    let _ = c_wr.shutdown(std::net::Shutdown::Write);
    let _ = h.join();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// bidir_copy 應在 client↔upstream 間雙向轉發，並在來源 EOF 後讓對端收完關閉。
    #[test]
    fn bidir_copy_proxies_both_directions() {
        // upstream = 一個會把收到內容轉大寫回送的 echo server
        let echo = TcpListener::bind("127.0.0.1:0").unwrap();
        let echo_addr = echo.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut s, _)) = echo.accept() {
                let mut buf = [0u8; 64];
                loop {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let up: Vec<u8> =
                                buf[..n].iter().map(|b| b.to_ascii_uppercase()).collect();
                            if s.write_all(&up).is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });
        let upstream = TcpStream::connect(echo_addr).unwrap();

        // client 側：用 loopback listener 造一對相連的 stream
        let front = TcpListener::bind("127.0.0.1:0").unwrap();
        let front_addr = front.local_addr().unwrap();
        let mut client_local = TcpStream::connect(front_addr).unwrap();
        let (client_remote, _) = front.accept().unwrap();

        thread::spawn(move || bidir_copy(client_remote, upstream));

        client_local.write_all(b"hello").unwrap();
        client_local
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut got = [0u8; 5];
        client_local.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"HELLO", "雙向轉發後應收到 upstream 的回應");
    }
}
