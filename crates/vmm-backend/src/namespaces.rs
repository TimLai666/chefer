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
    fn availability(&self) -> Availability {
        if !Path::new("/proc/self/ns/user").exists() {
            return Availability::Unavailable(
                "核心未啟用 user namespaces（缺 /proc/self/ns/user）；\
                 請改用支援 user namespaces 的 Linux 核心（3.8 以上）"
                    .to_string(),
            );
        }
        let knob = Path::new("/proc/sys/kernel/unprivileged_userns_clone");
        if knob.exists() {
            match std::fs::read_to_string(knob) {
                Ok(v) if v.trim() == "1" => {}
                Ok(_) => {
                    return Availability::Unavailable(
                        "系統停用非特權 user namespaces；\
                         請執行 `sudo sysctl -w kernel.unprivileged_userns_clone=1` 後重試"
                            .to_string(),
                    );
                }
                Err(e) => {
                    return Availability::Unavailable(format!(
                        "無法讀取 /proc/sys/kernel/unprivileged_userns_clone：{e}"
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
        })
    }
}
