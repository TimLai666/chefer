//! virtio-input 裝置邏輯（virtio spec §5.8，跨平台純邏輯）。
//!
//! WHP GUI（DESIGN §6 M8-b）：host 視窗（M8-c）把 Win32 鍵鼠訊息轉成 evdev 事件、
//! 經 eventq 送進 guest（cage/libinput 消費）。做兩種裝置：
//! - **keyboard**：EV_KEY（全鍵盤範圍）。
//! - **tablet**：EV_ABS（ABS_X/ABS_Y 絕對座標，避免滑鼠捕捉/相對座標飄移）+
//!   EV_KEY（BTN_LEFT/RIGHT/MIDDLE）+ EV_REL（REL_WHEEL 滾輪）。
//!
//! config space 是 select/subsel 查詢式（driver 寫 select/subsel，再讀 payload）；
//! eventq（q0）由 driver 供 8-byte 可寫 buffer，host 有事件時填入（比照 net rx）。
//! statusq（q1）為 driver→device（LED 之類），一律消化即可。

use super::GuestMemory;
use super::queue::DescChain;

// ── config select（virtio_input_config_select）──
const VIRTIO_INPUT_CFG_UNSET: u8 = 0x00;
const VIRTIO_INPUT_CFG_ID_NAME: u8 = 0x01;
const VIRTIO_INPUT_CFG_ID_SERIAL: u8 = 0x02;
const VIRTIO_INPUT_CFG_ID_DEVIDS: u8 = 0x03;
const VIRTIO_INPUT_CFG_PROP_BITS: u8 = 0x10;
const VIRTIO_INPUT_CFG_EV_BITS: u8 = 0x11;
const VIRTIO_INPUT_CFG_ABS_INFO: u8 = 0x12;

// ── evdev 事件型別/代碼（linux/input-event-codes.h 子集）──
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const SYN_REPORT: u16 = 0;
pub const REL_WHEEL: u16 = 0x08;
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;

/// 絕對座標軸的量化範圍（host 視窗座標映射到 0..=ABS_MAX）。
pub const ABS_MAX: u32 = 32767;

/// 一筆 evdev 事件（virtio_input_event：type/code/value，小端 8 bytes）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputEvent {
    pub ev_type: u16,
    pub code: u16,
    pub value: u32,
}

impl InputEvent {
    pub fn to_bytes(self) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[..2].copy_from_slice(&self.ev_type.to_le_bytes());
        b[2..4].copy_from_slice(&self.code.to_le_bytes());
        b[4..8].copy_from_slice(&self.value.to_le_bytes());
        b
    }

    pub fn syn() -> Self {
        Self {
            ev_type: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        }
    }
}

/// 裝置種類（決定 config 回報的名稱與 ev bits）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Keyboard,
    Tablet,
}

/// virtio-input 裝置：config 查詢狀態 + 待送事件佇列。
pub struct InputDevice {
    kind: InputKind,
    select: u8,
    subsel: u8,
    /// host 產生、尚未塞進 eventq 的事件（視窗執行緒 push、VM loop drain）。
    pending: std::collections::VecDeque<InputEvent>,
}

impl InputDevice {
    pub fn new(kind: InputKind) -> Self {
        Self {
            kind,
            select: VIRTIO_INPUT_CFG_UNSET,
            subsel: 0,
            pending: std::collections::VecDeque::new(),
        }
    }

    fn name(&self) -> &'static str {
        match self.kind {
            InputKind::Keyboard => "chefer-virtio-keyboard",
            InputKind::Tablet => "chefer-virtio-tablet",
        }
    }

    /// 目前 select/subsel 對應的 config payload（不含 select/subsel/size 前 8 bytes）。
    fn payload(&self) -> Vec<u8> {
        match (self.select, self.subsel) {
            (VIRTIO_INPUT_CFG_ID_NAME, _) => self.name().as_bytes().to_vec(),
            (VIRTIO_INPUT_CFG_ID_SERIAL, _) | (VIRTIO_INPUT_CFG_PROP_BITS, _) => vec![],
            (VIRTIO_INPUT_CFG_ID_DEVIDS, _) => {
                // bustype/vendor/product/version（各 le16）：虛擬值即可。
                let mut v = Vec::with_capacity(8);
                for x in [0x06u16 /* BUS_VIRTUAL */, 0x1af4, 0x0010, 1] {
                    v.extend_from_slice(&x.to_le_bytes());
                }
                v
            }
            (VIRTIO_INPUT_CFG_EV_BITS, sub) => self.ev_bits(sub as u16),
            (VIRTIO_INPUT_CFG_ABS_INFO, axis) => {
                if self.kind == InputKind::Tablet && (axis as u16 == ABS_X || axis as u16 == ABS_Y)
                {
                    // virtio_input_absinfo：min/max/fuzz/flat/res（各 le32）。
                    let mut v = Vec::with_capacity(20);
                    for x in [0u32, ABS_MAX, 0, 0, 0] {
                        v.extend_from_slice(&x.to_le_bytes());
                    }
                    v
                } else {
                    vec![]
                }
            }
            _ => vec![],
        }
    }

    /// EV_BITS：subsel = 事件型別；回傳該型別支援代碼的 bitmap（bit=code）。
    fn ev_bits(&self, ev_type: u16) -> Vec<u8> {
        let mut bits = vec![0u8; 128]; // 覆蓋 KEY_MAX(0x2ff)/8 = 96 bytes，取整 128
        let mut any = false;
        let mut set = |code: u16| {
            bits[(code / 8) as usize] |= 1 << (code % 8);
            any = true;
        };
        match (self.kind, ev_type) {
            (InputKind::Keyboard, EV_KEY) => {
                // 一般鍵盤範圍（KEY_ESC..=KEY_MICMUTE 0x1..0x100）整段宣告。
                for code in 1u16..=0xff {
                    set(code);
                }
            }
            (InputKind::Tablet, EV_KEY) => {
                for code in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE] {
                    set(code);
                }
            }
            (InputKind::Tablet, EV_ABS) => {
                for code in [ABS_X, ABS_Y] {
                    set(code);
                }
            }
            (InputKind::Tablet, EV_REL) => set(REL_WHEEL),
            _ => {}
        }
        if !any {
            return vec![];
        }
        // 截到最後一個非零 byte（driver 以 size 判斷長度）。
        let last = bits.iter().rposition(|&b| b != 0).unwrap_or(0);
        bits.truncate(last + 1);
        bits
    }

    /// config space 讀取。layout：0=select、1=subsel、2=size、3..8 保留、8..=payload。
    pub fn config_read(&self, offset: u64, len: u8) -> u32 {
        let payload = self.payload();
        let mut cfg = vec![0u8; 8 + payload.len()];
        cfg[0] = self.select;
        cfg[1] = self.subsel;
        cfg[2] = payload.len() as u8;
        cfg[8..].copy_from_slice(&payload);
        let mut out = [0u8; 4];
        for (i, slot) in out.iter_mut().enumerate().take(len as usize) {
            *slot = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
        u32::from_le_bytes(out)
    }

    /// config space 寫入（driver 設定 select/subsel；4-byte 對齊寫入時兩者常一起來）。
    pub fn config_write(&mut self, offset: u64, len: u8, value: u32) {
        let bytes = value.to_le_bytes();
        for i in 0..(len as usize) {
            match offset as usize + i {
                0 => self.select = bytes[i],
                1 => self.subsel = bytes[i],
                _ => {}
            }
        }
    }

    /// host 視窗端排入事件（不含 SYN；`queue_report` 幫忙補）。
    pub fn push_event(&mut self, ev: InputEvent) {
        self.pending.push_back(ev);
    }

    /// 排入一組事件並補 EV_SYN/SYN_REPORT（一個輸入報告的邊界）。
    pub fn queue_report(&mut self, evs: &[InputEvent]) {
        for &e in evs {
            self.pending.push_back(e);
        }
        self.pending.push_back(InputEvent::syn());
    }

    /// 是否有待送事件。
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// 把一筆待送事件寫進 driver 提供的 eventq buffer chain（8-byte 可寫段）。
    /// 回傳寫入 bytes（None = 無待送事件，chain 應留在 avail 待下次）。
    pub fn fill_event<M: GuestMemory>(
        &mut self,
        chain: &DescChain,
        mem: &mut M,
    ) -> Result<Option<u32>, String> {
        let Some(ev) = self.pending.front().copied() else {
            return Ok(None);
        };
        let Some(&(gpa, len)) = chain.writable.first() else {
            return Err("virtio-input event chain has no writable buffer".to_string());
        };
        if len < 8 {
            return Err(format!("virtio-input event buffer len {len} < 8"));
        }
        mem.write(gpa, &ev.to_bytes())?;
        self.pending.pop_front();
        Ok(Some(8))
    }

    /// statusq（driver→device：LED 等）——讀掉即可，回 used len 0。
    pub fn process_status_chain<M: GuestMemory>(
        &mut self,
        _chain: &DescChain,
        _mem: &mut M,
    ) -> Result<u32, String> {
        Ok(0)
    }
}

/// host 視窗座標 → ABS 值（0..=ABS_MAX 線性量化；窗外夾住）。
pub fn abs_from_window(pos: i32, extent: u32) -> u32 {
    if extent <= 1 {
        return 0;
    }
    let clamped = pos.clamp(0, extent as i32 - 1) as u64;
    (clamped * u64::from(ABS_MAX) / u64::from(extent - 1)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::SliceMem;

    #[test]
    fn config_select_name_and_size() {
        let mut d = InputDevice::new(InputKind::Keyboard);
        d.config_write(
            0,
            2,
            u32::from_le_bytes([VIRTIO_INPUT_CFG_ID_NAME, 0, 0, 0]),
        );
        // size @ offset 2
        let size = d.config_read(2, 1) as usize;
        assert_eq!(size, "chefer-virtio-keyboard".len());
        // payload 從 offset 8 起
        let b = d.config_read(8, 4).to_le_bytes();
        assert_eq!(&b, b"chef");
    }

    #[test]
    fn keyboard_reports_key_bits_and_no_abs() {
        let mut d = InputDevice::new(InputKind::Keyboard);
        d.config_write(
            0,
            2,
            u32::from_le_bytes([VIRTIO_INPUT_CFG_EV_BITS, EV_KEY as u8, 0, 0]),
        );
        assert!(d.config_read(2, 1) > 0, "鍵盤要宣告 EV_KEY bits");
        d.config_write(
            0,
            2,
            u32::from_le_bytes([VIRTIO_INPUT_CFG_EV_BITS, EV_ABS as u8, 0, 0]),
        );
        assert_eq!(d.config_read(2, 1), 0, "鍵盤不得宣告 EV_ABS");
    }

    #[test]
    fn tablet_reports_abs_axes_and_absinfo() {
        let mut d = InputDevice::new(InputKind::Tablet);
        d.config_write(
            0,
            2,
            u32::from_le_bytes([VIRTIO_INPUT_CFG_EV_BITS, EV_ABS as u8, 0, 0]),
        );
        let size = d.config_read(2, 1);
        assert!(size >= 1);
        let bits = d.config_read(8, 1) as u8;
        assert_eq!(bits & 0b11, 0b11, "ABS_X/ABS_Y bits");
        // ABS_INFO for ABS_X：max = ABS_MAX
        d.config_write(
            0,
            2,
            u32::from_le_bytes([VIRTIO_INPUT_CFG_ABS_INFO, ABS_X as u8, 0, 0]),
        );
        assert_eq!(d.config_read(8 + 4, 4), ABS_MAX);
    }

    #[test]
    fn event_fill_roundtrip() {
        let mut buf = vec![0u8; 4096];
        let mut mem = SliceMem::new(0, &mut buf);
        let mut d = InputDevice::new(InputKind::Tablet);
        d.queue_report(&[
            InputEvent {
                ev_type: EV_ABS,
                code: ABS_X,
                value: 123,
            },
            InputEvent {
                ev_type: EV_ABS,
                code: ABS_Y,
                value: 456,
            },
        ]);
        assert!(d.has_pending());
        let chain = DescChain {
            head: 0,
            readable: vec![],
            writable: vec![(0x100, 8)],
        };
        // 三筆：ABS_X、ABS_Y、SYN。
        for expect in [
            (EV_ABS, ABS_X, 123u32),
            (EV_ABS, ABS_Y, 456),
            (EV_SYN, SYN_REPORT, 0),
        ] {
            assert_eq!(d.fill_event(&chain, &mut mem).unwrap(), Some(8));
            let mut b = [0u8; 8];
            mem.read(0x100, &mut b).unwrap();
            assert_eq!(u16::from_le_bytes([b[0], b[1]]), expect.0);
            assert_eq!(u16::from_le_bytes([b[2], b[3]]), expect.1);
            assert_eq!(u32::from_le_bytes([b[4], b[5], b[6], b[7]]), expect.2);
        }
        assert_eq!(d.fill_event(&chain, &mut mem).unwrap(), None);
    }

    #[test]
    fn abs_quantization_clamps_and_scales() {
        assert_eq!(abs_from_window(0, 1280), 0);
        assert_eq!(abs_from_window(1279, 1280), ABS_MAX);
        assert_eq!(abs_from_window(-5, 1280), 0);
        assert_eq!(abs_from_window(99999, 1280), ABS_MAX);
        let mid = abs_from_window(640, 1280);
        assert!((16000..=17000).contains(&mid));
    }
}
