//! Windows `whp` backend scaffold.
//!
//! WHP is the planned non-WSL Windows backend. It will boot the same Linux
//! appliance used by the VM paths through Windows Hypervisor Platform, but the
//! host shim does not exist yet. Keep this backend visible in selection and use
//! a host preflight probe so diagnostics can say whether WHP itself is ready
//! without presenting the runtime path as supported.

use anyhow::Result;
use std::ffi::c_void;
use std::mem::size_of;

use windows_sys::Win32::Foundation::FreeLibrary;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExA,
};

use crate::{AppRunContext, Availability, ExecBackend};

const WIN_HV_PLATFORM_DLL: &[u8] = b"WinHvPlatform.dll\0";
const WHV_GET_CAPABILITY: &[u8] = b"WHvGetCapability\0";
const WHV_CAPABILITY_CODE_HYPERVISOR_PRESENT: u32 = 0;

type WhvGetCapability = unsafe extern "system" fn(
    capability_code: u32,
    capability_buffer: *mut c_void,
    capability_buffer_size_in_bytes: u32,
    written_size_in_bytes: *mut u32,
) -> i32;

pub struct WhpBackend;

impl ExecBackend for WhpBackend {
    fn name(&self) -> &'static str {
        "whp"
    }

    fn availability(&self, _ctx: &AppRunContext) -> Availability {
        availability()
    }

    fn run(&self, _ctx: &AppRunContext) -> Result<i32> {
        anyhow::bail!("{}", availability_reason())
    }
}

pub(crate) fn availability() -> Availability {
    Availability::Unavailable(availability_reason())
}

fn availability_reason() -> String {
    availability_reason_for(probe_host_capability())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WhpHostCapability {
    ApiUnavailable,
    ProbeUnavailable,
    ProbeFailed(i32),
    HypervisorAbsent,
    HypervisorPresent,
}

fn availability_reason_for(capability: WhpHostCapability) -> String {
    let preflight = match capability {
        WhpHostCapability::ApiUnavailable => {
            "The Windows Hypervisor Platform (WHP) API is not available \
             (WinHvPlatform.dll could not be loaded); enable the Windows Hypervisor \
             Platform optional feature and reboot."
                .to_string()
        }
        WhpHostCapability::ProbeUnavailable => {
            "The Windows Hypervisor Platform (WHP) API is present, but \
             WHvGetCapability could not be found; update Windows or use WSL2."
                .to_string()
        }
        WhpHostCapability::ProbeFailed(hr) => format!(
            "The Windows Hypervisor Platform (WHP) API probe failed \
             (WHvGetCapability HypervisorPresent returned HRESULT 0x{:08X}).",
            hr as u32
        ),
        WhpHostCapability::HypervisorAbsent => {
            "The Windows Hypervisor Platform (WHP) API is installed, but \
             WHvGetCapability reports no active hypervisor; enable hardware \
             virtualization and the WHP feature, then reboot."
                .to_string()
        }
        WhpHostCapability::HypervisorPresent => {
            "The Windows Hypervisor Platform (WHP) API is available and an active \
             hypervisor is present."
                .to_string()
        }
    };

    format!(
        "{preflight} The Chefer whp host shim is not implemented yet; use or repair WSL2 \
         to run Chefer apps on Windows today."
    )
}

fn probe_host_capability() -> WhpHostCapability {
    // SAFETY: The names are static NUL-terminated byte strings. We load the WHP
    // DLL dynamically from System32 so hosts without the optional feature still
    // get a diagnostic instead of a load-time failure, without using the
    // process DLL search path.
    let module = unsafe {
        LoadLibraryExA(
            WIN_HV_PLATFORM_DLL.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if module.is_null() {
        return WhpHostCapability::ApiUnavailable;
    }

    // SAFETY: The module handle came from LoadLibraryA above and remains loaded
    // until FreeLibrary below.
    let proc = unsafe { GetProcAddress(module, WHV_GET_CAPABILITY.as_ptr()) };
    let Some(proc) = proc else {
        // SAFETY: The handle is valid because LoadLibraryA succeeded.
        unsafe {
            FreeLibrary(module);
        }
        return WhpHostCapability::ProbeUnavailable;
    };

    // SAFETY: WHvGetCapability is exported by WinHvPlatform.dll with this ABI
    // and signature. The call below passes a BOOL-sized buffer for the
    // HypervisorPresent capability.
    let whv_get_capability: WhvGetCapability = unsafe { std::mem::transmute(proc) };
    let mut hypervisor_present = 0i32;
    let mut written = 0u32;
    // SAFETY: The buffer and written-size pointers are valid for the duration of
    // the call, and the buffer size matches the BOOL value requested.
    let hr = unsafe {
        whv_get_capability(
            WHV_CAPABILITY_CODE_HYPERVISOR_PRESENT,
            (&mut hypervisor_present as *mut i32).cast::<c_void>(),
            size_of::<i32>() as u32,
            &mut written,
        )
    };
    // SAFETY: The handle is valid because LoadLibraryA succeeded.
    unsafe {
        FreeLibrary(module);
    }

    if hr < 0 {
        return WhpHostCapability::ProbeFailed(hr);
    }

    if hypervisor_present == 0 {
        WhpHostCapability::HypervisorAbsent
    } else {
        WhpHostCapability::HypervisorPresent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_missing_whp_api_with_install_hint() {
        let reason = availability_reason_for(WhpHostCapability::ApiUnavailable);

        assert!(reason.contains("WinHvPlatform.dll"));
        assert!(reason.contains("not implemented"));
        assert!(reason.contains("WSL2"));
    }

    #[test]
    fn formats_hypervisor_absent_with_reboot_hint() {
        let reason = availability_reason_for(WhpHostCapability::HypervisorAbsent);

        assert!(reason.contains("no active hypervisor"));
        assert!(reason.contains("reboot"));
        assert!(reason.contains("not implemented"));
    }

    #[test]
    fn formats_hypervisor_present_as_still_unavailable() {
        let reason = availability_reason_for(WhpHostCapability::HypervisorPresent);

        assert!(reason.contains("active hypervisor is present"));
        assert!(reason.contains("host shim is not implemented"));
        assert!(reason.contains("WSL2"));
    }

    #[test]
    fn formats_probe_hresult() {
        let reason = availability_reason_for(WhpHostCapability::ProbeFailed(-1));

        assert!(reason.contains("HRESULT 0xFFFFFFFF"));
        assert!(reason.contains("not implemented"));
    }
}
