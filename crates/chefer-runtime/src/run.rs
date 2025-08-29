// src/run.rs
use anyhow::Result;
use camino::Utf8PathBuf;
use fs_err as fs;

#[derive(Debug)]
pub struct RuntimeContext {
    pub bundle_dir: Utf8PathBuf,
}

pub fn run(ctx: &RuntimeContext) -> Result<()> {
    // 後面會：
    // 1) 讀 ctx.bundle_dir/manifest.json
    // 2) 起 microVM（vmm-backend），mount services/*/rootfs、注入 rt/kernel/initrd/agent
    // 3) 檢查 service depends_on、interface_mode，配好網路/port
    // 4) 監控 guest-agent 回報的 service 狀態

    let mani = ctx.bundle_dir.join("manifest.json");
    if !fs::metadata(&mani).is_ok() {
        anyhow::bail!("manifest.json not found in {}", mani);
    }
    tracing::info!("(stub) manifest OK at {}", mani);
    Ok(())
}
