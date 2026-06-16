// chefer-vz-helper — macOS vz 後端的開機 helper（Apple Virtualization.framework，Swift）。
//
// 由 chefer-runtime 的 vz 後端 spawn；以參數帶入 appliance（kernel+initramfs）、kernel cmdline、
// 要 virtiofs 共享的 bundle/data 目錄、CPU/RAM，開一台 Linux micro-VM 並把 guest 的序列 console
// （hvc0）接到本程序 stdout。guest 內 appliance init 會在 console 印出 CHEFER_GUEST_IP / CHEFER_GUEST_EXIT
// 標記，由 chefer-runtime 解析（見 docs/DESIGN.md macOS（vz）節）。
//
// 此檔僅在 macOS 上以 swiftc 編譯（需 -framework Virtualization），並以
// com.apple.security.virtualization entitlement 簽章。

import Foundation
import Virtualization

@inline(__always)
func ewrite(_ s: String) {
    FileHandle.standardError.write(Data(s.utf8))
}

func fail(_ msg: String, _ code: Int32 = 1) -> Never {
    ewrite("chefer-vz-helper: \(msg)\n")
    exit(code)
}

// --- 解析參數：--kernel/--initramfs/--cmdline/--bundle-dir/--data-dir/--cpus/--memory-mib ---
var opts: [String: String] = [:]
var argIter = CommandLine.arguments.dropFirst().makeIterator()
while let a = argIter.next() {
    guard a.hasPrefix("--") else { fail("unexpected argument: \(a)") }
    guard let v = argIter.next() else { fail("missing value for \(a)") }
    opts[String(a.dropFirst(2))] = v
}
func req(_ k: String) -> String {
    guard let v = opts[k] else { fail("missing required --\(k)") }
    return v
}

let kernelPath = req("kernel")
let initramfsPath = req("initramfs")
let cmdline = req("cmdline")
let bundleDir = req("bundle-dir")
let dataDir = req("data-dir")
guard let cpus = Int(req("cpus")), cpus >= 1 else { fail("invalid --cpus") }
guard let memMib = UInt64(req("memory-mib")), memMib >= 1 else { fail("invalid --memory-mib") }

guard VZVirtualMachine.isSupported else {
    fail("Virtualization.framework is not supported on this machine", 2)
}

// --- 組態 ---
let bootLoader = VZLinuxBootLoader(kernelURL: URL(fileURLWithPath: kernelPath))
bootLoader.initialRamdiskURL = URL(fileURLWithPath: initramfsPath)
bootLoader.commandLine = cmdline

let config = VZVirtualMachineConfiguration()
config.bootLoader = bootLoader
config.cpuCount = cpus
config.memorySize = memMib * 1024 * 1024

// guest 序列 console（kernel 視為 hvc0）：寫出 → 本程序 stdout；讀入 ← 本程序 stdin。
let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
serial.attachment = VZFileHandleSerialPortAttachment(
    fileHandleForReading: FileHandle.standardInput,
    fileHandleForWriting: FileHandle.standardOutput)
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

do {
    try config.validate()
} catch {
    fail("VM configuration is invalid: \(error.localizedDescription)", 3)
}

// --- 開機 ---
// VZVirtualMachine 必須在其所屬 queue 上操作；用專屬序列 queue，delegate 回呼也在該 queue。
final class Delegate: NSObject, VZVirtualMachineDelegate {
    func guestDidStop(_ virtualMachine: VZVirtualMachine) {
        // guest 內 poweroff（appliance init 收尾）→ 乾淨關機。整體 exit code 由 console 的
        // CHEFER_GUEST_EXIT 標記回傳，helper 本身回 0。
        exit(0)
    }
    func virtualMachine(_ virtualMachine: VZVirtualMachine, didStopWithError error: Error) {
        ewrite("chefer-vz-helper: VM stopped with error: \(error.localizedDescription)\n")
        exit(4)
    }
}

let queue = DispatchQueue(label: "com.chefer.vz-helper")
let delegate = Delegate()
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

// 維持 run loop；關機/錯誤由 delegate 以 exit() 結束。
dispatchMain()
