# Chefer GPU Demo：把 GPU 加速程式打包成單檔

一個最小但完整的範例：把官方 CUDA 映像打包成單一執行檔，用 `gpu: true` 把 host 的 NVIDIA GPU
通進容器，再跑 `nvidia-smi` 證明整條鏈路真的通了——**不只是把裝置節點綁進去，而是驅動確實回話、列得出 GPU**。

預設關：不開 `gpu` 時容器的 `/dev` 只有固定 allowlist（`null`/`zero`/`random`/…），程式一律 CPU-only、不破壞隔離。

## 這個 demo 驗證什麼

- ✅ **opt-in GPU passthrough**：`gpu: true` 讓 chefer 把 host GPU 節點與驅動 libs 綁進「這個」服務的容器。
- ✅ **驅動真的通**：容器內 `nvidia-smi` 列出 host GPU（含即時溫度/記憶體/使用率），而非只是看得到 `/dev` 節點。

實測（Windows + WSL2，GeForce GT 1030 / driver 582.66，2026-07）：

```
== GPU 裝置節點 ==
crw-rw-rw- 1 root root 10, 125 /dev/dxg
== nvidia-smi（GPU 是否真的通到容器）==
+-----------------------------------------------------------------------------------------+
| NVIDIA-SMI 580.159.06             Driver Version: 582.66         CUDA Version: 13.0     |
|   0  NVIDIA GeForce GT 1030         On  |   00000000:01:00.0  On |                  N/A |
| 33%   42C    P0            N/A  /   30W |     917MiB /   2048MiB |     17%      Default |
+-----------------------------------------------------------------------------------------+
```

## 平台行為（架構限制，非未實作）

GPU compute 只在能實際觸及 host GPU 的後端可行：

| 後端 | 行為 |
|---|---|
| **Windows WSL2** | 綁 `/dev/dxg`（Microsoft GPU-PV）+ host `/usr/lib/wsl/lib` **與 `/usr/lib/wsl/drivers`**，並把 `lib` 加進 `LD_LIBRARY_PATH` → CUDA/DirectML/OpenCL/Vulkan 直接可用（image 免自帶驅動 libs）。 |
| **原生 Linux** | 綁 `/dev/nvidia*` + `/dev/dri`。userspace 驅動 libs 需與 host kernel module 相符（image 自帶，或見 roadmap 的自動注入）。 |
| **無 WSL 的 Windows（WHP）/ macOS VM** | 裸 micro-VM 拿不到 Microsoft/Apple 的 GPU 半虛擬化 → `gpu: true` 於**服務啟動時明確報錯**（不靜默降級）。 |

> **為什麼 WSL2 要綁兩個目錄**：`/usr/lib/wsl/lib` 內的 `libcuda`/`libnvidia-ml` 只是 loader shim，
> 執行期會 dlopen 真正的驅動實作於 `/usr/lib/wsl/drivers`（Windows DriverStore，WSL 以 9p 掛入）。
> 只綁 `lib` 而漏綁 `drivers`，容器內會回「couldn't communicate with the NVIDIA driver / Driver Not Loaded」。
> 一般 WSL distro 兩者皆由 WSL 自動掛載；容器換了 mount namespace，故 chefer 兩者都綁。
> `/dev/dri` 只在 host 存在時才綁（部分 WSL kernel 未暴露；CUDA 走 `/dev/dxg` 不需要）。

## 打包成單檔

不需要 Docker、不需要 `docker save`——`chefer build` 會直接從 registry 拉 `nvidia/cuda`：

```bash
cargo run -p chefer-cli -- build examples/gpu-demo/appcipe.yml --out dist
# 產物：dist/gpu-demo/gpu-demo_<target>[.exe]
```

打包 arm64 目標時設 `platform`（appcipe 的 `image:` 全形），chefer 會拉對應架構的映像。

## 執行與驗證

執行單檔後，容器內會印出裝置節點與 `nvidia-smi` 輸出。**看到 host GPU 被列出 = GPU passthrough 通了**；
服務為一次性任務，印完即 exit 0 收尾。

前置條件：host 已裝 GPU 驅動——WSL2 用近期支援 WSL 的 NVIDIA 驅動（`/dev/dxg` 才會出現）；原生 Linux 裝廠商驅動。

## 進一步：真的跑一段 CUDA kernel

要驗證的不是「看得到 GPU」而是「算得動」，把映像換成 `-devel`（含 `nvcc`），在 `cmd` 裡即時編譯並執行一段
kernel（例如 vectorAdd）。已實測 chefer 於 WSL2 + GT 1030 上：`nvcc -arch=sm_61` 編出的 vectorAdd
在 GPU 上算出 `n=1048576 mismatches=0 → PASS`、exit 0。注意 `-devel` 映像較大（數 GB），單檔體積也會相應變大。
