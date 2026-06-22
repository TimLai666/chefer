use std::io::{Read, Write};

// WHP virtio-blk 實機驗證用的最小 init：
// 掛 proc/dev → 嘗試讀 /dev/vda（virtio-blk 傳入的 bundle tar image）→ 在 raw bytes
// 找 host 餵入的 marker → 把結果寫到 /dev/kmsg（polled printk，會出現在 serial）→ 關機。
// 寫 /dev/kmsg 而非 tty：WHP 最小 8250 不觸發 serial IRQ4，user-space 寫 tty 不可靠。

fn report(msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = writeln!(f, "{msg}");
        let _ = f.flush();
    } else {
        let _ = writeln!(std::io::stdout(), "{msg}");
        let _ = std::io::stdout().flush();
    }
}

fn probe_vda() -> String {
    // /dev/vda 由 kernel 的 virtio-blk probe 動態建立（devtmpfs）；PID1 起來時可能尚未完成，
    // 故 retry 等待最多約 5 秒。
    for _ in 0..50 {
        match std::fs::File::open("/dev/vda") {
            Ok(mut f) => {
                let mut buf = vec![0u8; 8192];
                return match f.read(&mut buf) {
                    Ok(n) if buf[..n].windows(16).any(|w| w == b"CHEFER_VIRTIO_OK") => {
                        "CHEFER_VIRTIO_OK".to_string()
                    }
                    Ok(n) => format!("CHEFER_VDA_NO_MARKER read={n}"),
                    Err(e) => format!("CHEFER_VDA_READ_FAIL errno={:?}", e.raw_os_error()),
                };
            }
            Err(_) => {
                #[cfg(target_os = "linux")]
                unsafe {
                    libc::usleep(100_000);
                }
            }
        }
    }
    "CHEFER_VDA_OPEN_FAIL".to_string()
}

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

    // 驗證 virtio-blk：讀 /dev/vda 找 marker，結果寫到 serial（經 /dev/kmsg）。
    let result = probe_vda();
    report(&result);
    // 沿用既有的 exit code 通道，讓 host 端能判定乾淨結束。
    report("CHEFER_GUEST_EXIT=0");

    #[cfg(target_os = "linux")]
    unsafe {
        libc::sync();
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
}
