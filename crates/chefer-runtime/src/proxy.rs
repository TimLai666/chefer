//! 埠代理：host 埠 ≠ guest 埠時，把 127.0.0.1:host 的流量轉送到 guest 埠。
//!
//! - TCP：listener 接受連線後，連到 guest 埠並以兩條 copy threads 雙向轉送。
//!   後端依序嘗試 127.0.0.1 與 [::1]——Windows 上 WSL2 的 localhost 轉送
//!   （wslrelay）實測只綁 IPv6 [::1]，只連 v4 會失敗。
//! - UDP：雙 socket relay；記住最後一個 client 來源位址做回程。
//!   僅 Linux（共享 netns，可直送 127.0.0.1:guest）在此啟動。
//!   Windows 因 WSL2 的 wslrelay 不轉 UDP，改由 wsl2 後端取得 VM IP 後自行
//!   relay 到 `<vm_ip>:guest`（見 vmm-backend/wsl2.rs），故此處在 Windows 略過 UDP
//!   ——若在此也綁 127.0.0.1:host，會與後端的 relay 搶占同一 host 埠。
//! - host == guest：Linux 上不代理（共享網路，埠直接生效）；Windows 上
//!   另起 best-effort 的 IPv4 補橋（127.0.0.1:port → [::1]:port），
//!   讓只認 127.0.0.1 的 Windows 程式也能連到 WSL 內的服務；
//!   此補橋 bind 失敗只警告不致命（wslrelay 可能已綁 v4）。
//! - 代理 threads 為 daemon 性質：不 join，隨主程式結束而消滅。

#[cfg(not(windows))]
use std::net::UdpSocket;
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
#[cfg(not(windows))]
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result, bail};
use chefer_bundle::{Manifest, PortProto};

/// 對 manifest 中所有 `host != guest` 的埠映射啟動代理。
///
/// 任一 host 埠 bind 失敗（通常是埠已被占用）→ 啟動前整體報錯並列出埠號，
/// 不會啟動任何服務。
pub fn start_port_proxies(manifest: &Manifest) -> Result<()> {
    let mut failures: Vec<String> = Vec::new();
    for svc in &manifest.services {
        for spec in &svc.ports {
            if spec.host == spec.guest {
                // Windows：wslrelay 只綁 [::1]，補一座 127.0.0.1 → [::1] 的
                // IPv4 橋（best-effort：bind 不到就略過，可能已有人轉送 v4）
                #[cfg(windows)]
                if spec.proto == PortProto::Tcp {
                    match start_tcp_proxy_to(
                        spec.host,
                        vec![(Ipv6Addr::LOCALHOST, spec.guest).into()],
                    ) {
                        Ok(()) => tracing::debug!(
                            "IPv4 bridge started: 127.0.0.1:{} -> [::1]:{} (service `{}`)",
                            spec.host,
                            spec.guest,
                            svc.name
                        ),
                        Err(err) => tracing::debug!(
                            "IPv4 bridge not started (does not affect localhost/[::1] access): {err:#}"
                        ),
                    }
                }
                continue;
            }
            let result = match spec.proto {
                PortProto::Tcp => start_tcp_proxy(spec.host, spec.guest),
                #[cfg(not(windows))]
                PortProto::Udp => start_udp_proxy(spec.host, spec.guest),
                #[cfg(windows)]
                PortProto::Udp => {
                    // Windows：UDP 由 wsl2 後端取得 VM IP 後自行 relay（wslrelay 不轉 UDP），
                    // 此處不綁，避免與後端搶占 127.0.0.1:host；continue 以免誤印「已啟動」。
                    tracing::debug!(
                        "UDP port {spec} handled by the wsl2 backend (wslrelay does not forward UDP)"
                    );
                    continue;
                }
            };
            match result {
                Ok(()) => {
                    tracing::info!("Port proxy started: {} (service `{}`)", spec, svc.name);
                }
                Err(err) => {
                    failures.push(format!(
                        "  - {}/{} (service `{}`): {err:#}",
                        spec.host, spec.proto, svc.name
                    ));
                }
            }
        }
    }
    if !failures.is_empty() {
        bail!(
            "Could not start proxies for the following host ports (the ports may already be in use by another program; \
             close the program using them, or adjust the ports in appcipe.yml and repack):\n{}",
            failures.join("\n")
        );
    }
    Ok(())
}

// ── TCP ────────────────────────────────────────────────────────────

fn start_tcp_proxy(host: u16, guest: u16) -> Result<()> {
    start_tcp_proxy_to(host, guest_backends(guest))
}

/// guest 埠的後端候選位址：先試 IPv4 loopback，再試 IPv6 loopback
/// （WSL2 的 wslrelay 只綁 [::1]）。
fn guest_backends(guest: u16) -> Vec<SocketAddr> {
    vec![
        (Ipv4Addr::LOCALHOST, guest).into(),
        (Ipv6Addr::LOCALHOST, guest).into(),
    ]
}

fn start_tcp_proxy_to(host: u16, backends: Vec<SocketAddr>) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", host))
        .with_context(|| format!("failed to bind 127.0.0.1:{host}/tcp"))?;
    spawn_tcp_proxy(listener, backends);
    Ok(())
}

/// 在背景 thread 跑 TCP 代理的 accept 迴圈（測試可直接傳入已 bind 的 listener）。
pub(crate) fn spawn_tcp_proxy(listener: TcpListener, backends: Vec<SocketAddr>) {
    thread::spawn(move || {
        for conn in listener.incoming() {
            let client = match conn {
                Ok(c) => c,
                Err(err) => {
                    tracing::debug!("TCP proxy accept failed: {err}");
                    continue;
                }
            };
            let backends = backends.clone();
            thread::spawn(move || {
                if let Err(err) = relay_tcp(client, &backends) {
                    tracing::debug!("TCP proxy connection ended (backends {backends:?}): {err:#}");
                }
            });
        }
    });
}

/// 對單一連線做雙向轉送：client ↔ 第一個連得上的後端。
fn relay_tcp(client: TcpStream, backends: &[SocketAddr]) -> Result<()> {
    let mut last_err: Option<std::io::Error> = None;
    let mut connected: Option<TcpStream> = None;
    for addr in backends {
        match TcpStream::connect(addr) {
            Ok(s) => {
                connected = Some(s);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let upstream = connected.ok_or_else(|| {
        anyhow::anyhow!(
            "failed to connect to all backends {backends:?} (guest service not ready yet?): {}",
            last_err.map(|e| e.to_string()).unwrap_or_default()
        )
    })?;

    let mut client_read = client
        .try_clone()
        .context("failed to clone client socket")?;
    let mut upstream_write = upstream
        .try_clone()
        .context("failed to clone upstream socket")?;
    let mut upstream_read = upstream;
    let mut client_write = client;

    // client → guest
    thread::spawn(move || {
        let _ = std::io::copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(Shutdown::Write);
    });
    // guest → client（在本連線 thread 直接跑）
    let _ = std::io::copy(&mut upstream_read, &mut client_write);
    let _ = client_write.shutdown(Shutdown::Write);
    Ok(())
}

// ── UDP ────────────────────────────────────────────────────────────

#[cfg(not(windows))]
fn start_udp_proxy(host: u16, guest: u16) -> Result<()> {
    let host_sock = UdpSocket::bind(("127.0.0.1", host))
        .with_context(|| format!("failed to bind 127.0.0.1:{host}/udp"))?;
    spawn_udp_proxy(host_sock, guest)
}

/// 雙 socket UDP relay：host_sock 收 client 封包轉給 guest；
/// guest 回應送回「最後一個」client 來源（簡化版，足敷單一 client 情境）。
#[cfg(not(windows))]
pub(crate) fn spawn_udp_proxy(host_sock: UdpSocket, guest: u16) -> Result<()> {
    let guest_sock =
        UdpSocket::bind(("127.0.0.1", 0)).context("failed to create UDP relay socket")?;
    guest_sock
        .connect(("127.0.0.1", guest))
        .with_context(|| format!("UDP relay failed to connect to 127.0.0.1:{guest}"))?;

    let last_client: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));

    // client → guest
    {
        let host_sock = host_sock
            .try_clone()
            .context("failed to clone host UDP socket")?;
        let guest_sock = guest_sock
            .try_clone()
            .context("failed to clone guest UDP socket")?;
        let last_client = Arc::clone(&last_client);
        thread::spawn(move || {
            let mut buf = [0u8; 65535];
            loop {
                match host_sock.recv_from(&mut buf) {
                    Ok((n, src)) => {
                        *last_client.lock().unwrap() = Some(src);
                        let _ = guest_sock.send(&buf[..n]);
                    }
                    Err(err) => {
                        tracing::debug!("UDP proxy (host side) receive failed: {err}");
                        // 避免錯誤時熱迴圈空轉
                        thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            }
        });
    }

    // guest → 最後一個 client
    thread::spawn(move || {
        let mut buf = [0u8; 65535];
        loop {
            match guest_sock.recv(&mut buf) {
                Ok(n) => {
                    let target = *last_client.lock().unwrap();
                    if let Some(addr) = target {
                        let _ = host_sock.send_to(&buf[..n], addr);
                    }
                }
                Err(err) => {
                    tracing::debug!("UDP proxy (guest side) receive failed: {err}");
                    thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::Duration;

    #[test]
    fn tcp_proxy_relays_data() {
        // echo server 於 OS 隨機埠 A
        let echo = TcpListener::bind("127.0.0.1:0").unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        thread::spawn(move || {
            for conn in echo.incoming() {
                let Ok(stream) = conn else { continue };
                thread::spawn(move || {
                    let mut read_half = stream.try_clone().unwrap();
                    let mut write_half = stream;
                    let _ = std::io::copy(&mut read_half, &mut write_half);
                });
            }
        });

        // 代理 B → A（B 也用 OS 配發避免衝突）
        let proxy_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_port = proxy_listener.local_addr().unwrap().port();
        spawn_tcp_proxy(proxy_listener, guest_backends(echo_port));

        let mut client = TcpStream::connect(("127.0.0.1", proxy_port)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let msg = b"hello chefer proxy";
        client.write_all(msg).unwrap();

        let mut buf = vec![0u8; msg.len()];
        client.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, msg, "透過代理應收到 echo 回來的相同內容");
    }

    /// v4 後端連不上時應自動退到 v6 後端（WSL2 wslrelay 只綁 [::1] 的情境）。
    #[test]
    fn tcp_proxy_falls_back_to_ipv6_backend() {
        let echo6 = TcpListener::bind("[::1]:0").unwrap();
        let echo_port = echo6.local_addr().unwrap().port();
        thread::spawn(move || {
            for conn in echo6.incoming() {
                let Ok(stream) = conn else { continue };
                thread::spawn(move || {
                    let mut read_half = stream.try_clone().unwrap();
                    let mut write_half = stream;
                    let _ = std::io::copy(&mut read_half, &mut write_half);
                });
            }
        });

        let proxy_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_port = proxy_listener.local_addr().unwrap().port();
        // 後端候選：一個必死的 v4（剛 bind 又釋放的埠不可靠，改用 echo 埠的
        // v4 位址——echo 只綁 [::1]，其 v4 大機率無人監聽）→ 再退到 v6
        spawn_tcp_proxy(
            proxy_listener,
            vec![
                (Ipv4Addr::LOCALHOST, echo_port).into(),
                (Ipv6Addr::LOCALHOST, echo_port).into(),
            ],
        );

        let mut client = TcpStream::connect(("127.0.0.1", proxy_port)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let msg = b"fallback to v6";
        client.write_all(msg).unwrap();
        let mut buf = vec![0u8; msg.len()];
        client.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, msg);
    }

    #[cfg(not(windows))]
    #[test]
    fn udp_proxy_relays_data_with_return_path() {
        // UDP echo server 於 OS 隨機埠 G
        let echo = UdpSocket::bind("127.0.0.1:0").unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        thread::spawn(move || {
            let mut buf = [0u8; 65535];
            loop {
                let Ok((n, src)) = echo.recv_from(&mut buf) else {
                    return;
                };
                let _ = echo.send_to(&buf[..n], src);
            }
        });

        // 代理 H → G
        let host_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let host_port = host_sock.local_addr().unwrap().port();
        spawn_udp_proxy(host_sock, echo_port).unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let msg = b"udp ping";
        client.send_to(msg, ("127.0.0.1", host_port)).unwrap();

        let mut buf = [0u8; 64];
        let (n, _) = client.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], msg, "UDP 回程應送回最後一個 client");
    }
}
