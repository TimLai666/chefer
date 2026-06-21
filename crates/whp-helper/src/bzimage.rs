//! Linux bzImage 格式解析與 x86 開機參數構建。
//!
//! 純邏輯模組，不依賴 WHP 或任何 OS API，跨平台可編譯與測試。

// ── setup header 欄位在 bzImage 檔案（同時也是 boot_params）中的偏移 ──

const HDR_SETUP_SECTS: usize = 0x1F1;
const HDR_BOOT_FLAG: usize = 0x1FE;
const HDR_HEADER: usize = 0x202;
const HDR_VERSION: usize = 0x206;
const HDR_TYPE_OF_LOADER: usize = 0x210;
const HDR_LOADFLAGS: usize = 0x211;
const HDR_CODE32_START: usize = 0x214;
const HDR_RAMDISK_IMAGE: usize = 0x218;
const HDR_RAMDISK_SIZE: usize = 0x21C;
const HDR_CMD_LINE_PTR: usize = 0x228;

// ── boot_params 內的非 setup-header 欄位 ──

const BP_E820_ENTRIES: usize = 0x1E8;
const BP_E820_TABLE: usize = 0x2D0;

// ── 常數 ──

const HDRS_MAGIC: u32 = 0x5372_6448; // "HdrS"
const BOOT_FLAG: u16 = 0xAA55;
const E820_RAM: u32 = 1;
const E820_RESERVED: u32 = 2;

/// Guest 物理記憶體佈局常數。
pub const GPA_GDT: u64 = 0x1000;
pub const GPA_STACK_TOP: u64 = 0x7000;
pub const GPA_BOOT_PARAMS: u64 = 0x8000;
pub const GPA_CMDLINE: u64 = 0x2_0000;
pub const GPA_KERNEL: u64 = 0x10_0000; // 1 MB

/// 從 bzImage 解析出的開機資訊。
#[derive(Debug)]
pub struct BzImageInfo {
    pub protocol_version: u16,
    pub code32_start: u32,
    pub kernel_offset: usize,
    pub kernel_size: usize,
    /// 原始 setup header 位元組（file 0x1F1..header_end），用於貼入 boot_params。
    pub setup_header_raw: Vec<u8>,
}

/// 解析 bzImage 標頭。成功回傳 [`BzImageInfo`]。
pub fn parse(image: &[u8]) -> Result<BzImageInfo, String> {
    if image.len() < 0x268 {
        return Err(format!(
            "Image too small ({} bytes); need at least 0x268 for bzImage header",
            image.len()
        ));
    }

    let boot_flag = le_u16(image, HDR_BOOT_FLAG);
    if boot_flag != BOOT_FLAG {
        return Err(format!(
            "Invalid boot flag: expected 0x{BOOT_FLAG:04X}, got 0x{boot_flag:04X}"
        ));
    }

    let magic = le_u32(image, HDR_HEADER);
    if magic != HDRS_MAGIC {
        return Err(format!("Missing 'HdrS' magic at 0x202: got 0x{magic:08X}"));
    }

    let version = le_u16(image, HDR_VERSION);
    if version < 0x0206 {
        return Err(format!(
            "Boot protocol 0x{version:04X} too old; need >= 2.06 for initrd/cmdline"
        ));
    }

    let loadflags = image[HDR_LOADFLAGS];
    if loadflags & 0x01 == 0 {
        return Err(
            "Kernel lacks LOADED_HIGH flag; only bzImage (not zImage) is supported".to_string(),
        );
    }

    let mut setup_sects = image[HDR_SETUP_SECTS];
    if setup_sects == 0 {
        setup_sects = 4;
    }

    let code32_start = le_u32(image, HDR_CODE32_START);

    let header_end = if version >= 0x020A { 0x268 } else { 0x260 }.min(image.len());

    let setup_header_raw = image[HDR_SETUP_SECTS..header_end].to_vec();

    let kernel_offset = (setup_sects as usize + 1) * 512;
    if kernel_offset >= image.len() {
        return Err(format!(
            "Kernel offset 0x{kernel_offset:X} exceeds image size 0x{:X}",
            image.len()
        ));
    }

    Ok(BzImageInfo {
        protocol_version: version,
        code32_start,
        kernel_offset,
        kernel_size: image.len() - kernel_offset,
        setup_header_raw,
    })
}

/// 計算 initramfs 的 GPA（kernel 後方，對齊 4 KB）。
pub fn initrd_gpa(kernel_size: usize) -> u64 {
    let after = GPA_KERNEL + kernel_size as u64;
    (after + 0xFFF) & !0xFFF
}

/// 將 boot_params (zero page)、command line 寫入 guest 記憶體。
pub fn write_boot_params(
    mem: &mut [u8],
    info: &BzImageInfo,
    cmdline: &str,
    initrd_addr: u64,
    initrd_size: u64,
    total_memory: u64,
) -> Result<(), String> {
    let bp = GPA_BOOT_PARAMS as usize;
    if bp + 4096 > mem.len() {
        return Err("Guest memory too small for boot_params".into());
    }

    // 1. zero page
    mem[bp..bp + 4096].fill(0);

    // 2. 貼回 setup header
    let copy_len = info.setup_header_raw.len().min(4096 - HDR_SETUP_SECTS);
    mem[bp + HDR_SETUP_SECTS..bp + HDR_SETUP_SECTS + copy_len]
        .copy_from_slice(&info.setup_header_raw[..copy_len]);

    // 3. 覆寫必要欄位
    mem[bp + HDR_TYPE_OF_LOADER] = 0xFF;
    mem[bp + HDR_LOADFLAGS] |= 0x01; // LOADED_HIGH

    put_le_u32(mem, bp + HDR_CMD_LINE_PTR, GPA_CMDLINE as u32);
    put_le_u32(mem, bp + HDR_RAMDISK_IMAGE, initrd_addr as u32);
    put_le_u32(mem, bp + HDR_RAMDISK_SIZE, initrd_size as u32);

    // 4. E820
    let e820 = bp + BP_E820_TABLE;
    let mut n: u8 = 0;

    write_e820(mem, e820 + n as usize * 20, 0, 0xA_0000, E820_RAM);
    n += 1;
    write_e820(
        mem,
        e820 + n as usize * 20,
        0xA_0000,
        0x6_0000,
        E820_RESERVED,
    );
    n += 1;
    let high = total_memory.saturating_sub(0x10_0000);
    if high > 0 {
        write_e820(mem, e820 + n as usize * 20, 0x10_0000, high, E820_RAM);
        n += 1;
    }
    mem[bp + BP_E820_ENTRIES] = n;

    // 5. command line（NUL 結尾）
    let cl = GPA_CMDLINE as usize;
    let bytes = cmdline.as_bytes();
    if cl + bytes.len() + 1 > mem.len() {
        return Err("Guest memory too small for command line".into());
    }
    mem[cl..cl + bytes.len()].copy_from_slice(bytes);
    mem[cl + bytes.len()] = 0;

    Ok(())
}

/// GDT：Linux 32-bit boot protocol 要求 `__BOOT_CS=0x10`, `__BOOT_DS=0x18`。
/// 額外加一個 TSS entry（selector 0x28）供 WHP VP state validation 用。
pub const GDT_SIZE: usize = 48; // 6 entries × 8 bytes

pub fn build_gdt() -> [u8; GDT_SIZE] {
    let mut gdt = [0u8; GDT_SIZE];
    // [0] null, [1] null (padding)
    // [2] __BOOT_CS = 0x10: flat 32-bit code, execute/read, DPL 0
    put_le_u64(&mut gdt, 16, 0x00CF_9A00_0000_FFFF);
    // [3] __BOOT_DS = 0x18: flat 32-bit data, read/write, DPL 0
    put_le_u64(&mut gdt, 24, 0x00CF_9200_0000_FFFF);
    // [4] reserved
    // [5] TSS = 0x28: 32-bit TSS (busy), base=0, limit=0x67, present
    //     base=0, limit=0x67, type=0xB(32-bit TSS busy), DPL=0, P=1, G=0
    put_le_u64(&mut gdt, 40, 0x0000_8B00_0000_0067);
    gdt
}

// ── helpers ──

fn le_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

fn le_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

fn put_le_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_le_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn write_e820(mem: &mut [u8], off: usize, addr: u64, size: u64, ty: u32) {
    mem[off..off + 8].copy_from_slice(&addr.to_le_bytes());
    mem[off + 8..off + 16].copy_from_slice(&size.to_le_bytes());
    mem[off + 16..off + 20].copy_from_slice(&ty.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 產生帶有效 setup header 的最小合成 bzImage。
    fn synthetic_bzimage(setup_sects: u8) -> Vec<u8> {
        let total = (setup_sects as usize + 1) * 512 + 256; // +256 bytes fake kernel
        let mut img = vec![0u8; total];

        img[HDR_SETUP_SECTS] = setup_sects;
        img[HDR_BOOT_FLAG] = 0x55;
        img[HDR_BOOT_FLAG + 1] = 0xAA;
        // "HdrS"
        img[HDR_HEADER..HDR_HEADER + 4].copy_from_slice(&HDRS_MAGIC.to_le_bytes());
        // protocol version 2.15
        img[HDR_VERSION..HDR_VERSION + 2].copy_from_slice(&0x020Fu16.to_le_bytes());
        // loadflags: LOADED_HIGH
        img[HDR_LOADFLAGS] = 0x01;
        // code32_start
        img[HDR_CODE32_START..HDR_CODE32_START + 4].copy_from_slice(&0x0010_0000u32.to_le_bytes());

        img
    }

    #[test]
    fn parse_valid_bzimage() {
        let img = synthetic_bzimage(4);
        let info = parse(&img).unwrap();

        assert_eq!(info.protocol_version, 0x020F);
        assert_eq!(info.code32_start, 0x10_0000);
        assert_eq!(info.kernel_offset, 5 * 512);
        assert_eq!(info.kernel_size, 256);
    }

    #[test]
    fn rejects_too_small() {
        assert!(parse(&[0u8; 100]).is_err());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut img = synthetic_bzimage(4);
        img[HDR_HEADER] = 0x00; // break magic
        assert!(parse(&img).unwrap_err().contains("HdrS"));
    }

    #[test]
    fn rejects_old_protocol() {
        let mut img = synthetic_bzimage(4);
        img[HDR_VERSION..HDR_VERSION + 2].copy_from_slice(&0x0200u16.to_le_bytes());
        assert!(parse(&img).unwrap_err().contains("2.06"));
    }

    #[test]
    fn rejects_no_loaded_high() {
        let mut img = synthetic_bzimage(4);
        img[HDR_LOADFLAGS] = 0x00;
        assert!(parse(&img).unwrap_err().contains("LOADED_HIGH"));
    }

    #[test]
    fn default_setup_sects_is_four() {
        let mut img = synthetic_bzimage(4);
        img[HDR_SETUP_SECTS] = 0; // 0 means default = 4
        let info = parse(&img).unwrap();
        assert_eq!(info.kernel_offset, 5 * 512);
    }

    #[test]
    fn boot_params_and_e820() {
        let img = synthetic_bzimage(4);
        let info = parse(&img).unwrap();
        let total_mem: u64 = 128 * 1024 * 1024;
        let mut mem = vec![0u8; total_mem as usize];
        let initrd_addr: u64 = 0x20_0000;
        let initrd_size: u64 = 4096;

        write_boot_params(
            &mut mem,
            &info,
            "console=ttyS0",
            initrd_addr,
            initrd_size,
            total_mem,
        )
        .unwrap();

        let bp = GPA_BOOT_PARAMS as usize;
        // type_of_loader = 0xFF
        assert_eq!(mem[bp + HDR_TYPE_OF_LOADER], 0xFF);
        // LOADED_HIGH set
        assert_ne!(mem[bp + HDR_LOADFLAGS] & 0x01, 0);
        // cmd_line_ptr
        assert_eq!(le_u32(&mem, bp + HDR_CMD_LINE_PTR), GPA_CMDLINE as u32);
        // ramdisk
        assert_eq!(le_u32(&mem, bp + HDR_RAMDISK_IMAGE), initrd_addr as u32);
        assert_eq!(le_u32(&mem, bp + HDR_RAMDISK_SIZE), initrd_size as u32);
        // e820 entries (3: low + reserved + high)
        assert_eq!(mem[bp + BP_E820_ENTRIES], 3);
        // command line NUL-terminated
        let cl = GPA_CMDLINE as usize;
        assert_eq!(&mem[cl..cl + 13], b"console=ttyS0");
        assert_eq!(mem[cl + 13], 0);
    }

    #[test]
    fn gdt_layout() {
        let gdt = build_gdt();
        // null entries
        assert!(gdt[0..16].iter().all(|&b| b == 0));
        // __BOOT_CS at index 2 (offset 16)
        let cs = u64::from_le_bytes(gdt[16..24].try_into().unwrap());
        assert_eq!(cs, 0x00CF_9A00_0000_FFFF);
        // __BOOT_DS at index 3 (offset 24)
        let ds = u64::from_le_bytes(gdt[24..32].try_into().unwrap());
        assert_eq!(ds, 0x00CF_9200_0000_FFFF);
        // TSS at index 5 (offset 40)
        let tss = u64::from_le_bytes(gdt[40..48].try_into().unwrap());
        assert_eq!(tss, 0x0000_8B00_0000_0067);
    }

    #[test]
    fn initrd_gpa_aligned() {
        assert_eq!(initrd_gpa(0), GPA_KERNEL);
        assert_eq!(initrd_gpa(1), GPA_KERNEL + 0x1000);
        assert_eq!(initrd_gpa(0x1000), GPA_KERNEL + 0x1000);
        assert_eq!(initrd_gpa(0x1001), GPA_KERNEL + 0x2000);
    }
}
