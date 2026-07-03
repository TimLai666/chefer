//! WHP GUI 的 Win32 顯示視窗（DESIGN §6「WHP GUI」M8-c）。
//!
//! 在自己的執行緒跑 Win32 訊息迴圈：把 [`SharedFrame`] 的 BGRA framebuffer 以 GDI
//! `StretchDIBits` blit 到視窗；把鍵鼠訊息轉成 evdev 事件排進 [`SharedInput`]（VM loop
//! 再灌進 virtio-input 送 guest）。使用者關視窗 = 介面服務結束語意 → VM 收掉整個 app；
//! 反向 guest 先結束時，VM 端呼叫 [`GuiHandle::request_close`] 關視窗。
//!
//! 純轉換邏輯（像素格式、VK→evdev、座標量化）在 `virtio::gui_bridge` / `virtio::input`
//! 且已單元測試；本檔只有 Win32 呼叫與訊息分派，故 `#[cfg(windows)]`。
//! 本機（無 WHP）可用 `chefer-whp-helper --gui-selftest` 開一個動畫測試圖 + 印輸入事件
//! 來驗證視窗/blit/輸入這條路徑。

#![cfg(windows)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, DIB_RGB_COLORS, EndPaint, InvalidateRect,
    PAINTSTRUCT, SRCCOPY, StretchDIBits,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GWL_STYLE, GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, GetWindowLongW,
    IDC_ARROW, KillTimer, LoadCursorW, MSG, PostMessageW, PostQuitMessage, RegisterClassW, SW_SHOW,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetTimer, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, TranslateMessage, WM_CLOSE, WM_DESTROY, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_PAINT,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WNDCLASSW, WS_CAPTION,
    WS_MINIMIZEBOX, WS_SYSMENU, WS_VISIBLE,
};

use crate::virtio::gui_bridge::{
    InputTarget, SharedFrame, SharedInput, TargetedEvent, vk_to_evdev,
};
use crate::virtio::input::{
    ABS_X, ABS_Y, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EV_ABS, EV_KEY, EV_REL, EV_SYN, InputEvent,
    REL_WHEEL, SYN_REPORT, abs_from_window,
};

const REPAINT_TIMER_ID: usize = 1;
/// 重繪輪詢間隔（ms）：~60fps 上限，實際只在 framebuffer 有新 generation 時真的重畫。
const REPAINT_TIMER_MS: u32 = 16;

/// 視窗執行緒與 VM 端共用的狀態（存入 GWLP_USERDATA）。
struct WindowState {
    frame: SharedFrame,
    input: SharedInput,
    width: u32,
    height: u32,
    /// 使用者關視窗時置 true（VM loop 輪詢以收掉 app）。
    closed_by_user: Arc<AtomicBool>,
    /// 最近一次 blit 的 framebuffer generation（省下無變化的重繪）。
    last_gen: u64,
    /// 供 WM_PAINT blit 的 BGRA 快照（避免在 paint 內鎖 SharedFrame）。
    snapshot: Vec<u8>,
    snap_w: u32,
    snap_h: u32,
}

/// VM 端持有的視窗把手。
pub struct GuiHandle {
    closed_by_user: Arc<AtomicBool>,
    hwnd: Arc<AtomicIsize>,
    thread: Option<JoinHandle<()>>,
}

impl GuiHandle {
    /// 使用者是否已關視窗（VM loop 據此結束、收掉 app）。
    pub fn closed_by_user(&self) -> bool {
        self.closed_by_user.load(Ordering::Relaxed)
    }

    /// 從 VM 端請求關閉視窗（guest 先結束時呼叫）。PostMessage 是執行緒安全的。
    pub fn request_close(&self) {
        let h = self.hwnd.load(Ordering::Acquire);
        if h != 0 {
            unsafe {
                PostMessageW(h as HWND, WM_CLOSE, 0, 0);
            }
        }
    }

    /// 等視窗執行緒收尾（app 結束時呼叫）。
    pub fn join(mut self) {
        self.request_close();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// 啟動 GUI 視窗執行緒。`width`/`height` 為顯示（= guest scanout）尺寸。
pub fn spawn(
    title: &str,
    frame: SharedFrame,
    input: SharedInput,
    width: u32,
    height: u32,
) -> GuiHandle {
    let closed_by_user = Arc::new(AtomicBool::new(false));
    let hwnd = Arc::new(AtomicIsize::new(0));
    let title_w = wide(title);
    let (closed_c, hwnd_c) = (closed_by_user.clone(), hwnd.clone());
    let thread = std::thread::Builder::new()
        .name("chefer-gui".into())
        .spawn(move || {
            window_thread(&title_w, frame, input, width, height, closed_c, hwnd_c);
        })
        .expect("failed to spawn GUI window thread");
    GuiHandle {
        closed_by_user,
        hwnd,
        thread: Some(thread),
    }
}

fn window_thread(
    title_w: &[u16],
    frame: SharedFrame,
    input: SharedInput,
    width: u32,
    height: u32,
    closed_by_user: Arc<AtomicBool>,
    hwnd_pub: Arc<AtomicIsize>,
) {
    unsafe {
        let hinstance = GetModuleHandleW(std::ptr::null());
        let class_name = wide("CheferGuiWindow");
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        RegisterClassW(&wc);

        // 初始 client 區 = 傳入尺寸；首個 frame 後依 guest 實際 scanout 解析度
        // resize-to-content（見 WM_TIMER），以 1:1 顯示不失真。
        let style = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_VISIBLE;
        let mut rect = windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        AdjustWindowRect(&mut rect, style, 0);
        let win_w = rect.right - rect.left;
        let win_h = rect.bottom - rect.top;

        let state = Box::new(WindowState {
            frame,
            input,
            width,
            height,
            closed_by_user,
            last_gen: 0,
            snapshot: Vec::new(),
            snap_w: 0,
            snap_h: 0,
        });
        let state_ptr = Box::into_raw(state);

        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title_w.as_ptr(),
            style,
            // CW_USEDEFAULT
            0x8000_0000u32 as i32,
            0x8000_0000u32 as i32,
            win_w,
            win_h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            state_ptr.cast(),
        );
        if hwnd.is_null() {
            // 建視窗失敗：釋放 state、回報「已關」讓 VM 端不要卡等。
            drop(Box::from_raw(state_ptr));
            hwnd_pub.store(0, Ordering::Release);
            eprintln!("[whp-gui] failed to create the display window");
            return;
        }
        hwnd_pub.store(hwnd as isize, Ordering::Release);
        ShowWindow(hwnd, SW_SHOW);
        SetTimer(hwnd, REPAINT_TIMER_ID, REPAINT_TIMER_MS, None);

        let mut msg: MSG = std::mem::zeroed();
        loop {
            let ret = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            if ret <= 0 {
                break; // 0 = WM_QUIT，-1 = 錯誤
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        hwnd_pub.store(0, Ordering::Release);
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // WM_NCCREATE：把 CreateWindowExW 傳入的 state 指標放進 GWLP_USERDATA。
    if msg == WM_NCCREATE {
        let cs = unsafe { &*(lparam as *const CREATESTRUCTW) };
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize) };
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowState;
    if state_ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let state = unsafe { &mut *state_ptr };

    match msg {
        WM_TIMER => {
            // 有新 framebuffer 才觸發重繪。
            if let Some((px, w, h, generation)) = state.frame.snapshot_if_newer(state.last_gen) {
                state.snapshot = px;
                state.snap_w = w;
                state.snap_h = h;
                state.last_gen = generation;
                // guest 選定的 scanout 解析度可能 ≠ 初始視窗大小（cage/wlroots 挑的模式）。
                // 視窗 client 區跟著調整為 guest 實際解析度 → 1:1 blit、不放大失真；
                // 同時讓 abs 座標映射（用 state.width/height）與可見畫面一致。
                if (w, h) != (state.width, state.height) && w > 0 && h > 0 {
                    state.width = w;
                    state.height = h;
                    unsafe { resize_client(hwnd, w, h) };
                }
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
            0
        }
        WM_PAINT => {
            unsafe { paint(hwnd, state) };
            0
        }
        WM_MOUSEMOVE => {
            let (x, y) = (loword(lparam), hiword(lparam));
            let ax = abs_from_window(x, state.width);
            let ay = abs_from_window(y, state.height);
            state.input.push_batch(&targeted_report(
                InputTarget::Tablet,
                &[ev(EV_ABS, ABS_X, ax), ev(EV_ABS, ABS_Y, ay)],
            ));
            0
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP => {
            let (btn, val) = match msg {
                WM_LBUTTONDOWN => (BTN_LEFT, 1),
                WM_LBUTTONUP => (BTN_LEFT, 0),
                WM_RBUTTONDOWN => (BTN_RIGHT, 1),
                WM_RBUTTONUP => (BTN_RIGHT, 0),
                WM_MBUTTONDOWN => (BTN_MIDDLE, 1),
                _ => (BTN_MIDDLE, 0),
            };
            state.input.push_batch(&targeted_report(
                InputTarget::Tablet,
                &[ev(EV_KEY, btn, val)],
            ));
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam >> 16) & 0xFFFF) as i16 as i32 / 120;
            if delta != 0 {
                state.input.push_batch(&targeted_report(
                    InputTarget::Tablet,
                    &[ev(EV_REL, REL_WHEEL, delta as u32)],
                ));
            }
            0
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let vk = (wparam & 0xFFFF) as u16;
            if let Some(code) = vk_to_evdev(vk) {
                let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
                // lparam bit30 = 前一鍵狀態；按住重複時送 evdev repeat(2)。
                let repeat = down && (lparam & (1 << 30)) != 0;
                let val = if !down {
                    0
                } else if repeat {
                    2
                } else {
                    1
                };
                state.input.push_batch(&targeted_report(
                    InputTarget::Keyboard,
                    &[ev(EV_KEY, code, val)],
                ));
            }
            // 不吞掉系統鍵（Alt+F4 等仍走預設 → 關窗）。
            if msg == WM_SYSKEYDOWN || msg == WM_SYSKEYUP {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            } else {
                0
            }
        }
        WM_CLOSE => {
            state.closed_by_user.store(true, Ordering::Relaxed);
            unsafe {
                KillTimer(hwnd, REPAINT_TIMER_ID);
                DestroyWindow(hwnd);
            }
            0
        }
        WM_DESTROY => {
            // 回收 state Box、結束訊息迴圈。
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(state_ptr));
                PostQuitMessage(0);
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 調整視窗使 client 區為 `w`×`h`（依目前 window style 換算外框；不移動、不改 Z 序）。
unsafe fn resize_client(hwnd: HWND, w: u32, h: u32) {
    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32;
    let mut rect = windows_sys::Win32::Foundation::RECT {
        left: 0,
        top: 0,
        right: w as i32,
        bottom: h as i32,
    };
    unsafe { AdjustWindowRect(&mut rect, style, 0) };
    unsafe {
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// WM_PAINT：把 state.snapshot（BGRA，top-down）blit 到 client 區。
unsafe fn paint(hwnd: HWND, state: &WindowState) {
    let mut ps: PAINTSTRUCT = unsafe { std::mem::zeroed() };
    let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
    if !hdc.is_null() && !state.snapshot.is_empty() && state.snap_w > 0 && state.snap_h > 0 {
        // 負 biHeight = top-down DIB（我們的資料首列在頂端）。
        let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = state.snap_w as i32;
        bmi.bmiHeader.biHeight = -(state.snap_h as i32);
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;
        unsafe {
            StretchDIBits(
                hdc,
                0,
                0,
                state.width as i32,
                state.height as i32,
                0,
                0,
                state.snap_w as i32,
                state.snap_h as i32,
                state.snapshot.as_ptr().cast(),
                &bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
    }
    unsafe { EndPaint(hwnd, &ps) };
}

/// 一筆 evdev 事件。
fn ev(ev_type: u16, code: u16, value: u32) -> InputEvent {
    InputEvent {
        ev_type,
        code,
        value,
    }
}

/// 把一組事件包成 TargetedEvent 並補 SYN（一個輸入報告的邊界）。
fn targeted_report(target: InputTarget, evs: &[InputEvent]) -> Vec<TargetedEvent> {
    let mut out: Vec<TargetedEvent> = evs
        .iter()
        .map(|&event| TargetedEvent { target, event })
        .collect();
    out.push(TargetedEvent {
        target,
        event: ev(EV_SYN, SYN_REPORT, 0),
    });
    out
}

fn loword(l: LPARAM) -> i32 {
    (l & 0xFFFF) as i16 as i32
}
fn hiword(l: LPARAM) -> i32 {
    ((l >> 16) & 0xFFFF) as i16 as i32
}

/// UTF-8 → null-terminated UTF-16（Win32 W API 用）。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 本機驗證（無 WHP）：開一個 640×480 視窗，畫動畫測試圖、印輸入事件，直到關窗。
/// `chefer-whp-helper --gui-selftest`。
pub fn gui_selftest() -> Result<(), String> {
    let frame = SharedFrame::new();
    let input = SharedInput::new();
    let (w, h) = (640u32, 480u32);
    let handle = spawn("Chefer GUI self-test", frame.clone(), input.clone(), w, h);
    eprintln!(
        "[whp-gui] self-test window open; move the mouse / press keys, close the window to end."
    );

    let mut t: u32 = 0;
    let mut buf = vec![0u8; (w * h * 4) as usize];
    while !handle.closed_by_user() {
        // 動畫：垂直彩色帶隨 t 平移（BGRA）。
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let bar = ((x + t) / 40) % 3;
                let (b, g, r) = match bar {
                    0 => (0u8, 0, 255),
                    1 => (0, 255, 0),
                    _ => (255, 0, 0),
                };
                buf[i] = b;
                buf[i + 1] = g;
                buf[i + 2] = r;
                buf[i + 3] = 0xFF;
            }
        }
        // 已是 BGRA：用 B8G8R8A8 格式（=1）走 identity。
        frame.update(&buf, w, h, 1);
        for e in input.drain() {
            eprintln!("[whp-gui] input {:?} {:?}", e.target, e.event);
        }
        t = t.wrapping_add(2);
        std::thread::sleep(std::time::Duration::from_millis(33));
    }
    eprintln!("[whp-gui] window closed by user; ending self-test.");
    handle.join();
    Ok(())
}
