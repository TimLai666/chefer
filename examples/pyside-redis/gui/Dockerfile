# Chefer GUI demo（PySide6 + redis client）。
# Qt6 需要一組 X/GL/xcb 系統函式庫才能在 host 的 X server 上顯示（Windows 經 WSLg）。
FROM python:3.12-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        libglib2.0-0 libgl1 libegl1 libdbus-1-3 libfontconfig1 libfreetype6 \
        libx11-6 libx11-xcb1 libxext6 libxrender1 libxi6 \
        libxkbcommon0 libxkbcommon-x11-0 \
        libxcb1 libxcb-cursor0 libxcb-glx0 libxcb-icccm4 libxcb-image0 libxcb-keysyms1 \
        libxcb-randr0 libxcb-render0 libxcb-render-util0 libxcb-shape0 libxcb-shm0 \
        libxcb-sync1 libxcb-util1 libxcb-xfixes0 libxcb-xinerama0 libxcb-xkb1 \
    && rm -rf /var/lib/apt/lists/*
RUN pip install --no-cache-dir PySide6 redis
COPY app.py /app/app.py
# 用 xcb（X11）平台外掛：WSLg 提供 X server（DISPLAY=:0），chefer 會掛 /tmp/.X11-unix。
ENV QT_QPA_PLATFORM=xcb
CMD ["python", "/app/app.py"]
