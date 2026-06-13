# Demo「資料庫」：以 alpine + redis 套件自建，直接用 redis-server 當啟動命令。
#
# 為何不直接用官方 redis 映像：官方映像的 entrypoint 在以 root 執行時會
# `chown -R redis /data` 並 `gosu redis` 切到服務專用 uid（999）。Chefer 在
# 「單一 uid 映射」的環境（如 WSL2 巢狀 userns）下，uid 999 未被映射 → chown
# 回 EINVAL、服務起不來。自建映像沒有那段 entrypoint，redis 直接以容器 root
# 執行、無需 chown，故在所有平台都能跑。
#
# （在以 root 執行且可做「範圍 uid 映射」的環境——如真實 Linux / macOS vz 的 VM——
#  guest-agent 會映射完整 uid/gid 範圍，屆時官方 redis/postgres 等映像也能用。）
FROM alpine:3.20
RUN apk add --no-cache redis
# 只綁 loopback：與 app 共享網路 namespace，app 以 127.0.0.1 連入即可（內部網路），
# 同時即使在容器網路內也不對其他介面開放——縱深防禦。
# AOF 持久化到 /data（由 appcipe 的 persist_path 綁回 host）。
CMD ["redis-server", "--bind", "127.0.0.1", "--appendonly", "yes", "--dir", "/data"]
