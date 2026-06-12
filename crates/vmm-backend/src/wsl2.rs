//! Windows `wsl2` 後端：建立（或重用）chefer 專用的最小 WSL distro，
//! 在其中執行 bundle 內嵌的 musl guest-agent。
//!
//! 安全注意：所有 wsl.exe 呼叫一律以 `std::process::Command` 傳遞個別參數
//! （argv 陣列），絕不組 shell 字串；並設 `WSL_UTF8=1` 讓輸出為 UTF-8。

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::wsl_util::{
    DISTRO_PREFIX, agent_distro_name, build_min_rootfs_tar, decode_wsl_output, windows_path_to_wsl,
};
use crate::{AppRunContext, Availability, ExecBackend};

/// Windows WSL2 後端。
pub struct Wsl2Backend;

impl ExecBackend for Wsl2Backend {
    fn name(&self) -> &'static str {
        "wsl2"
    }

    /// 可用性：`wsl.exe --status` 成功（exit 0）即視為可用。
    fn availability(&self) -> Availability {
        match wsl_command().arg("--status").output() {
            Ok(out) if out.status.success() => Availability::Available,
            Ok(out) => Availability::Unavailable(format!(
                "`wsl.exe --status` 失敗（exit {}）：{}；\
                 請以系統管理員身分執行 `wsl --install` 啟用 WSL2 後重試",
                out.status.code().unwrap_or(-1),
                first_line(
                    &decode_wsl_output(&out.stderr),
                    &decode_wsl_output(&out.stdout)
                ),
            )),
            Err(e) => Availability::Unavailable(format!(
                "找不到或無法執行 wsl.exe：{e}；\
                 請以系統管理員身分執行 `wsl --install` 啟用 WSL2 後重試"
            )),
        }
    }

    fn run(&self, ctx: &AppRunContext) -> Result<i32> {
        // a. host arch → guest arch，找 bundle 內嵌的 guest-agent
        let arch = host_guest_arch()?;
        let agent_path = chefer_bundle::layout::agents_dir(ctx.bundle_dir)
            .join(chefer_bundle::layout::guest_agent_name(arch));
        if !agent_path.exists() {
            bail!(
                "此單檔未內嵌 guest-agent（缺 {}），無法在 Windows 執行；\
                 請在打包時提供 kit（含 guest-agent-{arch}），\
                 詳見 `chefer build` 的 --kit-dir 參數或環境變數 CHEFER_KIT_DIR",
                agent_path.display()
            );
        }
        let agent_bytes = std::fs::read(&agent_path)
            .with_context(|| format!("讀取 guest-agent 失敗：{}", agent_path.display()))?;

        // b. distro 名 = chefer-rt-<agent sha256 前 8 碼>（同 hash 冪等重用）
        let distro = agent_distro_name(&agent_bytes);

        // c. distro 不存在時匯入最小 rootfs
        if !distro_exists(&distro)? {
            import_distro(&distro, &agent_bytes)?;
        }

        // d. Windows 路徑 → WSL 路徑
        let bundle_wsl = to_wsl_path(ctx.bundle_dir)
            .with_context(|| format!("轉換 bundle 路徑失敗：{}", ctx.bundle_dir.display()))?;
        std::fs::create_dir_all(ctx.data_dir)
            .with_context(|| format!("建立資料目錄失敗：{}", ctx.data_dir.display()))?;
        let data_wsl = to_wsl_path(ctx.data_dir)
            .with_context(|| format!("轉換資料目錄路徑失敗：{}", ctx.data_dir.display()))?;

        // e. 在 distro 內執行 guest-agent；stdio 直通，exit code 透傳。
        //    rootfs 快取一律放 distro 內的 ext4（/var/lib/chefer/cache）：
        //    /mnt/c（drvfs）上 symlink/hardlink/權限不可靠且 I/O 慢，
        //    不能在那裡組 rootfs。
        let mut cmd = wsl_command();
        cmd.arg("-d")
            .arg(&distro)
            .arg("--user")
            .arg("root")
            .arg("--exec")
            .arg("/bin/guest-agent")
            .arg("run")
            .arg("--bundle")
            .arg(&bundle_wsl)
            .arg("--data")
            .arg(&data_wsl)
            .arg("--cache")
            .arg("/var/lib/chefer/cache");
        if ctx.opts.keep_tmp {
            cmd.arg("--keep-rootfs");
        }
        let status = cmd
            .status()
            .with_context(|| format!("在 WSL distro `{distro}` 內啟動 guest-agent 失敗"))?;
        Ok(status.code().unwrap_or(1))
    }
}

/// 建立帶 `WSL_UTF8=1` 的 wsl.exe 命令（個別 arg，不組 shell 字串）。
fn wsl_command() -> Command {
    let mut cmd = Command::new("wsl.exe");
    cmd.env("WSL_UTF8", "1");
    cmd
}

/// host 架構對應 guest 架構（WSL2 的 guest 與 host 同架構）。
fn host_guest_arch() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64"),
        "aarch64" => Ok("aarch64"),
        other => bail!("不支援的 host 架構：{other}；WSL2 後端目前支援 x86_64 與 aarch64"),
    }
}

/// 把 `Path` 轉成 WSL 路徑：必要時先絕對化（canonicalize 會帶 \\?\ 前綴，
/// 由 `windows_path_to_wsl` 處理）。
fn to_wsl_path(path: &Path) -> Result<String> {
    let abs: PathBuf = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::fs::canonicalize(path)
            .with_context(|| format!("無法解析為絕對路徑：{}", path.display()))?
    };
    let s = abs.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "路徑含無法以 UTF-8 表示的字元，無法轉換為 WSL 路徑：{}",
            abs.display()
        )
    })?;
    windows_path_to_wsl(s)
}

/// 以 `wsl.exe -l -q` 逐行比對 distro 是否已存在。
fn distro_exists(name: &str) -> Result<bool> {
    let out = wsl_command()
        .args(["-l", "-q"])
        .output()
        .context("無法執行 `wsl.exe -l -q` 列出 distro；請確認已安裝 WSL（`wsl --install`）")?;
    if !out.status.success() {
        // 尚無任何 distro 或 WSL 未初始化時可能回非 0——視為不存在，由匯入流程建立
        return Ok(false);
    }
    let text = decode_wsl_output(&out.stdout);
    Ok(text.lines().any(|l| l.trim() == name))
}

/// 產生最小 rootfs tar 並 `wsl --import` 成新 distro（WSL2）。
fn import_distro(distro: &str, agent_bytes: &[u8]) -> Result<()> {
    let local_app_data = std::env::var_os("LOCALAPPDATA").ok_or_else(|| {
        anyhow::anyhow!("找不到 %LOCALAPPDATA% 環境變數，無法決定 WSL distro 安裝目錄")
    })?;
    let install_dir = PathBuf::from(local_app_data)
        .join("chefer")
        .join("wsl")
        .join(distro);
    std::fs::create_dir_all(&install_dir)
        .with_context(|| format!("建立 distro 安裝目錄失敗：{}", install_dir.display()))?;

    // 在記憶體產 tar，寫到暫存目錄（讓 wsl.exe 能開檔讀取；TempDir 於離開時清理）
    let tar_bytes = build_min_rootfs_tar(agent_bytes)?;
    let tmp_dir = tempfile::tempdir().context("建立暫存目錄失敗")?;
    let tar_path = tmp_dir.path().join("rootfs.tar");
    std::fs::write(&tar_path, &tar_bytes)
        .with_context(|| format!("寫入暫存 rootfs tar 失敗：{}", tar_path.display()))?;

    let out = wsl_command()
        .arg("--import")
        .arg(distro)
        .arg(&install_dir)
        .arg(&tar_path)
        .arg("--version")
        .arg("2")
        .output()
        .context("執行 `wsl --import` 失敗")?;
    if !out.status.success() {
        bail!(
            "匯入 WSL distro `{distro}` 失敗（exit {}）：{}；\
             可嘗試手動執行 `wsl --unregister {distro}` 後重試，\
             或確認 WSL2 已啟用（`wsl --status`）",
            out.status.code().unwrap_or(-1),
            first_line(
                &decode_wsl_output(&out.stderr),
                &decode_wsl_output(&out.stdout)
            ),
        );
    }
    Ok(())
}

/// 清理所有 `chefer-rt-` 前綴的 distro；回傳已移除的名稱。
pub(crate) fn cleanup_distros_impl() -> Result<Vec<String>> {
    let out = wsl_command()
        .args(["-l", "-q"])
        .output()
        .context("無法執行 `wsl.exe -l -q` 列出 distro；請確認已安裝 WSL（`wsl --install`）")?;
    if !out.status.success() {
        // 沒有任何 distro 時可能回非 0——無事可清
        return Ok(Vec::new());
    }
    let text = decode_wsl_output(&out.stdout);
    let mut removed = Vec::new();
    for line in text.lines() {
        let name = line.trim();
        if name.is_empty() || !name.starts_with(DISTRO_PREFIX) {
            continue;
        }
        let st = wsl_command()
            .arg("--unregister")
            .arg(name)
            .output()
            .with_context(|| format!("執行 `wsl --unregister {name}` 失敗"))?;
        if !st.status.success() {
            bail!(
                "移除 WSL distro `{name}` 失敗（exit {}）：{}；\
                 可手動執行 `wsl --unregister {name}`",
                st.status.code().unwrap_or(-1),
                first_line(
                    &decode_wsl_output(&st.stderr),
                    &decode_wsl_output(&st.stdout)
                ),
            );
        }
        // 盡力清掉安裝目錄（失敗不致命：vhdx 已由 --unregister 移除）
        if let Some(lad) = std::env::var_os("LOCALAPPDATA") {
            let _ =
                std::fs::remove_dir_all(PathBuf::from(lad).join("chefer").join("wsl").join(name));
        }
        removed.push(name.to_string());
    }
    Ok(removed)
}

/// 取錯誤輸出的第一行（stderr 優先，空則用 stdout），避免訊息過長。
fn first_line<'a>(stderr: &'a str, stdout: &'a str) -> &'a str {
    let pick = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    pick.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("（無輸出）")
        .trim()
}
