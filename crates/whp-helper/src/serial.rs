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
            output: Vec::new(),
        }
    }

    /// 現在是否該對 PIC 拉 COM1 的 IRQ。
    ///
    /// THRE 是**準位**訊號不是邊緣：只要 guest 開著 THRI，而 THR 是空的，線就一直拉著。
    /// 本模擬的 TX 即時完成 → THR 恆空 → 條件等同「THRI 開著」。實機教訓：曾經把它做成
    /// 「讀 IIR 就清掉」的一次性旗標，結果 Linux 只要讀到一次 IIR 卻沒接著寫 THR/IER
    /// （例如那次中斷被判定為 spurious），這條線就再也拉不起來——driver 的 `ier` 快取裡
    /// THRI 仍是開的，於是 `serial8250_start_tx` 不會重寫 IER、也就永遠沒有新的 IO exit
    /// 可以觸發中斷，guest 卡在等一個不會來的 THRE 上（實機 2026-07-28：VM 開完機、服務
    /// 跑完，卻停在 `CHEFER_GUEST_EXIT` 之前不動，helper 逾時）。Linux 沒東西要送時會自己
    /// 清掉 THRI（`__stop_tx`），所以持續拉線不會變成中斷風暴——真硬體也是這個行為。
    pub fn irq_pending(&self) -> bool {
        self.ier & IER_THRI != 0
    }

    pub fn handles(port: u16) -> bool {
        (COM1_BASE..=COM1_END).contains(&port)
    }

    pub fn read(&self, port: u16) -> u8 {
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
                // IIR 照實回報目前的準位，讀取不改變狀態（見 irq_pending 的說明）。
                if self.irq_pending() {
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
                } else if self.output.len() < MAX_SERIAL_OUTPUT {
                    self.output.push(value);
                }
            }
            1 => {
                if self.dlab() {
                    self.dlh = value;
                } else {
                    self.ier = value;
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
        let sp = SerialPort::new();
        assert_eq!(sp.read(COM1_BASE + 5), 0x60);
    }

    #[test]
    fn iir_no_pending_interrupt() {
        let sp = SerialPort::new();
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
    }

    /// 回歸測試（實機 2026-07-28 卡死）：THRE 是準位不是邊緣。讀 IIR **不會**把線清掉，
    /// 否則 Linux 讀到一次 IIR 卻沒接著寫 THR/IER 時，driver 的 ier 快取裡 THRI 仍開著、
    /// 不會重寫 IER，也就再也沒有 IO exit 能重新拉線——guest 永遠等不到下一次 THRE。
    #[test]
    fn reading_iir_does_not_drop_the_line() {
        let mut sp = SerialPort::new();
        sp.write(COM1_BASE + 1, 0x02);
        for _ in 0..3 {
            assert_eq!(sp.read(COM1_BASE + 2), 0x02);
            assert!(sp.irq_pending(), "THRI 還開著，這條線就不該掉");
        }
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
