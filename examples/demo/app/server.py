"""Chefer demo「應用」：連到同 app 內的 redis（內部網路 127.0.0.1:6379），
每次 GET / 對計數器 +1 並回傳。計數存在 redis、由 redis 持久化到 /data，
因此整個 app 重啟後計數仍會延續。

depends_on 只保證「db 先啟動」，不保證就緒，故啟動時對 db 做重試連線。
"""

import os
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import redis


def connect_with_retry():
    host = os.environ.get("REDIS_HOST", "127.0.0.1")
    port = int(os.environ.get("REDIS_PORT", "6379"))
    last = None
    for i in range(60):
        try:
            c = redis.Redis(host=host, port=port, socket_connect_timeout=1)
            c.ping()
            print(f"[app] 已連上 db（{host}:{port}）", flush=True)
            return c
        except Exception as e:  # noqa: BLE001
            last = e
            print(f"[app] 等待 db 就緒（第 {i + 1} 次）：{e}", flush=True)
            time.sleep(1)
    raise SystemExit(f"[app] 無法連上 db：{last}")


DB = connect_with_retry()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def _text(self, status: int, text: str):
        body = text.encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/health":
            self._text(200, "ok\n")
            return
        n = DB.incr("visits")
        self._text(200, f"Hello from Chefer demo! 這是第 {n} 次造訪（計數存於 redis 並持久化）\n")


def main():
    port = int(os.environ.get("PORT", "8080"))
    print(f"[app] 在 0.0.0.0:{port} 提供服務", flush=True)
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()


if __name__ == "__main__":
    main()
