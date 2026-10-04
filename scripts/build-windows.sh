#!/usr/bin/env bash
# Cross-build amty for Windows (x86_64-pc-windows-gnu) and deploy the .exe
# to the Windows user's .local\bin (already on PATH).
#
#   ./scripts/build-windows.sh          # build + deploy (fails if amty.exe is running)
#   ./scripts/build-windows.sh --force  # kill running amty.exe on Windows first
#   AMTY_WIN_BIN_DIR=<dir> ./scripts/build-windows.sh   # custom deploy dir
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=x86_64-pc-windows-gnu
SRC="target/$TARGET/release/amty.exe"

if [ -z "${AMTY_WIN_BIN_DIR:-}" ]; then
    win_home="$(cmd.exe /c 'echo %USERPROFILE%' 2>/dev/null | tr -d '\r')"
    [ -n "$win_home" ] || { echo "error: cannot resolve Windows %USERPROFILE% via cmd.exe" >&2; exit 1; }
    AMTY_WIN_BIN_DIR="$(wslpath "$win_home")/.local/bin"
fi
DEST="$AMTY_WIN_BIN_DIR/amty.exe"

command -v x86_64-w64-mingw32-gcc >/dev/null || {
    echo "error: x86_64-w64-mingw32-gcc not found (apt install gcc-mingw-w64 / brew install mingw-w64)" >&2
    exit 1
}
rustup target list --installed | grep -qx "$TARGET" || rustup target add "$TARGET"

win_amty_running() {
    powershell.exe -NoProfile -Command 'if (Get-Process amty -ErrorAction SilentlyContinue) { exit 0 } else { exit 1 }' 2>/dev/null
}

if [ "${1:-}" = "--force" ]; then
    powershell.exe -NoProfile -Command 'Stop-Process -Name amty -Force -ErrorAction SilentlyContinue' || true
elif win_amty_running; then
    echo "error: amty.exe is running on Windows — quit it, or rerun with --force" >&2
    exit 1
fi

echo "==> cargo build --release --target $TARGET"
cargo build --release --target "$TARGET"

mkdir -p "$AMTY_WIN_BIN_DIR"
cp -f "$SRC" "$DEST.new" && mv -f "$DEST.new" "$DEST"

"$DEST" --version
echo "deployed → $(wslpath -w "$DEST")"
