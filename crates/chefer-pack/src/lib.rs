mod api;
mod bundle;
mod image;

pub use api::*;

use anyhow::Result;
use appcipe_spec::AppCipe;

pub(crate) fn lib_pack_all(app: &AppCipe, opts: &PackOptions) -> Result<PackResult> {
    let layout = bundle::prepare_layout(app, opts)?;

    // 解每個 service 的 rootfs
    for (name, svc) in &app.services {
        image::extract_rootfs(&layout, name, svc)?; // MVP: 支援 image: tar
    }

    // 寫入 manifest / persist-map / appcipe.yml（可選）
    bundle::write_metadata(&layout, app, opts)?;

    Ok(PackResult { bundle_dir: layout.bundle_dir.clone() })
}
