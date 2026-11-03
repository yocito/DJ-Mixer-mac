#!/bin/bash
# DJ Mixer.app を作る（Mac で実行する）。
#   mac/build_app.sh            # dist/DJ Mixer.app を作る
#   mac/build_app.sh --install  # 作ったあと /Applications に入れる（書けなければ ~/Applications）
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
APP="$ROOT/dist/DJ Mixer.app"
FEATURES="${DJMIX_FEATURES:-window,ort-download,coreml}"

if [[ "$(uname)" != "Darwin" ]]; then
  echo "このスクリプトは Mac で実行してください" >&2
  exit 1
fi

# C / C++ の部品（SQLite・Signalsmith Stretch・libvorbis）は Apple のコンパイラと macOS の SDK でビルドする。
# PATH に Homebrew の gcc / llvm があったり SDKROOT が無かったりすると、<complex> などの標準ヘッダーが見つからないことがある
SDK="$(xcrun --sdk macosx --show-sdk-path 2>/dev/null || true)"
if [[ -z "$SDK" || ! -d "$SDK" ]]; then
  echo "macOS の SDK が見つかりません。Xcode のコマンドラインツールを入れ直してください:" >&2
  echo "  sudo rm -rf /Library/Developer/CommandLineTools && xcode-select --install" >&2
  exit 1
fi
export SDKROOT="$SDK"
export CC="$(xcrun --sdk macosx -f clang)"
export CXX="$(xcrun --sdk macosx -f clang++)"
if ! printf '#include <complex>\nint main(){std::complex<float> c(1,2);return (int)c.real()-1;}\n' \
     | "$CXX" -std=c++14 -x c++ - -o /dev/null 2>/dev/null; then
  echo "C++ の標準ヘッダー（<complex>）が使えません。Xcode のコマンドラインツールが壊れているか古いので、入れ直してください:" >&2
  echo "  sudo rm -rf /Library/Developer/CommandLineTools && xcode-select --install" >&2
  echo "（入れ直したあと、もう一度 ./setup_mac.sh）" >&2
  exit 1
fi

echo "== Rust のビルド（release, features: ${FEATURES}）"
(cd native && cargo build --release --features "$FEATURES")
VERSION="$(grep -m1 '^version' native/Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"

echo "== アプリの組み立て: $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/config"
cp native/target/release/djmix "$APP/Contents/MacOS/djmix"
sed "s/__VERSION__/$VERSION/g" mac/Info.plist > "$APP/Contents/Info.plist"
# 画面・アイコン・既定の割り当て（読むだけ）。データ（DB・解析結果・割り当て・モデル）は ~/Library/Application Support/DJ Mixer
rsync -a --delete --exclude tests frontend/ "$APP/Contents/Resources/frontend/"
mkdir -p "$APP/Contents/Resources/assets/icons/a"
cp assets/icons/a/*.png "$APP/Contents/Resources/assets/icons/a/"
rsync -a config/mappings "$APP/Contents/Resources/config/"
# ONNX Runtime を動的に読む版でビルドしたときは、そのライブラリも入れる
if [[ -n "${ORT_DYLIB_PATH:-}" && -f "${ORT_DYLIB_PATH}" ]]; then
  mkdir -p "$APP/Contents/Frameworks"
  cp "$ORT_DYLIB_PATH" "$APP/Contents/Frameworks/"
fi

echo "== アイコン"
ICONSET="$(mktemp -d)/AppIcon.iconset"
mkdir -p "$ICONSET"
SRC="assets/icons/a/256.png"
for s in 16 32 128 256 512; do
  sips -z $s $s "$SRC" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  d=$((s * 2))
  sips -z $d $d "$SRC" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
# 小さいサイズは元の絵（細部を描き分けてある）を使う
cp assets/icons/a/16.png "$ICONSET/icon_16x16.png"
cp assets/icons/a/32.png "$ICONSET/icon_16x16@2x.png"
cp assets/icons/a/32.png "$ICONSET/icon_32x32.png"
cp assets/icons/a/64.png "$ICONSET/icon_32x32@2x.png"
cp assets/icons/a/128.png "$ICONSET/icon_128x128.png"
cp assets/icons/a/256.png "$ICONSET/icon_128x128@2x.png"
cp assets/icons/a/256.png "$ICONSET/icon_256x256.png"
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"

echo "== 署名（このマシン用のアドホック署名）"
codesign --force --deep --sign - "$APP"

echo "できました: $APP"
if [[ "${1:-}" == "--install" ]]; then
  DEST="/Applications"
  [[ -w "$DEST" ]] || DEST="$HOME/Applications"
  mkdir -p "$DEST"
  rm -rf "$DEST/DJ Mixer.app"
  cp -R "$APP" "$DEST/"
  echo "入れました: $DEST/DJ Mixer.app"
fi
