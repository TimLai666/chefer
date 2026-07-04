---
name: write-appcipe
description: >-
  撰寫或修正 Chefer 的 appcipe.yml（把 Docker/OCI 映像打包成免容器引擎單檔的食譜）。
  當使用者要把某個服務／一組容器打包成 Chefer 單檔、新增或修改 appcipe.yml、
  或詢問 appcipe 欄位、image 來源、ports、persist、depends_on、內部網路等時使用。
  涵蓋實測得到的「真陷阱」（format 連字號、官方映像 chown/uid、db 暴露、persist 規則）。
---

# 撰寫 appcipe.yml

appcipe.yml 是 Chefer 的「食譜」：描述一或多個 Docker/OCI 映像 + 執行參數，
`chefer build` 會把它們打包成**單一執行檔**（免裝 Docker/容器引擎，雙擊即跑）。

權威驗證規則在 `crates/appcipe-spec/src/validate.rs`；完整契約在 `docs/DESIGN.md §3/§6`；
有註解的範例見 `examples/appcipe.yml`、可運作的多服務範例見 `examples/demo/`。
**改完一定要 `cargo run -p chefer-cli -- check <appcipe.yml>` 驗證**（會一次列出所有錯誤）。

## 整體流程（先講給使用者聽）

```
docker build / docker pull  →  docker save -o x.tar <image>   # 取得 image tar
寫 appcipe.yml（image 指向那些 tar）
chefer build appcipe.yml --out dist                            # 產出單檔
```
image 來源可為 **tar**（`docker save`/OCI archive）、**registry ref**（釘版 tag 或 `@sha256`，build 時匿名拉取）或 **Dockerfile**（打包機需有 docker/podman/nerdctl）。

## 最小可用範例

```yaml
version: "0.1"          # 必填，目前固定 "0.1"
name: MyApp             # 必填；[A-Za-z][A-Za-z0-9_-]*，≤64；也是輸出檔名與資料夾名
services:
  web:
    image: ./images/web.tar          # 短寫＝tar 路徑（相對 appcipe.yml）
    ports: ["8080:8080"]             # host:guest，從 host 連 127.0.0.1:8080
```

## 頂層欄位

| 欄位 | 必填 | 規則／用途 |
|---|---|---|
| `version` | ✓ | 固定 `"0.1"`。 |
| `name` | ✓ | `[A-Za-z][A-Za-z0-9_-]*`，≤64。輸出檔名 `<name>_<target>[.exe]`、資料目錄名。 |
| `app_version` | | **純顯示/中繼資料**：check/build/inspect 會印、執行時 log 一次。**不影響打包，容器內程式讀不到**。要在程式裡用版本就自己塞進服務的 `env:`。 |
| `old_names` | | 字串清單，每項須是**單一目錄名**（同 name 規則，不可含 `/ \ : ..` 或絕對路徑）。data dir 不存在時，依序找舊名目錄自動改名遷移。 |
| `data_dir` | | 覆蓋持久化資料的父目錄；未設用平台預設（Win `%LOCALAPPDATA%\{name}`、mac `~/Library/Application Support/{name}`、Linux `$XDG_DATA_HOME` 或 `~/.local/share/{name}`）。 |
| `crash` | | 目前僅 `fail_fast`（任一服務非 0 退出→整個 app 以該碼退出）。舊欄位名 `crash_policy` 仍接受。 |
| `network` | | `bridge`（預設）｜`internal`｜`shared`。`bridge`=app 專屬網路、只開宣告的 ports、可出網；`internal`=同 bridge 但無對外網路；`shared`=共用 host 網路（不隔離、舊行為）。見下方「內部網路」。 |
| `console` | | `auto`（預設）｜`shown`｜`hidden`。共用主控台（彙整日誌 + Ctrl+C 的終端視窗）顯示策略。`auto`=有 terminal/both→顯示、只有 gui→隱藏、全無介面→顯示。`hidden` 僅當 app 另有 gui 或 terminal/both 可關閉才允許（全無介面用 hidden 會驗證失敗）。隱藏只在 Windows 雙擊啟動且獨佔該 console 時生效。 |
| `services` | ✓ | 服務字典，見下。 |

## services.<name> 欄位

service 名規則：`[a-z][a-z0-9_]*`，≤32。

- **`image`**（必填）。寫法：
  - 短寫：`image: ./x.tar`（tar 路徑）或 `image: redis:7.2-alpine`（registry ref，自動判別）。
  - 完整（`source` 三選一）：
    ```yaml
    image:
      source: tar                 # tar | image | dockerfile
      file: ./images/x.tar        # tar=tar 路徑；image=registry ref（須釘版、非 latest）；dockerfile=Dockerfile 路徑
      format: docker-archive      # auto | docker-archive | oci-archive（**用連字號**，非底線；僅 source=tar 有意義）
      platform: linux/amd64       # linux/amd64 | linux/arm64（windows/amd64 不支援執行）
    ```
  - `source: dockerfile` 另可加 `context:`（build context 目錄，省略 = Dockerfile 所在目錄）與 `build_args: { K: V }`。**需打包機上有 docker/podman/nerdctl**；不保證可重現（要可重現用 `source: image` 釘 `@sha256` digest）。執行期仍不需 Docker。
- **`cmd`**：字串或陣列，覆蓋映像的 **CMD（不覆蓋 ENTRYPOINT）**。有效命令 = entrypoint + (cmd 或 image CMD)。
- **`workdir`**：容器內工作目錄。
- **`env`**：`key: value`；key 須符合 `[A-Za-z_][A-Za-z0-9_]*`。會覆蓋映像自帶的同名 env。
- **`persist_path`**：要持久化的**容器內絕對路徑**（須以 `/` 開頭）。實體落在
  `{data_dir 或預設}/{name}/data/{service}/`，跨重啟保留。沒設＝退出即清空。
- **`ports`**：`["host:guest[/proto]"]`（proto 預設 tcp）。**同一 app 內 host 埠不可重複**。
  只有列在這裡的埠才會被 chefer 代理到 host。
- **`mounts`**：`["<host路徑>:<容器內絕對路徑>"]`；容器內路徑須以 `/` 開頭；host 路徑在 build 時須存在。
- **`interface_mode`**：`gui | terminal | both | none`（預設 none）。**全 app 最多一個 terminal/both**。
- **`depends_on`**：服務名清單。決定啟動順序；若被依賴的服務有 `healthcheck`，依賴者會**等它 healthy 才啟動**（wait-until-ready），否則等它 spawn 即可。須指向存在的服務、不可循環、不可自指。
- **`healthcheck`**（選填，對齊 Docker HEALTHCHECK）：
  - `test`：命令字串（= `sh -c`）或陣列（`["CMD", ...]` 直接 argv；`["CMD-SHELL", "..."]` 走 `sh -c`）。在容器內執行，exit 0 = 健康。例：`["CMD", "redis-cli", "ping"]`、`["CMD", "pg_isready", "-U", "postgres"]`。
  - `interval`（預設 `2s`）、`timeout`（預設 `5s`）、`start_period`（預設 `0s`）：接受 `<n>ms`/`<n>s`/`<n>m` 或裸整數（秒）。
  - `retries`（預設 `10`）：連續失敗幾次（扣除 start_period 寬限）才算 unhealthy → **fail_fast 拆掉整個 app**。
  - 啟動目前**序列化**：一個服務的 healthcheck 會擋住其後所有服務（含不相依者），通常無感；之後會改成只擋實際 dependents。
- **`gpu`**（選填，預設 `false`）：opt-in GPU passthrough。可寫 **`false`（關）/ `true`（全部 GPU）/ `[0, 2]`（只綁指定 NVIDIA 卡，硬隔離）**。開啟後 guest-agent 把 host GPU 裝置節點（`/dev/nvidia*`、`/dev/dri`、AMD 的 `/dev/kfd`、WSL2 的 `/dev/dxg`）bind 進**這個服務**的容器 → CUDA/ROCm/OpenCL 計算與 NVENC/NVDEC 視訊。驅動 userspace：**原生 NVIDIA 由 chefer 自動注入 host 相符的 `libcuda`/`libnvidia-*`（image 免自帶）**；WSL2 由 `/usr/lib/wsl/lib`+`/usr/lib/wsl/drivers` 提供；**AMD/Intel 由映像自帶（`rocm/*`、`intel/oneapi`）**。**只在原生 Linux 與 Windows WSL2 後端可行**（WHP micro-VM／macOS VM 服務啟動時明確報錯）。`gpu: [i,…]` 卡索引硬隔離**僅原生 NVIDIA 有效**——WSL2/AMD/Intel 請改用 `env` 的 `CUDA_VISIBLE_DEVICES`／`HIP_VISIBLE_DEVICES`／`ZE_AFFINITY_MASK`（軟選擇）。**只給真的需要 GPU 的服務開**。實機驗證：CUDA/PyTorch/NVENC/nvidia-smi（RTX 4070 + WSL2 GT 1030）；OpenGL/Vulkan **算繪**無頭容器不支援。

## 內部網路與「不對外暴露」（重要、實測過）

- **整個 app 跑在自己的 network namespace**（預設 `network: bridge`）。服務間互連用 `127.0.0.1:<port>`
  （例：app 連 db 設 `env: { DB_HOST: "127.0.0.1", DB_PORT: "6379" }`）。
- 只有列在某服務 `ports:` 的埠才會被代理到 host；**未宣告的埠在 `bridge`/`internal` 下真正不對外**
  （服務只在 app netns 的 `lo` 監聽 → host 連不到、WSL2 wslrelay 也看不到）。所以「不列 ports 的 db」
  在預設 `bridge` 下確實內部專用。已於原生 Linux 與實機 WSL2 驗證。
- 想讓服務**能出網**（裝套件、call 外部 API）用預設 `bridge`；**只要服務間互通、不需出網**用 `internal`。
- ⚠️ 只有顯式寫 `network: shared` 才回到舊的「共享 host 網路、未宣告埠也可從 host 連到」行為——
  這時才**不要**向使用者保證 db 不可達。

## 真實映像的陷阱（實測過）

- **官方 redis/postgres/nginx 等**的 entrypoint 常在以 root 執行時 `chown`+`gosu` 切到
  服務專用 uid（如 999）。這類映像在 **WSL2、macOS VM、以及原生 Linux 以 root 執行**時
  可直接使用——這些後端讓服務以真實 root 執行（不開 user namespace），chown/gosu 到任何
  uid 都成功（官方 `redis` 已在 WSL2 實測通過）。原生 Linux 的 **rootless** 路徑（以非
  root 使用者執行單檔）也支援：host 有 `newuidmap`/`newgidmap`（`uidmap` 套件）且使用者在
  `/etc/subuid`/`/etc/subgid` 有委派範圍時（多數發行版預設都有），guest-agent 走範圍映射
  （同 rootless podman），chown/gosu 映像照跑（官方 `redis` 已以非 root 實測）。**只有**
  無委派環境（無 uidmap 或無 subuid 範圍）會退回單一 uid 映射，那些映像可能 `chown`
  失敗而起不來——此時裝 `uidmap` 並補 subuid 範圍，或改用**以容器 root 直接執行、不
  chown** 的映像（例：自建 `alpine + apk add redis`）。參考 `examples/demo/db/Dockerfile`。
- `format` 用**連字號**：`docker-archive` / `oci-archive`（底線形式也接受，但文件統一用連字號）。
- registry ref **必須釘版**（`redis:7.2-alpine` 或 `@sha256:…`）；`latest`/未帶 tag 會被驗證拒絕。私有 registry 認證尚未支援，仍可 `docker pull` + `docker save` 走 tar。

## 多服務範例（app + db，內部連線 + 持久化）

```yaml
version: "0.1"
name: CheferDemo
app_version: "1.0.0"      # 顯示用；要在程式裡用就另外塞 env
services:
  db:
    image: ./images/db.tar          # 建議：自建 alpine+redis（CMD 直接跑、不 chown）
    persist_path: /data             # redis AOF 持久化到此
    interface_mode: none
    # 不列 ports：不主動對外（但見上面 v1 netns 缺口說明）
  app:
    image: ./images/app.tar
    env:
      DB_HOST: "127.0.0.1"          # 內部網路：與 db 共享 localhost
      DB_PORT: "6379"
    ports: ["18080:8080"]           # 唯一對外的埠
    interface_mode: none
    depends_on: [db]                # 先啟動 db（app 仍應自行重試連線）
```

## 收尾檢查清單

1. `cargo run -p chefer-cli -- check <appcipe.yml>` → exit 0。
2. 每個 `image` 指向的 tar 都存在（`docker save` 產生）。
3. `persist_path`、`mounts` 的容器內路徑都以 `/` 開頭。
4. host 埠全 app 唯一；只有真要對外的服務才列 `ports`。
5. 跨服務連線用 `127.0.0.1:<port>` + `env`，不要假設 DNS 服務名。
