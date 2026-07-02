# WHP virtio-mmio 實作 Roadmap / 實機接手指南

目標：讓 chefer 單檔 app 在**沒有 WSL** 的 Windows 上，用 Windows Hypervisor Platform
（WHP）開一台 Linux micro-VM 把**真 bundle 一鍵跑起來**。架構契約見
[DESIGN.md](DESIGN.md) §6「whp 後端 / virtio-mmio 裝置模型」。

本檔記錄**已完成的純邏輯地基**與**剩餘需在實機完成/驗證的接線**，供之後在實機旁接手。

## 為何剩餘部分需要實機

- GitHub runner 開不了 WHP（巢狀虛擬化限制），與 macOS vz 同樣困境 → 接線層（真開 VM、
  讓 Linux guest 的 virtio 驅動跟我們的裝置模型互動）**無法靠 CI 驗證**，只能在實體
  Windows（硬體虛擬化 + WHP 功能開啟）手動跑。
- virtio 接線的正確性魔鬼在實機細節：指令模擬器 callback 語意、中斷時序、virtqueue 記憶體
  屏障——盲寫無法保證，必須邊跑邊除錯。故純邏輯地基先以單元測試鎖死，接線階段才不會
  兩頭不確定。

## 已完成（純邏輯，CI 三平台綠，28 單元測試）

| 元件 | 檔案 | 內容 |
|---|---|---|
| 後端覆寫開關 | `vmm-backend/src/lib.rs` | `CHEFER_BACKEND=whp` 強制選後端（有 WSL 的機器才測得到 whp） |
| virtio-mmio transport | `whp-helper/src/virtio/mmio.rs` | register state machine：feature 協商、status、queue 位址、IRQ status、`MmioAction` 回報 |
| split virtqueue | `whp-helper/src/virtio/queue.rs` | `pop_avail`（走 desc chain）、`push_used`（回填 used ring），idx 環繞、拒 indirect |
| virtio-blk 裝置 | `whp-helper/src/virtio/blk.rs` | `process_chain`：IN/OUT/FLUSH/GET_ID、唯讀 IOERR、越界 IOERR、capacity config |
| image 轉換 | `whp-helper/src/virtio/image.rs` | `pack_dir`（目錄→sector 對齊 tar image）、`unpack_image`（解回，供 data 回寫） |

這些已是穩固、可組合的積木；接線階段直接用，不需重寫。

## 剩餘里程碑（實機）

### M3 接線 — virtio-blk 接上 WHP run loop
1. `whp_api::run_loop` 的 `EXIT_MEM_ACCESS`（目前一律報錯，main.rs ~line 819）改為：
   GPA 落在某個 virtio-mmio 視窗 → 用 **`WinHvEmulation.dll`** 的
   `WHvEmulatorCreateEmulator` + `WHvEmulatorTryMmioEmulation`（Memory callback 內依
   offset 呼叫 `Mmio::read`/`write`）完成存取。**不要自寫 x86 指令 decoder。**
2. `MmioAction::QueueNotify(q)` → 用 `SplitQueue::new(mmio.queue(q), mem, last_avail)` 取
   chain，餵 `BlkDevice::process_chain`，`push_used`，`mmio.signal_used()`，經 PIC 注入該
   裝置 IRQ。
3. `MmioAction::ConfigRead/Write` → 轉給 `BlkDevice::config_read`（capacity）。
4. boot 前：`image::pack_dir(bundle_dir)` → vda backing（ro）、`pack_dir(data_dir)` → vdb
   backing（rw）。關機後：`image::unpack_image(vdb_backing, data_dir)` 持久化回寫。
5. `GuestMemory` 用 `SliceMem::new(0, &mut host_mapped_ram)` 包 boot loop 的 VirtualAlloc 區。

### M4 — virtio-net + 埠轉發
- 新 `virtio/net.rs`：兩個 queue（rx/tx），virtio-net header 解析，tx 取封包送 host backend、
  rx 把 host 來的封包填回。
- host backend：user-mode TCP/IP（slirp 風格，較重）或橋接 host loopback。最重的一塊。
- guest 取得 IP 後，appliance init 印 `CHEFER_GUEST_IP=<ipv4>`（vmm-backend 已會解析並起
  `vz_util::spawn_tcp_forward` / UDP relay，與 vz 共用）。

### M5 — appliance（需 Linux/WSL build kernel）
- kernel config 加：`CONFIG_VIRTIO_MMIO`、`CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES`、
  `CONFIG_VIRTIO_BLK`、`CONFIG_VIRTIO_NET`（保留現有 virtiofs/hvc0 給 vz 共用）。
- cmdline 追加各裝置：`virtio_mmio.device=0x200@0xd0000000:5 virtio_mmio.device=0x200@0xd0000200:6 ...`
  （base/IRQ 對齊 boot loop 的 GPA 佈局與 PIC 線）。
- `scripts/appliance/init` 改**自適應**：偵測 `/dev/hvc0`（vz）→ 掛 virtiofs；否則（whp）→
  `mount /dev/vda /mnt/bundle`（ro）、`mount /dev/vdb /mnt/data`（rw）、console 用 ttyS0。
  維持「whp 與 vz 共用一份 appliance」。
- 若 bundle 大導致 tar 展開吃 RAM：vda 改唯讀 squashfs image（host 端產生需 mksquashfs，
  屆時再評估）。

### M6 — 端到端真 bundle（實機驗證）
1. 在 Linux/WSL `bash scripts/build-appliance.sh`（含上述 virtio config）產 vmlinuz/initramfs。
2. `cargo build --release -p whp-helper`，放進 kit。
3. `cargo run -p chefer-cli -- build examples/demo/appcipe.yml`（產單檔，內含 appliance + helper）。
4. **在沒有 WSL 或設 `CHEFER_BACKEND=whp` 的 Windows** 跑單檔，驗 redis + app 起來、埠可連、
   `CHEFER_GUEST_EXIT` 正確回傳、data 持久化。

## 已知陷阱（前期 boot shim 實測已確認，務必沿用）

- init 必須 **non-PIE 靜態 ELF**（`ET_EXEC`；PIE 在無 dynamic linker 的 minimal VM segfault）。
- initramfs 必須含 `/dev/console`（char 5:1）供 kernel 開 init 的 stdio。
- exit code 走 **`/dev/kmsg`**（非 stdout）：user-space 寫 tty 需 serial IRQ4，WHP 最小 8250
  不觸發；`/dev/kmsg` 走 kernel printk polled I/O。
- WHP 專屬 cmdline：`nolapic lpj=1000000 notsc clocksource=jiffies`（見 DESIGN §4）。
- timer interrupt 由 host 背景執行緒每 10ms 注入（WHP LAPIC emulation 不完整）。
- **bridge 出網（pasta）在 VM guest 的三個前提**（缺一即靜默降級 internal）：kernel
  `CONFIG_TUN`（x86_64 defconfig 不含）；`/dev/net/tun` 節點 0666（devtmpfs 預設 0600，
  pasta 以非特權 uid 開不了）；根不能是 initramfs 的 rootfs（pasta 自我沙箱 `pivot_root`
  在 rootfs 上 EINVAL）——appliance init 已依序處理（switch_root 到 tmpfs 根 + tun 節點）。
  另外 Windows host 打包記錄不了執行位，guest-agent 對 pasta exec EACCES 以 staged copy
  補 0755 重試（`netns.rs`）。除錯時 guest-agent stderr 在 WHP 不可見，可暫改 init 把
  stderr 收 `/tmp/ga.err` 再逐行 `report` 重播（`<0>` 優先序，`quiet` 下才上得了 console）。

## 在這台（有 WSL）怎麼測 whp

`CHEFER_BACKEND=whp` 強制走 whp 後端（否則 `run_app` 會先選中 wsl2）。
