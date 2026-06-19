//! Windows `whp` backend scaffold.
//!
//! WHP is the planned non-WSL Windows backend. It will boot the same Linux
//! appliance used by the VM paths through Windows Hypervisor Platform, but the
//! host shim does not exist yet. Keep this backend visible in selection and
//! diagnostics so users get an honest fallback story when WSL2 is unavailable.

use anyhow::Result;

use crate::{AppRunContext, Availability, ExecBackend};

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
    "The Windows Hypervisor Platform (whp) backend is planned but not implemented yet; \
     install or repair WSL2 to run Chefer apps on Windows today."
        .to_string()
}
