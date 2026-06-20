//! Windows WHP host helper.
//!
//! Supports two modes:
//! - `--preflight [--cpus <n>]`: validates WHP API by running a partition
//!   lifecycle (create → configure → setup → delete) without booting a guest.
//! - Full invocation (all args): not yet implemented — reserved for the future
//!   WHP device model that will boot the Linux appliance.

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
    Err(format!(
        "chefer-whp-helper does not boot a VM yet. \
         Requested kernel={}, initramfs={}, bundle={}, data={}, cpus={}, memory={}MiB. \
         Use --preflight to validate WHP API, or use the WSL2 backend today.",
        request.kernel.display(),
        request.initramfs.display(),
        request.bundle_dir.display(),
        request.data_dir.display(),
        request.cpus,
        request.memory_mib
    ))
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
         2. Full boot (not yet implemented):\n\
           chefer-whp-helper \\\n\
             --kernel <path> --initramfs <path> --cmdline <text> \\\n\
             --bundle-dir <path> --data-dir <path> --cpus <n> --memory-mib <n>\n\
         \n\
         The preflight mode dynamically loads WinHvPlatform.dll and runs a\n\
         WHP device model lifecycle (partition/GPA mapping/vCPU) to verify the\n\
         API works.\n\
         \n\
         The future full mode will boot the Chefer Linux appliance through\n\
         Windows Hypervisor Platform and stream guest console markers to stdout."
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
// WHP API dynamic loading + partition lifecycle (Windows only)
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

    // WHV_PARTITION_PROPERTY_CODE::WHvPartitionPropertyCodeProcessorCount
    const WHV_PROPERTY_PROCESSOR_COUNT: u32 = 0x0000_1fff;

    // WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute
    const WHV_MAP_GPA_RANGE_FLAGS_RWX: u32 = 0x1 | 0x2 | 0x4;

    type CreatePartitionFn = unsafe extern "system" fn(*mut *mut c_void) -> i32;
    type SetPartitionPropertyFn =
        unsafe extern "system" fn(*mut c_void, u32, *const c_void, u32) -> i32;
    type SetupPartitionFn = unsafe extern "system" fn(*mut c_void) -> i32;
    type DeletePartitionFn = unsafe extern "system" fn(*mut c_void) -> i32;
    type MapGpaRangeFn =
        unsafe extern "system" fn(*mut c_void, *const c_void, u64, u64, u32) -> i32;
    type UnmapGpaRangeFn = unsafe extern "system" fn(*mut c_void, u64, u64) -> i32;
    // WHvCreateVirtualProcessor(Partition, VpIndex, Flags) — Flags reserved, must be 0
    type CreateVpFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> i32;
    type DeleteVpFn = unsafe extern "system" fn(*mut c_void, u32) -> i32;

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
    }

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

            let create = resolve::<CreatePartitionFn>(module, b"WHvCreatePartition\0")?;
            let set_property =
                resolve::<SetPartitionPropertyFn>(module, b"WHvSetPartitionProperty\0")?;
            let setup = resolve::<SetupPartitionFn>(module, b"WHvSetupPartition\0")?;
            let delete = resolve::<DeletePartitionFn>(module, b"WHvDeletePartition\0")?;
            let map_gpa = resolve::<MapGpaRangeFn>(module, b"WHvMapGpaRange\0")?;
            let unmap_gpa = resolve::<UnmapGpaRangeFn>(module, b"WHvUnmapGpaRange\0")?;
            let create_vp = resolve::<CreateVpFn>(module, b"WHvCreateVirtualProcessor\0")?;
            let delete_vp = resolve::<DeleteVpFn>(module, b"WHvDeleteVirtualProcessor\0")?;

            Ok(Self {
                module,
                create,
                set_property,
                setup,
                delete,
                map_gpa,
                unmap_gpa,
                create_vp,
                delete_vp,
            })
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

    /// WHP device model 基礎生命週期：
    /// partition → processor count → setup → map GPA page → create vCPU →
    /// delete vCPU → unmap GPA → delete partition。
    pub fn run_partition_lifecycle(cpus: u16) -> Result<(), String> {
        let api = WhpApi::load()?;

        // --- 1. Create partition ---
        let mut partition: *mut c_void = std::ptr::null_mut();
        let hr = unsafe { (api.create)(&mut partition) };
        if hr < 0 {
            return Err(format!(
                "WHvCreatePartition failed (HRESULT 0x{:08X})",
                hr as u32
            ));
        }

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
            unsafe {
                (api.delete)(partition);
            }
            return Err(format!(
                "WHvSetPartitionProperty(ProcessorCount={cpus}) failed (HRESULT 0x{:08X})",
                hr as u32
            ));
        }

        let hr = unsafe { (api.setup)(partition) };
        if hr < 0 {
            unsafe {
                (api.delete)(partition);
            }
            return Err(format!(
                "WHvSetupPartition failed (HRESULT 0x{:08X})",
                hr as u32
            ));
        }

        // --- 2. Map one page of GPA at address 0 ---
        let host_page = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                PAGE_SIZE,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if host_page.is_null() {
            unsafe {
                (api.delete)(partition);
            }
            return Err("VirtualAlloc for GPA test page failed".to_string());
        }

        let hr = unsafe {
            (api.map_gpa)(
                partition,
                host_page,
                0, // GPA start
                PAGE_SIZE as u64,
                WHV_MAP_GPA_RANGE_FLAGS_RWX,
            )
        };
        if hr < 0 {
            unsafe {
                VirtualFree(host_page, 0, MEM_RELEASE);
                (api.delete)(partition);
            }
            return Err(format!(
                "WHvMapGpaRange failed (HRESULT 0x{:08X})",
                hr as u32
            ));
        }

        // --- 3. Create and immediately delete a virtual processor ---
        let hr = unsafe { (api.create_vp)(partition, 0, 0) };
        if hr < 0 {
            unsafe {
                (api.unmap_gpa)(partition, 0, PAGE_SIZE as u64);
                VirtualFree(host_page, 0, MEM_RELEASE);
                (api.delete)(partition);
            }
            return Err(format!(
                "WHvCreateVirtualProcessor(0) failed (HRESULT 0x{:08X})",
                hr as u32
            ));
        }

        let hr = unsafe { (api.delete_vp)(partition, 0) };
        if hr < 0 {
            unsafe {
                (api.unmap_gpa)(partition, 0, PAGE_SIZE as u64);
                VirtualFree(host_page, 0, MEM_RELEASE);
                (api.delete)(partition);
            }
            return Err(format!(
                "WHvDeleteVirtualProcessor(0) failed (HRESULT 0x{:08X})",
                hr as u32
            ));
        }

        // --- 4. Cleanup ---
        let hr = unsafe { (api.unmap_gpa)(partition, 0, PAGE_SIZE as u64) };
        if hr < 0 {
            unsafe {
                VirtualFree(host_page, 0, MEM_RELEASE);
                (api.delete)(partition);
            }
            return Err(format!(
                "WHvUnmapGpaRange failed (HRESULT 0x{:08X})",
                hr as u32
            ));
        }

        unsafe {
            VirtualFree(host_page, 0, MEM_RELEASE);
        }

        let hr = unsafe { (api.delete)(partition) };
        if hr < 0 {
            return Err(format!(
                "WHvDeletePartition failed (HRESULT 0x{:08X})",
                hr as u32
            ));
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Full boot mode: CLI contract (not yet implemented)
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
}

impl HelperRequest {
    fn parse(args: Vec<String>) -> Result<Self, String> {
        let mut parser = ArgParser::new(args);
        let request = HelperRequest {
            kernel: parser.path("--kernel")?,
            initramfs: parser.path("--initramfs")?,
            cmdline: parser.value("--cmdline")?,
            bundle_dir: parser.path("--bundle-dir")?,
            data_dir: parser.path("--data-dir")?,
            cpus: parse_cpus(&parser.value("--cpus")?)?,
            memory_mib: parse_memory_mib(&parser.value("--memory-mib")?)?,
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
    fn full_invocation_returns_not_implemented() {
        let err = run(valid_args()).unwrap_err();
        assert!(err.contains("does not boot a VM yet"));
    }

    #[test]
    fn preflight_parses_standalone() {
        let args = vec!["--preflight".to_string()];
        // On non-Windows: returns Err about requiring Windows
        // On Windows without WHP: returns Err about WHP API
        let result = run_preflight(args);
        // Just verify it doesn't panic and doesn't return a usage error
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
