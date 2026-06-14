"""Chefer demo：PySide6 GUI 連 redis。

視窗顯示存在 redis 的點擊計數，按鈕 INCR、每秒自動刷新。
- 視窗出現 = GUI 鏈路通（容器內 Qt/xcb 連到 host 的 X server；Windows 經 WSLg）。
- 計數能讀寫 = 內部網路連到 db（redis）正常。
redis 連線參數由 chefer 注入的環境變數提供（REDIS_HOST/REDIS_PORT）。
"""

import os
import sys

import redis
from PySide6.QtCore import Qt, QTimer
from PySide6.QtWidgets import (
    QApplication,
    QLabel,
    QPushButton,
    QVBoxLayout,
    QWidget,
)

HOST = os.environ.get("REDIS_HOST", "127.0.0.1")
PORT = int(os.environ.get("REDIS_PORT", "6379"))
R = redis.Redis(host=HOST, port=PORT, socket_connect_timeout=2, decode_responses=True)


class Demo(QWidget):
    def __init__(self) -> None:
        super().__init__()
        self.setWindowTitle("Chefer × PySide6 × Redis")
        self.resize(380, 220)

        layout = QVBoxLayout(self)
        self.title = QLabel("Chefer GUI demo — clicks stored in redis")
        self.title.setAlignment(Qt.AlignCenter)
        self.count = QLabel("—")
        self.count.setAlignment(Qt.AlignCenter)
        self.count.setStyleSheet("font-size: 56px; font-weight: bold; color: #2d7;")
        self.status = QLabel("connecting…")
        self.status.setAlignment(Qt.AlignCenter)
        button = QPushButton("Click me  (INCR in redis)")
        button.clicked.connect(self.incr)

        for w in (self.title, self.count, button, self.status):
            layout.addWidget(w)

        self.refresh()
        timer = QTimer(self)
        timer.timeout.connect(self.refresh)
        timer.start(1000)

    def refresh(self) -> None:
        try:
            value = R.get("clicks") or "0"
            self.count.setText(str(value))
            self.status.setText(f"redis OK @ {HOST}:{PORT}")
        except Exception as exc:  # noqa: BLE001 - 顯示給使用者看
            self.status.setText(f"redis error: {exc}")

    def incr(self) -> None:
        try:
            R.incr("clicks")
            self.refresh()
        except Exception as exc:  # noqa: BLE001
            self.status.setText(f"redis error: {exc}")


def main() -> int:
    app = QApplication(sys.argv)
    widget = Demo()
    widget.show()
    return app.exec()


if __name__ == "__main__":
    sys.exit(main())
