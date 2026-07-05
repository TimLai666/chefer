//! macOS `vz` 後端的純函式：appliance（kernel+initramfs）查找、VM 資源計算、
//! kernel command line 組裝、guest 共享掛載標籤。
//!
//! 全部與作業系統無關，跨平台皆可編譯與測試（實際的 Virtualization.framework
//! 開機在 `vz.rs`，僅 macOS 編譯、僅能在實體 Mac 驗證）。

use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};

/// guest 內 virtiofs 共享標籤：bundle（唯讀）與 data（讀寫）。
/// initramfs 的 `/init` 依此標籤掛載。
pub const SHARE_TAG_BUNDLE: &str = "chefer-bundle";
pub const SHARE_TAG_DATA: &str = "chefer-data";

/// guest console 標記前綴：appliance `init` 開機收尾時印到序列埠，VM host
/// （macOS vz / QEMU）解析以取得 guest 的整體結束碼與對外 IPv4。
pub const GUEST_EXIT_MARKER: &str = "CHEFER_GUEST_EXIT=";
pub const GUEST_IP_MARKER: &str = "CHEFER_GUEST_IP=";

/// guest 內的掛載點（initramfs 約定）。
pub const GUEST_BUNDLE_DIR: &str = "/mnt/bundle";
pub const GUEST_DATA_DIR: &str = "/mnt/data";

/// VM 記憶體預設（MiB）與下限。
pub const DEFAULT_MEMORY_MIB: u64 = 1536;
pub const MIN_MEMORY_MIB: u64 = 512;
/// VM vCPU 數上限（避免在多核 host 上過度配置）。
pub const MAX_VCPUS: usize = 4;

/// macOS micro-VM appliance 的兩個檔案路徑。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliancePaths {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
}

/// 從已解壓 bundle 的 `vm/` 目錄找 appliance（kernel + initramfs 都在才回傳）。
pub fn appliance_in_bundle(bundle_dir: &Path, arch: &str) -> Option<AppliancePaths> {
    let vm = chefer_bundle::layout::vm_dir(bundle_dir);
    let kernel = vm.join(chefer_bundle::layout::kernel_name(arch));
    let initramfs = vm.join(chefer_bundle::layout::initramfs_name(arch));
    if kernel.is_file() && initramfs.is_file() {
        Some(AppliancePaths { kernel, initramfs })
    } else {
        None
    }
}

/// host 架構 → guest 架構（VZ 的 Linux guest 與 host 同架構）。
pub fn host_guest_arch(host_arch: &str) -> Option<&'static str> {
    match host_arch {
        "x86_64" => Some("x86_64"),
        "aarch64" => Some("aarch64"),
        _ => None,
    }
}

/// VM 資源配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmResources {
    pub cpu_count: usize,
    pub memory_mib: u64,
}

impl VmResources {
    /// 由 host 可用核心數與（選用的）記憶體覆寫值計算。
    /// cpu = clamp(host_cpus, 1..=MAX_VCPUS)；memory = max(override 或預設, 下限)。
    pub fn compute(host_cpus: usize, mem_override_mib: Option<u64>) -> Self {
        let cpu_count = host_cpus.clamp(1, MAX_VCPUS);
        let memory_mib = mem_override_mib
            .unwrap_or(DEFAULT_MEMORY_MIB)
            .max(MIN_MEMORY_MIB);
        VmResources {
            cpu_count,
            memory_mib,
        }
    }

    pub fn memory_bytes(&self) -> u64 {
        self.memory_mib * 1024 * 1024
    }
}

/// kernel command line：序列埠 console + 由 initramfs 解讀的 chefer 參數
/// （keep_rootfs 與共享標籤）。實際掛載由 initramfs `/init` 執行。
pub fn kernel_command_line(keep_rootfs: bool) -> String {
    let mut s = String::from("console=hvc0 quiet ip=dhcp panic=-1");
    s.push_str(&format!(
        " chefer.bundle_tag={SHARE_TAG_BUNDLE} chefer.data_tag={SHARE_TAG_DATA}"
    ));
    s.push_str(&format!(
        " chefer.bundle_dir={GUEST_BUNDLE_DIR} chefer.data_dir={GUEST_DATA_DIR}"
    ));
    if keep_rootfs {
        s.push_str(" chefer.keep_rootfs=1");
    }
    s
}

/// vz 開機 helper（`chefer-vz-helper-<arch>`，Swift）的 CLI 參數——與 helper 的解析
/// 契約一致（鏡像 `whp_util::HelperInvocation::args()` 的做法，抽純函式供跨平台單元測試）。
/// `gui = true` 時附 `--gui`（helper 掛 virtio-gpu/鍵盤/指標並開 `VZVirtualMachineView`
/// 視窗；剪貼簿 token 由 helper 自行產生並附加到 kernel cmdline，非本函式責任）；
/// `app_name` 非空時附 `--gui-title <name>`（視窗標題；空 → helper 用預設 "Chefer"）。
#[allow(clippy::too_many_arguments)] // CLI 契約的平鋪參數；集中一處、有單元測試鎖住
pub fn helper_args(
    kernel: &Path,
    initramfs: &Path,
    cmdline: &str,
    bundle_dir: &Path,
    data_dir: &Path,
    cpus: usize,
    memory_mib: u64,
    gui: bool,
    app_name: &str,
) -> Vec<std::ffi::OsString> {
    let mut v: Vec<std::ffi::OsString> = vec![
        "--kernel".into(),
        kernel.into(),
        "--initramfs".into(),
        initramfs.into(),
        "--cmdline".into(),
        cmdline.into(),
        "--bundle-dir".into(),
        bundle_dir.into(),
        "--data-dir".into(),
        data_dir.into(),
        "--cpus".into(),
        cpus.to_string().into(),
        "--memory-mib".into(),
        memory_mib.to_string().into(),
    ];
    if gui {
        v.push("--gui".into());
        if !app_name.is_empty() {
            v.push("--gui-title".into());
            v.push(app_name.into());
        }
    }
    v
}

/// 從 guest console 文字解析**最後一個** `CHEFER_GUEST_EXIT=<n>`（容忍前綴與多次列印，
/// 取最後一個為準；與 qemu-e2e 的 `sed` 子字串比對一致）。
pub fn parse_guest_exit_code(console: &str) -> Option<i32> {
    console.lines().rev().find_map(|line| {
        let pos = line.find(GUEST_EXIT_MARKER)?;
        let rest = &line[pos + GUEST_EXIT_MARKER.len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse::<i32>().ok()
    })
}

/// 從 guest console 文字解析**最後一個** `CHEFER_GUEST_IP=<ipv4>`（VM host 用以對 guest
/// 做 host→guest 埠轉發）。
pub fn parse_guest_ip(console: &str) -> Option<Ipv4Addr> {
    console.lines().rev().find_map(|line| {
        let pos = line.find(GUEST_IP_MARKER)?;
        let rest = &line[pos + GUEST_IP_MARKER.len()..];
        let token: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
        token.parse::<Ipv4Addr>().ok()
    })
}

/// 一條 host→guest 埠轉發（VM host 端把 `127.0.0.1:host` relay 到 `<guest_ip>:guest`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortForward {
    pub proto: chefer_bundle::PortProto,
    pub host: u16,
    pub guest: u16,
}

/// 規劃 host→guest 埠轉發清單。VZ 的 NAT 讓 host 可直接連到 guest IP，但**不會**
/// 自動把 host 埠轉進 guest（NAT 只管 guest 對外）；故所有宣告的 ports（TCP 與 UDP）
/// 都需在 host 端 relay 到 `<guest_ip>:guest`——與 WSL2 後端對 UDP 的處理同理，VZ 連
/// TCP 也得自己 relay。
pub fn forward_ports(manifest: &chefer_bundle::Manifest) -> Vec<PortForward> {
    manifest
        .services
        .iter()
        .flat_map(|s| &s.ports)
        .map(|p| PortForward {
            proto: p.proto,
            host: p.host,
            guest: p.guest,
        })
        .collect()
}

/// 在 host 端起一條 TCP relay：`127.0.0.1:host_port` → `<guest_ip>:guest_port`（背景執行緒）。
/// VZ 後端取得 guest IP 後，對每個宣告的 TCP 埠呼叫一次。bind 失敗（多半是埠被占用）即回錯，
/// 由呼叫端決定如何回報。每個連線各自雙向轉送、互不阻塞。
pub fn spawn_tcp_forward(guest_ip: Ipv4Addr, host_port: u16, guest_port: u16) -> io::Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, host_port))?;
    let target = SocketAddr::from((guest_ip, guest_port));
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(client) = conn else { continue };
            std::thread::spawn(move || {
                let _ = relay_tcp_conn(client, target);
            });
        }
    });
    Ok(())
}

/// 單一 TCP 連線的雙向轉送：client ↔ upstream(`target`)。
fn relay_tcp_conn(client: TcpStream, target: SocketAddr) -> io::Result<()> {
    let upstream = TcpStream::connect(target)?;
    let mut client_to_up = client.try_clone()?;
    let mut up_from_client = upstream.try_clone()?;
    let up_thread = std::thread::spawn(move || {
        let _ = io::copy(&mut client_to_up, &mut up_from_client);
        // 上行結束 → 關掉 upstream 的寫端，讓下行的 copy 收到 EOF。
        let _ = up_from_client.shutdown(std::net::Shutdown::Write);
    });
    let mut up_to_client = upstream;
    let mut client_write = client;
    let _ = io::copy(&mut up_to_client, &mut client_write);
    let _ = client_write.shutdown(std::net::Shutdown::Write);
    let _ = up_thread.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_mapping() {
        assert_eq!(host_guest_arch("aarch64"), Some("aarch64"));
        assert_eq!(host_guest_arch("x86_64"), Some("x86_64"));
        assert_eq!(host_guest_arch("riscv64"), None);
    }

    #[test]
    fn resources_clamp_and_floor() {
        // 多核 host → 夾在 MAX_VCPUS
        let r = VmResources::compute(16, None);
        assert_eq!(r.cpu_count, MAX_VCPUS);
        assert_eq!(r.memory_mib, DEFAULT_MEMORY_MIB);
        // 0 核 → 至少 1
        assert_eq!(VmResources::compute(0, None).cpu_count, 1);
        // 記憶體覆寫低於下限 → 拉到下限
        assert_eq!(
            VmResources::compute(2, Some(128)).memory_mib,
            MIN_MEMORY_MIB
        );
        // 合理覆寫保留
        assert_eq!(VmResources::compute(2, Some(4096)).memory_mib, 4096);
        assert_eq!(
            VmResources::compute(2, Some(4096)).memory_bytes(),
            4096 * 1024 * 1024
        );
    }

    #[test]
    fn cmdline_contains_tags_and_keep_flag() {
        let c = kernel_command_line(false);
        assert!(c.contains("console=hvc0"));
        assert!(c.contains("ip=dhcp"));
        assert!(c.contains(&format!("chefer.bundle_tag={SHARE_TAG_BUNDLE}")));
        assert!(c.contains(&format!("chefer.data_dir={GUEST_DATA_DIR}")));
        assert!(!c.contains("keep_rootfs"));
        assert!(kernel_command_line(true).contains("chefer.keep_rootfs=1"));
    }

    #[test]
    fn helper_args_contract() {
        let args = |gui: bool, name: &str| {
            helper_args(
                Path::new("/k/vmlinuz"),
                Path::new("/k/initramfs"),
                "console=hvc0 quiet",
                Path::new("/b"),
                Path::new("/d"),
                2,
                1536,
                gui,
                name,
            )
            .into_iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect::<Vec<_>>()
        };
        let has_pair =
            |v: &[String], k: &str, val: &str| v.windows(2).any(|w| w[0] == k && w[1] == val);

        let a = args(false, "");
        assert!(has_pair(&a, "--kernel", "/k/vmlinuz"));
        assert!(has_pair(&a, "--initramfs", "/k/initramfs"));
        assert!(has_pair(&a, "--cmdline", "console=hvc0 quiet"));
        assert!(has_pair(&a, "--bundle-dir", "/b"));
        assert!(has_pair(&a, "--data-dir", "/d"));
        assert!(has_pair(&a, "--cpus", "2"));
        assert!(has_pair(&a, "--memory-mib", "1536"));
        // 非 GUI：不帶 --gui / --gui-title。
        assert!(!a.iter().any(|s| s == "--gui" || s == "--gui-title"));

        // GUI + app 名 → --gui 與 --gui-title <name>。
        let g = args(true, "MyApp");
        assert!(g.iter().any(|s| s == "--gui"));
        assert!(has_pair(&g, "--gui-title", "MyApp"));
        // GUI 但無名 → 只帶 --gui（helper 用預設標題）。
        let g2 = args(true, "");
        assert!(g2.iter().any(|s| s == "--gui"));
        assert!(!g2.iter().any(|s| s == "--gui-title"));
    }

    #[test]
    fn parse_exit_marker_takes_last_and_tolerates_prefix() {
        // 無標記 → None
        assert_eq!(parse_guest_exit_code("hello\nworld\n"), None);
        // 基本
        assert_eq!(parse_guest_exit_code("CHEFER_GUEST_EXIT=0\n"), Some(0));
        // 行內有前綴（console 噪音）仍以子字串比對
        assert_eq!(
            parse_guest_exit_code("[  12.3] CHEFER_GUEST_EXIT=42 trailing"),
            Some(42)
        );
        // 多次列印取最後一個
        let log = "CHEFER_GUEST_EXIT=1\n...\nCHEFER_GUEST_EXIT=7\n";
        assert_eq!(parse_guest_exit_code(log), Some(7));
        // 非數字 → None
        assert_eq!(parse_guest_exit_code("CHEFER_GUEST_EXIT=oops"), None);
    }

    #[test]
    fn parse_ip_marker() {
        assert_eq!(parse_guest_ip("no marker here"), None);
        assert_eq!(
            parse_guest_ip("CHEFER_GUEST_IP=10.0.2.15"),
            Some(Ipv4Addr::new(10, 0, 2, 15))
        );
        // 前綴 + 尾隨內容
        assert_eq!(
            parse_guest_ip("[init] CHEFER_GUEST_IP=192.168.64.3 (eth0)"),
            Some(Ipv4Addr::new(192, 168, 64, 3))
        );
        // 取最後一個
        assert_eq!(
            parse_guest_ip("CHEFER_GUEST_IP=1.1.1.1\nCHEFER_GUEST_IP=2.2.2.2"),
            Some(Ipv4Addr::new(2, 2, 2, 2))
        );
        // 壞值 → None
        assert_eq!(parse_guest_ip("CHEFER_GUEST_IP=not-an-ip"), None);
    }

    #[test]
    fn forward_ports_collects_all_protos() {
        use chefer_bundle::PortProto;
        // 以最小 manifest JSON 反序列化（缺的欄位由 serde default 補；schema 擴張時仍可編）。
        let manifest: chefer_bundle::Manifest = serde_json::from_str(
            r#"{
                "format_version": 1,
                "app": {"name": "t", "spec_version": "0.1", "generated_at_utc": ""},
                "services": [
                    {"name": "web", "platform": "linux/amd64", "layers": [], "image_config": {},
                     "ports": [
                        {"host": 8080, "guest": 80, "proto": "tcp"},
                        {"host": 5353, "guest": 53, "proto": "udp"}
                     ]}
                ]
            }"#,
        )
        .unwrap();
        let fwd = forward_ports(&manifest);
        assert_eq!(fwd.len(), 2);
        // TCP 與 UDP 都要轉發（VZ NAT 不自動轉 host→guest）
        assert!(
            fwd.iter()
                .any(|f| f.proto == PortProto::Tcp && f.host == 8080 && f.guest == 80)
        );
        assert!(
            fwd.iter()
                .any(|f| f.proto == PortProto::Udp && f.host == 5353 && f.guest == 53)
        );
    }

    #[test]
    fn tcp_forward_relays_bidirectionally() {
        use std::io::{Read, Write};
        use std::net::TcpListener as L;

        // 假「guest」服務：收到資料後回 "echo:" + 原文。
        let upstream = L::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let guest_port = upstream.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = upstream.accept().unwrap();
            let mut buf = [0u8; 16];
            let n = s.read(&mut buf).unwrap();
            let mut reply = b"echo:".to_vec();
            reply.extend_from_slice(&buf[..n]);
            s.write_all(&reply).unwrap();
        });

        // 在某個 host port 起 relay → 127.0.0.1:guest_port（以 127.0.0.1 代替 guest IP）。
        let host_listener = L::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let host_port = host_listener.local_addr().unwrap().port();
        drop(host_listener); // 釋放埠給 relay 用（仍有 TOCTOU 風險，但測試環境可接受）
        spawn_tcp_forward(Ipv4Addr::LOCALHOST, host_port, guest_port).unwrap();

        // 透過 host_port 連線，應被轉到 guest 服務。
        let mut c = TcpStream::connect((Ipv4Addr::LOCALHOST, host_port)).unwrap();
        c.write_all(b"hi").unwrap();
        c.shutdown(std::net::Shutdown::Write).unwrap();
        let mut got = String::new();
        c.read_to_string(&mut got).unwrap();
        assert_eq!(got, "echo:hi");
        server.join().unwrap();
    }

    #[test]
    fn appliance_lookup_requires_both_files() {
        let tmp = std::env::temp_dir().join("chefer-vz-util-test");
        let bundle = tmp.join("bundle");
        let vm = chefer_bundle::layout::vm_dir(&bundle);
        std::fs::create_dir_all(&vm).unwrap();
        // 只有 kernel → None
        std::fs::write(vm.join(chefer_bundle::layout::kernel_name("aarch64")), b"k").unwrap();
        assert!(appliance_in_bundle(&bundle, "aarch64").is_none());
        // 補上 initramfs → Some
        std::fs::write(
            vm.join(chefer_bundle::layout::initramfs_name("aarch64")),
            b"i",
        )
        .unwrap();
        let found = appliance_in_bundle(&bundle, "aarch64").unwrap();
        assert!(found.kernel.ends_with("chefer-vmlinuz-aarch64"));
        assert!(found.initramfs.ends_with("chefer-initramfs-aarch64"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
