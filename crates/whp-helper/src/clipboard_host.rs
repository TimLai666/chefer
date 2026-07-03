//! WHP GUI 的 host 端剪貼簿同步（DESIGN §6「WHP GUI」M8-e）。
//!
//! 與 guest 的 `guest-agent::clipboard` 對接：helper 透過**既有埠轉發**連到 guest 的
//! 剪貼簿服務（`[::1]:<port>` → smoltcp → guest eth0:<port>），送 token 後雙向同步
//! Windows 剪貼簿與 cage 剪貼簿。純 Win32，故 `#[cfg(windows)]`。
//!
//! 協定同 guest 端：token 行 → 各於本地剪貼簿變更時送 `<u8 kind><u32 be len><payload>`
//! （kind 0 = `text/plain` UTF-8、1 = `image/png` 原始 PNG bytes）；套用對端內容後記住
//! `(kind, payload)` 以抑制回音。**文字與 PNG 圖片皆同步**——圖片在線協定上一律用 PNG；
//! Windows 端同時支援註冊的 `"PNG"` 格式（現代 app，bytes 直通）與 CF_DIB（傳統 app：
//! 小畫家/剪取工具/Office，經 `dib` 模組以 `png` codec 轉換）。輪詢 Windows 剪貼簿。

#![cfg(windows)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use windows_sys::Win32::Foundation::{HANDLE, HGLOBAL};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};

const POLL: Duration = Duration::from_millis(400);
/// 單則剪貼簿 payload 上限（文字或 PNG 圖片）。與 guest 端一致。
const MAX_PAYLOAD: usize = 32 * 1024 * 1024;

/// 線協定的內容類別位元組（與 guest 端一致）。
const KIND_TEXT: u8 = 0;
const KIND_PNG: u8 = 1;

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

    let trace = std::env::var_os("CHEFER_CLIP_TRACE").is_some();
    let mut reader = stream.try_clone()?;
    let mut last_applied: Option<(u8, Vec<u8>)> = None; // 對端送來、已寫入 Windows 剪貼簿
    let mut last_sent: Option<(u8, Vec<u8>)> = None; // 我方最近送出
    // 讀對端 `<u8 kind><u32 len><payload>`（跨迴圈保留部分讀取狀態）。
    let mut kind_byte = [0u8; 1];
    let mut kind_filled = 0usize;
    let mut hdr = [0u8; 4];
    let mut hdr_filled = 0usize;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        // 讀 1 byte kind。
        if kind_filled < 1 {
            match reader.read(&mut kind_byte[..]) {
                Ok(0) => return Ok(()),
                Ok(n) => kind_filled += n,
                Err(ref e) if is_would_block(e) => {}
                Err(e) => return Err(e),
            }
        }
        // kind 到齊才讀 4 byte len。
        if kind_filled == 1 {
            match reader.read(&mut hdr[hdr_filled..]) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    hdr_filled += n;
                    if hdr_filled == 4 {
                        let kind = kind_byte[0];
                        let len = u32::from_be_bytes(hdr) as usize;
                        kind_filled = 0;
                        hdr_filled = 0;
                        if len > MAX_PAYLOAD {
                            return Ok(());
                        }
                        let mut buf = vec![0u8; len];
                        read_exact_timeout(&mut reader, &mut buf, stop)?;
                        if trace {
                            eprintln!(
                                "[clip-host] applied from guest (kind {kind}, {} bytes)",
                                buf.len()
                            );
                        }
                        set_clipboard_any(kind, &buf);
                        last_applied = Some((kind, buf));
                    }
                }
                Err(ref e) if is_would_block(e) => {}
                Err(e) => return Err(e),
            }
        }

        // host→guest：Windows 剪貼簿變了且非回音就送。
        if let Some((kind, data)) = get_clipboard_any() {
            let is_echo = last_applied
                .as_ref()
                .is_some_and(|(k, d)| *k == kind && d == &data);
            let is_resent = last_sent
                .as_ref()
                .is_some_and(|(k, d)| *k == kind && d == &data);
            if !is_echo && !is_resent && data.len() <= MAX_PAYLOAD {
                if trace {
                    eprintln!(
                        "[clip-host] sent to guest (kind {kind}, {} bytes)",
                        data.len()
                    );
                }
                stream.write_all(&[kind])?;
                stream.write_all(&(data.len() as u32).to_be_bytes())?;
                stream.write_all(&data)?;
                stream.flush()?;
                last_sent = Some((kind, data));
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
            Err(ref e) if is_would_block(e) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn is_would_block(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Windows 註冊的 `"PNG"` 剪貼簿格式 id（同名多次註冊回同一 id）。失敗回 0。
fn png_format() -> u32 {
    // "PNG" as UTF-16 + NUL.
    let name: [u16; 4] = [b'P' as u16, b'N' as u16, b'G' as u16, 0];
    unsafe { RegisterClipboardFormatW(name.as_ptr()) }
}

/// 讀目前剪貼簿某格式的原始 bytes（呼叫時 clipboard **必須已 Open**）。無/空回 None。
fn read_global_bytes(fmt: u32) -> Option<Vec<u8>> {
    if fmt == 0 {
        return None;
    }
    unsafe {
        let h = GetClipboardData(fmt);
        if h.is_null() {
            return None;
        }
        let size = GlobalSize(h as HGLOBAL);
        if size == 0 {
            return None;
        }
        let ptr = GlobalLock(h as HGLOBAL) as *const u8;
        if ptr.is_null() {
            return None;
        }
        let data = std::slice::from_raw_parts(ptr, size).to_vec();
        let _ = GlobalUnlock(h as HGLOBAL);
        Some(data)
    }
}

/// 讀 Windows 剪貼簿。優先圖片：有 `"PNG"` 格式 → 直接 (KIND_PNG, bytes)；否則 CF_DIB
/// （小畫家/剪取工具等傳統 app）→ 轉 PNG。都沒有 → CF_UNICODETEXT → (KIND_TEXT, utf8)。
fn get_clipboard_any() -> Option<(u8, Vec<u8>)> {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let mut out = None;
        // 先看 "PNG" 格式（現代 app；bytes 原樣直通）。
        let png_fmt = png_format();
        if png_fmt != 0
            && let Some(bytes) = read_global_bytes(png_fmt)
        {
            out = Some((KIND_PNG, bytes));
        }
        // 否則 CF_DIB（傳統 app）→ 轉成 PNG。
        if out.is_none()
            && let Some(dib) = read_global_bytes(CF_DIB as u32)
            && let Some(png) = crate::dib::dib_to_png(&dib)
        {
            out = Some((KIND_PNG, png));
        }
        // 否則文字。
        if out.is_none() {
            let h = GetClipboardData(CF_UNICODETEXT as u32);
            if !h.is_null() {
                let ptr = GlobalLock(h as HGLOBAL) as *const u16;
                if !ptr.is_null() {
                    let mut len = 0usize;
                    while *ptr.add(len) != 0 {
                        len += 1;
                    }
                    let slice = std::slice::from_raw_parts(ptr, len);
                    let s = String::from_utf16_lossy(slice);
                    let _ = GlobalUnlock(h as HGLOBAL);
                    out = Some((KIND_TEXT, s.into_bytes()));
                }
            }
        }
        CloseClipboard();
        out
    }
}

/// 設定 Windows 剪貼簿。text → CF_UNICODETEXT（UTF-16+NUL）；png → **同時**放註冊的 `"PNG"`
/// 格式（現代 app）與 CF_DIB（傳統 app：小畫家/剪取工具/Office），兩者互通。
fn set_clipboard_any(kind: u8, data: &[u8]) {
    // 準備要放的 (格式 id, bytes) 清單。
    let mut items: Vec<(u32, Vec<u8>)> = Vec::new();
    if kind == KIND_PNG {
        let fmt = png_format();
        if fmt != 0 {
            items.push((fmt, data.to_vec()));
        }
        // CF_DIB 供傳統 app（PNG 解碼失敗則只放 "PNG"）。
        if let Some(dib) = crate::dib::png_to_dib(data) {
            items.push((CF_DIB as u32, dib));
        }
    } else {
        let mut utf16: Vec<u16> = String::from_utf8_lossy(data).encode_utf16().collect();
        utf16.push(0);
        let mut bytes = Vec::with_capacity(utf16.len() * 2);
        for u in utf16 {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        items.push((CF_UNICODETEXT as u32, bytes));
    }
    items.retain(|(_, p)| !p.is_empty());
    if items.is_empty() {
        return;
    }
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return;
        }
        EmptyClipboard();
        for (fmt, payload) in &items {
            let hmem = GlobalAlloc(GMEM_MOVEABLE, payload.len());
            if hmem.is_null() {
                continue;
            }
            let dst = GlobalLock(hmem) as *mut u8;
            if dst.is_null() {
                continue;
            }
            std::ptr::copy_nonoverlapping(payload.as_ptr(), dst, payload.len());
            let _ = GlobalUnlock(hmem);
            // SetClipboardData 成功後 hmem 所有權轉給系統，不可再釋放。
            if SetClipboardData(*fmt, hmem as HANDLE).is_null() {
                // best-effort（失敗時所有權仍在我方，罕見，不釋放）。
            }
        }
        CloseClipboard();
    }
}
