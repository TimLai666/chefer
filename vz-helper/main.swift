// chefer-vz-helper — macOS vz 後端的開機 helper（Apple Virtualization.framework，Swift）。
//
// 由 chefer-runtime 的 vz 後端 spawn；以參數帶入 appliance（kernel+initramfs）、kernel cmdline、
// 要 virtiofs 共享的 bundle/data 目錄、CPU/RAM，開一台 Linux micro-VM 並把 guest 的序列 console
// （hvc0）接到本程序 stdout。guest 內 appliance init 會在 console 印出 CHEFER_GUEST_IP / CHEFER_GUEST_EXIT
// 標記，由 chefer-runtime 解析（見 docs/DESIGN.md macOS（vz）節）。
//
// stdin 契約（liveness）：stdin 同時是 guest console（hvc0）輸入與存活通道——vz.rs 以
// piped stdin spawn 並握住寫端，本程式讀到 **stdin EOF 即自我了結**（VM 在行程內，exit
// 即拆 VM）。這讓 runtime 被單發 SIGINT/SIGTERM/SIGKILL（非終端 Ctrl+C 的整組訊號）時
// helper/VM 不會殘留。手動測試時請保持 stdin 開啟（`< /dev/null` 會立即結束）。
//
// GUI 模式（`--gui`，DESIGN §6「macOS GUI」分期 ③④）：VM 組態加 virtio-gpu scanout +
// USB 鍵盤/絕對座標指標，開一個 AppKit 視窗承載 `VZVirtualMachineView`（VZ 自動把顯示與
// 鍵鼠 HID 接進 guest；guest 側 cage overlay 與 WHP 完全共用）。**關窗 = app 結束**（與 WHP
// 同語意）。剪貼簿與 WHP 共用協定：helper 產生隨機 token 附進 kernel cmdline
// （chefer.clip_token/clip_port），guest 的剪貼簿服務監聽 0.0.0.0:<port>，host 端（本程式）
// 經 VZ NAT **直連 guest IP**（自 console 的 CHEFER_GUEST_IP 標記得知）雙向同步 NSPasteboard
// （文字 + PNG；線協定 `<u8 kind><u32 be len><payload>`，kind 0=text/plain UTF-8、1=image/png）。
//
// 此檔僅在 macOS 上以 swiftc 編譯（需 -framework Virtualization，target macos13.0），並以
// com.apple.security.virtualization entitlement 簽章。GUI/剪貼簿路徑在 CI 僅 compile-check，
// 端到端需實體 Mac 驗證（GHA runner 開不了 VZ guest）。

import AppKit
import Foundation
import Network
import Security
import Virtualization

@inline(__always)
func ewrite(_ s: String) {
    FileHandle.standardError.write(Data(s.utf8))
}

func fail(_ msg: String, _ code: Int32 = 1) -> Never {
    ewrite("chefer-vz-helper: \(msg)\n")
    exit(code)
}

// --- 剪貼簿常數（與 guest-agent/src/clipboard.rs、whp-helper 一致）---
let CLIP_PORT: UInt16 = 55381
let CLIP_MAX_PAYLOAD = 32 * 1024 * 1024 // 單則上限（文字或 PNG）；防惡意/巨量 payload
let CLIP_KIND_TEXT: UInt8 = 0
let CLIP_KIND_PNG: UInt8 = 1

// --- 解析參數：--kernel/--initramfs/--cmdline/--bundle-dir/--data-dir/--cpus/--memory-mib
//     GUI：--gui（bare flag）/--gui-title <name> ---
let bareFlags: Set<String> = ["gui"]
var opts: [String: String] = [:]
let rawArgs = Array(CommandLine.arguments.dropFirst())
var argIdx = 0
while argIdx < rawArgs.count {
    let a = rawArgs[argIdx]
    guard a.hasPrefix("--") else { fail("unexpected argument: \(a)") }
    let key = String(a.dropFirst(2))
    if bareFlags.contains(key) {
        opts[key] = "1"
        argIdx += 1
        continue
    }
    guard argIdx + 1 < rawArgs.count else { fail("missing value for \(a)") }
    opts[key] = rawArgs[argIdx + 1]
    argIdx += 2
}
func req(_ k: String) -> String {
    guard let v = opts[k] else { fail("missing required --\(k)") }
    return v
}

let kernelPath = req("kernel")
let initramfsPath = req("initramfs")
var cmdline = req("cmdline")
let bundleDir = req("bundle-dir")
let dataDir = req("data-dir")
guard let cpus = Int(req("cpus")), cpus >= 1 else { fail("invalid --cpus") }
guard let memMib = UInt64(req("memory-mib")), memMib >= 1 else { fail("invalid --memory-mib") }
let guiMode = opts["gui"] == "1"
let guiTitle = opts["gui-title"].flatMap { $0.isEmpty ? nil : $0 } ?? "Chefer"

guard VZVirtualMachine.isSupported else {
    fail("Virtualization.framework is not supported on this machine", 2)
}

// --- GUI 尺寸（scanout 像素）：CHEFER_VZ_GUI_SIZE=WxH 覆寫，預設 1280x800，夾住荒謬值。---
func guiSize() -> (Int, Int) {
    if let v = ProcessInfo.processInfo.environment["CHEFER_VZ_GUI_SIZE"] {
        let parts = v.lowercased().split(separator: "x")
        if parts.count == 2, let w = Int(parts[0]), let h = Int(parts[1]) {
            return (min(max(w, 320), 7680), min(max(h, 240), 4320))
        }
        ewrite("chefer-vz-helper: ignoring invalid CHEFER_VZ_GUI_SIZE '\(v)' (want WxH)\n")
    }
    return (1280, 800)
}

// --- 剪貼簿 token（CSPRNG 16 bytes → hex；同 whp-helper 的 BCryptGenRandom 作法）---
func generateClipToken() -> String {
    var bytes = [UInt8](repeating: 0, count: 16)
    if SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes) == errSecSuccess {
        return bytes.map { String(format: "%02x", $0) }.joined()
    }
    // 極罕見失敗 → 退回 UUID 熵（仍不可預測到足以防同機竊聽的程度較弱，但可用）。
    return UUID().uuidString.replacingOccurrences(of: "-", with: "").lowercased()
}

let clipToken: String? = guiMode ? generateClipToken() : nil
if let token = clipToken {
    // 與 whp-helper 相同：helper 自行把剪貼簿 token/port 附進 kernel cmdline，guest 的
    // clipboard.rs 讀 /proc/cmdline 拿到後才會起服務並驗 token。
    cmdline += " chefer.clip_token=\(token) chefer.clip_port=\(CLIP_PORT)"
}

// 動態解析度：macOS 14+ **預設開**（實機驗證通過後翻預設，DESIGN §6 ④——guest 側
// re-modeset 與 HiDPI output scale 均由 guest-agent resize watcher 提供）。
// CHEFER_VZ_DYNAMIC_RESOLUTION=0 退回 view 等比縮放——保底用（例如手動搭配舊 overlay
// （cage <0.2.0，無 wlr-output-management）的 kit；正常 kit 的 overlay 與 helper 同版
// 出貨不會有此組合）。macOS 13 無 automaticallyReconfiguresDisplay，恆走 view 縮放。
let dynamicResolution: Bool = {
    if #available(macOS 14.0, *) {
        return ProcessInfo.processInfo.environment["CHEFER_VZ_DYNAMIC_RESOLUTION"] != "0"
    }
    return false
}()

// HiDPI（動態解析度模式限定）：automaticallyReconfiguresDisplay 推給 guest 的是
// Retina「實體像素」尺寸，Linux guest 無 HiDPI 概念 → UI/游標縮半。把開機當下螢幕的
// backingScaleFactor 附進 cmdline（同 clip_token 作法），guest-agent 的 resize watcher
// 據此對 cage 設 wlr-output-management 的 output scale——邏輯尺寸回到「點」數。
// 視窗跨螢幕拖移造成的 scale 變化暫不追蹤（恆用開機值）。
if #available(macOS 14.0, *), guiMode, dynamicResolution {
    let scale = (NSScreen.main ?? NSScreen.screens.first)?.backingScaleFactor ?? 1.0
    if scale > 1.0 {
        cmdline += " chefer.gui_scale=\(String(format: "%g", scale))"
    }
}

// --- 組態 ---
let bootLoader = VZLinuxBootLoader(kernelURL: URL(fileURLWithPath: kernelPath))
bootLoader.initialRamdiskURL = URL(fileURLWithPath: initramfsPath)
bootLoader.commandLine = cmdline

let config = VZVirtualMachineConfiguration()
config.bootLoader = bootLoader
config.cpuCount = cpus
config.memorySize = memMib * 1024 * 1024

// guest 序列 console（kernel 視為 hvc0）：
//  - 無頭：寫出 → 本程序 stdout（zero-copy 直通）；讀入 ← stdin（經下方 relay）。
//  - GUI：寫出 → 內部 pipe（tee 執行緒轉發到 stdout **同時**掃 CHEFER_GUEST_IP 標記，
//    取得 guest IP 後才能起剪貼簿同步——runtime 端讀我們的 stdout 解析標記，直通不變）。
//
// stdin 不能直接交給 VZ 讀：本程式需要「讀到 stdin EOF」作為 chefer-runtime 行程死亡的
// 訊號（見檔頭 stdin 契約；runtime 被單發 SIGINT/SIGTERM/SIGKILL 時 helper 收不到訊號，
// 實機曾因此殘留 VM 吃 CPU）。讀 stdin 的只能有一個，故由專屬執行緒代讀、轉進內部
// pipe 餵 VZ serial（console 輸入路徑不變，多一跳 copy 對鍵盤輸入無感），EOF 即收攤。
let stdinRelayPipe = Pipe()
let stdinWatchThread = Thread {
    let src = FileHandle.standardInput
    let dst = stdinRelayPipe.fileHandleForWriting
    while true {
        let chunk = src.availableData
        if chunk.isEmpty { break } // EOF：runtime 行程已死（或明確關閉 stdin）
        do { try dst.write(contentsOf: chunk) } catch { break } // 讀端沒了 → 一樣收攤
    }
    ewrite("chefer-vz-helper: stdin closed (chefer-runtime is gone); shutting down\n")
    exit(0) // VM 在行程內，exit 即拆 VM；此時 runtime 已死，exit code 無人讀取
}
stdinWatchThread.name = "chefer-stdin-watch"
stdinWatchThread.start()

let consolePipe: Pipe? = guiMode ? Pipe() : nil
let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
serial.attachment = VZFileHandleSerialPortAttachment(
    fileHandleForReading: stdinRelayPipe.fileHandleForReading,
    fileHandleForWriting: consolePipe?.fileHandleForWriting ?? FileHandle.standardOutput)
config.serialPorts = [serial]

// virtiofs 共享：bundle（唯讀）與 data（讀寫），tag 與 appliance init 約定一致。
func shareDevice(tag: String, path: String, readOnly: Bool) -> VZVirtioFileSystemDeviceConfiguration {
    let dev = VZVirtioFileSystemDeviceConfiguration(tag: tag)
    let dir = VZSharedDirectory(url: URL(fileURLWithPath: path), readOnly: readOnly)
    dev.share = VZSingleDirectoryShare(directory: dir)
    return dev
}
config.directorySharingDevices = [
    shareDevice(tag: "chefer-bundle", path: bundleDir, readOnly: true),
    shareDevice(tag: "chefer-data", path: dataDir, readOnly: false),
]

// NAT 網路：guest 取得 NAT IP（host 在同一子網可直接連），entropy + 記憶體氣球為慣例裝置。
let net = VZVirtioNetworkDeviceConfiguration()
net.attachment = VZNATNetworkDeviceAttachment()
config.networkDevices = [net]
config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
config.memoryBalloonDevices = [VZVirtioTraditionalMemoryBalloonDeviceConfiguration()]

// GUI：virtio-gpu 一個 scanout + USB 鍵盤 + 絕對座標指標（VZVirtualMachineView 自動把
// HID 事件轉進這些裝置；絕對座標同 WHP 的 tablet 選擇，避免滑鼠捕捉問題）。
//
// 動態解析度為 macOS 14+ 預設（實機驗證後定案，DESIGN §6 ④）：
// automaticallyReconfiguresDisplay 把視窗 backing 像素尺寸推進 guest，guest-agent 的
// resize watcher 讓 cage re-modeset 跟隨、並依上面的 chefer.gui_scale 設 output scale
// （Retina 實體像素模式下邏輯尺寸回到「點」數，游標/UI 尺寸正常）——比例自由、無
// letterbox（拖拉中的細黑邊為 debounce 前的暫態）。實機檢核（游標/UI 尺寸、座標映射、
// Xwayland 畫質、xdpyinfo 邏輯尺寸）2026-07 全數通過。
// CHEFER_VZ_DYNAMIC_RESOLUTION=0（或 macOS 13）退回舊取捨：scanout=視窗「點」數、
// automaticallyReconfiguresDisplay 關、view 等比縮放＋鎖長寬比（Retina 上 2× 放大略軟
// 但尺寸正確）——guest 不 re-modeset（如舊 overlay 的 cage <0.2.0）時的保底。
let (guiW, guiH) = guiSize()
if guiMode {
    let gpu = VZVirtioGraphicsDeviceConfiguration()
    gpu.scanouts = [
        VZVirtioGraphicsScanoutConfiguration(widthInPixels: guiW, heightInPixels: guiH)
    ]
    config.graphicsDevices = [gpu]
    config.keyboards = [VZUSBKeyboardConfiguration()]
    config.pointingDevices = [VZUSBScreenCoordinatePointingDeviceConfiguration()]
}

do {
    try config.validate()
} catch {
    fail("VM configuration is invalid: \(error.localizedDescription)", 3)
}

// --- 剪貼簿 host（GUI 模式；與 whp-helper/src/clipboard_host.rs 同協定）---
//
// 經 VZ NAT 直連 guest_ip:CLIP_PORT（guest 服務起來前連線會失敗 → 每秒重試）。
// 連上後先送一行 token（\n 結尾），之後雙向 `<u8 kind><u32 be len><payload>`。
// 回音抑制：套用對端內容後記住 (kind, payload) 與 changeCount，本地 watcher 看到相同
// 內容或自己造成的 changeCount 就不回送。NSPasteboard 操作一律經主執行緒。
final class ClipboardHost {
    private let queue = DispatchQueue(label: "com.chefer.vz-clip")
    private let host: NWEndpoint.Host
    private let port: NWEndpoint.Port
    private let token: String
    private let trace = ProcessInfo.processInfo.environment["CHEFER_CLIP_TRACE"] != nil
    private var conn: NWConnection?
    private var ready = false
    private var lastApplied: (UInt8, Data)?
    private var lastSent: (UInt8, Data)?
    private var suppressChangeCount = -1
    private var lastSeenChangeCount: Int
    private var timer: DispatchSourceTimer?
    /// connect() 每次遞增；讓 .waiting 的逾時回呼辨識「還是不是同一條連線」。
    private var generation = 0
    /// 同一條連線只排一次 .waiting 逾時（.waiting 可能因錯誤變化多次觸發）。
    private var waitingRetryArmed = false
    private static var warnedLocalNetwork = false

    init(guestIP: String, token: String) {
        self.host = NWEndpoint.Host(guestIP)
        self.port = NWEndpoint.Port(rawValue: CLIP_PORT)!
        self.token = token
        self.lastSeenChangeCount = onMain { NSPasteboard.general.changeCount }
    }

    func start() {
        queue.async { self.connect() }
    }

    private func tracef(_ s: String) {
        if trace { ewrite("chefer-vz-helper: clipboard: \(s)\n") }
    }

    private func connect() {
        generation += 1
        waitingRetryArmed = false
        let gen = generation
        let c = NWConnection(host: host, port: port, using: .tcp)
        conn = c
        ready = false
        c.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .ready:
                self.tracef("connected to guest, sending token")
                var hello = Data(self.token.utf8)
                hello.append(0x0A) // token 行（\n 結尾）
                c.send(content: hello, completion: .contentProcessed { [weak self] err in
                    guard let self else { return }
                    if err != nil {
                        self.reconnect()
                        return
                    }
                    self.ready = true
                    self.receiveHeader()
                    self.startWatcher()
                })
            case .waiting(let err):
                // 實機發現：對 guest:55381 的連線偶發卡在 .waiting 數十秒或永不成功
                // （同一時間 shell 的 nc 可秒連）——疑為 macOS「區域網路」隱私權對
                // ad-hoc 簽章 helper 的閘門。此狀態以前落在 default 分支完全靜默，
                // 剪貼簿「就是不動」無從排查。現在：印一次性 stderr 提示引導放行，
                // 並在 3 秒仍未 ready 時棄掉這條連線重連（新連線常可立即成功）。
                self.tracef("connection waiting: \(err)")
                ClipboardHost.warnLocalNetworkOnce(err)
                if !self.waitingRetryArmed {
                    self.waitingRetryArmed = true
                    self.queue.asyncAfter(deadline: .now() + 3) { [weak self] in
                        guard let self, self.generation == gen, !self.ready else { return }
                        self.tracef("still waiting after 3s; dropping the connection and retrying")
                        self.reconnect()
                    }
                }
            case .failed(let err):
                self.tracef("connection failed: \(err); retrying in 1s")
                self.reconnect()
            case .cancelled:
                break
            default:
                break
            }
        }
        c.start(queue: queue)
    }

    private func reconnect() {
        // 多個失敗路徑（send/receive 錯誤、.failed、.waiting 逾時）可能對同一條連線
        // 各觸發一次——conn 已被清掉代表重連在途，去重以免疊出多條並行連線。
        guard let c = conn else { return }
        ready = false
        c.cancel()
        conn = nil
        queue.asyncAfter(deadline: .now() + 1) { [weak self] in self?.connect() }
    }

    /// 區域網路隱私權提示：一個行程只印一次（不 gated on CHEFER_CLIP_TRACE——這是
    /// 使用者可行動的排查線索，靜默時幾乎無從發現）。
    private static func warnLocalNetworkOnce(_ err: NWError) {
        guard !warnedLocalNetwork else { return }
        warnedLocalNetwork = true
        ewrite(
            "chefer-vz-helper: clipboard sync to the guest is stuck in 'waiting' (\(err)). "
                + "This is usually macOS Local Network privacy blocking the connection - open "
                + "System Settings > Privacy & Security > Local Network and enable the app that "
                + "launched this program (e.g. your terminal). A helper signed with a stable "
                + "identity (CHEFER_CODESIGN_IDENTITY in scripts/build-vz-helper.sh) lets macOS "
                + "remember the grant. Clipboard sync keeps retrying; the app itself is unaffected.\n"
        )
    }

    /// 讀 5-byte 標頭（1 kind + 4 BE len），再讀 payload；EOF/錯誤 → 重連。
    private func receiveHeader() {
        conn?.receive(minimumIncompleteLength: 5, maximumLength: 5) { [weak self] data, _, complete, err in
            guard let self else { return }
            guard err == nil, let data, data.count == 5 else {
                if complete || err != nil { self.reconnect() }
                return
            }
            let kind = data[data.startIndex]
            let len = data.dropFirst().reduce(0) { ($0 << 8) | Int($1) }
            if len > CLIP_MAX_PAYLOAD {
                self.tracef("payload too large (\(len)); dropping connection")
                self.reconnect()
                return
            }
            if len == 0 {
                self.apply(kind: kind, payload: Data())
                self.receiveHeader()
                return
            }
            self.receivePayload(kind: kind, remaining: len, acc: Data())
        }
    }

    private func receivePayload(kind: UInt8, remaining: Int, acc: Data) {
        conn?.receive(minimumIncompleteLength: remaining, maximumLength: remaining) {
            [weak self] data, _, complete, err in
            guard let self else { return }
            guard err == nil, let data, data.count == remaining else {
                if complete || err != nil { self.reconnect() }
                return
            }
            var payload = acc
            payload.append(data)
            self.apply(kind: kind, payload: payload)
            self.receiveHeader()
        }
    }

    /// guest → host：套用到 NSPasteboard（PNG 同時放 .png 與 .tiff，讓傳統 app 也吃得到；
    /// 鏡像 whp 同時寫 "PNG" 與 CF_DIB 的作法）。
    private func apply(kind: UInt8, payload: Data) {
        onMain {
            let pb = NSPasteboard.general
            switch kind {
            case CLIP_KIND_TEXT:
                guard let s = String(data: payload, encoding: .utf8) else { return }
                pb.clearContents()
                pb.setString(s, forType: .string)
            case CLIP_KIND_PNG:
                pb.clearContents()
                pb.setData(payload, forType: .png)
                if let rep = NSBitmapImageRep(data: payload), let tiff = rep.tiffRepresentation {
                    pb.setData(tiff, forType: .tiff)
                }
            default:
                return // 未知 kind：忽略（新版協定的前向相容由同版出貨保證）
            }
            self.suppressChangeCount = pb.changeCount
            self.lastSeenChangeCount = pb.changeCount
        }
        lastApplied = (kind, payload)
        tracef("applied from guest (kind \(kind), \(payload.count) bytes)")
    }

    /// host → guest：輪詢 NSPasteboard changeCount，變更且非回音就送出。
    private func startWatcher() {
        timer?.cancel()
        let t = DispatchSource.makeTimerSource(queue: queue)
        t.schedule(deadline: .now() + 0.3, repeating: 0.3)
        t.setEventHandler { [weak self] in
            guard let self, self.ready else { return }
            guard let item = self.readLocalIfChanged() else { return }
            if let last = self.lastApplied, last == item {
                return // 回音：剛套用自 guest 的內容
            }
            if let sent = self.lastSent, sent == item {
                return // 已送過相同內容
            }
            self.send(item: item)
        }
        t.resume()
        timer = t
    }

    /// changeCount 變了才讀（PNG 優先，無 PNG 但有 TIFF 則轉 PNG——鏡像 whp 的
    /// "PNG"/CF_DIB 讀取；再退回文字）。自己造成的變更（suppressChangeCount）跳過。
    private func readLocalIfChanged() -> (UInt8, Data)? {
        onMain {
            let pb = NSPasteboard.general
            let cc = pb.changeCount
            if cc == self.lastSeenChangeCount { return nil }
            self.lastSeenChangeCount = cc
            if cc == self.suppressChangeCount { return nil }
            if let png = pb.data(forType: .png), png.count <= CLIP_MAX_PAYLOAD {
                return (CLIP_KIND_PNG, png)
            }
            if let tiff = pb.data(forType: .tiff),
                let rep = NSBitmapImageRep(data: tiff),
                let png = rep.representation(using: .png, properties: [:]),
                png.count <= CLIP_MAX_PAYLOAD
            {
                return (CLIP_KIND_PNG, png)
            }
            if let s = pb.string(forType: .string) {
                let data = Data(s.utf8)
                if data.count <= CLIP_MAX_PAYLOAD { return (CLIP_KIND_TEXT, data) }
            }
            return nil
        }
    }

    private func send(item: (UInt8, Data)) {
        let (kind, payload) = item
        var frame = Data(capacity: 5 + payload.count)
        frame.append(kind)
        var be = UInt32(payload.count).bigEndian
        withUnsafeBytes(of: &be) { frame.append(contentsOf: $0) }
        frame.append(payload)
        lastSent = item
        tracef("sent to guest (kind \(kind), \(payload.count) bytes)")
        conn?.send(content: frame, completion: .contentProcessed { [weak self] err in
            if err != nil { self?.reconnect() }
        })
    }
}

/// NSPasteboard/AppKit 操作一律回主執行緒（主執行緒直呼、其他執行緒 sync 過去）。
func onMain<T>(_ body: () -> T) -> T {
    if Thread.isMainThread { return body() }
    return DispatchQueue.main.sync(execute: body)
}

// --- 開機 ---
// VZVirtualMachine 必須在其所屬 queue 上操作：無頭走專屬序列 queue；GUI 走主 queue
// （VZVirtualMachineView 要求 VM 綁主 queue）。delegate 回呼在該 queue 上。
final class Delegate: NSObject, VZVirtualMachineDelegate {
    /// GUI 模式下 console 經內部 pipe tee 轉發，行程結束前給 tee 執行緒一點時間把
    /// 佇列中的 console 尾巴（含 CHEFER_GUEST_EXIT 標記）沖到 stdout。無頭模式（直寫
    /// stdout）為 0。
    static var drainBeforeExit: TimeInterval = 0

    private static func exitAfterDrain(_ code: Int32) -> Never {
        if drainBeforeExit > 0 { Thread.sleep(forTimeInterval: drainBeforeExit) }
        exit(code)
    }

    func guestDidStop(_ virtualMachine: VZVirtualMachine) {
        // guest 內 poweroff（appliance init 收尾）→ 乾淨關機。整體 exit code 由 console 的
        // CHEFER_GUEST_EXIT 標記回傳，helper 本身回 0。
        Delegate.exitAfterDrain(0)
    }
    func virtualMachine(_ virtualMachine: VZVirtualMachine, didStopWithError error: Error) {
        ewrite("chefer-vz-helper: VM stopped with error: \(error.localizedDescription)\n")
        Delegate.exitAfterDrain(4)
    }
}

/// GUI 視窗委派：**關窗 = app 結束**（與 WHP 同語意）。先試著停 VM，2 秒保底直接退出
/// （helper 乾淨退出且無 exit 標記 → runtime 視為 app 以 0 結束）。
final class WindowDelegate: NSObject, NSWindowDelegate {
    let vm: VZVirtualMachine
    init(vm: VZVirtualMachine) { self.vm = vm }
    func windowWillClose(_ notification: Notification) {
        DispatchQueue.main.asyncAfter(deadline: .now() + 2) { exit(0) }
        if vm.canStop {
            vm.stop { _ in exit(0) }
        } else {
            exit(0)
        }
    }
}

let delegate = Delegate()

if guiMode {
    Delegate.drainBeforeExit = 0.3

    // GUI：VM 綁主 queue（VZVirtualMachineView 要求），AppKit 事件迴圈驅動。
    let vm = VZVirtualMachine(configuration: config)
    vm.delegate = delegate

    let app = NSApplication.shared
    app.setActivationPolicy(.regular)

    let view = VZVirtualMachineView(
        frame: NSRect(x: 0, y: 0, width: CGFloat(guiW), height: CGFloat(guiH)))
    view.capturesSystemKeys = true
    view.virtualMachine = vm
    view.autoresizingMask = [.width, .height]

    // 視窗以「點」開 guiW×guiH（scanout 初值同尺寸）。動態解析度（macOS 14+ 預設）：
    // automaticallyReconfiguresDisplay 開機即把 backing 像素尺寸推進 guest、拖拉時
    // re-modeset 跟隨（guest 側 watcher + chefer.gui_scale，見上）。opt-out（=0 或
    // macOS 13）：automaticallyReconfiguresDisplay 關、view 等比縮放＋鎖長寬比。
    let window = NSWindow(
        contentRect: NSRect(x: 0, y: 0, width: CGFloat(guiW), height: CGFloat(guiH)),
        styleMask: [.titled, .closable, .miniaturizable, .resizable],
        backing: .buffered,
        defer: false)
    if #available(macOS 14.0, *), dynamicResolution {
        view.automaticallyReconfiguresDisplay = true
    } else {
        // guest 不 re-modeset（cage 固定模式）→ view 等比縮放；視窗內容區若偏離 guest
        // 的長寬比會 letterbox 出黑邊（實測：拖到近全螢幕時標題列/選單列吃掉高度、比例
        // 跑掉）。鎖定內容區長寬比＝guest 比例，拖拉永遠無縫。真動態解析度模式下不鎖
        // （guest 會跟著換模式，自由比例才有意義）。
        window.contentAspectRatio = NSSize(width: CGFloat(guiW), height: CGFloat(guiH))
    }
    window.title = guiTitle
    window.contentView = view
    // ARC 下的既知陷阱：程式化 NSWindow 預設 isReleasedWhenClosed=true，關窗會對已由
    // ARC 管理的物件多一次 release → 可能在 windowWillClose 的 exit(0) 落地前 crash。
    window.isReleasedWhenClosed = false
    let windowDelegate = WindowDelegate(vm: vm)
    window.delegate = windowDelegate
    window.center()
    window.makeKeyAndOrderFront(nil)
    app.activate(ignoringOtherApps: true)

    // 測試鉤子（vz-smoke 自動驗證動態解析度用；僅動態解析度模式有意義）：
    // CHEFER_VZ_GUI_TEST_RESIZE="<秒>:<W>x<H>" → N 秒後程式化改視窗 content 尺寸。
    // view 尺寸變更後的下游與使用者拖拉完全相同（automaticallyReconfiguresDisplay →
    // VZ 對 guest virtio-gpu 重組態 → guest-agent resize watcher 讓 cage re-modeset），
    // 讓沒有 Accessibility 權限的自動化環境也能驗證整條鏈。W/H 為 point，Retina 下
    // guest 實際拿到的是像素尺寸（×backingScaleFactor）；guest 端 output scale
    // （chefer.gui_scale）會把邏輯尺寸縮回點數（xdpyinfo 應回報 WxH 而非像素尺寸）。
    if #available(macOS 14.0, *), dynamicResolution,
        let spec = ProcessInfo.processInfo.environment["CHEFER_VZ_GUI_TEST_RESIZE"]
    {
        let parts = spec.split(separator: ":", maxSplits: 1)
        let dims = parts.count == 2 ? parts[1].lowercased().split(separator: "x") : []
        if parts.count == 2, let delay = Double(parts[0]),
            dims.count == 2, let w = Int(dims[0]), let h = Int(dims[1])
        {
            DispatchQueue.main.asyncAfter(deadline: .now() + delay) {
                ewrite("chefer-vz-helper: test resize -> \(w)x\(h)\n")
                window.setContentSize(NSSize(width: w, height: h))
            }
        } else {
            ewrite(
                "chefer-vz-helper: ignoring invalid CHEFER_VZ_GUI_TEST_RESIZE '\(spec)' (want <seconds>:<W>x<H>)\n"
            )
        }
    }

    // console tee：pipe → stdout 直通 + 掃 CHEFER_GUEST_IP（首見即起剪貼簿同步）。
    var clipboardHost: ClipboardHost? = nil
    let teePipe = consolePipe! // guiMode 下必建
    let teeThread = Thread {
        let fh = teePipe.fileHandleForReading
        let out = FileHandle.standardOutput
        var buf = [UInt8]()
        var clipStarted = false
        while true {
            let chunk = fh.availableData
            if chunk.isEmpty { break } // 寫端全關（行程收尾）
            out.write(chunk)
            buf.append(contentsOf: chunk)
            while let nl = buf.firstIndex(of: 0x0A) {
                let lineBytes = Array(buf[..<nl])
                buf.removeSubrange(...nl)
                guard !clipStarted, let token = clipToken,
                    let line = String(bytes: lineBytes, encoding: .utf8),
                    let range = line.range(of: "CHEFER_GUEST_IP=")
                else { continue }
                let ip = String(line[range.upperBound...].prefix { !$0.isWhitespace })
                guard !ip.isEmpty else { continue }
                clipStarted = true
                let host = ClipboardHost(guestIP: ip, token: token)
                clipboardHost = host
                host.start()
            }
        }
        _ = clipboardHost // 保持存活（行程隨 VM 結束而退出）
    }
    teeThread.name = "chefer-console-tee"
    teeThread.start()

    DispatchQueue.main.async {
        vm.start { result in
            if case .failure(let error) = result {
                ewrite("chefer-vz-helper: failed to start VM: \(error.localizedDescription)\n")
                exit(5)
            }
        }
    }
    app.run() // 事件迴圈；關機/錯誤/關窗由 delegate 以 exit() 結束
} else {
    // 無頭：專屬序列 queue + dispatchMain（原路徑，console 直寫 stdout、零額外一跳）。
    let queue = DispatchQueue(label: "com.chefer.vz-helper")
    let vm = VZVirtualMachine(configuration: config, queue: queue)
    vm.delegate = delegate

    queue.async {
        vm.start { result in
            switch result {
            case .success:
                break
            case .failure(let error):
                ewrite("chefer-vz-helper: failed to start VM: \(error.localizedDescription)\n")
                exit(5)
            }
        }
    }
    dispatchMain()
}
