use std::io::Write;

fn main() {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        let proc_src = CString::new("proc").unwrap();
        let proc_dst = CString::new("/proc").unwrap();
        let proc_type = CString::new("proc").unwrap();
        unsafe {
            libc::mkdir(proc_dst.as_ptr(), 0o755);
            libc::mount(
                proc_src.as_ptr(),
                proc_dst.as_ptr(),
                proc_type.as_ptr(),
                0,
                std::ptr::null(),
            );
        }

        let dev_src = CString::new("devtmpfs").unwrap();
        let dev_dst = CString::new("/dev").unwrap();
        let dev_type = CString::new("devtmpfs").unwrap();
        unsafe {
            libc::mkdir(dev_dst.as_ptr(), 0o755);
            libc::mount(
                dev_src.as_ptr(),
                dev_dst.as_ptr(),
                dev_type.as_ptr(),
                0,
                std::ptr::null(),
            );
        }
    }

    // 寫到 /dev/kmsg（走 kernel printk polled I/O），避開需要 serial IRQ 的 tty 路徑
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = f.write_all(b"CHEFER_GUEST_EXIT=0\n");
        let _ = f.flush();
    } else {
        let _ = std::io::stdout().write_all(b"\nCHEFER_GUEST_EXIT=0\n");
        let _ = std::io::stdout().flush();
    }

    #[cfg(target_os = "linux")]
    unsafe {
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
}
