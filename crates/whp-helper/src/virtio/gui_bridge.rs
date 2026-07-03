//! GUI 顯示/輸入的跨執行緒橋接與純轉換邏輯（DESIGN §6「WHP GUI」M8-c）。
//!
//! WHP 的 VM run loop 與 Win32 視窗執行緒各跑一條：
//! - **顯示**：VM loop 處理 virtio-gpu control queue（`RESOURCE_FLUSH` 時把 scanout
//!   像素轉成 BGRA 存進 [`SharedFrame`]）；視窗執行緒讀 [`SharedFrame`] 以 GDI blit。
//! - **輸入**：視窗執行緒把 Win32 鍵鼠訊息轉成 evdev 事件排進 [`SharedInput`]；VM loop
//!   把它們灌進 virtio-input 的 eventq 送給 guest。
//!
//! 只共享「一塊 framebuffer 拷貝」與「一條事件佇列」（非整個裝置），把鎖粒度壓到最小，
//! 避免視窗執行緒與每次 MMIO 存取互卡。本模組全跨平台、可單元測試；真正的 Win32 呼叫
//! 在 `gui_window`（`#[cfg(windows)]`）。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::input::InputEvent;

/// 視窗執行緒待送給 VM 的事件上限（防呆：視窗爆量時丟最舊的，不無限長）。
const MAX_QUEUED_EVENTS: usize = 4096;

/// VM loop 寫、視窗執行緒讀的 framebuffer 拷貝（BGRA，Win32 DIB 直接可用）。
#[derive(Default)]
struct FrameInner {
    width: u32,
    height: u32,
    /// BGRA8888，長度 = width*height*4；尚未有畫面時為空。
    bgra: Vec<u8>,
    /// 每次更新 +1；視窗執行緒比對以決定是否需重繪。
    generation: u64,
}

/// 顯示用共享 framebuffer 把手（可 clone，兩執行緒各持一份）。
#[derive(Clone, Default)]
pub struct SharedFrame(Arc<Mutex<FrameInner>>);

impl SharedFrame {
    pub fn new() -> Self {
        Self::default()
    }

    /// VM loop：以 virtio-gpu scanout 更新畫面。`format` 為 virtio-gpu resource 格式碼，
    /// `src` 為 guest 端線性像素（stride = width*4）。轉成 BGRA 存起、generation +1。
    pub fn update(&self, src: &[u8], width: u32, height: u32, format: u32) {
        let mut inner = self.0.lock().unwrap();
        let need = width as usize * height as usize * 4;
        if src.len() < need {
            return; // 尺寸對不上（不完整的一幀）：略過，不要 panic
        }
        if inner.bgra.len() != need {
            inner.bgra = vec![0u8; need];
        }
        to_bgra(src, format, &mut inner.bgra);
        inner.width = width;
        inner.height = height;
        inner.generation = inner.generation.wrapping_add(1);
    }

    /// 視窗執行緒：若 generation 較 `last_seen` 新，回傳 (bgra 拷貝, w, h, 新 generation)。
    /// 無新畫面回 None（免無謂重繪）。
    pub fn snapshot_if_newer(&self, last_seen: u64) -> Option<(Vec<u8>, u32, u32, u64)> {
        let inner = self.0.lock().unwrap();
        if inner.generation == last_seen || inner.bgra.is_empty() {
            return None;
        }
        Some((
            inner.bgra.clone(),
            inner.width,
            inner.height,
            inner.generation,
        ))
    }
}

/// 視窗執行緒寫、VM loop 讀的 evdev 事件佇列（含目標裝置標記）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputTarget {
    Keyboard,
    Tablet,
}

/// 帶目標裝置的一筆事件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetedEvent {
    pub target: InputTarget,
    pub event: InputEvent,
}

/// 輸入事件共享佇列（可 clone）。
#[derive(Clone, Default)]
pub struct SharedInput(Arc<Mutex<VecDeque<TargetedEvent>>>);

impl SharedInput {
    pub fn new() -> Self {
        Self::default()
    }

    /// 視窗執行緒：排入一組事件（呼叫端已含 SYN）。滿了丟最舊的。
    pub fn push_batch(&self, evs: &[TargetedEvent]) {
        let mut q = self.0.lock().unwrap();
        for &e in evs {
            if q.len() >= MAX_QUEUED_EVENTS {
                q.pop_front();
            }
            q.push_back(e);
        }
    }

    /// VM loop：取出目前累積的所有事件（清空佇列）。
    pub fn drain(&self) -> Vec<TargetedEvent> {
        let mut q = self.0.lock().unwrap();
        q.drain(..).collect()
    }

    /// 是否有待處理事件（VM loop 快速檢查，免每輪都上鎖 drain）。
    pub fn has_pending(&self) -> bool {
        !self.0.lock().unwrap().is_empty()
    }
}

// ── 像素格式轉換（virtio-gpu scanout → Win32 DIB 的 BGRA）──

// virtio-gpu formats（virtio_gpu_formats，spec §5.7.2）。
const B8G8R8A8_UNORM: u32 = 1;
const B8G8R8X8_UNORM: u32 = 2;
const A8R8G8B8_UNORM: u32 = 3;
const X8R8G8B8_UNORM: u32 = 4;
const R8G8B8A8_UNORM: u32 = 67;
const X8B8G8R8_UNORM: u32 = 68;
const A8B8G8R8_UNORM: u32 = 121;
const R8G8B8X8_UNORM: u32 = 134;

/// 把 virtio-gpu 線性像素（4bpp，`format` 決定通道順序）轉成 Win32 32bpp DIB 用的
/// BGRA（byte 序 B,G,R,A；BI_RGB 32bpp 即此序）。`dst` 長度須 == `src` 可用長度。
pub fn to_bgra(src: &[u8], format: u32, dst: &mut [u8]) {
    let n = dst.len().min(src.len()) / 4 * 4;
    // (b_idx, g_idx, r_idx) = 來源 byte 內 B/G/R 的位置；alpha 一律填 0xFF（忽略來源 X/A）。
    let (bi, gi, ri) = match format {
        B8G8R8A8_UNORM | B8G8R8X8_UNORM => (0, 1, 2),
        A8R8G8B8_UNORM | X8R8G8B8_UNORM => (3, 2, 1), // 記憶體序 A,R,G,B（大端命名）→ B@3,G@2,R@1
        R8G8B8A8_UNORM | R8G8B8X8_UNORM => (2, 1, 0),
        X8B8G8R8_UNORM | A8B8G8R8_UNORM => (1, 2, 3), // 記憶體序 X/A,B,G,R → B@1,G@2,R@3
        // 未知格式：原樣拷貝（多數 host 端 fourcc 已是 BGRA 序）。
        _ => {
            dst[..n].copy_from_slice(&src[..n]);
            return;
        }
    };
    for px in 0..n / 4 {
        let s = px * 4;
        dst[s] = src[s + bi];
        dst[s + 1] = src[s + gi];
        dst[s + 2] = src[s + ri];
        dst[s + 3] = 0xFF;
    }
}

// ── Win32 virtual-key → Linux evdev keycode ──

/// Win32 VK 碼轉 Linux evdev keycode（linux/input-event-codes.h）。無對應回 None。
/// 涵蓋字母/數字/常用符號/修飾鍵/方向/功能鍵/編輯鍵——kiosk GUI app 的常用集。
pub fn vk_to_evdev(vk: u16) -> Option<u16> {
    // KEY_* 常數（evdev）。
    let k = match vk {
        0x08 => 14,  // BACKSPACE
        0x09 => 15,  // TAB
        0x0D => 28,  // ENTER
        0x10 => 42,  // SHIFT → LEFTSHIFT
        0x11 => 29,  // CONTROL → LEFTCTRL
        0x12 => 56,  // ALT(MENU) → LEFTALT
        0x14 => 58,  // CAPSLOCK
        0x1B => 1,   // ESC
        0x20 => 57,  // SPACE
        0x21 => 104, // PAGEUP
        0x22 => 109, // PAGEDOWN
        0x23 => 107, // END
        0x24 => 102, // HOME
        0x25 => 105, // LEFT
        0x26 => 103, // UP
        0x27 => 106, // RIGHT
        0x28 => 108, // DOWN
        0x2D => 110, // INSERT
        0x2E => 111, // DELETE
        // 數字列 0..9（VK '0'..'9' = 0x30..0x39）
        0x30 => 11,
        0x31..=0x39 => vk - 0x31 + 2, // '1'→KEY_1(2) .. '9'→KEY_9(10)
        // 字母 A..Z（VK 0x41..0x5A）→ evdev QWERTY 掃描碼
        0x41 => 30,
        0x42 => 48,
        0x43 => 46,
        0x44 => 32,
        0x45 => 18,
        0x46 => 33,
        0x47 => 34,
        0x48 => 35,
        0x49 => 23,
        0x4A => 36,
        0x4B => 37,
        0x4C => 38,
        0x4D => 50,
        0x4E => 49,
        0x4F => 24,
        0x50 => 25,
        0x51 => 16,
        0x52 => 19,
        0x53 => 31,
        0x54 => 20,
        0x55 => 22,
        0x56 => 47,
        0x57 => 17,
        0x58 => 45,
        0x59 => 21,
        0x5A => 44,
        // 功能鍵 F1..F12（VK 0x70..0x7B）
        0x70..=0x79 => vk - 0x70 + 59, // F1→59 .. F10→68
        0x7A => 87,                    // F11
        0x7B => 88,                    // F12
        // 常用符號（OEM，US 佈局）
        0xBA => 39, // ; :  → SEMICOLON
        0xBB => 13, // = +  → EQUAL
        0xBC => 51, // , <  → COMMA
        0xBD => 12, // - _  → MINUS
        0xBE => 52, // . >  → DOT
        0xBF => 53, // / ?  → SLASH
        0xC0 => 41, // ` ~  → GRAVE
        0xDB => 26, // [ {  → LEFTBRACE
        0xDC => 43, // \ |  → BACKSLASH
        0xDD => 27, // ] }  → RIGHTBRACE
        0xDE => 40, // ' "  → APOSTROPHE
        _ => return None,
    };
    Some(k)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::input::{ABS_X, EV_ABS, EV_SYN};

    #[test]
    fn bgra_identity_and_swaps() {
        // 一像素，來源記憶體序視格式而定；期望輸出 BGRA = B,G,R,0xFF。
        // B8G8R8A8：來源已是 B,G,R,A
        let mut dst = [0u8; 4];
        to_bgra(&[0x11, 0x22, 0x33, 0x44], B8G8R8A8_UNORM, &mut dst);
        assert_eq!(dst, [0x11, 0x22, 0x33, 0xFF]);
        // R8G8B8A8：來源 R,G,B,A → 輸出 B,G,R = 0x33,0x22,0x11
        to_bgra(&[0x11, 0x22, 0x33, 0x44], R8G8B8A8_UNORM, &mut dst);
        assert_eq!(dst, [0x33, 0x22, 0x11, 0xFF]);
        // A8R8G8B8：來源 A,R,G,B → B@3,G@2,R@1
        to_bgra(&[0x44, 0x11, 0x22, 0x33], A8R8G8B8_UNORM, &mut dst);
        assert_eq!(dst, [0x33, 0x22, 0x11, 0xFF]);
        // X8B8G8R8：來源 X,B,G,R → B@1,G@2,R@3
        to_bgra(&[0x44, 0x11, 0x22, 0x33], X8B8G8R8_UNORM, &mut dst);
        assert_eq!(dst, [0x11, 0x22, 0x33, 0xFF]);
    }

    #[test]
    fn shared_frame_generation_and_snapshot() {
        let f = SharedFrame::new();
        assert!(f.snapshot_if_newer(0).is_none()); // 尚無畫面
        let src = vec![0x10u8; 2 * 2 * 4];
        f.update(&src, 2, 2, B8G8R8A8_UNORM);
        let (px, w, h, generation) = f.snapshot_if_newer(0).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(px.len(), 16);
        assert_eq!(px[3], 0xFF); // alpha 補滿
        assert!(f.snapshot_if_newer(generation).is_none()); // 同 generation 不重繪
        f.update(&src, 2, 2, B8G8R8A8_UNORM);
        assert!(f.snapshot_if_newer(generation).is_some()); // 更新後有新 generation
    }

    #[test]
    fn shared_frame_ignores_incomplete_frame() {
        let f = SharedFrame::new();
        f.update(&[0u8; 4], 2, 2, B8G8R8A8_UNORM); // 只有 1 像素，需 4 像素
        assert!(f.snapshot_if_newer(0).is_none());
    }

    #[test]
    fn shared_input_fifo_and_drain() {
        let q = SharedInput::new();
        assert!(!q.has_pending());
        let ev = TargetedEvent {
            target: InputTarget::Tablet,
            event: InputEvent {
                ev_type: EV_ABS,
                code: ABS_X,
                value: 123,
            },
        };
        let syn = TargetedEvent {
            target: InputTarget::Tablet,
            event: InputEvent {
                ev_type: EV_SYN,
                code: 0,
                value: 0,
            },
        };
        q.push_batch(&[ev, syn]);
        assert!(q.has_pending());
        let got = q.drain();
        assert_eq!(got, vec![ev, syn]);
        assert!(!q.has_pending());
        assert!(q.drain().is_empty());
    }

    #[test]
    fn shared_input_caps_backlog() {
        let q = SharedInput::new();
        let ev = TargetedEvent {
            target: InputTarget::Keyboard,
            event: InputEvent {
                ev_type: EV_SYN,
                code: 0,
                value: 0,
            },
        };
        let batch: Vec<_> = std::iter::repeat_n(ev, MAX_QUEUED_EVENTS + 500).collect();
        q.push_batch(&batch);
        assert_eq!(q.drain().len(), MAX_QUEUED_EVENTS);
    }

    #[test]
    fn vk_mapping_covers_common_keys() {
        assert_eq!(vk_to_evdev(0x41), Some(30)); // A
        assert_eq!(vk_to_evdev(0x5A), Some(44)); // Z
        assert_eq!(vk_to_evdev(0x30), Some(11)); // 0
        assert_eq!(vk_to_evdev(0x31), Some(2)); // 1
        assert_eq!(vk_to_evdev(0x39), Some(10)); // 9
        assert_eq!(vk_to_evdev(0x0D), Some(28)); // Enter
        assert_eq!(vk_to_evdev(0x1B), Some(1)); // Esc
        assert_eq!(vk_to_evdev(0x20), Some(57)); // Space
        assert_eq!(vk_to_evdev(0x25), Some(105)); // Left
        assert_eq!(vk_to_evdev(0x70), Some(59)); // F1
        assert_eq!(vk_to_evdev(0x7B), Some(88)); // F12
        assert_eq!(vk_to_evdev(0x00), None); // 未定義
    }
}
