//! VM guest 的剪貼簿同步服務（DESIGN §6「WHP GUI」M8-e）。
//!
//! micro-VM（WHP/vz）內的 cage 是獨立 Wayland compositor，與 host 剪貼簿無通道。
//! 本模組在 guest 內起一個服務，經**既有網路通道 + cmdline token** 與 host 端
//! （whp-helper / 未來 vz helper）雙向同步文字剪貼簿——**不走 vsock**（省一個裝置模型）。
//!
//! 傳輸：guest 服務在 root netns 的 `0.0.0.0:<clip_port>` 監聽（helper 以既有埠轉發
//! 從 host `[::1]:<clip_port>` 連進來，經 smoltcp 到 guest eth0）。連線後 host 先送
//! token（一行），比對 `chefer.clip_token=` cmdline 值——不符即斷線（防同機其他程序
//! 連 localhost 轉發埠偷讀/注入剪貼簿）。
//!
//! 協定：token 行之後，雙方各自於**本地剪貼簿變更**時送一則 `<u32 be len><utf8>`。
//! 回音抑制：收到對端更新並套用到本地後記住該內容；本地 watcher 之後看到相同內容
//! 就不回送（避免 host↔guest 無限迴圈）。首版只同步 UTF-8 文字（`text/plain`）。
//!
//! guest 端與 cage 的互動用 overlay 內的 `wl-copy`/`wl-paste`（wl-clipboard）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 剪貼簿同步埠（guest 監聽；helper 以同埠轉發）。與 app 服務埠不衝突的高位固定埠。
pub const CLIP_PORT: u16 = 55381;
/// 本地剪貼簿輪詢間隔。
const POLL: Duration = Duration::from_millis(400);
/// 單則剪貼簿文字上限（防惡意/巨量 payload；超過截斷）。
const MAX_TEXT: usize = 4 * 1024 * 1024;

/// 存活中的剪貼簿同步；Drop 時通知背景執行緒收尾。
pub struct ClipboardSync {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ClipboardSync {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // 不 join 太久：背景執行緒卡在 accept/read 時靠 stop 旗標於下次輪詢退出。
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 從 `/proc/cmdline` 取 `chefer.clip_token=` 與 `chefer.clip_port=`。無 token 回 None。
fn read_cmdline_clip() -> Option<(String, u16)> {
    let cmdline = std::fs::read_to_string("/proc/cmdline").ok()?;
    let mut token = None;
    let mut port = CLIP_PORT;
    for tok in cmdline.split_whitespace() {
        if let Some(v) = tok.strip_prefix("chefer.clip_token=") {
            if !v.is_empty() {
                token = Some(v.to_string());
            }
        } else if let Some(v) = tok.strip_prefix("chefer.clip_port=")
            && let Ok(p) = v.parse::<u16>()
        {
            port = p;
        }
    }
    token.map(|t| (t, port))
}

/// 需要時啟動剪貼簿同步。回 `Ok(None)` 表示不適用（無 token = host 未啟用剪貼簿）
/// 或 wl-clipboard 不可用。呼叫端須先設好 `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`。
pub fn maybe_start() -> Option<ClipboardSync> {
    let (token, port) = read_cmdline_clip()?;
    if which("wl-copy").is_none() || which("wl-paste").is_none() {
        eprintln!(
            "[guest-agent] clipboard: wl-clipboard not found in the GUI overlay; host↔guest clipboard disabled"
        );
        return None;
    }
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[guest-agent] clipboard: cannot listen on 0.0.0.0:{port} ({e}); disabled");
            return None;
        }
    };
    // accept 用短 timeout 輪詢，讓 stop 旗標能中止（TcpListener 無原生 timeout → 設非阻塞）。
    let _ = listener.set_nonblocking(true);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_c = stop.clone();
    let handle = std::thread::Builder::new()
        .name("chefer-clip".into())
        .spawn(move || serve(listener, token, stop_c))
        .ok()?;
    eprintln!("[guest-agent] clipboard: host↔guest text sync active (port {port})");
    Some(ClipboardSync {
        stop,
        handle: Some(handle),
    })
}

/// 接受 host 連線（token 驗證）並跑雙向同步；連線斷了就等下一個（host 可能重連）。
fn serve(listener: TcpListener, token: String, stop: Arc<std::sync::atomic::AtomicBool>) {
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                if let Err(e) = handle_peer(stream, &token, &stop) {
                    eprintln!("[guest-agent] clipboard: peer session ended: {e}");
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(POLL);
            }
            Err(e) => {
                eprintln!("[guest-agent] clipboard: accept failed: {e}");
                std::thread::sleep(POLL);
            }
        }
    }
}

/// 一條 host 連線：驗 token → 雙向同步直到斷線或 stop。
fn handle_peer(
    stream: TcpStream,
    token: &str,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(POLL))?;
    let mut reader = stream.try_clone()?;
    // token 握手：host 連上後先送一行 token（\n 結尾）。
    let mut got = Vec::new();
    let mut byte = [0u8; 1];
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match reader.read(&mut byte) {
            Ok(0) => return Ok(()), // 對端關閉
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                if got.len() < 256 {
                    got.push(byte[0]);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() > deadline {
                    return Ok(());
                }
            }
            Err(e) => return Err(e),
        }
    }
    if got != token.as_bytes() {
        eprintln!("[guest-agent] clipboard: rejected peer (bad token)");
        return Ok(());
    }

    // 最近一次「套用到本地」的內容——本地 watcher 看到相同就不回送（回音抑制）。
    let last_applied: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let writer = Arc::new(Mutex::new(stream));

    // guest → host：輪詢 wl-paste，變更且非回音就送出。
    let mut last_sent: Option<String> = None;
    // host → guest：讀對端訊息 → wl-copy。
    let mut hdr = [0u8; 4];
    let mut hdr_filled = 0usize;
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        // 讀對端一則（非阻塞式；POLL timeout 就換去做 guest→host 輪詢）。
        match reader.read(&mut hdr[hdr_filled..]) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                hdr_filled += n;
                if hdr_filled == 4 {
                    hdr_filled = 0;
                    let len = u32::from_be_bytes(hdr) as usize;
                    if len > MAX_TEXT {
                        return Ok(()); // 異常長度：斷線
                    }
                    let mut buf = vec![0u8; len];
                    read_exact_timeout(&mut reader, &mut buf, stop)?;
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    if std::env::var_os("CHEFER_CLIP_TRACE").is_some() {
                        eprintln!("[clip-guest] applied from host ({} bytes)", text.len());
                    }
                    *last_applied.lock().unwrap() = Some(text.clone());
                    wl_copy(&text);
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e),
        }

        // guest → host：本地剪貼簿變了？
        if let Some(cur) = wl_paste() {
            let is_echo = last_applied
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|a| a == &cur);
            if !is_echo && last_sent.as_ref() != Some(&cur) {
                let bytes = cur.as_bytes();
                if bytes.len() <= MAX_TEXT {
                    if std::env::var_os("CHEFER_CLIP_TRACE").is_some() {
                        eprintln!("[clip-guest] sent to host ({} bytes)", bytes.len());
                    }
                    let mut w = writer.lock().unwrap();
                    w.write_all(&(bytes.len() as u32).to_be_bytes())?;
                    w.write_all(bytes)?;
                    w.flush()?;
                    last_sent = Some(cur);
                }
            }
        }
    }
}

/// 讀滿 `buf`（尊重 stop 旗標；read timeout 下輪詢）。
fn read_exact_timeout(
    r: &mut TcpStream,
    buf: &mut [u8],
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> std::io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(std::io::Error::other("stopped"));
        }
        match r.read(&mut buf[filled..]) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 讀 cage 目前的文字剪貼簿（無/空/非文字回 None）。
fn wl_paste() -> Option<String> {
    let out = Command::new("wl-paste")
        .args(["--no-newline", "--type", "text/plain"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// 設定 cage 的文字剪貼簿。
fn wl_copy(text: &str) {
    if let Ok(mut child) = Command::new("wl-copy")
        .args(["--type", "text/plain"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        // wl-copy 會 fork 常駐提供選取內容；不 wait（否則卡住）。
        let _ = child.id();
    }
}

/// 在 PATH 找執行檔（overlay 解壓後的 /usr/bin 等）。
fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(bin);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmdline_parse_token_and_port() {
        // 直接測解析邏輯（不經 /proc）。
        let parse = |s: &str| -> Option<(String, u16)> {
            let mut token = None;
            let mut port = CLIP_PORT;
            for tok in s.split_whitespace() {
                if let Some(v) = tok.strip_prefix("chefer.clip_token=") {
                    if !v.is_empty() {
                        token = Some(v.to_string());
                    }
                } else if let Some(v) = tok.strip_prefix("chefer.clip_port=") {
                    if let Ok(p) = v.parse::<u16>() {
                        port = p;
                    }
                }
            }
            token.map(|t| (t, port))
        };
        assert_eq!(
            parse("quiet chefer.clip_token=abc123 chefer.clip_port=40000 ro"),
            Some(("abc123".to_string(), 40000))
        );
        // 無 port → 預設。
        assert_eq!(
            parse("chefer.clip_token=deadbeef"),
            Some(("deadbeef".to_string(), CLIP_PORT))
        );
        // 無 token → None。
        assert_eq!(parse("quiet ro"), None);
        // 空 token → None。
        assert_eq!(parse("chefer.clip_token="), None);
    }
}
