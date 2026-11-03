#!/bin/bash
# DJ Mixer（Mac 版）のセットアップ。何度実行しても大丈夫です。
#
#   1. 足りない道具を入れる（Xcode のコマンドラインツール・Homebrew の ffmpeg と uv・Rust）
#   2. 拍の検出（Beat This!）と音源分離（Demucs）の学習済みモデルを取得して ONNX に変換する（初回だけ。約 3GB のダウンロード）
#   3. DJ Mixer.app を作って「アプリケーション」に入れる
#
# 使い方: ターミナルでこのフォルダに移動して  ./setup_mac.sh
set -euo pipefail
cd "$(dirname "$0")"
DATA="$HOME/Library/Application Support/DJ Mixer"
MODELS="$DATA/models"

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

if [[ "$(uname)" != "Darwin" ]]; then
  echo "このスクリプトは Mac で実行してください" >&2
  exit 1
fi

say "Xcode のコマンドラインツール"
if ! xcode-select -p >/dev/null 2>&1; then
  echo "インストールの窓が開きます。終わったら、もう一度 ./setup_mac.sh を実行してください。"
  xcode-select --install || true
  exit 1
fi
echo "OK"

say "Homebrew（ffmpeg・uv）"
if ! command -v brew >/dev/null 2>&1; then
  for b in /opt/homebrew/bin/brew /usr/local/bin/brew; do [[ -x $b ]] && eval "$($b shellenv)"; done
fi
if ! command -v brew >/dev/null 2>&1; then
  echo "Homebrew が見つかりません。https://brew.sh の手順で入れてから、もう一度実行してください。" >&2
  exit 1
fi
command -v ffmpeg >/dev/null 2>&1 || brew install ffmpeg
command -v uv >/dev/null 2>&1 || brew install uv
echo "OK"

say "Rust"
if ! command -v cargo >/dev/null 2>&1; then
  [[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"
fi
if ! command -v cargo >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  source "$HOME/.cargo/env"
fi
cargo --version

say "学習済みモデル（$MODELS）"
if [[ -f "$MODELS/beat_this_final0.onnx" && -f "$MODELS/htdemucs.onnx" && "${1:-}" != "--models" ]]; then
  echo "変換済みです（作り直すときは ./setup_mac.sh --models）"
else
  # 変換に使う Python の部品は、このときだけ uv の一時環境に入れる（アプリの実行には Python を使わない）
  if [[ "$(uname -m)" == "arm64" ]]; then
    TORCH="torch==2.11.0 torchaudio==2.11.0"; DEMUCS="demucs==4.1.0"
  else
    TORCH="torch==2.2.2 torchaudio==2.2.2"; DEMUCS="demucs==4.0.1"     # Intel の Mac
  fi
  WITH=()
  for p in $TORCH beat-this==1.1.0 $DEMUCS onnx==1.23.2 onnxscript onnxruntime soxr; do WITH+=(--with "$p"); done
  uv run --no-project --python 3.11 "${WITH[@]}" python tools/export_onnx.py --out "$MODELS"
fi

say "DJ Mixer.app を作る"
mac/build_app.sh --install

say "できました"
echo "「アプリケーション」の DJ Mixer から起動してください（はじめての起動で、音楽フォルダを選びます）。"
echo "データ（ライブラリ・解析結果・ミックス・ログ）: $DATA"
