//! `source: dockerfile` —— 在**打包機**上以既有的 container builder 建置成 docker-archive
//! tar，之後完全併入 `source: tar` 的解析路徑（見 docs/DESIGN.md「Dockerfile build」）。
//!
//! chefer 本身不建 image：偵測並呼叫 host 上的 docker / podman / nerdctl（CLI 相容）。
//! 執行期一如往常不需 Docker——builder 只在打包時用到。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// 依序嘗試的 builder（首個可執行者）。
/// - `docker`/`podman`/`nerdctl`：`build` 與 `save -o` CLI 相容（docker-archive）。
///   macOS 上的 **OrbStack / Docker Desktop** 都提供 drop-in `docker` CLI，故直接走這條。
/// - `container`（Apple 開源 container 工具）：`build` 旗標相容，但 save 是
///   `container image save -o`（產出 OCI archive，chefer 解析端兩種格式都吃）。
const BUILDERS: &[&str] = &["docker", "podman", "nerdctl", "container"];

/// 以 host builder 建置 Dockerfile 成 docker-archive tar，回傳 tar 路徑（位於 `out_dir`）。
pub(crate) fn build_image_tar(
    name: &str,
    dockerfile: &str,
    context: Option<&str>,
    build_args: &HashMap<String, String>,
    platform: &str,
    out_dir: &Path,
) -> Result<PathBuf> {
    let df = Path::new(dockerfile);
    if !df.is_file() {
        bail!(
            "service `{name}`: Dockerfile not found: {dockerfile}. Check image.file in appcipe.yml."
        );
    }
    // context 預設為 Dockerfile 所在目錄。
    let ctx: PathBuf = match context {
        Some(c) => PathBuf::from(c),
        None => df
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
    };
    if !ctx.is_dir() {
        bail!(
            "service `{name}`: build context directory not found: {}. Check image.context in appcipe.yml.",
            ctx.display()
        );
    }

    let builder = detect_builder()?;
    let tag = format!("chefer-build-{name}:{}", std::process::id());

    eprintln!(
        "[chefer] building image for service `{name}` from {dockerfile} with {builder} (platform {platform})…"
    );

    // build —— stdio 直通，讓使用者看到建置過程。
    let mut cmd = Command::new(builder);
    cmd.arg("build").arg("--platform").arg(platform);
    // build_args 依 key 排序，讓相同輸入的命令列固定（雖然 Dockerfile build 本身不保證可重現）。
    let sorted: BTreeMap<&String, &String> = build_args.iter().collect();
    for (k, v) in sorted {
        cmd.arg("--build-arg").arg(format!("{k}={v}"));
    }
    cmd.arg("-f").arg(df).arg("-t").arg(&tag).arg(&ctx);
    let status = cmd
        .status()
        .with_context(|| format!("failed to run `{builder} build` for service `{name}`"))?;
    if !status.success() {
        bail!(
            "`{builder} build` failed for service `{name}` (Dockerfile: {dockerfile}). \
             See the build output above for the cause."
        );
    }

    // save → image tar（docker/podman/nerdctl: docker-archive；Apple container: OCI archive）。
    // Apple container 的 save 在 `image` 子命令下（`container image save`），其餘為 `<tool> save`。
    let tar = out_dir.join("image.tar");
    let mut save_cmd = Command::new(builder);
    if builder == "container" {
        save_cmd.arg("image").arg("save");
    } else {
        save_cmd.arg("save");
    }
    let save = save_cmd
        .arg("-o")
        .arg(&tar)
        .arg(&tag)
        .output()
        .with_context(|| format!("failed to run `{builder} save` for service `{name}`"))?;

    // best-effort 清掉建置用的暫時 image（不影響成敗）。
    let _ = Command::new(builder)
        .arg("image")
        .arg("rm")
        .arg("-f")
        .arg(&tag)
        .output();

    if !save.status.success() {
        bail!(
            "`{builder} save` failed for service `{name}`: {}",
            String::from_utf8_lossy(&save.stderr).trim()
        );
    }
    Ok(tar)
}

/// 偵測可用的 container builder（以 `--version` 探測）。找不到 → 可行動錯誤。
fn detect_builder() -> Result<&'static str> {
    for b in BUILDERS {
        let ok = Command::new(b)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Ok(b);
        }
    }
    bail!(
        "source: dockerfile needs a container builder on this machine, but none of \
         docker/podman/nerdctl/container were found on PATH. Install one (e.g. Docker, OrbStack, \
         or Apple's `container`), or pre-build the image and use source: tar / source: image."
    )
}
