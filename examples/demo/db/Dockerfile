# Demo「資料庫」：以 alpine + redis 套件自建，直接用 redis-server 當啟動命令。
#
# 為何自建而非直接用官方 redis 映像（取捨說明）：官方映像的 entrypoint 以 root
# 執行時會 `chown -R redis /data` 並 `gosu redis` 切到服務專用 uid（999）。
#  - 在 **WSL2 / macOS VM / 原生 Linux 以 root 執行** 時，Chefer 讓服務以真實 root
#    執行（不開 user namespace），chown/gosu 到 999 可直接成功 → 官方 redis 可用
#    （已在 WSL2 實測）。
#  - 但在 **原生 Linux 的 rootless 路徑**（以非 root 使用者執行單檔）下，核心只允許
#    自映射單一 uid，999 未被映射 → chown 回 EINVAL、服務起不來。
# 自建映像沒有那段 entrypoint，redis 直接以容器 root 執行、無需 chown，故在
# **所有平台（含 rootless）** 都能跑——這就是 demo 選它的原因：最大可攜性。
FROM alpine:3.20
RUN apk add --no-cache redis
# 只綁 loopback：與 app 共享網路 namespace，app 以 127.0.0.1 連入即可（內部網路），
# 同時即使在容器網路內也不對其他介面開放——縱深防禦。
# AOF 持久化到 /data（由 appcipe 的 persist_path 綁回 host）。
CMD ["redis-server", "--bind", "127.0.0.1", "--appendonly", "yes", "--dir", "/data"]
