//! macOS `vz` 後端（Virtualization.framework）——v1 骨架。
//!
//! 完整實作需要 guest kit（kernel + initrd），將於後續版本提供；
//! 此處保留 trait 實作位置，availability 一律回報不可用。

use anyhow::Result;

use crate::{AppRunContext, Availability, ExecBackend};

/// 未支援訊息（依 docs/DESIGN.md §6）。
const UNSUPPORTED_MSG: &str = "macOS 後端需要 guest kit（kernel+initrd），將於後續版本提供；\
                               目前請於 Linux 或 Windows 執行";

/// macOS Virtualization.framework 後端（骨架）。
pub struct VzBackend;

impl ExecBackend for VzBackend {
    fn name(&self) -> &'static str {
        "vz"
    }

    fn availability(&self) -> Availability {
        // v1：檢查 OS 版本後仍回報不可用（骨架）
        Availability::Unavailable(UNSUPPORTED_MSG.to_string())
    }

    fn run(&self, _ctx: &AppRunContext) -> Result<i32> {
        anyhow::bail!("{UNSUPPORTED_MSG}")
    }
}
