//! Windows `wsl2` 後端：建立（或重用）chefer 專用的最小 WSL distro，
//! 在其中執行 bundle 內嵌的 musl guest-agent。
//!
//! 安全注意：所有 wsl.exe 呼叫一律以 `std::process::Command` 傳遞個別參數
//! （argv 陣列），絕不組 shell 字串；並設 `WSL_UTF8=1` 讓輸出為 UTF-8。

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chefer_bundle::PortProto;

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
    fn availability(&self, _ctx: &AppRunContext) -> Availability {
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
            ensure_distro_imported(&distro, &agent_bytes)?;
        }

        // d. Windows 路徑 → WSL 路徑
        let bundle_wsl = to_wsl_path(ctx.bundle_dir)
            .with_context(|| format!("轉換 bundle 路徑失敗：{}", ctx.bundle_dir.display()))?;
        std::fs::create_dir_all(ctx.data_dir)
            .with_context(|| format!("建立資料目錄失敗：{}", ctx.data_dir.display()))?;
        let data_wsl = to_wsl_path(ctx.data_dir)
            .with_context(|| format!("轉換資料目錄路徑失敗：{}", ctx.data_dir.display()))?;

        // e. UDP 埠：wslrelay 不轉 UDP，故在啟動 guest-agent 前先取 VM eth0 的 IPv4，
        //    對每個 UDP PortSpec 於 host 端起 `127.0.0.1:host → <vm_ip>:guest` relay
        //    （含 host == guest——UDP 在 Windows 不會自然生效，一律 relay）。
        //    VM 內側的 `<vm_ip>:guest → 127.0.0.1:guest` 橋接由下方 --udp-bridge 觸發。
        let udp_specs = udp_port_specs(ctx.manifest);
        if !udp_specs.is_empty() {
            let vm_ip = query_vm_ipv4(&distro)?;
            start_host_udp_relays(vm_ip, &udp_specs)?;
        }

        // f. 在 distro 內執行 guest-agent；stdio 直通，exit code 透傳。
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
            .arg("/var/lib/chefer/cache")
            // VM 內補起 UDP 埠的 eth0→loopback 橋接（無 UDP 埠時於 guest 內為 no-op）
            .arg("--udp-bridge");
        if ctx.opts.keep_tmp {
            cmd.arg("--keep-rootfs");
        }
        let status = cmd
            .status()
            .with_context(|| format!("在 WSL distro `{distro}` 內啟動 guest-agent 失敗"))?;
        Ok(status.code().unwrap_or(1))
    }
}

/// 蒐集 manifest 中所有 UDP PortSpec 的 `(host, guest)`（含 host == guest）。
fn udp_port_specs(manifest: &chefer_bundle::Manifest) -> Vec<(u16, u16)> {
    manifest
        .services
        .iter()
        .flat_map(|s| &s.ports)
        .filter(|p| p.proto == PortProto::Udp)
        .map(|p| (p.host, p.guest))
        .collect()
}

/// 以 `wsl -d <distro> --user root --exec /bin/guest-agent vmip` 取得 VM eth0 的 IPv4。
/// 此呼叫會順帶啟動 distro；輸出為單行 IP 字串。
fn query_vm_ipv4(distro: &str) -> Result<Ipv4Addr> {
    let out = wsl_command()
        .arg("-d")
        .arg(distro)
        .arg("--user")
        .arg("root")
        .arg("--exec")
        .arg("/bin/guest-agent")
        .arg("vmip")
        .output()
        .with_context(|| format!("在 WSL distro `{distro}` 內查詢 VM IP 失敗"))?;
    if !out.status.success() {
        bail!(
            "guest-agent vmip 失敗（exit {}）：{}；無法為 UDP 埠建立轉發",
            out.status.code().unwrap_or(-1),
            first_line(
                &decode_wsl_output(&out.stderr),
                &decode_wsl_output(&out.stdout)
            ),
        );
    }
    let text = decode_wsl_output(&out.stdout);
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    line.parse::<Ipv4Addr>().with_context(|| {
        format!("無法解析 guest-agent vmip 的輸出為 IPv4：{line:?}（distro `{distro}`）")
    })
}

/// 對每個 UDP `(host, guest)` 於 host 端 bind `127.0.0.1:host`，relay 至 `<vm_ip>:guest`。
/// 任一 host 埠 bind 失敗（通常已被占用）即整體報錯（與 TCP 埠代理一致的 fail-fast）。
fn start_host_udp_relays(vm_ip: Ipv4Addr, specs: &[(u16, u16)]) -> Result<()> {
    let mut failures: Vec<String> = Vec::new();
    for &(host, guest) in specs {
        match UdpSocket::bind((Ipv4Addr::LOCALHOST, host)) {
            Ok(sock) => {
                let target = SocketAddr::from((vm_ip, guest));
                guest_agent::udp_bridge::spawn_udp_relay(sock, target);
                eprintln!("[chefer] UDP 埠轉發：127.0.0.1:{host} → {vm_ip}:{guest}");
            }
            Err(e) => failures.push(format!("  - {host}/udp：bind 127.0.0.1:{host} 失敗：{e}")),
        }
    }
    if !failures.is_empty() {
        bail!(
            "以下 host UDP 埠無法啟動轉發（埠可能已被其他程式占用；\
             請關閉占用的程式，或調整 appcipe.yml 的 ports 後重新打包）：\n{}",
            failures.join("\n")
        );
    }
    Ok(())
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
///
/// 並行安全：distro_exists 與 import 之間存在 TOCTOU——同一 kit 打包的多個 app
/// 同時首次啟動時，可能都判定 distro 不存在而都嘗試 import，後者會因「名稱已存在」
/// 失敗。此處對 import 失敗後**重新檢查**：若 distro 此時已存在（被另一個 process
/// 搶先建立），視為成功；否則才真正回報錯誤，並清掉殘留的安裝目錄。
fn ensure_distro_imported(distro: &str, agent_bytes: &[u8]) -> Result<()> {
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
    if out.status.success() {
        return Ok(());
    }

    // import 失敗：可能是另一個 process 已搶先建立同名 distro（TOCTOU）。
    // 重新檢查；若已存在則視為成功（冪等）。
    if distro_exists(distro).unwrap_or(false) {
        return Ok(());
    }

    // 真正失敗：清掉本次建立的殘留安裝目錄（避免殘留 vhdx 卡死後續 import）。
    let _ = std::fs::remove_dir_all(&install_dir);
    bail!(
        "匯入 WSL distro `{distro}` 失敗（exit {}）：{}；\
         請確認 WSL2 已啟用（`wsl --status`）後重試。\
         （注意：若同名 distro 正被另一個 Chefer app 使用，請勿手動 unregister，\
         直接重試即可。）",
        out.status.code().unwrap_or(-1),
        first_line(
            &decode_wsl_output(&out.stderr),
            &decode_wsl_output(&out.stdout)
        ),
    );
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
