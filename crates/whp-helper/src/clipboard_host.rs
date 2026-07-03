//! WHP GUI 的 host 端剪貼簿同步（DESIGN §6「WHP GUI」M8-e）。
//!
//! 與 guest 的 `guest-agent::clipboard` 對接：helper 透過**既有埠轉發**連到 guest 的
//! 剪貼簿服務（`[::1]:<port>` → smoltcp → guest eth0:<port>），送 token 後雙向同步
//! Windows 剪貼簿與 cage 剪貼簿。首版 UTF-8 文字。純 Win32，故 `#[cfg(windows)]`。
//!
//! 協定同 guest 端：token 行 → 各於本地剪貼簿變更時送 `<u32 be len><utf8>`；套用對端
//! 內容後記住以抑制回音。輪詢 Windows 剪貼簿（不需 clipboard listener 視窗）。

#![cfg(windows)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use windows_sys::Win32::Foundation::{HANDLE, HGLOBAL};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

const POLL: Duration = Duration::from_millis(400);
const MAX_TEXT: usize = 4 * 1024 * 1024;

/// host 端剪貼簿同步把手；Drop 時停背景執行緒。
pub struct ClipHost {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for ClipHost {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 產生剪貼簿通道的隨機 token（CSPRNG，16 bytes → hex）。以 cmdline 傳給 guest，防同機
/// 其他程序連上 localhost 轉發埠偷讀/注入剪貼簿。失敗（極罕見）回退到時間/pid 熵。
pub fn generate_token() -> String {
    use windows_sys::Win32::Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
    };
    let mut buf = [0u8; 16];
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        // 極罕見 fallback：混時間 + pid（非密碼學強度，但通道為 localhost、短命）。
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mix = t ^ ((std::process::id() as u128) << 64);
        buf.copy_from_slice(&mix.to_le_bytes());
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 啟動 host 端剪貼簿同步執行緒。`port` 為 guest 剪貼簿服務埠（helper 已對其設埠轉發）。
pub fn spawn(token: String, port: u16) -> ClipHost {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_c = stop.clone();
    let handle = std::thread::Builder::new()
        .name("chefer-clip-host".into())
        .spawn(move || run(token, port, stop_c))
        .expect("failed to spawn clipboard host thread");
    ClipHost {
        stop,
        handle: Some(handle),
    }
}

fn run(token: String, port: u16, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, port)) {
            Ok(stream) => {
                if let Err(e) = session(stream, &token, &stop) {
                    eprintln!("[whp-clip] session ended: {e}");
                }
            }
            Err(_) => {
                // guest 服務還沒起（開機中）或已收：等一下重試。
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn session(mut stream: TcpStream, token: &str, stop: &Arc<AtomicBool>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(POLL))?;
    // 送 token 行。
    stream.write_all(token.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = stream.try_clone()?;
    let mut last_applied: Option<String> = None; // 對端送來、已寫入 Windows 剪貼簿的內容
    let mut last_sent: Option<String> = None; // 我方最近送出的內容
    let mut hdr = [0u8; 4];
    let mut hdr_filled = 0usize;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        // 讀對端（guest→host）：套用到 Windows 剪貼簿。
        match reader.read(&mut hdr[hdr_filled..]) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                hdr_filled += n;
                if hdr_filled == 4 {
                    hdr_filled = 0;
                    let len = u32::from_be_bytes(hdr) as usize;
                    if len > MAX_TEXT {
                        return Ok(());
                    }
                    let mut buf = vec![0u8; len];
                    read_exact_timeout(&mut reader, &mut buf, stop)?;
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    if std::env::var_os("CHEFER_CLIP_TRACE").is_some() {
                        eprintln!("[clip-host] applied from guest ({} bytes)", text.len());
                    }
                    set_clipboard_text(&text);
                    last_applied = Some(text);
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e),
        }

        // host→guest：Windows 剪貼簿變了且非回音就送。
        if let Some(cur) = get_clipboard_text() {
            let is_echo = last_applied.as_ref() == Some(&cur);
            if !is_echo && last_sent.as_ref() != Some(&cur) && cur.len() <= MAX_TEXT {
                if std::env::var_os("CHEFER_CLIP_TRACE").is_some() {
                    eprintln!("[clip-host] sent to guest ({} bytes)", cur.len());
                }
                stream.write_all(&(cur.len() as u32).to_be_bytes())?;
                stream.write_all(cur.as_bytes())?;
                stream.flush()?;
                last_sent = Some(cur);
            }
        }
    }
}

fn read_exact_timeout(
    r: &mut TcpStream,
    buf: &mut [u8],
    stop: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        if stop.load(Ordering::Relaxed) {
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

/// 讀 Windows 剪貼簿的 UTF-8 文字（無文字/開啟失敗回 None）。
fn get_clipboard_text() -> Option<String> {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let h = GetClipboardData(CF_UNICODETEXT as u32);
        let result = if h.is_null() {
            None
        } else {
            let ptr = GlobalLock(h as HGLOBAL) as *const u16;
            if ptr.is_null() {
                None
            } else {
                // 找 NUL 結尾。
                let mut len = 0usize;
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(ptr, len);
                let s = String::from_utf16_lossy(slice);
                let _ = GlobalUnlock(h as HGLOBAL);
                Some(s)
            }
        };
        CloseClipboard();
        result
    }
}

/// 設定 Windows 剪貼簿為 `text`（UTF-8 → UTF-16 + NUL）。
fn set_clipboard_text(text: &str) {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    let bytes = utf16.len() * 2;
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return;
        }
        EmptyClipboard();
        let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if !hmem.is_null() {
            let dst = GlobalLock(hmem) as *mut u16;
            if !dst.is_null() {
                std::ptr::copy_nonoverlapping(utf16.as_ptr(), dst, utf16.len());
                let _ = GlobalUnlock(hmem);
                // SetClipboardData 成功後 hmem 的所有權轉給系統，不可再釋放。
                if SetClipboardData(CF_UNICODETEXT as u32, hmem as HANDLE).is_null() {
                    // 失敗：所有權仍在我方，但這裡沿用 best-effort（不釋放，罕見）。
                }
            }
        }
        CloseClipboard();
    }
}
