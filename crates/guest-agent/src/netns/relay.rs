//! per-app netns 的 inbound port relay（`internal` / `bridge` 共用）。
//!
//! supervisor 在 **parent netns** 為每個宣告的 `ports:` 開 listener；連入的流量轉到
//! **app netns** 內的 `127.0.0.1:guest`。upstream socket 必須誕生在 app netns，但 rootless
//! 下 supervisor 無法 `setns` 進該 netns（見 `netns` 模組說明）。故透過 [`netns::Dialer`]
//! 向 **holder socket factory** 請求 upstream（holder 在 netns 內建立 socket、以 SCM_RIGHTS
//! 傳回），listener 與位元組搬運則留在 parent netns。
//!
//! 未宣告的 port 不開 listener → parent netns 無對應入口 → host 連不到（隔離核心）。

use std::collections::BTreeSet;
use std::io;
use std::net::{Ipv4Addr, TcpListener, TcpStream, UdpSocket};
use std::thread;

use chefer_bundle::{Manifest, PortProto};

use crate::netns::Dialer;
use crate::udp_bridge::spawn_udp_relay;

/// 依 manifest 為所有宣告的 inbound port 起跨 netns relay（parent netns ↔ app netns）。
///
/// `dialer` 為通往 holder socket factory 的把手。relay 執行緒皆為 daemon 性質，隨行程結束消滅。
/// listener 綁 `127.0.0.1:guest`（parent netns）——與 `shared` 模式下服務自己綁 loopback
/// 等效，故 chefer-runtime 既有的 host→`127.0.0.1:guest` 代理無需改動。
pub fn start_inbound_relays(manifest: &Manifest, dialer: Dialer) {
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
