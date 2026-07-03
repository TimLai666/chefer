//! virtio-gpu 裝置邏輯（virtio spec §5.7，2D-only，跨平台純邏輯）。
//!
//! WHP GUI（DESIGN §6 M8-b）：guest 的 cage/wlroots 經 DRM/KMS 把畫面放上 scanout，
//! 本裝置只需支援 2D 命令集（無 virgl/3D——WHP 路線 CPU-only，guest 以 llvmpipe 軟算繪）。
//! host 視窗端（M8-c）從 [`GpuDevice::scanout`] 取 framebuffer 呈現，以
//! [`GpuDevice::take_dirty`] 決定何時重繪。
//!
//! 支援的 control queue 命令：GET_DISPLAY_INFO / RESOURCE_CREATE_2D / RESOURCE_UNREF /
//! SET_SCANOUT / RESOURCE_FLUSH / TRANSFER_TO_HOST_2D / RESOURCE_ATTACH_BACKING /
//! RESOURCE_DETACH_BACKING。cursor queue 的命令一律回 OK（軟游標後續）。
//! 其餘回 ERR_UNSPEC。帶 FENCE flag 的命令回應時 echo fence_id（driver 靠它同步）。

use std::collections::HashMap;

use super::GuestMemory;
use super::queue::DescChain;

// ── ctrl 命令型別（virtio_gpu_ctrl_type）──
const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const VIRTIO_GPU_CMD_RESOURCE_UNREF: u32 = 0x0102;
const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
// cursor queue（q1）
const VIRTIO_GPU_CMD_UPDATE_CURSOR: u32 = 0x0300;
const VIRTIO_GPU_CMD_MOVE_CURSOR: u32 = 0x0301;
// 回應
const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const VIRTIO_GPU_RESP_ERR_UNSPEC: u32 = 0x1200;
const VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1204;
const VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

const VIRTIO_GPU_FLAG_FENCE: u32 = 1 << 0;

/// ctrl header 長度（type+flags+fence_id+ctx_id+ring_idx+padding）。
const CTRL_HDR_LEN: usize = 24;
/// GET_DISPLAY_INFO 回應的 pmode 數（spec 固定 16 格）。
const MAX_SCANOUTS: usize = 16;
/// 單一命令 readable payload 上限（ATTACH_BACKING 的 entries 決定；4096 頁綽綽有餘）。
const MAX_REQ_BYTES: usize = 64 * 1024;

/// 每像素 bytes：本裝置只接受 4bpp 的 2D 格式（spec 的 8 種 fourcc 皆為 32-bit）。
const BPP: usize = 4;

/// 一個 2D resource：host 端持有一份線性 framebuffer 拷貝 + guest backing 頁清單。
struct Resource {
    width: u32,
    height: u32,
    /// host 端像素（w*h*4，格式照 guest 宣告原樣保存；呈現端依 scanout 格式解讀）。
    pixels: Vec<u8>,
    /// guest backing（gpa, len）串接成線性資源內容。
    backing: Vec<(u64, u32)>,
    format: u32,
}

/// virtio-gpu 裝置狀態。
pub struct GpuDevice {
    width: u32,
    height: u32,
    resources: HashMap<u32, Resource>,
    /// 目前接上 scanout 0 的 resource id（0 = 無）。
    scanout_res: u32,
    /// RESOURCE_FLUSH 後為 true；host 呈現端以 take_dirty 消費。
    dirty: bool,
}

impl GpuDevice {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            resources: HashMap::new(),
            scanout_res: 0,
            dirty: false,
        }
    }

    /// config space（virtio_gpu_config：events_read/events_clear/num_scanouts/num_capsets）。
    pub fn config_read(&self, offset: u64, len: u8) -> u32 {
        let cfg: [u32; 4] = [0, 0, 1, 0]; // events=0、1 個 scanout、0 個 capset
        let bytes: Vec<u8> = cfg.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut out = [0u8; 4];
        for (i, slot) in out.iter_mut().enumerate().take(len as usize) {
            *slot = bytes.get(offset as usize + i).copied().unwrap_or(0);
        }
        u32::from_le_bytes(out)
    }

    /// 顯示解析度（host 視窗端建立視窗用）。
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 目前 scanout 的像素（線性 w*h*4；尚未 SET_SCANOUT 時為 None）。
    pub fn scanout(&self) -> Option<(&[u8], u32, u32, u32)> {
        let r = self.resources.get(&self.scanout_res)?;
        Some((&r.pixels, r.width, r.height, r.format))
    }

    /// 取走 dirty 旗標（RESOURCE_FLUSH 置位；host 呈現端消費後重繪）。
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// 處理一條 control/cursor queue 的命令 chain，回傳寫回 guest 的 bytes 數。
    pub fn process_chain<M: GuestMemory>(
        &mut self,
        chain: &DescChain,
        mem: &mut M,
    ) -> Result<u32, String> {
        // 蒐集 readable payload（跨多段 descriptor）。
        let req_len: usize = chain.readable.iter().map(|&(_, l)| l as usize).sum();
        if !(CTRL_HDR_LEN..=MAX_REQ_BYTES).contains(&req_len) {
            return Err(format!("virtio-gpu request len {req_len} out of range"));
        }
        let mut req = vec![0u8; req_len];
        let mut off = 0;
        for &(gpa, len) in &chain.readable {
            mem.read(gpa, &mut req[off..off + len as usize])?;
            off += len as usize;
        }
        let cmd = u32_at(&req, 0);
        let flags = u32_at(&req, 4);
        let fence_id = u64::from(u32_at(&req, 8)) | (u64::from(u32_at(&req, 12)) << 32);

        // 執行命令 → (回應 type, 額外 payload)。
        let (resp_type, payload) = self.execute(cmd, &req, mem);

        // 回應：hdr（echo fence）+ payload 寫進 writable descriptors。
        let mut resp = Vec::with_capacity(CTRL_HDR_LEN + payload.len());
        resp.extend_from_slice(&resp_type.to_le_bytes());
        let resp_flags = flags & VIRTIO_GPU_FLAG_FENCE;
        resp.extend_from_slice(&resp_flags.to_le_bytes());
        resp.extend_from_slice(&fence_id.to_le_bytes());
        resp.extend_from_slice(&0u32.to_le_bytes()); // ctx_id
        resp.extend_from_slice(&0u32.to_le_bytes()); // ring_idx + padding
        resp.extend_from_slice(&payload);

        let mut written = 0usize;
        for &(gpa, len) in &chain.writable {
            if written >= resp.len() {
                break;
            }
            let n = (resp.len() - written).min(len as usize);
            mem.write(gpa, &resp[written..written + n])?;
            written += n;
        }
        if written < resp.len() {
            return Err(format!(
                "virtio-gpu response truncated: need {} bytes, wrote {written}",
                resp.len()
            ));
        }
        Ok(written as u32)
    }

    fn execute<M: GuestMemory>(&mut self, cmd: u32, req: &[u8], mem: &mut M) -> (u32, Vec<u8>) {
        match cmd {
            VIRTIO_GPU_CMD_GET_DISPLAY_INFO => {
                (VIRTIO_GPU_RESP_OK_DISPLAY_INFO, self.display_info_payload())
            }
            VIRTIO_GPU_CMD_RESOURCE_CREATE_2D => self.cmd_create_2d(req),
            VIRTIO_GPU_CMD_RESOURCE_UNREF => {
                let res_id = u32_at(req, CTRL_HDR_LEN);
                self.resources.remove(&res_id);
                if self.scanout_res == res_id {
                    self.scanout_res = 0;
                }
                (VIRTIO_GPU_RESP_OK_NODATA, vec![])
            }
            VIRTIO_GPU_CMD_SET_SCANOUT => self.cmd_set_scanout(req),
            VIRTIO_GPU_CMD_RESOURCE_FLUSH => {
                let res_id = u32_at(req, CTRL_HDR_LEN + 16);
                if res_id != 0 && !self.resources.contains_key(&res_id) {
                    return (VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID, vec![]);
                }
                self.dirty = true;
                (VIRTIO_GPU_RESP_OK_NODATA, vec![])
            }
            VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D => self.cmd_transfer_to_host(req, mem),
            VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING => self.cmd_attach_backing(req),
            VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING => {
                let res_id = u32_at(req, CTRL_HDR_LEN);
                match self.resources.get_mut(&res_id) {
                    Some(r) => {
                        r.backing.clear();
                        (VIRTIO_GPU_RESP_OK_NODATA, vec![])
                    }
                    None => (VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID, vec![]),
                }
            }
            // 軟游標後續（M8-c 之後）：先回 OK 讓 driver 不視為錯誤。
            VIRTIO_GPU_CMD_UPDATE_CURSOR | VIRTIO_GPU_CMD_MOVE_CURSOR => {
                (VIRTIO_GPU_RESP_OK_NODATA, vec![])
            }
            _ => (VIRTIO_GPU_RESP_ERR_UNSPEC, vec![]),
        }
    }

    /// GET_DISPLAY_INFO 回應 payload：16 個 pmode（rect + enabled + flags），只開 scanout 0。
    fn display_info_payload(&self) -> Vec<u8> {
        let mut p = Vec::with_capacity(MAX_SCANOUTS * 24);
        for i in 0..MAX_SCANOUTS {
            let (w, h, enabled) = if i == 0 {
                (self.width, self.height, 1u32)
            } else {
                (0, 0, 0)
            };
            for v in [0u32, 0, w, h, enabled, 0] {
                p.extend_from_slice(&v.to_le_bytes());
            }
        }
        p
    }

    fn cmd_create_2d(&mut self, req: &[u8]) -> (u32, Vec<u8>) {
        let res_id = u32_at(req, CTRL_HDR_LEN);
        let format = u32_at(req, CTRL_HDR_LEN + 4);
        let width = u32_at(req, CTRL_HDR_LEN + 8);
        let height = u32_at(req, CTRL_HDR_LEN + 12);
        if res_id == 0 || width == 0 || height == 0 {
            return (VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER, vec![]);
        }
        // 上限防守：拒絕荒謬尺寸（16K x 16K 已遠超支援情境）。
        if width > 16384 || height > 16384 {
            return (VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER, vec![]);
        }
        let bytes = width as usize * height as usize * BPP;
        self.resources.insert(
            res_id,
            Resource {
                width,
                height,
                pixels: vec![0u8; bytes],
                backing: Vec::new(),
                format,
            },
        );
        (VIRTIO_GPU_RESP_OK_NODATA, vec![])
    }

    fn cmd_set_scanout(&mut self, req: &[u8]) -> (u32, Vec<u8>) {
        // virtio_gpu_set_scanout：rect(16) + scanout_id(4) + resource_id(4)
        let scanout_id = u32_at(req, CTRL_HDR_LEN + 16);
        let res_id = u32_at(req, CTRL_HDR_LEN + 20);
        if scanout_id != 0 {
            return (VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER, vec![]);
        }
        if res_id != 0 && !self.resources.contains_key(&res_id) {
            return (VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID, vec![]);
        }
        self.scanout_res = res_id;
        (VIRTIO_GPU_RESP_OK_NODATA, vec![])
    }

    /// TRANSFER_TO_HOST_2D：把 guest backing（offset 起）的 rect 區域複製進 host 像素。
    /// req：hdr + rect(x,y,w,h) + offset(u64) + resource_id + padding。
    fn cmd_transfer_to_host<M: GuestMemory>(&mut self, req: &[u8], mem: &mut M) -> (u32, Vec<u8>) {
        let x = u32_at(req, CTRL_HDR_LEN) as usize;
        let y = u32_at(req, CTRL_HDR_LEN + 4) as usize;
        let w = u32_at(req, CTRL_HDR_LEN + 8) as usize;
        let h = u32_at(req, CTRL_HDR_LEN + 12) as usize;
        let offset = u64::from(u32_at(req, CTRL_HDR_LEN + 16))
            | (u64::from(u32_at(req, CTRL_HDR_LEN + 20)) << 32);
        let res_id = u32_at(req, CTRL_HDR_LEN + 24);

        let Some(r) = self.resources.get_mut(&res_id) else {
            return (VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID, vec![]);
        };
        let (rw, rh) = (r.width as usize, r.height as usize);
        if x + w > rw || y + h > rh {
            return (VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER, vec![]);
        }
        // guest 端資源是線性 stride = width*4；rect 逐列複製。
        let stride = rw * BPP;
        for row in 0..h {
            let line_off = offset as usize + (y + row) * stride + x * BPP;
            let dst_off = (y + row) * stride + x * BPP;
            let mut line = vec![0u8; w * BPP];
            if let Err(e) = read_backing(&r.backing, line_off, &mut line, mem) {
                return (
                    VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER,
                    e.into_bytes().into_iter().take(0).collect(),
                );
            }
            r.pixels[dst_off..dst_off + w * BPP].copy_from_slice(&line);
        }
        (VIRTIO_GPU_RESP_OK_NODATA, vec![])
    }

    /// ATTACH_BACKING：hdr + resource_id + nr_entries，後接 nr_entries × {addr u64, length u32, pad u32}。
    fn cmd_attach_backing(&mut self, req: &[u8]) -> (u32, Vec<u8>) {
        let res_id = u32_at(req, CTRL_HDR_LEN);
        let nr = u32_at(req, CTRL_HDR_LEN + 4) as usize;
        let entries_off = CTRL_HDR_LEN + 8;
        if req.len() < entries_off + nr * 16 {
            return (VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER, vec![]);
        }
        let Some(r) = self.resources.get_mut(&res_id) else {
            return (VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID, vec![]);
        };
        let mut backing = Vec::with_capacity(nr);
        for i in 0..nr {
            let e = entries_off + i * 16;
            let addr = u64::from(u32_at(req, e)) | (u64::from(u32_at(req, e + 4)) << 32);
            let len = u32_at(req, e + 8);
            backing.push((addr, len));
        }
        r.backing = backing;
        (VIRTIO_GPU_RESP_OK_NODATA, vec![])
    }
}

/// 由 backing 頁清單讀出「線性資源位移 offset 起的 buf.len() bytes」。
fn read_backing<M: GuestMemory>(
    backing: &[(u64, u32)],
    mut offset: usize,
    buf: &mut [u8],
    mem: &mut M,
) -> Result<(), String> {
    let mut filled = 0usize;
    for &(gpa, len) in backing {
        let seg = len as usize;
        if offset >= seg {
            offset -= seg;
            continue;
        }
        let avail = seg - offset;
        let n = avail.min(buf.len() - filled);
        mem.read(gpa + offset as u64, &mut buf[filled..filled + n])?;
        filled += n;
        offset = 0;
        if filled == buf.len() {
            return Ok(());
        }
    }
    Err(format!(
        "virtio-gpu transfer reads past the attached backing (missing {} bytes)",
        buf.len() - filled
    ))
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    let mut v = [0u8; 4];
    let n = b.len().saturating_sub(off).min(4);
    v[..n].copy_from_slice(&b[off..off + n]);
    u32::from_le_bytes(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::SliceMem;

    /// 在 mem 上鋪一條 chain：readable=req bytes @0x1000、writable=resp buf @0x8000。
    fn run_cmd(dev: &mut GpuDevice, mem: &mut SliceMem<'_>, req: &[u8], resp_len: u32) -> Vec<u8> {
        mem.write(0x1000, req).unwrap();
        let chain = DescChain {
            head: 0,
            readable: vec![(0x1000, req.len() as u32)],
            writable: vec![(0x8000, resp_len)],
        };
        let written = dev.process_chain(&chain, mem).unwrap();
        let mut resp = vec![0u8; written as usize];
        mem.read(0x8000, &mut resp).unwrap();
        resp
    }

    fn hdr(cmd: u32) -> Vec<u8> {
        let mut v = vec![0u8; CTRL_HDR_LEN];
        v[..4].copy_from_slice(&cmd.to_le_bytes());
        v
    }

    fn push_u32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    #[test]
    fn display_info_reports_single_scanout() {
        let mut buf = vec![0u8; 1 << 20];
        let mut mem = SliceMem::new(0, &mut buf);
        let mut dev = GpuDevice::new(1280, 800);
        let resp = run_cmd(
            &mut dev,
            &mut mem,
            &hdr(VIRTIO_GPU_CMD_GET_DISPLAY_INFO),
            4096,
        );
        assert_eq!(u32_at(&resp, 0), VIRTIO_GPU_RESP_OK_DISPLAY_INFO);
        assert_eq!(resp.len(), CTRL_HDR_LEN + 16 * 24);
        // pmode 0：w/h/enabled
        assert_eq!(u32_at(&resp, CTRL_HDR_LEN + 8), 1280);
        assert_eq!(u32_at(&resp, CTRL_HDR_LEN + 12), 800);
        assert_eq!(u32_at(&resp, CTRL_HDR_LEN + 16), 1);
        // pmode 1：disabled
        assert_eq!(u32_at(&resp, CTRL_HDR_LEN + 24 + 16), 0);
    }

    #[test]
    fn create_attach_transfer_flush_roundtrip() {
        let mut buf = vec![0u8; 1 << 20];
        // guest backing 頁：兩段不連續，各 32 bytes（4x2 資源 = 32 bytes）。
        buf[0x2000..0x2010].copy_from_slice(&[0xAA; 16]);
        buf[0x3000..0x3010].copy_from_slice(&[0xBB; 16]);
        let mut mem = SliceMem::new(0, &mut buf);
        let mut dev = GpuDevice::new(640, 480);

        // CREATE_2D：res 1，4x2。
        let mut req = hdr(VIRTIO_GPU_CMD_RESOURCE_CREATE_2D);
        for v in [1u32, 2, 4, 2] {
            push_u32(&mut req, v);
        }
        let resp = run_cmd(&mut dev, &mut mem, &req, 64);
        assert_eq!(u32_at(&resp, 0), VIRTIO_GPU_RESP_OK_NODATA);

        // ATTACH_BACKING：兩段 16-byte 頁。
        let mut req = hdr(VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING);
        push_u32(&mut req, 1); // res id
        push_u32(&mut req, 2); // nr_entries
        for (addr, len) in [(0x2000u64, 16u32), (0x3000, 16)] {
            req.extend_from_slice(&addr.to_le_bytes());
            push_u32(&mut req, len);
            push_u32(&mut req, 0);
        }
        let resp = run_cmd(&mut dev, &mut mem, &req, 64);
        assert_eq!(u32_at(&resp, 0), VIRTIO_GPU_RESP_OK_NODATA);

        // TRANSFER_TO_HOST_2D：整張（rect 0,0,4,2、offset 0）。
        let mut req = hdr(VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D);
        for v in [0u32, 0, 4, 2] {
            push_u32(&mut req, v);
        }
        req.extend_from_slice(&0u64.to_le_bytes());
        push_u32(&mut req, 1);
        push_u32(&mut req, 0);
        let resp = run_cmd(&mut dev, &mut mem, &req, 64);
        assert_eq!(u32_at(&resp, 0), VIRTIO_GPU_RESP_OK_NODATA);

        // SET_SCANOUT + FLUSH → scanout 可讀、dirty 置位、跨頁內容正確。
        let mut req = hdr(VIRTIO_GPU_CMD_SET_SCANOUT);
        for v in [0u32, 0, 4, 2, 0, 1] {
            push_u32(&mut req, v);
        }
        let resp = run_cmd(&mut dev, &mut mem, &req, 64);
        assert_eq!(u32_at(&resp, 0), VIRTIO_GPU_RESP_OK_NODATA);

        let mut req = hdr(VIRTIO_GPU_CMD_RESOURCE_FLUSH);
        for v in [0u32, 0, 4, 2, 1, 0] {
            push_u32(&mut req, v);
        }
        run_cmd(&mut dev, &mut mem, &req, 64);
        assert!(dev.take_dirty());
        assert!(!dev.take_dirty());
        let (px, w, h, _fmt) = dev.scanout().unwrap();
        assert_eq!((w, h), (4, 2));
        assert_eq!(&px[..16], &[0xAA; 16]);
        assert_eq!(&px[16..32], &[0xBB; 16]);
    }

    #[test]
    fn fence_flag_is_echoed() {
        let mut buf = vec![0u8; 1 << 20];
        let mut mem = SliceMem::new(0, &mut buf);
        let mut dev = GpuDevice::new(640, 480);
        let mut req = hdr(VIRTIO_GPU_CMD_GET_DISPLAY_INFO);
        req[4..8].copy_from_slice(&VIRTIO_GPU_FLAG_FENCE.to_le_bytes());
        req[8..16].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        let resp = run_cmd(&mut dev, &mut mem, &req, 4096);
        assert_eq!(u32_at(&resp, 4), VIRTIO_GPU_FLAG_FENCE);
        assert_eq!(
            u64::from(u32_at(&resp, 8)) | (u64::from(u32_at(&resp, 12)) << 32),
            0x1122_3344_5566_7788
        );
    }

    #[test]
    fn unknown_command_and_bad_resource_are_rejected() {
        let mut buf = vec![0u8; 1 << 20];
        let mut mem = SliceMem::new(0, &mut buf);
        let mut dev = GpuDevice::new(640, 480);
        let resp = run_cmd(&mut dev, &mut mem, &hdr(0x0999), 64);
        assert_eq!(u32_at(&resp, 0), VIRTIO_GPU_RESP_ERR_UNSPEC);

        // SET_SCANOUT 指到不存在的 resource。
        let mut req = hdr(VIRTIO_GPU_CMD_SET_SCANOUT);
        for v in [0u32, 0, 4, 2, 0, 42] {
            push_u32(&mut req, v);
        }
        let resp = run_cmd(&mut dev, &mut mem, &req, 64);
        assert_eq!(u32_at(&resp, 0), VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID);
    }

    #[test]
    fn transfer_out_of_bounds_rejected() {
        let mut buf = vec![0u8; 1 << 20];
        let mut mem = SliceMem::new(0, &mut buf);
        let mut dev = GpuDevice::new(640, 480);
        let mut req = hdr(VIRTIO_GPU_CMD_RESOURCE_CREATE_2D);
        for v in [1u32, 2, 4, 2] {
            push_u32(&mut req, v);
        }
        run_cmd(&mut dev, &mut mem, &req, 64);
        // rect 超界（w=8 > 資源寬 4）。
        let mut req = hdr(VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D);
        for v in [0u32, 0, 8, 2] {
            push_u32(&mut req, v);
        }
        req.extend_from_slice(&0u64.to_le_bytes());
        push_u32(&mut req, 1);
        push_u32(&mut req, 0);
        let resp = run_cmd(&mut dev, &mut mem, &req, 64);
        assert_eq!(u32_at(&resp, 0), VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER);
    }
}
