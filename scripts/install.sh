#!/bin/sh
# Chefer 一行安裝腳本（Linux / macOS）。
#
#   curl -fsSL https://raw.githubusercontent.com/TimLai666/chefer/main/scripts/install.sh | sh
#
# 會自動偵測 OS/arch、從 GitHub Releases 抓對應的平台包、驗 sha256、解壓到安裝目錄、
# 並把它加進常見 shell 的 PATH。安裝後即可 `chefer version`，之後用 `chefer upgrade` 更新。
#
# 可用環境變數：
#   CHEFER_VERSION       指定版本 tag（預設：最新 release）
#   CHEFER_INSTALL_DIR   安裝目錄（預設：$HOME/.chefer）
#
# 終端使用者「執行」用 chefer 打包好的單檔不需要安裝任何東西；本腳本是給「要用
# chefer 打包 app」的開發者裝 CLI + kit 用的。
set -eu

REPO="TimLai666/chefer"
INSTALL_DIR="${CHEFER_INSTALL_DIR:-$HOME/.chefer}"

err() { printf 'chefer-install: 錯誤：%s\n' "$1" >&2; exit 1; }

# 下載工具：curl 優先，否則 wget
if command -v curl >/dev/null 2>&1; then
  dl_stdout() { curl -fsSL "$1"; }
  dl_file() { curl -fsSL -o "$1" "$2"; }
elif command -v wget >/dev/null 2>&1; then
  dl_stdout() { wget -qO- "$1"; }
  dl_file() { wget -qO "$1" "$2"; }
else
  err "需要 curl 或 wget，但兩者都找不到"
fi
command -v tar >/dev/null 2>&1 || err "需要 tar，但找不到"

# OS / arch → Rust target triple（對齊 release 的資產命名）
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux) plat="unknown-linux-musl" ;;
  Darwin) plat="apple-darwin" ;;
  *) err "不支援的作業系統：$os（本腳本支援 Linux / macOS；Windows 請用 install.ps1）" ;;
esac
case "$arch" in
  x86_64 | amd64) cpu="x86_64" ;;
  aarch64 | arm64) cpu="aarch64" ;;
  *) err "不支援的架構：$arch" ;;
esac
target="${cpu}-${plat}"

# 版本：未指定就問 GitHub 最新 release
ver="${CHEFER_VERSION:-}"
if [ -z "$ver" ]; then
  ver="$(dl_stdout "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
    | grep '"tag_name"' | head -n1 \
    | sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')" || true
  [ -n "$ver" ] || err "抓不到最新 release（可能尚未發布任何 release）；可用 CHEFER_VERSION 指定版本後重試"
fi

asset="chefer_${ver}_${target}.tar.gz"
base="https://github.com/$REPO/releases/download/$ver"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

printf 'chefer-install: 下載 %s（%s）…\n' "$asset" "$ver"
dl_file "$tmp/$asset" "$base/$asset" || err "下載失敗：$base/$asset"

# sha256 校驗（有 .sha256 才驗；缺工具則略過但提示）
if dl_file "$tmp/$asset.sha256" "$base/$asset.sha256" 2>/dev/null; then
  want="$(awk '{print $1}' "$tmp/$asset.sha256")"
  if command -v sha256sum >/dev/null 2>&1; then
    got="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    got="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"
  else
    got=""
    printf 'chefer-install: 找不到 sha256sum/shasum，略過校驗\n' >&2
  fi
  if [ -n "$got" ] && [ "$got" != "$want" ]; then
    err "sha256 不符（下載可能損毀）：want=$want got=$got"
  fi
fi

# 解壓：壓縮包內為 chefer_<ver>_<target>/{chefer,kit/}
tar -xzf "$tmp/$asset" -C "$tmp"
src="$tmp/chefer_${ver}_${target}"
[ -f "$src/chefer" ] || err "壓縮包結構非預期（缺 $src/chefer）"

# 重裝／修復情境：本腳本完全不依賴既有的 chefer（純 curl/tar 重新下載安裝），
# 故即使某一版把 `chefer upgrade` 弄壞，重跑這行安裝指令仍能直接覆蓋救回。
if [ -x "$INSTALL_DIR/chefer" ]; then
  oldver="$("$INSTALL_DIR/chefer" version 2>/dev/null | head -n1 || true)"
  printf 'chefer-install: 偵測到既有安裝（%s），將直接覆蓋重裝\n' "${oldver:-版本未知}"
fi

# 安裝：只替換 chefer 與 kit（不動安裝目錄內其他東西），kit 必須與 chefer 同層
mkdir -p "$INSTALL_DIR"
rm -rf "$INSTALL_DIR/kit"
cp -R "$src/kit" "$INSTALL_DIR/kit"
cp -f "$src/chefer" "$INSTALL_DIR/chefer"
chmod +x "$INSTALL_DIR/chefer" 2>/dev/null || true
printf 'chefer-install: 已安裝到 %s\n' "$INSTALL_DIR"

# 安裝後煙霧測試：確認剛裝好的 binary 真能跑（重裝／修復最重要的確認）
if "$INSTALL_DIR/chefer" version >/dev/null 2>&1; then
  printf 'chefer-install: 驗證 OK：%s\n' "$("$INSTALL_DIR/chefer" version 2>/dev/null | head -n1)"
else
  printf 'chefer-install: 警告：剛安裝的 chefer 無法執行 `version`（macOS 可能是 Gatekeeper 攔未簽章檔，於系統設定→隱私權與安全性放行；其餘情況請回報）\n' >&2
fi

# PATH：把 INSTALL_DIR 加進存在的 shell rc（冪等；kit 與 chefer 同層，故直接把該目錄上 PATH）
line="export PATH=\"$INSTALL_DIR:\$PATH\""
touched=""
for rc in "$HOME/.profile" "$HOME/.bashrc" "$HOME/.zshrc"; do
  [ -e "$rc" ] || continue
  if ! grep -qF "$INSTALL_DIR" "$rc" 2>/dev/null; then
    printf '\n# chefer\n%s\n' "$line" >> "$rc"
    touched="$touched $rc"
  fi
done

# 若 PATH 上已有「別的位置」的 chefer，會蓋過本次安裝，提醒使用者（重裝時常見：
# 之前用原始碼/手動裝在別處）。
existing="$(command -v chefer 2>/dev/null || true)"
if [ -n "$existing" ] && [ "$existing" != "$INSTALL_DIR/chefer" ]; then
  printf 'chefer-install: 注意：PATH 上另有 chefer（%s）會優先於本次安裝（%s/chefer）；\n  要改用本次安裝請調整 PATH 順序或移除舊的那個。\n' "$existing" "$INSTALL_DIR" >&2
fi

case ":${PATH:-}:" in
  *":$INSTALL_DIR:"*)
    printf 'chefer-install: 完成。執行 `chefer version` 確認；之後可用 `chefer upgrade` 更新。\n'
    ;;
  *)
    [ -n "$touched" ] && printf 'chefer-install: 已把 %s 加入 PATH（%s）。\n' "$INSTALL_DIR" "$touched"
    printf 'chefer-install: 請開新的終端機，或先執行：\n  export PATH="%s:$PATH"\n然後 `chefer version` 確認。\n' "$INSTALL_DIR"
    ;;
esac
