//! 8250/16550 UART 序列埠模擬（COM1, I/O 0x3F8–0x3FF）。
//!
//! 僅實作 guest → host transmit；不支援 host → guest receive。
//! 跨平台可編譯與測試。

pub const COM1_BASE: u16 = 0x3F8;
pub const COM1_END: u16 = 0x3FF;
const MAX_SERIAL_OUTPUT: usize = 16 * 1024 * 1024; // 16 MiB

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
            2 => 0x01, // IIR: no pending interrupt
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
