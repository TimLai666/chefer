//! `source: dockerfile` —— 在**打包機**上以既有的 container builder 建置成 image
//! archive，之後完全併入 `source: tar` 的解析路徑（見 docs/DESIGN.md「Dockerfile build」）。
//!
//! chefer 本身不建 image：偵測並呼叫 host 上既有的 container builder（Windows 另支援 WSL Containers）。
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
// Windows 的 wslc 與 Apple container 同樣以 `image save` 匯出。
#[cfg(windows)]
const BUILDERS: &[&str] = &["docker", "podman", "nerdctl", "wslc", "container"];
#[cfg(not(windows))]
const BUILDERS: &[&str] = &["docker", "podman", "nerdctl", "container"];

/// 以 host builder 建置 Dockerfile 成 image archive，回傳 tar 路徑（位於 `out_dir`）。
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
    let wslc = cfg!(windows) && matches!(builder, "wslc" | "container");
    #[cfg(windows)]
    if wslc {
        let native = wslc_native_platform()?;
        if platform != native {
            bail!(
                "Chefer currently supports `{builder}` builds only for the native platform {native}, but service `{name}` requests {platform}. \
                 Use docker/podman/nerdctl for cross-platform builds, or set image.platform to {native}."
            );
        }
    }
    let tag = format!("chefer-build-{name}:{}", std::process::id());

    eprintln!(
        "[chefer] building image for service `{name}` from {dockerfile} with {builder} (platform {platform})…"
    );

    // build —— stdio 直通，讓使用者看到建置過程。
    let mut cmd = Command::new(builder);
    cmd.arg("build");
    // WSLC 3.0.3 的 build 沒有 --platform，僅建置 Windows 原生架構。
    if !wslc {
        cmd.arg("--platform").arg(platform);
    }
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

    // save → image archive。Apple container 與 WSL Containers 用 `image save`。
    let tar = out_dir.join("image.tar");
    let mut save_cmd = Command::new(builder);
    if matches!(builder, "container" | "wslc") {
        save_cmd.arg("image").arg("save");
    } else {
        save_cmd.arg("save");
    }
    let save = save_cmd.arg("-o").arg(&tar).arg(&tag).output();

    // best-effort 清掉建置用的暫時 image（不影響成敗）。
    let _ = Command::new(builder)
        .arg("image")
        .arg("rm")
        .arg("-f")
        .arg(&tag)
        .output();

    let save =
        save.with_context(|| format!("failed to run `{builder} save` for service `{name}`"))?;
    if !save.status.success() {
        bail!(
            "`{builder} save` failed for service `{name}`: {}",
            String::from_utf8_lossy(&save.stderr).trim()
        );
    }
    Ok(tar)
}

/// 偵測可用的 container builder。`CHEFER_DOCKERFILE_BUILDER` 可覆寫自動偵測
/// （限已知 builder 之一，避免被指使去執行任意檔）。找不到 → 可行動錯誤。
fn detect_builder() -> Result<&'static str> {
    if let Ok(forced) = std::env::var("CHEFER_DOCKERFILE_BUILDER") {
        let forced = forced.trim();
        if !forced.is_empty() {
            let Some(&b) = BUILDERS.iter().find(|b| **b == forced) else {
                bail!(
                    "CHEFER_DOCKERFILE_BUILDER=`{forced}` is not a supported builder \
                     (expected one of {})",
                    BUILDERS.join("/")
                );
            };
            if runnable(b) {
                return Ok(b);
            }
            bail!("CHEFER_DOCKERFILE_BUILDER=`{b}` was set, but `{b}` is not runnable on PATH");
        }
    }
    for b in BUILDERS {
        if runnable(b) {
            return Ok(b);
        }
    }
    bail!(
        "source: dockerfile needs a container builder on this machine, but none of \
         {} were found on PATH. Install a supported builder (on Windows, update WSL for `wslc`), \
         or pre-build the image and use source: tar / source: image.",
        BUILDERS.join("/")
    )
}

/// 回傳第一個可用的 container builder 名稱，找不到則 None。
///
/// Doctor 用來告訴使用者是否能用 `source: dockerfile`。
pub fn available_builder() -> Option<&'static str> {
    if let Ok(forced) = std::env::var("CHEFER_DOCKERFILE_BUILDER") {
        let forced = forced.trim();
        if !forced.is_empty() {
            if let Some(&b) = BUILDERS.iter().find(|b| **b == forced && runnable(b)) {
                return Some(b);
            }
            return None;
        }
    }
    BUILDERS.iter().copied().find(|b| runnable(b))
}

/// WSLC 使用 Windows 原生架構；模擬執行的 CLI 架構與環境變數不能作為判斷依據。
#[cfg(windows)]
fn wslc_native_platform() -> Result<&'static str> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn IsWow64Process2(
            process: *mut std::ffi::c_void,
            process_machine: *mut u16,
            native_machine: *mut u16,
        ) -> i32;
    }
    let (mut process_machine, mut native_machine) = (0u16, 0u16);
    // SAFETY: GetCurrentProcess 提供有效的目前程序 handle，兩個輸出指標均指向有效 u16。
    let ok = unsafe {
        IsWow64Process2(
            GetCurrentProcess(),
            &mut process_machine,
            &mut native_machine,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to detect the native Windows architecture for WSL Containers");
    }
    match native_machine {
        0x8664 => Ok("linux/amd64"), // IMAGE_FILE_MACHINE_AMD64
        0xaa64 => Ok("linux/arm64"), // IMAGE_FILE_MACHINE_ARM64
        _ => bail!(
            "WSL Containers native machine architecture is unsupported: {native_machine:#06x}"
        ),
    }
}

fn runnable(builder: &str) -> bool {
    Command::new(builder)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
