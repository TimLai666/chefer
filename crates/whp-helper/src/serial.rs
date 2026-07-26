//! 8250/16550 UART 序列埠模擬（COM1, I/O 0x3F8–0x3FF）。
//!
//! 僅實作 guest → host transmit；不支援 host → guest receive。
//! 跨平台可編譯與測試。
//!
//! **THRE 中斷是必要的，不是選配**：kernel 的 printk console 走 polled write（讀 LSR 直接
//! 塞 THR），但 Linux 的 8250 **tty** 送字路徑是 interrupt-driven——`start_tx` 開 IER bit1
//! 之後就等 THRE 中斷才續傳。少了這條線，userspace 寫進 /dev/console 的位元組一個都到不了
//! host（實機症狀：只看得到 kernel 與 init 的 kmsg 訊息，服務輸出全黑，看起來像服務沒起來）。

pub const COM1_BASE: u16 = 0x3F8;
pub const COM1_END: u16 = 0x3FF;
/// COM1 在 8259 master 上的 legacy IRQ 線。
pub const COM1_IRQ: u8 = 4;
const MAX_SERIAL_OUTPUT: usize = 16 * 1024 * 1024; // 16 MiB

/// IER bit1：THR empty 中斷致能。
const IER_THRI: u8 = 0x02;
/// IIR：無待處理中斷。
const IIR_NO_PENDING: u8 = 0x01;
/// IIR：THR empty（本模擬唯一會產生的中斷源）。
const IIR_THR_EMPTY: u8 = 0x02;

pub struct SerialPort {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlh: u8,
    /// THR 已空、尚未被 guest 讀 IIR 認掉的中斷。本模擬的 TX 即時完成，所以只要
    /// guest 開了 THRI 或剛送完一個 byte，THR 就是空的。
    thre_pending: bool,
    output: Vec<u8>,
}

impl SerialPort {
    pub fn new() -> Self {
        Self {
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            dll: 0,
            dlh: 0,
            thre_pending: false,
            output: Vec::new(),
        }
    }

    /// 現在是否該對 PIC 拉 COM1 的 IRQ（guest 開了 THRI 且有未認的 THR-empty）。
    pub fn irq_pending(&self) -> bool {
        self.ier & IER_THRI != 0 && self.thre_pending
    }

    pub fn handles(port: u16) -> bool {
        (COM1_BASE..=COM1_END).contains(&port)
    }

    pub fn read(&mut self, port: u16) -> u8 {
        match port - COM1_BASE {
            0 => {
                if self.dlab() {
                    self.dll
                } else {
                    0 // RBR: no input data
                }
            }
            1 => {
                if self.dlab() {
                    self.dlh
                } else {
                    self.ier
                }
            }
            2 => {
                // 讀 IIR = guest 認掉這次中斷（真硬體同語意）。
                if self.irq_pending() {
                    self.thre_pending = false;
                    IIR_THR_EMPTY
                } else {
                    IIR_NO_PENDING
                }
            }
            3 => self.lcr,
            4 => self.mcr,
            5 => 0x60, // LSR: THRE + TEMT (ready)
            6 => 0x00, // MSR
            7 => self.scr,
            _ => 0xFF,
        }
    }

    pub fn write(&mut self, port: u16, value: u8) {
        match port - COM1_BASE {
            0 => {
                if self.dlab() {
                    self.dll = value;
                } else {
                    if self.output.len() < MAX_SERIAL_OUTPUT {
                        self.output.push(value);
                    }
                    // TX 即時完成 → THR 立刻又是空的，該通知 guest 續傳下一個 byte。
                    self.thre_pending = true;
                }
            }
            1 => {
                if self.dlab() {
                    self.dlh = value;
                } else {
                    self.ier = value;
                    if value & IER_THRI != 0 {
                        // guest 剛開啟 TX 中斷，而 THR 本來就是空的——立刻給它第一次踢。
                        self.thre_pending = true;
                    }
                }
            }
            2 => {}
            3 => self.lcr = value,
            4 => self.mcr = value,
            7 => self.scr = value,
            _ => {}
        }
    }

    /// 取得完整輸出字串（lossy UTF-8）。
    pub fn output_str(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }

    fn dlab(&self) -> bool {
        self.lcr & 0x80 != 0
    }
}

/// CMOS (RTC) 讀取。Linux 開機時探測 RTC，回傳合理的預設值。
pub fn cmos_read(addr: u8) -> u8 {
    match addr {
        0x0A => 0x26, // Status A: 正常更新速率
        0x0B => 0x02, // Status B: 24h, BCD
        0x0C => 0x00, // Status C: 無中斷
        0x0D => 0x80, // Status D: 電池正常
        _ => 0x00,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transmit_captures_output() {
        let mut sp = SerialPort::new();
        sp.write(COM1_BASE, b'H');
        sp.write(COM1_BASE, b'i');
        assert_eq!(sp.output_str(), "Hi");
    }

    #[test]
    fn lsr_reports_tx_ready() {
        let mut sp = SerialPort::new();
        assert_eq!(sp.read(COM1_BASE + 5), 0x60);
    }

    #[test]
    fn iir_no_pending_interrupt() {
        let mut sp = SerialPort::new();
        assert_eq!(sp.read(COM1_BASE + 2), 0x01);
    }

    // 以下四項鎖住 THRE 中斷契約。Linux 的 8250 **tty** 送字是 interrupt-driven：
    // start_tx 開 IER bit1 後就等 THRE IRQ 才續傳。少了這條線，userspace 寫到
    // /dev/console 的位元組一個都出不來（kernel printk 走 polled write，不受影響），
    // 實機症狀是 guest 服務看起來沒起來——實際上只是輸出全丟。

    #[test]
    fn enabling_the_tx_interrupt_raises_thre() {
        let mut sp = SerialPort::new();
        sp.write(COM1_BASE + 1, 0x02); // IER: THRI on
        assert!(sp.irq_pending());
        assert_eq!(sp.read(COM1_BASE + 2), 0x02); // IIR: THR empty
        assert!(!sp.irq_pending(), "讀 IIR 應清掉本次中斷");
        assert_eq!(sp.read(COM1_BASE + 2), 0x01);
    }

    #[test]
    fn transmitting_re_arms_thre() {
        let mut sp = SerialPort::new();
        sp.write(COM1_BASE + 1, 0x02);
        sp.read(COM1_BASE + 2); // 清掉開啟中斷那次

        sp.write(COM1_BASE, b'X'); // TX 即時完成 → THR 又空了
        assert!(
            sp.irq_pending(),
            "送完一個 byte 要再拉一次中斷，否則續傳會停住"
        );
        assert_eq!(sp.read(COM1_BASE + 2), 0x02);
    }

    #[test]
    fn no_interrupt_while_the_guest_keeps_it_masked() {
        let mut sp = SerialPort::new();
        sp.write(COM1_BASE, b'X');
        assert!(!sp.irq_pending());
        assert_eq!(sp.read(COM1_BASE + 2), 0x01);
    }

    #[test]
    fn disabling_the_tx_interrupt_stops_it() {
        let mut sp = SerialPort::new();
        sp.write(COM1_BASE + 1, 0x02);
        sp.write(COM1_BASE, b'X');
        sp.write(COM1_BASE + 1, 0x00); // Linux 的 __stop_tx：沒東西可送就關掉
        assert!(
            !sp.irq_pending(),
            "關掉 THRI 後不得再拉中斷，否則變成中斷風暴"
        );
        assert_eq!(sp.read(COM1_BASE + 2), 0x01);
    }

    #[test]
    fn dlab_switches_to_divisor_latch() {
        let mut sp = SerialPort::new();
        // DLAB off: THR
        sp.write(COM1_BASE, b'X');
        assert_eq!(sp.output_str(), "X");

        // Set DLAB
        sp.write(COM1_BASE + 3, 0x80);
        assert!(sp.dlab());

        // Now port 0x3F8 write goes to DLL, not THR
        sp.write(COM1_BASE, 0x0C);
        assert_eq!(sp.output_str(), "X"); // no new output added
        assert_eq!(sp.read(COM1_BASE), 0x0C); // DLL readback

        // Clear DLAB
        sp.write(COM1_BASE + 3, 0x00);
        assert!(!sp.dlab());
    }

    #[test]
    fn scratch_register_roundtrip() {
        let mut sp = SerialPort::new();
        sp.write(COM1_BASE + 7, 0x42);
        assert_eq!(sp.read(COM1_BASE + 7), 0x42);
    }

    #[test]
    fn handles_range() {
        assert!(SerialPort::handles(0x3F8));
        assert!(SerialPort::handles(0x3FF));
        assert!(!SerialPort::handles(0x3F7));
        assert!(!SerialPort::handles(0x400));
    }

    #[test]
    fn output_buffer_capped() {
        let mut sp = SerialPort::new();
        sp.output = vec![0u8; MAX_SERIAL_OUTPUT];
        sp.write(COM1_BASE, b'X');
        assert_eq!(sp.output.len(), MAX_SERIAL_OUTPUT);
    }

    #[test]
    fn cmos_status_d_battery_ok() {
        assert_eq!(cmos_read(0x0D), 0x80);
    }
}
