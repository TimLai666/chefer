// crates/chefer-pack/src/api.rs
use std::path::PathBuf;
use anyhow::Result;
use appcipe_spec::AppCipe;

#[derive(Clone, Debug)]
pub struct PackOptions { pub out_dir: PathBuf, pub clean: bool, pub write_original_yml: bool, pub squashfs: bool }
#[derive(Clone, Debug)]
pub struct PackResult { pub bundle_dir: PathBuf }

pub fn pack_all(app: &AppCipe, opts: &PackOptions) -> Result<PackResult> {
    crate::lib_pack_all(app, opts)
}
