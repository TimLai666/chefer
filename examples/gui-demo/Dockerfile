# Chefer GUI demo：最小 X11 圖形程式（xeyes）。
#
# 用來驗證 chefer 的 GUI 支援：Linux 直通 X11/Wayland socket、Windows 經 WSLg。
# xeyes 來自 x11-apps，是最經典、一眼可辨的 X11 測試程式（一對會追游標的眼睛）。
# 視窗一旦出現，就代表容器內的 X client 成功連到 host 的 X server——GUI 鏈路通了。
FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends x11-apps \
    && rm -rf /var/lib/apt/lists/*
# xeyes 預設連 $DISPLAY（chefer 會把 host 的 X11 socket 掛進 /tmp/.X11-unix 並設 DISPLAY）
CMD ["xeyes"]
