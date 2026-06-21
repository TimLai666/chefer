//! 8259A PIC (Programmable Interrupt Controller) 基本模擬。
//!
//! 只實作 Linux early boot 所需的最小子集：ICW1-ICW4 初始化序列、
//! IMR (interrupt mask) 讀寫、EOI、讀 IRR/ISR。
//! 不模擬實際 interrupt delivery——WHP 不走外部 PIC 路徑（用 local APIC），
//! 但 Linux 會在 early boot 對 legacy PIC 做初始化和 mask-all，如果讀寫
//! 全部吃 0xFF 會讓 init 過程卡住或 panic。

/// Master PIC I/O port range
pub const PIC1_CMD: u16 = 0x20;
pub const PIC1_DATA: u16 = 0x21;
/// Slave PIC I/O port range
pub const PIC2_CMD: u16 = 0xA0;
pub const PIC2_DATA: u16 = 0xA1;

pub struct Pic {
    /// ICW 初始化階段：0=正常, 1..=4 表示等待 ICW2/3/4
    init_step: u8,
    /// Interrupt Mask Register（IMR）
    imr: u8,
    /// In-Service Register（ISR）
    isr: u8,
    /// Interrupt Request Register（IRR）
    irr: u8,
    /// ICW4 需求旗標
    icw4_needed: bool,
    /// 讀 IRR (false) 或 ISR (true)
    read_isr: bool,
    /// 基底向量（ICW2 設定）
    vector_offset: u8,
}

impl Pic {
    pub fn new() -> Self {
        Self {
            init_step: 0,
            imr: 0xFF, // 預設 mask all
            isr: 0,
            irr: 0,
            icw4_needed: false,
            read_isr: false,
            vector_offset: 0,
        }
    }

    pub fn read_cmd(&self) -> u8 {
        if self.read_isr {
            self.isr
        } else {
            self.irr
        }
    }

    pub fn read_data(&self) -> u8 {
        self.imr
    }

    pub fn write_cmd(&mut self, val: u8) {
        if val & 0x10 != 0 {
            // ICW1：開始初始化序列
            self.init_step = 1;
            self.icw4_needed = val & 0x01 != 0;
            self.isr = 0;
            self.irr = 0;
            self.imr = 0;
            self.read_isr = false;
        } else if val & 0x08 != 0 {
            // OCW3：讀 IRR/ISR 選擇
            if val & 0x02 != 0 {
                self.read_isr = val & 0x01 != 0;
            }
        } else if val == 0x20 || (val & 0xE0) == 0x60 {
            // OCW2：EOI（non-specific 或 specific）
            if val == 0x20 {
                // non-specific EOI：清最高優先的 ISR bit
                for i in 0..8 {
                    if self.isr & (1 << i) != 0 {
                        self.isr &= !(1 << i);
                        break;
                    }
                }
            } else {
                // specific EOI：清指定的 bit
                let irq = val & 0x07;
                self.isr &= !(1 << irq);
            }
        }
    }

    pub fn write_data(&mut self, val: u8) {
        match self.init_step {
            1 => {
                // ICW2：向量基底
                self.vector_offset = val;
                self.init_step = 2;
            }
            2 => {
                // ICW3：cascade 配置（直接吃掉）
                self.init_step = if self.icw4_needed { 3 } else { 0 };
            }
            3 => {
                // ICW4：模式（直接吃掉，我們不在意 8086 mode 等細節）
                self.init_step = 0;
            }
            _ => {
                // OCW1：寫 IMR
                self.imr = val;
            }
        }
    }
}

/// PIT (8254 Programmable Interval Timer) 基本 stub。
/// Linux 在 early boot 會讀寫 PIT channel 0/2 做校準。
/// 不需要精確計時——只要不讓讀取卡住就好。
pub struct Pit {
    /// 各 channel 的 counter 值（只有 0 和 2 被 Linux 用到）
    counters: [u16; 3],
    /// 各 channel 的 latch 狀態
    latched: [bool; 3],
    latch_val: [u16; 3],
    /// 讀取高/低位元組交替
    read_hi: [bool; 3],
    /// 模式配置
    mode: [u8; 3],
    /// 存取模式：0=latch, 1=lo, 2=hi, 3=lo/hi
    access: [u8; 3],
    /// port 0x61 bit 5（channel 2 output）讀取計數
    port61_count: u32,
}

impl Pit {
    pub fn new() -> Self {
        Self {
            counters: [0xFFFF; 3],
            latched: [false; 3],
            latch_val: [0; 3],
            read_hi: [false; 3],
            mode: [0; 3],
            access: [3; 3], // 預設 lo/hi
            port61_count: 0,
        }
    }

    /// 讀 channel data port (0x40, 0x41, 0x42)
    pub fn read(&mut self, ch: usize) -> u8 {
        if ch >= 3 {
            return 0xFF;
        }
        let val = if self.latched[ch] {
            self.latch_val[ch]
        } else {
            self.tick(ch);
            self.counters[ch]
        };

        match self.access[ch] {
            1 => {
                // lo only
                self.latched[ch] = false;
                val as u8
            }
            2 => {
                // hi only
                self.latched[ch] = false;
                (val >> 8) as u8
            }
            _ => {
                // lo/hi alternating
                if self.read_hi[ch] {
                    self.read_hi[ch] = false;
                    self.latched[ch] = false;
                    (val >> 8) as u8
                } else {
                    self.read_hi[ch] = true;
                    val as u8
                }
            }
        }
    }

    /// 寫 channel data port
    pub fn write(&mut self, ch: usize, val: u8) {
        if ch >= 3 {
            return;
        }
        // 簡化：直接把寫入值當 counter reload
        match self.access[ch] {
            1 => self.counters[ch] = val as u16,
            2 => self.counters[ch] = (val as u16) << 8,
            _ => {
                if self.read_hi[ch] {
                    self.counters[ch] = (self.counters[ch] & 0xFF) | ((val as u16) << 8);
                    self.read_hi[ch] = false;
                } else {
                    self.counters[ch] = (self.counters[ch] & 0xFF00) | val as u16;
                    self.read_hi[ch] = true;
                }
            }
        }
    }

    /// 寫 mode/command register (0x43)
    pub fn write_cmd(&mut self, val: u8) {
        let ch = ((val >> 6) & 0x03) as usize;
        if ch >= 3 {
            return; // readback command，忽略
        }
        let access = (val >> 4) & 0x03;
        if access == 0 {
            // latch command
            if !self.latched[ch] {
                self.latch_val[ch] = self.counters[ch];
                self.latched[ch] = true;
            }
            return;
        }
        self.access[ch] = access;
        self.mode[ch] = (val >> 1) & 0x07;
        self.read_hi[ch] = false;
        self.latched[ch] = false;
    }

    /// 讀 port 0x61（System Control Port B）。
    /// bit 4: refresh request（交替）。
    /// bit 5: PIT channel 2 output——每 100 次讀取翻轉一次，
    /// 讓 TSC calibration 和 timer check 能看到輸出變化。
    pub fn tick_port61(&mut self) -> u8 {
        self.port61_count = self.port61_count.wrapping_add(1);
        let bit4 = if self.port61_count & 1 != 0 { 0x10 } else { 0 };
        let bit5 = if self.port61_count > 100 { 0x20 } else { 0 };
        bit4 | bit5
    }

    fn tick(&mut self, ch: usize) {
        // 模擬 counter 遞減——每次讀取 decrement，讓 calibration loop 不要卡住
        self.counters[ch] = self.counters[ch].wrapping_sub(100);
    }
}

pub const PIT_CH0: u16 = 0x40;
pub const PIT_CMD: u16 = 0x43;

pub fn pit_handles(port: u16) -> bool {
    (PIT_CH0..=PIT_CMD).contains(&port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pic_init_sequence() {
        let mut pic = Pic::new();
        assert_eq!(pic.imr, 0xFF);

        // ICW1: edge trigger, ICW4 needed
        pic.write_cmd(0x11);
        // ICW2: vector offset 0x20
        pic.write_data(0x20);
        assert_eq!(pic.vector_offset, 0x20);
        // ICW3: slave at IRQ2
        pic.write_data(0x04);
        // ICW4: 8086 mode
        pic.write_data(0x01);
        // init_step 回到 0，可寫 IMR
        assert_eq!(pic.init_step, 0);

        // 寫 IMR
        pic.write_data(0xFB); // mask all except IRQ2
        assert_eq!(pic.read_data(), 0xFB);
    }

    #[test]
    fn pic_eoi() {
        let mut pic = Pic::new();
        pic.isr = 0x05; // IRQ0 + IRQ2 in service

        // non-specific EOI：清最低 bit（IRQ0）
        pic.write_cmd(0x20);
        assert_eq!(pic.isr, 0x04);

        // specific EOI IRQ2
        pic.write_cmd(0x62);
        assert_eq!(pic.isr, 0x00);
    }

    #[test]
    fn pic_read_irr_isr() {
        let mut pic = Pic::new();
        pic.irr = 0x42;
        pic.isr = 0x81;

        // 預設讀 IRR
        assert_eq!(pic.read_cmd(), 0x42);

        // OCW3：切到讀 ISR
        pic.write_cmd(0x0B);
        assert_eq!(pic.read_cmd(), 0x81);

        // OCW3：切回讀 IRR
        pic.write_cmd(0x0A);
        assert_eq!(pic.read_cmd(), 0x42);
    }

    #[test]
    fn pit_counter_decrement() {
        let mut pit = Pit::new();
        // 預設 counter = 0xFFFF
        let lo = pit.read(0);
        let hi = pit.read(0);
        let val = lo as u16 | ((hi as u16) << 8);
        // 每次 read 會 tick(-100)，所以第一次讀取應該 < 0xFFFF
        assert!(val < 0xFFFF);
    }

    #[test]
    fn pit_latch_freezes_value() {
        let mut pit = Pit::new();
        pit.counters[0] = 0x1234;

        // latch channel 0
        pit.write_cmd(0x00);
        assert!(pit.latched[0]);

        // 讀 latched value
        let lo = pit.read(0);
        let hi = pit.read(0);
        let val = lo as u16 | ((hi as u16) << 8);
        assert_eq!(val, 0x1234);
        assert!(!pit.latched[0]); // latch consumed
    }

    #[test]
    fn pit_mode_command() {
        let mut pit = Pit::new();
        // channel 0, access lo/hi, mode 2 (rate generator)
        pit.write_cmd(0x34);
        assert_eq!(pit.access[0], 3);
        assert_eq!(pit.mode[0], 2);
    }

    #[test]
    fn pit_write_reload() {
        let mut pit = Pit::new();
        pit.write_cmd(0x34); // ch0, lo/hi, mode 2
        pit.write(0, 0x00); // lo
        pit.write(0, 0x10); // hi
        assert_eq!(pit.counters[0], 0x1000);
    }
}
