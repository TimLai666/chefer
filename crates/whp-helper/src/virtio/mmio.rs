//! virtio-mmio transport register state machine（modern / version 2，跨平台純邏輯）。
//!
//! 對應 virtio spec §4.2.2 的 MMIO Device Register Layout。本型別只保存 transport
//! 狀態（feature 協商、status、各 virtqueue 的位址/大小/ready），不碰 guest 記憶體、
//! 不含 device-specific config space（交由具體裝置以 [`MmioAction::ConfigRead`]/
//! [`MmioAction::ConfigWrite`] 處理）。接線層攔到 WHP MMIO 存取後呼叫 [`Mmio::read`] /
//! [`Mmio::write`]，依回傳的 [`MmioAction`] 觸發 queue notify、reset 或 config 存取。

use super::{STATUS_DRIVER_OK, STATUS_FEATURES_OK, VIRTIO_MMIO_MAGIC, VIRTIO_MMIO_VERSION};

// ── Register offsets（virtio spec §4.2.2）──
const REG_MAGIC: u64 = 0x000;
const REG_VERSION: u64 = 0x004;
const REG_DEVICE_ID: u64 = 0x008;
const REG_VENDOR_ID: u64 = 0x00c;
const REG_DEVICE_FEATURES: u64 = 0x010;
const REG_DEVICE_FEATURES_SEL: u64 = 0x014;
const REG_DRIVER_FEATURES: u64 = 0x020;
const REG_DRIVER_FEATURES_SEL: u64 = 0x024;
const REG_QUEUE_SEL: u64 = 0x030;
const REG_QUEUE_NUM_MAX: u64 = 0x034;
const REG_QUEUE_NUM: u64 = 0x038;
const REG_QUEUE_READY: u64 = 0x044;
const REG_QUEUE_NOTIFY: u64 = 0x050;
const REG_INTERRUPT_STATUS: u64 = 0x060;
const REG_INTERRUPT_ACK: u64 = 0x064;
const REG_STATUS: u64 = 0x070;
const REG_QUEUE_DESC_LOW: u64 = 0x080;
const REG_QUEUE_DESC_HIGH: u64 = 0x084;
const REG_QUEUE_DRIVER_LOW: u64 = 0x090; // avail ring
const REG_QUEUE_DRIVER_HIGH: u64 = 0x094;
const REG_QUEUE_DEVICE_LOW: u64 = 0x0a0; // used ring
const REG_QUEUE_DEVICE_HIGH: u64 = 0x0a4;
// shared-memory region 視窗（virtio spec §4.2.2；virtio-gpu 的 host-visible region 查詢）。
// 我們不提供任何 SHM region：SHMSel 寫入後，SHMLen 讀 0xFFFFFFFF（len=-1=不存在），
// driver 便跳過 host-visible 路徑（否則 virtio-gpu probe 會因「Could not reserve host
// visible region」失敗 -EBUSY）。
const REG_SHM_SEL: u64 = 0x0ac;
const REG_SHM_LEN_LOW: u64 = 0x0b0;
const REG_SHM_LEN_HIGH: u64 = 0x0b4;
const REG_SHM_BASE_LOW: u64 = 0x0b8;
const REG_SHM_BASE_HIGH: u64 = 0x0bc;
const REG_CONFIG_GENERATION: u64 = 0x0fc;
const REG_CONFIG_START: u64 = 0x100;

const VENDOR_ID_CHEFER: u32 = 0x4645_4843; // "CHEF"

/// InterruptStatus bit：used buffer notification（virtio spec §4.2.2）。
pub const INT_USED_BUFFER: u32 = 0x1;

/// 單一 virtqueue 的 transport 設定（位址由 driver 透過 MMIO 寫入）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueConfig {
    pub num: u16,
    pub ready: bool,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
}

/// MMIO 存取需要接線層後續處理的動作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmioAction {
    /// 純 transport 狀態更新，無需額外動作。
    None,
    /// driver 寫了 QueueNotify：應處理該 queue 的 avail ring。
    QueueNotify(u32),
    /// driver 把 Status 寫 0：裝置 reset。
    Reset,
    /// 讀 config space（offset 相對 0x100，長度 1/2/4）。
    ConfigRead { offset: u64, len: u8 },
    /// 寫 config space。
    ConfigWrite { offset: u64, len: u8, value: u32 },
}

/// virtio-mmio transport register state machine。
#[derive(Debug, Clone)]
pub struct Mmio {
    device_id: u32,
    device_features: u64,
    device_features_sel: u32,
    driver_features: u64,
    driver_features_sel: u32,
    status: u32,
    queue_sel: u32,
    queue_num_max: u16,
    queues: Vec<QueueConfig>,
    interrupt_status: u32,
    config_generation: u32,
}

impl Mmio {
    /// 建一個 transport：`device_id`（見 super::DEVICE_ID_*）、`device_features`（裝置
    /// 提供的 feature bits，須含 VIRTIO_F_VERSION_1）、`num_queues` 與每佇列上限 `queue_num_max`。
    pub fn new(
        device_id: u32,
        device_features: u64,
        num_queues: usize,
        queue_num_max: u16,
    ) -> Self {
        Self {
            device_id,
            device_features,
            device_features_sel: 0,
            driver_features: 0,
            driver_features_sel: 0,
            status: 0,
            queue_sel: 0,
            queue_num_max,
            queues: vec![QueueConfig::default(); num_queues],
            interrupt_status: 0,
            config_generation: 0,
        }
    }

    /// 讀一個 MMIO register（offset 相對裝置視窗起點）。config space（≥0x100）回 0，
    /// 由接線層改走 [`MmioAction::ConfigRead`]；此處的回傳僅供純 transport register。
    pub fn read(&self, offset: u64) -> u32 {
        match offset {
            REG_MAGIC => VIRTIO_MMIO_MAGIC,
            REG_VERSION => VIRTIO_MMIO_VERSION,
            REG_DEVICE_ID => self.device_id,
            REG_VENDOR_ID => VENDOR_ID_CHEFER,
            REG_DEVICE_FEATURES => {
                if self.device_features_sel == 0 {
                    self.device_features as u32
                } else {
                    (self.device_features >> 32) as u32
                }
            }
            REG_QUEUE_NUM_MAX => self.queue_num_max as u32,
            REG_QUEUE_READY => self.current_queue().map(|q| q.ready as u32).unwrap_or(0),
            REG_INTERRUPT_STATUS => self.interrupt_status,
            REG_STATUS => self.status,
            // 不存在的 SHM region：len 全 1（= u64 -1），base 全 1。driver 據此跳過。
            REG_SHM_LEN_LOW | REG_SHM_LEN_HIGH | REG_SHM_BASE_LOW | REG_SHM_BASE_HIGH => {
                0xFFFF_FFFF
            }
            REG_CONFIG_GENERATION => self.config_generation,
            _ => 0,
        }
    }

    /// 寫一個 MMIO register；回傳接線層需執行的動作。
    pub fn write(&mut self, offset: u64, value: u32) -> MmioAction {
        match offset {
            REG_DEVICE_FEATURES_SEL => self.device_features_sel = value,
            REG_DRIVER_FEATURES_SEL => self.driver_features_sel = value,
            REG_DRIVER_FEATURES => {
                if self.driver_features_sel == 0 {
                    self.driver_features =
                        (self.driver_features & 0xFFFF_FFFF_0000_0000) | value as u64;
                } else {
                    self.driver_features =
                        (self.driver_features & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
                }
            }
            REG_QUEUE_SEL => self.queue_sel = value,
            REG_QUEUE_NUM => {
                if let Some(q) = self.current_queue_mut() {
                    q.num = value as u16;
                }
            }
            REG_QUEUE_READY => {
                if let Some(q) = self.current_queue_mut() {
                    q.ready = value & 1 != 0;
                }
            }
            REG_QUEUE_DESC_LOW => self.set_queue_addr(|q| &mut q.desc, value, false),
            REG_QUEUE_DESC_HIGH => self.set_queue_addr(|q| &mut q.desc, value, true),
            REG_QUEUE_DRIVER_LOW => self.set_queue_addr(|q| &mut q.avail, value, false),
            REG_QUEUE_DRIVER_HIGH => self.set_queue_addr(|q| &mut q.avail, value, true),
            REG_QUEUE_DEVICE_LOW => self.set_queue_addr(|q| &mut q.used, value, false),
            REG_QUEUE_DEVICE_HIGH => self.set_queue_addr(|q| &mut q.used, value, true),
            REG_INTERRUPT_ACK => self.interrupt_status &= !value,
            REG_QUEUE_NOTIFY => return MmioAction::QueueNotify(value),
            REG_STATUS => {
                if value == 0 {
                    self.reset();
                    return MmioAction::Reset;
                }
                self.status = value;
            }
            off if off >= REG_CONFIG_START => {
                return MmioAction::ConfigWrite {
                    offset: off - REG_CONFIG_START,
                    len: 4,
                    value,
                };
            }
            _ => {}
        }
        MmioAction::None
    }

    /// driver 是否已完成初始化（Status 含 DRIVER_OK）。
    pub fn driver_ok(&self) -> bool {
        self.status & STATUS_DRIVER_OK != 0
    }

    /// driver 是否已接受 feature 集（Status 含 FEATURES_OK）。
    pub fn features_ok(&self) -> bool {
        self.status & STATUS_FEATURES_OK != 0
    }

    /// driver 與裝置協商後的 feature 集（雙方都提供的交集，由 driver 寫入決定）。
    pub fn negotiated_features(&self) -> u64 {
        self.driver_features & self.device_features
    }

    /// 取得某 queue 的設定（依 index）。
    pub fn queue(&self, idx: usize) -> Option<&QueueConfig> {
        self.queues.get(idx)
    }

    /// 標記一次 used buffer notification（接線層注入 IRQ 前呼叫）。
    pub fn signal_used(&mut self) {
        self.interrupt_status |= INT_USED_BUFFER;
    }

    fn reset(&mut self) {
        self.status = 0;
        self.driver_features = 0;
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.queue_sel = 0;
        self.interrupt_status = 0;
        for q in &mut self.queues {
            *q = QueueConfig::default();
        }
    }

    fn current_queue(&self) -> Option<&QueueConfig> {
        self.queues.get(self.queue_sel as usize)
    }

    fn current_queue_mut(&mut self) -> Option<&mut QueueConfig> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    fn set_queue_addr(
        &mut self,
        field: impl Fn(&mut QueueConfig) -> &mut u64,
        value: u32,
        high: bool,
    ) {
        if let Some(q) = self.queues.get_mut(self.queue_sel as usize) {
            let slot = field(q);
            if high {
                *slot = (*slot & 0x0000_0000_FFFF_FFFF) | ((value as u64) << 32);
            } else {
                *slot = (*slot & 0xFFFF_FFFF_0000_0000) | value as u64;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::{DEVICE_ID_BLK, STATUS_ACKNOWLEDGE, STATUS_DRIVER, VIRTIO_F_VERSION_1};

    fn blk() -> Mmio {
        Mmio::new(DEVICE_ID_BLK, VIRTIO_F_VERSION_1, 1, 256)
    }

    #[test]
    fn identity_registers() {
        let m = blk();
        assert_eq!(m.read(REG_MAGIC), VIRTIO_MMIO_MAGIC);
        assert_eq!(m.read(REG_VERSION), 2);
        assert_eq!(m.read(REG_DEVICE_ID), DEVICE_ID_BLK);
        assert_eq!(m.read(REG_QUEUE_NUM_MAX), 256);
    }

    #[test]
    fn device_features_are_selectable_by_word() {
        let mut m = blk();
        // sel=0 → 低 32 bits（VIRTIO_F_VERSION_1 在 bit 32，故低字為 0）
        m.write(REG_DEVICE_FEATURES_SEL, 0);
        assert_eq!(m.read(REG_DEVICE_FEATURES), 0);
        // sel=1 → 高 32 bits（bit 32 → 高字 bit 0）
        m.write(REG_DEVICE_FEATURES_SEL, 1);
        assert_eq!(m.read(REG_DEVICE_FEATURES), 1);
    }

    #[test]
    fn driver_feature_negotiation() {
        let mut m = blk();
        // driver 接受 VIRTIO_F_VERSION_1（寫高字 bit 0）
        m.write(REG_DRIVER_FEATURES_SEL, 1);
        m.write(REG_DRIVER_FEATURES, 1);
        m.write(REG_DRIVER_FEATURES_SEL, 0);
        m.write(REG_DRIVER_FEATURES, 0);
        assert_eq!(m.negotiated_features(), VIRTIO_F_VERSION_1);
    }

    #[test]
    fn status_progression_and_reset() {
        let mut m = blk();
        assert!(!m.driver_ok());
        m.write(REG_STATUS, STATUS_ACKNOWLEDGE);
        m.write(REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
        m.write(
            REG_STATUS,
            STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK,
        );
        assert!(m.features_ok());
        m.write(
            REG_STATUS,
            STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
        );
        assert!(m.driver_ok());
        // reset
        assert_eq!(m.write(REG_STATUS, 0), MmioAction::Reset);
        assert!(!m.driver_ok());
        assert_eq!(m.read(REG_STATUS), 0);
    }

    #[test]
    fn queue_addr_split_writes_compose_64bit() {
        let mut m = blk();
        m.write(REG_QUEUE_SEL, 0);
        m.write(REG_QUEUE_NUM, 128);
        m.write(REG_QUEUE_DESC_LOW, 0x1000);
        m.write(REG_QUEUE_DESC_HIGH, 0xABCD);
        m.write(REG_QUEUE_DRIVER_LOW, 0x2000);
        m.write(REG_QUEUE_DEVICE_LOW, 0x3000);
        m.write(REG_QUEUE_READY, 1);
        let q = m.queue(0).unwrap();
        assert_eq!(q.num, 128);
        assert_eq!(q.desc, 0xABCD_0000_1000);
        assert_eq!(q.avail, 0x2000);
        assert_eq!(q.used, 0x3000);
        assert!(q.ready);
    }

    #[test]
    fn queue_notify_returns_action() {
        let mut m = blk();
        assert_eq!(m.write(REG_QUEUE_NOTIFY, 0), MmioAction::QueueNotify(0));
    }

    #[test]
    fn interrupt_status_set_and_ack() {
        let mut m = blk();
        m.signal_used();
        assert_eq!(m.read(REG_INTERRUPT_STATUS), INT_USED_BUFFER);
        m.write(REG_INTERRUPT_ACK, INT_USED_BUFFER);
        assert_eq!(m.read(REG_INTERRUPT_STATUS), 0);
    }

    #[test]
    fn config_write_is_reported() {
        let mut m = blk();
        assert_eq!(
            m.write(REG_CONFIG_START + 4, 0x55),
            MmioAction::ConfigWrite {
                offset: 4,
                len: 4,
                value: 0x55,
            }
        );
    }

    #[test]
    fn shm_region_reads_as_nonexistent() {
        // virtio-gpu 查 host-visible SHM region：SHMLen/Base 須讀全 1（len=-1=不存在），
        // 否則 driver 會嘗試 reserve 一塊不存在的 region 而 probe 失敗（-EBUSY）。
        let mut m = blk();
        assert_eq!(m.write(REG_SHM_SEL, 0), MmioAction::None); // 選 shmid 0：no-op
        assert_eq!(m.read(REG_SHM_LEN_LOW), 0xFFFF_FFFF);
        assert_eq!(m.read(REG_SHM_LEN_HIGH), 0xFFFF_FFFF);
        assert_eq!(m.read(REG_SHM_BASE_LOW), 0xFFFF_FFFF);
        assert_eq!(m.read(REG_SHM_BASE_HIGH), 0xFFFF_FFFF);
    }
}
