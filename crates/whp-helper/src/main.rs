//! Windows WHP host helper: preflight 驗證或以 WHP 啟動 Linux appliance。

// 這些模組是跨平台可測的純邏輯（bzImage 解析、PIC/PIT、serial 模擬），但其
// 非測試消費端（whp_api::boot_vm 等）為 #[cfg(windows)]。非 Windows 上僅供單元
// 測試使用，故對 bin build 標 dead_code 容許（與 vmm-backend 的 vz_util/whp_util 同模式）。
#[cfg_attr(not(windows), allow(dead_code))]
mod bzimage;
#[cfg_attr(not(windows), allow(dead_code))]
mod pic;
#[cfg_attr(not(windows), allow(dead_code))]
mod serial;
// virtio-mmio 裝置模型（M1：純邏輯，尚未接線到 boot loop）。無條件 allow(dead_code)：
// 連 Windows 上都還沒有消費端，待接上 run_loop 的 MMIO dispatch 後移除此 allow。
#[allow(dead_code)]
mod virtio;

use std::path::PathBuf;

// sysexits.h convention
const EXIT_USAGE: i32 = 64;
const EXIT_UNAVAILABLE: i32 = 69;

fn main() {
    let code = match run(std::env::args().skip(1)) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("{err}");
            if err.starts_with("usage:") {
                EXIT_USAGE
            } else {
                EXIT_UNAVAILABLE
            }
        }
    };
    std::process::exit(code);
}

fn run(args: impl IntoIterator<Item = String>) -> Result<(), String> {
    let args: Vec<String> = args.into_iter().collect();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_help();
        return Ok(());
    }

    if args.iter().any(|arg| arg == "--preflight") {
        return run_preflight(args);
    }

    let request = HelperRequest::parse(args)?;
    run_boot(request)
}

fn print_help() {
    println!(
        "chefer-whp-helper\n\
         \n\
         Modes:\n\
         \n\
         1. Preflight (validate WHP API without booting):\n\
           chefer-whp-helper --preflight [--cpus <n>]\n\
         \n\
         2. Boot Linux appliance via WHP:\n\
           chefer-whp-helper \\\n\
             --kernel <path> --initramfs <path> --cmdline <text> \\\n\
             --bundle-dir <path> --data-dir <path> --cpus <n> --memory-mib <n> \\\n\
             [--timeout <seconds>]   (default: 300)\n\
         \n\
         Preflight dynamically loads WinHvPlatform.dll and validates the full\n\
         device model lifecycle (partition/GPA mapping/vCPU).\n\
         \n\
         Boot mode loads a Linux bzImage kernel + initramfs, runs the VM via\n\
         WHP, and streams guest console markers to stdout."
    );
}

// ---------------------------------------------------------------------------
// Preflight mode: WHP partition lifecycle validation
// ---------------------------------------------------------------------------

fn run_preflight(args: Vec<String>) -> Result<(), String> {
    let mut cpus: u16 = 1;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--preflight" => {}
            "--cpus" => {
                i += 1;
                if i >= args.len() {
                    return Err("usage: --cpus requires a value".to_string());
                }
                cpus = parse_cpus(&args[i])?;
            }
            other => {
                return Err(format!(
                    "usage: unexpected argument in preflight mode: {other}"
                ));
            }
        }
        i += 1;
    }
    preflight_whp_api(cpus)
}

#[cfg(windows)]
fn preflight_whp_api(cpus: u16) -> Result<(), String> {
    whp_api::run_partition_lifecycle(cpus)?;
    println!("WHP device model preflight: OK (cpus={cpus}, GPA mapped, vCPU created)");
    Ok(())
}

#[cfg(not(windows))]
fn preflight_whp_api(_cpus: u16) -> Result<(), String> {
    Err("WHP preflight requires Windows.".to_string())
}

// ---------------------------------------------------------------------------
// Boot mode
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn run_boot(request: HelperRequest) -> Result<(), String> {
    whp_api::boot_vm(&request)
}

#[cfg(not(windows))]
fn run_boot(_request: HelperRequest) -> Result<(), String> {
    Err("WHP boot requires Windows.".to_string())
}

// ---------------------------------------------------------------------------
// WHP API dynamic loading + preflight + boot (Windows only)
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod whp_api {
    use std::ffi::c_void;
    use std::mem::size_of;

    use windows_sys::Win32::Foundation::FreeLibrary;
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExA,
    };
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    const WIN_HV_PLATFORM_DLL: &[u8] = b"WinHvPlatform.dll\0";
    const PAGE_SIZE: usize = 4096;

    // ── WHP property codes ──

    const WHV_PROPERTY_PROCESSOR_COUNT: u32 = 0x0000_1fff;
    const WHV_MAP_GPA_RANGE_FLAGS_RWX: u32 = 0x7; // R|W|X

    // ── WHP register name constants (WHV_REGISTER_NAME) ──
    // 取自 Windows SDK WinHvPlatformDefs.h
    const REG_RAX: u32 = 0x00;
    const REG_RBX: u32 = 0x03;
    const REG_RSP: u32 = 0x04;
    const REG_RBP: u32 = 0x05;
    const REG_RSI: u32 = 0x06;
    const REG_RDI: u32 = 0x07;
    const REG_RIP: u32 = 0x10;
    const REG_RFLAGS: u32 = 0x11;
    const REG_ES: u32 = 0x12;
    const REG_CS: u32 = 0x13;
    const REG_SS: u32 = 0x14;
    const REG_DS: u32 = 0x15;
    const REG_FS: u32 = 0x16;
    const REG_GS: u32 = 0x17;
    const REG_LDTR: u32 = 0x18;
    const REG_TR: u32 = 0x19;
    const REG_IDTR: u32 = 0x1A;
    const REG_GDTR: u32 = 0x1B;
    const REG_CR0: u32 = 0x1C;
    const REG_CR3: u32 = 0x1E;
    const REG_CR4: u32 = 0x1F;

    // ── VM exit reasons ──

    const EXIT_NONE: u32 = 0x0000;
    const EXIT_MEM_ACCESS: u32 = 0x0001;
    const EXIT_IO_PORT: u32 = 0x0002;
    const EXIT_UNRECOVERABLE: u32 = 0x0004;
    const EXIT_INVALID_VP_STATE: u32 = 0x0005;
    const EXIT_HALT: u32 = 0x0008;
    const EXIT_EXCEPTION: u32 = 0x1002;
    const EXIT_CANCELED: u32 = 0x2001;

    // exception context 欄位偏移（union 從 offset 48 開始）
    // InstructionByteCount(1)+Rsvd(3)+InstructionBytes(16)+ExceptionInfo(4)+ExceptionType(1)+Rsvd(3)+ErrorCode(4)+ExceptionParameter(8)
    const EC_EXC_TYPE: usize = 48 + 24; // ExceptionType (u8)
    const EC_EXC_ERROR: usize = 48 + 28; // ErrorCode (u32)
    const EC_EXC_PARAM: usize = 48 + 32; // ExceptionParameter (u64, = CR2 for #PF)

    // ── WHV_RUN_VP_EXIT_CONTEXT layout offsets ──
    // ExitReason (u32) at 0, Reserved (u32) at 4
    const EC_EXIT_REASON: usize = 0;
    // VpContext starts at 8: ExecutionState(u16)+InstrLen:4/Cr8:4(u8)+Rsvd(u8)+Rsvd2(u32)+Cs(16)+Rip(u64)+Rflags(u64)
    const EC_INSTR_LEN: usize = 10; // 低 4 bits
    const EC_RIP: usize = 32;
    const EC_RFLAGS: usize = 40;
    // union 從 offset 48 開始
    // IoPortAccessContext: InstrByteCount(1)+Rsvd(3)+InstrBytes(16) = 20 bytes, 然後:
    const EC_IO_ACCESS_INFO: usize = 48 + 20; // = 68
    const EC_IO_PORT: usize = 48 + 24; // = 72
    // Rsvd2[3] (6 bytes) at 74
    const EC_IO_RAX: usize = 48 + 32; // = 80
    // MemoryAccessContext: InstrByteCount(1)+Rsvd(3)+InstrBytes(16)+AccessInfo(4) = 24, 然後:
    const EC_MEM_GPA: usize = 48 + 24; // = 72

    // ── Console markers ──

    const GUEST_EXIT_MARKER: &str = "CHEFER_GUEST_EXIT=";

    // ── Function pointer types ──

    type CreatePartitionFn = unsafe extern "system" fn(*mut *mut c_void) -> i32;
    type SetPartitionPropertyFn =
        unsafe extern "system" fn(*mut c_void, u32, *const c_void, u32) -> i32;
    type SetupPartitionFn = unsafe extern "system" fn(*mut c_void) -> i32;
    type DeletePartitionFn = unsafe extern "system" fn(*mut c_void) -> i32;
    type MapGpaRangeFn =
        unsafe extern "system" fn(*mut c_void, *const c_void, u64, u64, u32) -> i32;
    type UnmapGpaRangeFn = unsafe extern "system" fn(*mut c_void, u64, u64) -> i32;
    type CreateVpFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> i32;
    type DeleteVpFn = unsafe extern "system" fn(*mut c_void, u32) -> i32;
    type RunVpFn = unsafe extern "system" fn(*mut c_void, u32, *mut c_void, u32) -> i32;
    type SetRegsFn =
        unsafe extern "system" fn(*mut c_void, u32, *const u32, u32, *const c_void) -> i32;
    type CancelRunVpFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> i32;

    struct WhpApi {
        module: *mut c_void,
        create: CreatePartitionFn,
        set_property: SetPartitionPropertyFn,
        setup: SetupPartitionFn,
        delete: DeletePartitionFn,
        map_gpa: MapGpaRangeFn,
        unmap_gpa: UnmapGpaRangeFn,
        create_vp: CreateVpFn,
        delete_vp: DeleteVpFn,
        run_vp: RunVpFn,
        set_regs: SetRegsFn,
        cancel_run_vp: CancelRunVpFn,
    }

    unsafe impl Send for WhpApi {}
    unsafe impl Sync for WhpApi {}

    impl WhpApi {
        fn load() -> Result<Self, String> {
            let module = unsafe {
                LoadLibraryExA(
                    WIN_HV_PLATFORM_DLL.as_ptr(),
                    std::ptr::null_mut(),
                    LOAD_LIBRARY_SEARCH_SYSTEM32,
                )
            };
            if module.is_null() {
                return Err("WinHvPlatform.dll could not be loaded; \
                     enable the Windows Hypervisor Platform feature and reboot."
                    .to_string());
            }

            Ok(Self {
                module,
                create: resolve(module, b"WHvCreatePartition\0")?,
                set_property: resolve(module, b"WHvSetPartitionProperty\0")?,
                setup: resolve(module, b"WHvSetupPartition\0")?,
                delete: resolve(module, b"WHvDeletePartition\0")?,
                map_gpa: resolve(module, b"WHvMapGpaRange\0")?,
                unmap_gpa: resolve(module, b"WHvUnmapGpaRange\0")?,
                create_vp: resolve(module, b"WHvCreateVirtualProcessor\0")?,
                delete_vp: resolve(module, b"WHvDeleteVirtualProcessor\0")?,
                run_vp: resolve(module, b"WHvRunVirtualProcessor\0")?,
                set_regs: resolve(module, b"WHvSetVirtualProcessorRegisters\0")?,
                cancel_run_vp: resolve(module, b"WHvCancelRunVirtualProcessor\0")?,
            })
        }

        fn inject_timer_irq(&self, partition: *mut c_void) -> Result<(), String> {
            // 直接設定 PendingInterruption register（繞過 LAPIC）
            // WHvRegisterPendingInterruption = 0x80000000
            // Layout: bit0=Pending, bits1-3=Type(0=External), bits16-31=Vector
            let reg_name: u32 = 0x8000_0000;
            let vector: u32 = 0x20; // PIT IRQ0 → vector 0x20
            let pending: u64 = 1 | ((vector as u64) << 16);
            let value = [pending.to_le_bytes(), [0u8; 8]];
            let hr = unsafe { (self.set_regs)(partition, 0, &reg_name, 1, value.as_ptr().cast()) };
            if hr < 0 {
                return Err(format!("inject timer IRQ failed: 0x{hr:08X}"));
            }
            Ok(())
        }
    }

    impl Drop for WhpApi {
        fn drop(&mut self) {
            unsafe {
                FreeLibrary(self.module);
            }
        }
    }

    fn resolve<T>(module: *mut c_void, name: &[u8]) -> Result<T, String> {
        let proc = unsafe { GetProcAddress(module, name.as_ptr()) };
        let Some(proc) = proc else {
            unsafe {
                FreeLibrary(module);
            }
            let fn_name = std::str::from_utf8(&name[..name.len() - 1]).unwrap_or("?");
            return Err(format!("{fn_name} not found in WinHvPlatform.dll"));
        };
        Ok(unsafe { std::mem::transmute_copy(&proc) })
    }

    fn check_hr(hr: i32, ctx: &str) -> Result<(), String> {
        if hr < 0 {
            Err(format!("{ctx} failed (HRESULT 0x{:08X})", hr as u32))
        } else {
            Ok(())
        }
    }

    // ── WHV_REGISTER_VALUE helpers (16-byte union) ──

    fn reg_u64(v: u64) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&v.to_le_bytes());
        b
    }

    fn reg_seg(base: u64, limit: u32, sel: u16, attrs: u16) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&base.to_le_bytes());
        b[8..12].copy_from_slice(&limit.to_le_bytes());
        b[12..14].copy_from_slice(&sel.to_le_bytes());
        b[14..16].copy_from_slice(&attrs.to_le_bytes());
        b
    }

    fn reg_table(base: u64, limit: u16) -> [u8; 16] {
        let mut b = [0u8; 16];
        // WHV_X64_TABLE_REGISTER: Pad[3] (6 bytes) | Limit (u16) | Base (u64)
        b[6..8].copy_from_slice(&limit.to_le_bytes());
        b[8..16].copy_from_slice(&base.to_le_bytes());
        b
    }

    // ── Preflight ──

    pub fn run_partition_lifecycle(cpus: u16) -> Result<(), String> {
        let api = WhpApi::load()?;

        let mut partition: *mut c_void = std::ptr::null_mut();
        let hr = unsafe { (api.create)(&mut partition) };
        check_hr(hr, "WHvCreatePartition")?;

        let cpu_count = cpus as u32;
        let hr = unsafe {
            (api.set_property)(
                partition,
                WHV_PROPERTY_PROCESSOR_COUNT,
                (&cpu_count as *const u32).cast::<c_void>(),
                size_of::<u32>() as u32,
            )
        };
        if hr < 0 {
            unsafe { (api.delete)(partition) };
            return Err(format!(
                "WHvSetPartitionProperty(ProcessorCount={cpus}) failed (HRESULT 0x{:08X})",
                hr as u32
            ));
        }

        let hr = unsafe { (api.setup)(partition) };
        if hr < 0 {
            unsafe { (api.delete)(partition) };
            return check_hr(hr, "WHvSetupPartition");
        }

        let host_page = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                PAGE_SIZE,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if host_page.is_null() {
            unsafe { (api.delete)(partition) };
            return Err("VirtualAlloc for GPA test page failed".to_string());
        }

        let hr = unsafe {
            (api.map_gpa)(
                partition,
                host_page,
                0,
                PAGE_SIZE as u64,
                WHV_MAP_GPA_RANGE_FLAGS_RWX,
            )
        };
        if hr < 0 {
            unsafe {
                VirtualFree(host_page, 0, MEM_RELEASE);
                (api.delete)(partition);
            }
            return check_hr(hr, "WHvMapGpaRange");
        }

        let hr = unsafe { (api.create_vp)(partition, 0, 0) };
        if hr < 0 {
            unsafe {
                (api.unmap_gpa)(partition, 0, PAGE_SIZE as u64);
                VirtualFree(host_page, 0, MEM_RELEASE);
                (api.delete)(partition);
            }
            return check_hr(hr, "WHvCreateVirtualProcessor(0)");
        }

        let hr = unsafe { (api.delete_vp)(partition, 0) };
        if hr < 0 {
            unsafe {
                (api.unmap_gpa)(partition, 0, PAGE_SIZE as u64);
                VirtualFree(host_page, 0, MEM_RELEASE);
                (api.delete)(partition);
            }
            return check_hr(hr, "WHvDeleteVirtualProcessor(0)");
        }

        unsafe {
            (api.unmap_gpa)(partition, 0, PAGE_SIZE as u64);
            VirtualFree(host_page, 0, MEM_RELEASE);
            (api.delete)(partition);
        }

        Ok(())
    }

    // ── Boot VM ──

    pub fn boot_vm(req: &super::HelperRequest) -> Result<(), String> {
        use super::bzimage;

        // 1. 讀取 kernel + initramfs
        let kernel_data = std::fs::read(&req.kernel)
            .map_err(|e| format!("Failed to read kernel {}: {e}", req.kernel.display()))?;
        let initramfs_data = std::fs::read(&req.initramfs)
            .map_err(|e| format!("Failed to read initramfs {}: {e}", req.initramfs.display()))?;

        // 2. 解析 bzImage
        let info = bzimage::parse(&kernel_data)?;
        eprintln!(
            "bzImage: protocol {}.{:02}, kernel at file offset 0x{:X} ({} bytes)",
            info.protocol_version >> 8,
            info.protocol_version & 0xFF,
            info.kernel_offset,
            info.kernel_size,
        );

        // 3. 計算 layout
        let mem_size = req.memory_mib as usize * 1024 * 1024;
        let initrd_gpa = bzimage::initrd_gpa(info.kernel_size);
        let initrd_end = initrd_gpa as usize + initramfs_data.len();
        if initrd_end > mem_size {
            return Err(format!(
                "Kernel + initramfs ({initrd_end} bytes) exceed allocated memory ({} MiB)",
                req.memory_mib,
            ));
        }

        // 4. 載入 WHP API + 建立 partition
        let api = WhpApi::load()?;
        let mut partition: *mut c_void = std::ptr::null_mut();
        check_hr(
            unsafe { (api.create)(&mut partition) },
            "WHvCreatePartition",
        )?;

        let cpu_count = req.cpus as u32;
        let hr = unsafe {
            (api.set_property)(
                partition,
                WHV_PROPERTY_PROCESSOR_COUNT,
                (&cpu_count as *const u32).cast(),
                size_of::<u32>() as u32,
            )
        };
        if hr < 0 {
            unsafe { (api.delete)(partition) };
            return check_hr(hr, "WHvSetPartitionProperty(ProcessorCount)");
        }

        // 啟用 Local APIC 模擬（xAPIC mode）
        let apic_mode: u32 = 1; // WHvX64LocalApicEmulationModeXApic
        let hr = unsafe {
            (api.set_property)(
                partition,
                0x0000_0003, // WHvPartitionPropertyCodeLocalApicEmulationMode
                (&apic_mode as *const u32).cast(),
                4,
            )
        };
        if hr < 0 {
            eprintln!("Warning: LAPIC emulation not set (0x{hr:08X})");
        }

        // 設定 exception exit bitmap：攔截所有 x86 異常以便診斷
        let exc_bitmap: u64 = 0xFFFF_FFFF;
        let hr = unsafe {
            (api.set_property)(
                partition,
                0x0000_0002, // WHvPartitionPropertyCodeExceptionExitBitmap
                (&exc_bitmap as *const u64).cast(),
                8,
            )
        };
        if hr < 0 {
            eprintln!("Warning: exception bitmap not set (0x{hr:08X})");
        }

        let hr = unsafe { (api.setup)(partition) };
        if hr < 0 {
            unsafe { (api.delete)(partition) };
            return check_hr(hr, "WHvSetupPartition");
        }

        // 5. 配置 guest 記憶體
        let host_mem = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                mem_size,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if host_mem.is_null() {
            unsafe { (api.delete)(partition) };
            return Err(format!("VirtualAlloc({mem_size} bytes) failed"));
        }

        let hr = unsafe {
            (api.map_gpa)(
                partition,
                host_mem,
                0,
                mem_size as u64,
                WHV_MAP_GPA_RANGE_FLAGS_RWX,
            )
        };
        if hr < 0 {
            unsafe {
                VirtualFree(host_mem, 0, MEM_RELEASE);
                (api.delete)(partition);
            }
            return check_hr(hr, "WHvMapGpaRange");
        }

        let mem: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(host_mem as *mut u8, mem_size) };

        // 5b. 映射 LAPIC MMIO 頁面（GPA 0xFEE00000, 4KB）
        // WHP LAPIC emulation 在此平台不攔截 MMIO，改用 dummy 記憶體頁
        const GPA_LAPIC: u64 = 0xFEE0_0000;
        let lapic_page = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4096,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if !lapic_page.is_null() {
            let lapic = unsafe { std::slice::from_raw_parts_mut(lapic_page as *mut u8, 4096) };
            // LAPIC Version (0x30): version=0x14, max_lvt=5
            lapic[0x30..0x34].copy_from_slice(&0x0005_0014u32.to_le_bytes());
            // Spurious IRQ Vector (0xF0): APIC software enable + vector 0xFF
            lapic[0xF0..0xF4].copy_from_slice(&0x0000_01FFu32.to_le_bytes());

            let hr = unsafe {
                (api.map_gpa)(
                    partition,
                    lapic_page,
                    GPA_LAPIC,
                    4096,
                    WHV_MAP_GPA_RANGE_FLAGS_RWX,
                )
            };
            if hr < 0 {
                eprintln!("Warning: LAPIC page map failed (0x{hr:08X})");
                unsafe { VirtualFree(lapic_page, 0, MEM_RELEASE) };
            }
        }

        // 6. 載入 kernel
        let k_start = bzimage::GPA_KERNEL as usize;
        mem[k_start..k_start + info.kernel_size]
            .copy_from_slice(&kernel_data[info.kernel_offset..]);

        // 7. 載入 initramfs
        let ir_start = initrd_gpa as usize;
        mem[ir_start..ir_start + initramfs_data.len()].copy_from_slice(&initramfs_data);

        // 8. 寫入 boot_params + cmdline + GDT + page directory
        bzimage::write_boot_params(
            mem,
            &info,
            &req.cmdline,
            initrd_gpa,
            initramfs_data.len() as u64,
            mem_size as u64,
        )?;
        let gdt = bzimage::build_gdt();
        let g = bzimage::GPA_GDT as usize;
        mem[g..g + gdt.len()].copy_from_slice(&gdt);

        // 不需要預建 page tables——kernel 自行建 PML4 + 開啟 PAE paging

        // 9. 建立 vCPU
        let hr = unsafe { (api.create_vp)(partition, 0, 0) };
        if hr < 0 {
            unsafe {
                (api.unmap_gpa)(partition, 0, mem_size as u64);
                VirtualFree(host_mem, 0, MEM_RELEASE);
                (api.delete)(partition);
            }
            return check_hr(hr, "WHvCreateVirtualProcessor");
        }

        // 10. 設定初始暫存器
        set_initial_registers(&api, partition, &info)?;

        eprintln!(
            "Starting VM: {} vCPU(s), {} MiB RAM, kernel entry 0x{:X}",
            req.cpus, req.memory_mib, info.code32_start
        );

        // 11. 啟動 timer 執行緒（每 10ms cancel VP → run loop 注入 timer IRQ）
        let stop_timer = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_flag = stop_timer.clone();
        let part_ptr = partition as usize;
        let cancel_fn = api.cancel_run_vp;
        let timer_thread = std::thread::spawn(move || {
            let partition = part_ptr as *mut c_void;
            while !stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(10));
                if !stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    unsafe { cancel_fn(partition, 0, 0) };
                }
            }
        });

        // 12. 執行 run loop
        let mut serial_port = super::serial::SerialPort::new();
        let mut cmos_addr: u8 = 0;
        let mut pic1 = super::pic::Pic::new();
        let mut pic2 = super::pic::Pic::new();
        let mut pit = super::pic::Pit::new();
        let mut last_printed: usize = 0;

        let result = run_loop(
            &api,
            partition,
            &mut serial_port,
            &mut cmos_addr,
            &mut pic1,
            &mut pic2,
            &mut pit,
            &mut last_printed,
            std::time::Duration::from_secs(req.timeout_secs),
        );

        // 13. 停止 timer 執行緒
        stop_timer.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = timer_thread.join();

        // 14. 印出剩餘 serial 輸出
        flush_serial(&serial_port, &mut last_printed);

        // 13. 清理
        unsafe {
            (api.delete_vp)(partition, 0);
            (api.unmap_gpa)(partition, 0, mem_size as u64);
            VirtualFree(host_mem, 0, MEM_RELEASE);
            (api.delete)(partition);
        }

        result
    }

    fn set_initial_registers(
        api: &WhpApi,
        partition: *mut c_void,
        info: &super::bzimage::BzImageInfo,
    ) -> Result<(), String> {
        use super::bzimage;

        // Linux 32-bit boot protocol 要求的初始狀態：
        //   protected mode, paging OFF, flat segments
        // kernel 自己會設 CR4.PAE, 建 PML4 page tables, 設 EFER.LME,
        // 最後才開 CR0.PG 進入 long mode。
        let names: [u32; 20] = [
            REG_CR0, REG_CR3, REG_CR4, REG_RFLAGS, REG_RIP, REG_RSP, REG_RSI, REG_RBP, REG_RDI,
            REG_RBX, REG_CS, REG_DS, REG_ES, REG_FS, REG_GS, REG_SS, REG_LDTR, REG_TR, REG_IDTR,
            REG_GDTR,
        ];

        // segment attributes（VMX 格式）
        const CS_ATTRS: u16 = 0xC09B;
        const DS_ATTRS: u16 = 0xC093;
        const TR_ATTRS: u16 = 0x008B;

        let values: [[u8; 16]; 20] = [
            reg_u64(0x0000_0031),                    // CR0: PE + NE + ET, NO PG
            reg_u64(0),                              // CR3: 0 (paging off)
            reg_u64(0),                              // CR4: 0 (kernel sets PAE)
            reg_u64(0x0000_0002),                    // RFLAGS: reserved bit 1
            reg_u64(info.code32_start as u64),       // RIP: kernel entry
            reg_u64(bzimage::GPA_STACK_TOP),         // RSP
            reg_u64(bzimage::GPA_BOOT_PARAMS),       // RSI: boot_params address
            reg_u64(0),                              // RBP: 0
            reg_u64(0),                              // RDI: 0
            reg_u64(0),                              // RBX: 0
            reg_seg(0, 0xFFFF_FFFF, 0x10, CS_ATTRS), // CS: __BOOT_CS
            reg_seg(0, 0xFFFF_FFFF, 0x18, DS_ATTRS), // DS: __BOOT_DS
            reg_seg(0, 0xFFFF_FFFF, 0x18, DS_ATTRS), // ES
            reg_seg(0, 0xFFFF_FFFF, 0x18, DS_ATTRS), // FS
            reg_seg(0, 0xFFFF_FFFF, 0x18, DS_ATTRS), // GS
            reg_seg(0, 0xFFFF_FFFF, 0x18, DS_ATTRS), // SS
            reg_seg(0, 0, 0, 0),                     // LDTR: not present
            reg_seg(0, 0x0000_0067, 0x28, TR_ATTRS), // TR: selector=0x28, busy TSS
            reg_table(0, 0xFFFF),                    // IDTR: full range, base 0
            reg_table(bzimage::GPA_GDT, bzimage::GDT_SIZE as u16 - 1), // GDTR
        ];

        let hr = unsafe {
            (api.set_regs)(
                partition,
                0,
                names.as_ptr(),
                names.len() as u32,
                values.as_ptr().cast(),
            )
        };
        check_hr(hr, "WHvSetVirtualProcessorRegisters(initial)")
    }

    #[allow(clippy::too_many_arguments)]
    fn run_loop(
        api: &WhpApi,
        partition: *mut c_void,
        serial: &mut super::serial::SerialPort,
        cmos_addr: &mut u8,
        pic1: &mut super::pic::Pic,
        pic2: &mut super::pic::Pic,
        pit: &mut super::pic::Pit,
        last_printed: &mut usize,
        timeout: std::time::Duration,
    ) -> Result<(), String> {
        let mut exit_ctx = [0u8; 4096];
        let start = std::time::Instant::now();

        loop {
            if start.elapsed() > timeout {
                flush_serial(serial, last_printed);
                return Err(format!(
                    "Guest boot timed out after {} seconds",
                    timeout.as_secs()
                ));
            }
            let hr = unsafe {
                (api.run_vp)(
                    partition,
                    0,
                    exit_ctx.as_mut_ptr().cast(),
                    exit_ctx.len() as u32,
                )
            };
            check_hr(hr, "WHvRunVirtualProcessor")?;

            let reason = u32::from_le_bytes(
                exit_ctx[EC_EXIT_REASON..EC_EXIT_REASON + 4]
                    .try_into()
                    .unwrap(),
            );

            match reason {
                EXIT_IO_PORT => {
                    handle_io_exit(
                        api, partition, &exit_ctx, serial, cmos_addr, pic1, pic2, pit,
                    )?;
                    flush_serial(serial, last_printed);
                }
                EXIT_HALT | EXIT_NONE | EXIT_CANCELED => {
                    flush_serial(serial, last_printed);
                    let output = serial.output_str();
                    if let Some(code) = parse_guest_exit(&output) {
                        if code != 0 {
                            return Err(format!("Guest exited with code {code}"));
                        }
                        return Ok(());
                    }
                    if output.contains("Kernel panic") {
                        return Err("Kernel panic".to_string());
                    }
                    // RFLAGS.IF 檢查：如果 guest 在 IF=0（interrupts disabled）時 halt，
                    // 代表 kernel 已完成 shutdown（cli; hlt loop）
                    let rflags =
                        u64::from_le_bytes(exit_ctx[EC_RFLAGS..EC_RFLAGS + 8].try_into().unwrap());
                    if rflags & 0x200 == 0 && reason == EXIT_HALT {
                        // IF=0 + HLT = kernel shutdown（reboot/halt/poweroff）
                        if output.contains("System halted") || output.contains("Power down") {
                            return Ok(());
                        }
                        return Err("Guest halted with interrupts disabled".to_string());
                    }
                    if rflags & 0x200 != 0 {
                        let _ = api.inject_timer_irq(partition);
                    }
                }
                EXIT_MEM_ACCESS => {
                    let gpa = u64::from_le_bytes(
                        exit_ctx[EC_MEM_GPA..EC_MEM_GPA + 8].try_into().unwrap(),
                    );
                    let rip = u64::from_le_bytes(exit_ctx[EC_RIP..EC_RIP + 8].try_into().unwrap());
                    return Err(format!(
                        "Memory access fault at GPA 0x{gpa:016X} (RIP=0x{rip:016X})"
                    ));
                }
                EXIT_EXCEPTION => {
                    let rip = u64::from_le_bytes(exit_ctx[EC_RIP..EC_RIP + 8].try_into().unwrap());
                    let exc_type = exit_ctx[EC_EXC_TYPE];
                    let error_code = u32::from_le_bytes(
                        exit_ctx[EC_EXC_ERROR..EC_EXC_ERROR + 4].try_into().unwrap(),
                    );
                    let exc_param = u64::from_le_bytes(
                        exit_ctx[EC_EXC_PARAM..EC_EXC_PARAM + 8].try_into().unwrap(),
                    );
                    let name = match exc_type {
                        0x00 => "#DE (divide error)",
                        0x01 => "#DB (debug)",
                        0x06 => "#UD (invalid opcode)",
                        0x07 => "#NM (device not available)",
                        0x08 => "#DF (double fault)",
                        0x0A => "#TS (invalid TSS)",
                        0x0B => "#NP (segment not present)",
                        0x0C => "#SS (stack fault)",
                        0x0D => "#GP (general protection)",
                        0x0E => "#PF (page fault)",
                        _ => "unknown",
                    };
                    eprintln!(
                        "Exception {name} (vec=0x{exc_type:02X}) at RIP=0x{rip:016X}, error=0x{error_code:08X}, param=0x{exc_param:016X}"
                    );
                    // #PF: param 是 faulting address (CR2)
                    if exc_type == 0x0E {
                        eprintln!("  Page fault at address 0x{exc_param:016X}");
                    }
                    return Err(format!("Guest exception {name} at RIP=0x{rip:016X}"));
                }
                EXIT_UNRECOVERABLE => {
                    let rip = u64::from_le_bytes(exit_ctx[EC_RIP..EC_RIP + 8].try_into().unwrap());
                    return Err(format!(
                        "Unrecoverable exception (triple fault) at RIP=0x{rip:016X}"
                    ));
                }
                EXIT_INVALID_VP_STATE => {
                    let rip = u64::from_le_bytes(exit_ctx[EC_RIP..EC_RIP + 8].try_into().unwrap());
                    return Err(format!("Invalid VP register state at RIP=0x{rip:016X}"));
                }
                other => {
                    return Err(format!("Unhandled VM exit reason 0x{other:04X}"));
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_io_exit(
        api: &WhpApi,
        partition: *mut c_void,
        ctx: &[u8; 4096],
        serial: &mut super::serial::SerialPort,
        cmos_addr: &mut u8,
        pic1: &mut super::pic::Pic,
        pic2: &mut super::pic::Pic,
        pit: &mut super::pic::Pit,
    ) -> Result<(), String> {
        let instr_len = (ctx[EC_INSTR_LEN] & 0x0F) as u64;
        let rip = u64::from_le_bytes(ctx[EC_RIP..EC_RIP + 8].try_into().unwrap());
        let access_info = u32::from_le_bytes(
            ctx[EC_IO_ACCESS_INFO..EC_IO_ACCESS_INFO + 4]
                .try_into()
                .unwrap(),
        );
        let is_write = access_info & 1 != 0;
        let access_size = ((access_info >> 1) & 0x7) as u8;
        let port = u16::from_le_bytes(ctx[EC_IO_PORT..EC_IO_PORT + 2].try_into().unwrap());
        let rax = u64::from_le_bytes(ctx[EC_IO_RAX..EC_IO_RAX + 8].try_into().unwrap());

        let mut new_rax = rax;

        if is_write {
            let val = rax as u8;
            if super::serial::SerialPort::handles(port) {
                serial.write(port, val);
            } else if port == 0x70 {
                *cmos_addr = val & 0x7F;
            } else if port == super::pic::PIC1_CMD {
                pic1.write_cmd(val);
            } else if port == super::pic::PIC1_DATA {
                pic1.write_data(val);
            } else if port == super::pic::PIC2_CMD {
                pic2.write_cmd(val);
            } else if port == super::pic::PIC2_DATA {
                pic2.write_data(val);
            } else if super::pic::pit_handles(port) {
                if port == super::pic::PIT_CMD {
                    pit.write_cmd(val);
                } else {
                    pit.write((port - super::pic::PIT_CH0) as usize, val);
                }
            }
            // 其餘 port: ignore
        } else {
            let val: u8 = if super::serial::SerialPort::handles(port) {
                serial.read(port)
            } else if port == 0x71 {
                super::serial::cmos_read(*cmos_addr)
            } else if port == super::pic::PIC1_CMD {
                pic1.read_cmd()
            } else if port == super::pic::PIC1_DATA {
                pic1.read_data()
            } else if port == super::pic::PIC2_CMD {
                pic2.read_cmd()
            } else if port == super::pic::PIC2_DATA {
                pic2.read_data()
            } else if super::pic::pit_handles(port) {
                if port == super::pic::PIT_CMD {
                    0xFF // command register 不可讀
                } else {
                    pit.read((port - super::pic::PIT_CH0) as usize)
                }
            } else if port == 0x61 {
                // System Control Port B：bit 5 = PIT channel 2 output
                // 模擬 timer output 交替，讓 TSC calibration 不會卡住
                pit.tick_port61()
            } else if port == 0x64 {
                0x00 // keyboard controller status
            } else {
                0xFF
            };

            match access_size {
                1 => new_rax = (rax & !0xFF) | val as u64,
                2 => new_rax = (rax & !0xFFFF) | val as u64,
                _ => new_rax = val as u64,
            }
        }

        // 推進 RIP + 更新 RAX
        let names = [REG_RIP, REG_RAX];
        let values = [reg_u64(rip + instr_len), reg_u64(new_rax)];
        let hr = unsafe { (api.set_regs)(partition, 0, names.as_ptr(), 2, values.as_ptr().cast()) };
        check_hr(hr, "WHvSetVirtualProcessorRegisters(advance RIP)")
    }

    fn flush_serial(serial: &super::serial::SerialPort, last_printed: &mut usize) {
        use std::io::Write;
        let output = serial.output_str();
        if output.len() > *last_printed {
            let new = &output[*last_printed..];
            print!("{new}");
            let _ = std::io::stdout().flush();
            *last_printed = output.len();
        }
    }

    fn parse_guest_exit(output: &str) -> Option<i32> {
        for line in output.lines().rev() {
            if let Some(rest) = line.strip_prefix(GUEST_EXIT_MARKER) {
                return rest.trim().parse().ok();
            }
            if let Some(idx) = line.find(GUEST_EXIT_MARKER) {
                let rest = &line[idx + GUEST_EXIT_MARKER.len()..];
                return rest.trim().parse().ok();
            }
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn reg_u64_layout() {
            let v = reg_u64(0x1234_5678_9ABC_DEF0);
            assert_eq!(
                u64::from_le_bytes(v[..8].try_into().unwrap()),
                0x1234_5678_9ABC_DEF0
            );
            assert_eq!(u64::from_le_bytes(v[8..16].try_into().unwrap()), 0);
        }

        #[test]
        fn reg_seg_layout() {
            let v = reg_seg(0x1000, 0xFFFF_FFFF, 0x10, 0xC09B);
            assert_eq!(u64::from_le_bytes(v[..8].try_into().unwrap()), 0x1000); // base
            assert_eq!(
                u32::from_le_bytes(v[8..12].try_into().unwrap()),
                0xFFFF_FFFF
            ); // limit
            assert_eq!(u16::from_le_bytes(v[12..14].try_into().unwrap()), 0x10); // selector
            assert_eq!(u16::from_le_bytes(v[14..16].try_into().unwrap()), 0xC09B); // attrs
        }

        #[test]
        fn reg_table_layout() {
            let v = reg_table(0x1000, 31);
            assert_eq!(u16::from_le_bytes(v[6..8].try_into().unwrap()), 31); // limit
            assert_eq!(u64::from_le_bytes(v[8..16].try_into().unwrap()), 0x1000); // base
        }

        #[test]
        fn parse_guest_exit_code() {
            assert_eq!(parse_guest_exit("CHEFER_GUEST_EXIT=0"), Some(0));
            assert_eq!(parse_guest_exit("CHEFER_GUEST_EXIT=42"), Some(42));
            assert_eq!(parse_guest_exit("boot log\nCHEFER_GUEST_EXIT=1\n"), Some(1));
            assert_eq!(parse_guest_exit("no marker here"), None);
        }
    }
}

// ---------------------------------------------------------------------------
// CLI contract
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct HelperRequest {
    kernel: PathBuf,
    initramfs: PathBuf,
    cmdline: String,
    bundle_dir: PathBuf,
    data_dir: PathBuf,
    cpus: u16,
    memory_mib: u64,
    timeout_secs: u64,
}

impl HelperRequest {
    fn parse(args: Vec<String>) -> Result<Self, String> {
        let mut parser = ArgParser::new(args);
        let timeout_secs = if parser.has("--timeout") {
            parser
                .value("--timeout")?
                .parse::<u64>()
                .map_err(|_| "usage: --timeout must be a positive integer".to_string())?
        } else {
            300
        };
        let request = HelperRequest {
            kernel: parser.path("--kernel")?,
            initramfs: parser.path("--initramfs")?,
            cmdline: parser.value("--cmdline")?,
            bundle_dir: parser.path("--bundle-dir")?,
            data_dir: parser.path("--data-dir")?,
            cpus: parse_cpus(&parser.value("--cpus")?)?,
            memory_mib: parse_memory_mib(&parser.value("--memory-mib")?)?,
            timeout_secs,
        };
        parser.finish()?;
        Ok(request)
    }
}

struct ArgParser {
    args: Vec<String>,
}

impl ArgParser {
    fn new(args: Vec<String>) -> Self {
        Self { args }
    }

    fn has(&self, flag: &str) -> bool {
        self.args.iter().any(|a| a == flag)
    }

    fn value(&mut self, flag: &str) -> Result<String, String> {
        let Some(pos) = self.args.iter().position(|arg| arg == flag) else {
            return Err(format!("usage: missing required argument {flag}"));
        };
        self.args.remove(pos);
        if pos >= self.args.len() {
            return Err(format!("usage: {flag} requires a value"));
        }
        let value = self.args.remove(pos);
        if value.starts_with("--") {
            return Err(format!("usage: {flag} requires a value"));
        }
        Ok(value)
    }

    fn path(&mut self, flag: &str) -> Result<PathBuf, String> {
        Ok(PathBuf::from(self.value(flag)?))
    }

    fn finish(self) -> Result<(), String> {
        if self.args.is_empty() {
            Ok(())
        } else {
            Err(format!("usage: unexpected argument {}", self.args[0]))
        }
    }
}

fn parse_cpus(value: &str) -> Result<u16, String> {
    let cpus = value
        .parse::<u16>()
        .map_err(|_| format!("usage: --cpus must be a positive integer, got {value}"))?;
    if cpus == 0 {
        return Err("usage: --cpus must be at least 1".to_string());
    }
    Ok(cpus)
}

fn parse_memory_mib(value: &str) -> Result<u64, String> {
    let memory_mib = value
        .parse::<u64>()
        .map_err(|_| format!("usage: --memory-mib must be a positive integer, got {value}"))?;
    if memory_mib < 512 {
        return Err("usage: --memory-mib must be at least 512".to_string());
    }
    Ok(memory_mib)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_args() -> Vec<String> {
        [
            "--kernel",
            "vm/vmlinuz",
            "--initramfs",
            "vm/initramfs",
            "--cmdline",
            "console=ttyS0",
            "--bundle-dir",
            "bundle",
            "--data-dir",
            "data",
            "--cpus",
            "2",
            "--memory-mib",
            "1024",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    }

    #[test]
    fn parses_contract_arguments() {
        let req = HelperRequest::parse(valid_args()).unwrap();

        assert_eq!(req.kernel, PathBuf::from("vm/vmlinuz"));
        assert_eq!(req.initramfs, PathBuf::from("vm/initramfs"));
        assert_eq!(req.cmdline, "console=ttyS0");
        assert_eq!(req.bundle_dir, PathBuf::from("bundle"));
        assert_eq!(req.data_dir, PathBuf::from("data"));
        assert_eq!(req.cpus, 2);
        assert_eq!(req.memory_mib, 1024);
        assert_eq!(req.timeout_secs, 300); // default
    }

    #[test]
    fn parses_explicit_timeout() {
        let mut args = valid_args();
        args.push("--timeout".to_string());
        args.push("60".to_string());
        let req = HelperRequest::parse(args).unwrap();
        assert_eq!(req.timeout_secs, 60);
    }

    #[test]
    fn rejects_missing_required_argument() {
        let mut args = valid_args();
        args.drain(0..2);

        let err = HelperRequest::parse(args).unwrap_err();
        assert!(err.contains("--kernel"));
    }

    #[test]
    fn rejects_unexpected_argument() {
        let mut args = valid_args();
        args.push("--extra".to_string());

        let err = HelperRequest::parse(args).unwrap_err();
        assert!(err.contains("unexpected argument --extra"));
    }

    #[test]
    fn rejects_zero_cpus_and_tiny_memory() {
        assert!(parse_cpus("0").unwrap_err().contains("at least 1"));
        assert!(
            parse_memory_mib("128")
                .unwrap_err()
                .contains("at least 512")
        );
    }

    #[test]
    fn help_flag_succeeds() {
        assert!(run(["--help".to_string()].into_iter()).is_ok());
        assert!(run(["-h".to_string()].into_iter()).is_ok());
    }

    #[test]
    fn full_invocation_needs_valid_files() {
        let err = run(valid_args()).unwrap_err();
        // On Windows: fails reading the kernel file (doesn't exist)
        // On non-Windows: "WHP boot requires Windows"
        assert!(
            err.contains("Failed to read") || err.contains("requires Windows"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn preflight_parses_standalone() {
        let args = vec!["--preflight".to_string()];
        let result = run_preflight(args);
        if let Err(e) = &result {
            assert!(!e.starts_with("usage:"));
        }
    }

    #[test]
    fn preflight_parses_with_cpus() {
        let args = vec![
            "--preflight".to_string(),
            "--cpus".to_string(),
            "4".to_string(),
        ];
        let result = run_preflight(args);
        if let Err(e) = &result {
            assert!(!e.starts_with("usage:"));
        }
    }

    #[test]
    fn preflight_rejects_unexpected_args() {
        let args = vec!["--preflight".to_string(), "--kernel".to_string()];
        let err = run_preflight(args).unwrap_err();
        assert!(err.starts_with("usage:"));
        assert!(err.contains("unexpected"));
    }

    #[test]
    fn preflight_rejects_cpus_without_value() {
        let args = vec!["--preflight".to_string(), "--cpus".to_string()];
        let err = run_preflight(args).unwrap_err();
        assert!(err.contains("--cpus requires a value"));
    }
}
