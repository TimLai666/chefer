//! Linux `namespaces` 後端：in-process 呼叫 guest-agent lib，
//! 以 user/mount/pid namespaces 啟動服務（細節在 guest-agent）。

use std::path::Path;

use anyhow::Result;

use crate::{AppRunContext, Availability, ExecBackend};

/// Linux namespaces 後端。
pub struct NamespacesBackend;

impl ExecBackend for NamespacesBackend {
    fn name(&self) -> &'static str {
        "namespaces"
    }

    /// 可用性：/proc/self/ns/user 存在；若 /proc/sys/kernel/unprivileged_userns_clone
    /// 存在則必須為 1（部分發行版用此開關停用非特權 user namespaces）。
    fn availability(&self, _ctx: &AppRunContext) -> Availability {
        if !Path::new("/proc/self/ns/user").exists() {
            return Availability::Unavailable(
                "the kernel does not have user namespaces enabled (missing /proc/self/ns/user); \
                 use a Linux kernel that supports user namespaces (3.8 or newer)"
                    .to_string(),
            );
        }
        let knob = Path::new("/proc/sys/kernel/unprivileged_userns_clone");
        if knob.exists() {
            match std::fs::read_to_string(knob) {
                Ok(v) if v.trim() == "1" => {}
                Ok(_) => {
                    return Availability::Unavailable(
                        "unprivileged user namespaces are disabled on this system; \
                         run `sudo sysctl -w kernel.unprivileged_userns_clone=1` and try again"
                            .to_string(),
                    );
                }
                Err(e) => {
                    return Availability::Unavailable(format!(
                        "could not read /proc/sys/kernel/unprivileged_userns_clone: {e}"
                    ));
                }
            }
        }
        Availability::Available
    }

    fn run(&self, ctx: &AppRunContext) -> Result<i32> {
        guest_agent::run_bundle(&guest_agent::RunConfig {
            bundle_dir: ctx.bundle_dir.to_path_buf(),
            data_dir: ctx.data_dir.to_path_buf(),
            cache_dir: None,
            keep_rootfs: ctx.opts.keep_tmp,
            // 原生 Linux 共享 netns：UDP 埠以 loopback 直接生效，不啟用 VM 內橋接
            // （啟用會綁 eth0/LAN IP，等同把服務暴露到區網）。
            udp_bridge: false,
        })
    }
}
