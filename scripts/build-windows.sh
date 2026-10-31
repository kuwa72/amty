#!/usr/bin/env bash
# Cross-build amty for Windows (x86_64-pc-windows-gnu) and deploy the .exe
# to the Windows user's .local\bin (already on PATH).
#
#   ./scripts/build-windows.sh          # build + deploy (fails if amty.exe is running)
#   ./scripts/build-windows.sh --force  # kill running amty.exe on Windows first
#   ./scripts/build-windows.sh --watch  # rebuild + deploy on every source change,
#                                       # killing and relaunching amty.exe if it ran
#   AMTY_WIN_BIN_DIR=<dir> ./scripts/build-windows.sh   # custom deploy dir
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=x86_64-pc-windows-gnu
SRC="target/$TARGET/release/amty.exe"
MODE="${1:-}"

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
command -v x86_64-w64-mingw32-windres >/dev/null || {
    echo "error: x86_64-w64-mingw32-windres not found (apt install binutils-mingw-w64 / brew install mingw-w64) — needed to embed the exe icon" >&2
    exit 1
}
rustup target list --installed | grep -qx "$TARGET" || rustup target add "$TARGET"

win_amty_running() {
    powershell.exe -NoProfile -Command 'if (Get-Process amty -ErrorAction SilentlyContinue) { exit 0 } else { exit 1 }' 2>/dev/null
}

kill_win_amty() {
    powershell.exe -NoProfile -Command 'Stop-Process -Name amty -Force -ErrorAction SilentlyContinue' || true
}

# Build + deploy once. Returns non-zero on a failed build/copy so the watch loop
# keeps going.
deploy() {
    local was_running=0
    if win_amty_running; then was_running=1; fi

    if [ "$MODE" = "--force" ] || [ "$MODE" = "--watch" ]; then
        if [ "$was_running" = 1 ]; then kill_win_amty; fi
    elif [ "$was_running" = 1 ]; then
        echo "error: amty.exe is running on Windows — quit it, or rerun with --force" >&2
        return 1
    fi

    echo "==> cargo build --release --target $TARGET"
    cargo build --release --target "$TARGET" || return 1

    mkdir -p "$AMTY_WIN_BIN_DIR"
    cp -f "$SRC" "$DEST.new" && mv -f "$DEST.new" "$DEST" || return 1

    "$DEST" --version
    echo "deployed → $(wslpath -w "$DEST")"

    if [ "$MODE" = "--watch" ] && [ "$was_running" = 1 ]; then
        powershell.exe -NoProfile -Command "Start-Process -FilePath '$(wslpath -w "$DEST")'" || true
    fi
}

if [ "$MODE" != "--watch" ]; then
    deploy
    exit
fi

# short poll instead of inotifywait (not installed here); 2 s is well under a build
stamp=""
while true; do
    now="$(find src build.rs Cargo.toml assets -type f -printf '%T@\n' 2>/dev/null | sort -n | tail -1)"
    if [ "$now" != "$stamp" ]; then
        stamp="$now"
        deploy || echo "error: build/deploy failed — still watching for changes" >&2
    fi
    sleep 2
done
