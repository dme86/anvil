#!/usr/bin/env bash
# Run against a built binary without physical DRM hardware; all children are cleaned up on exit.
set -euo pipefail
cd "$(dirname "$0")/../.."
profile="${1:-default}"
build="${ANVIL_PROTOCOL_BUILD_DIR:-target/protocol-tests}"
export XDG_RUNTIME_DIR
XDG_RUNTIME_DIR="$(mktemp -d)"
chmod 700 "$XDG_RUNTIME_DIR"
export LIBGL_ALWAYS_SOFTWARE=1
unset WAYLAND_DISPLAY
xvfb_pid='' compositor_pid='' waybar_pid=''
cleanup() {
    for pid in "$waybar_pid" "$compositor_pid" "$xvfb_pid"; do
        if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
    done
    rm -rf "$XDG_RUNTIME_DIR"
}
trap cleanup EXIT
# -displayfd avoids collisions with an existing display and tells us when Xvfb is ready.
Xvfb -displayfd 3 -screen 0 1280x720x24 -ac -nolisten tcp 3>"$build/display" >"$build/xvfb.log" 2>&1 &
xvfb_pid=$!
for _ in $(seq 1 100); do
    [[ -s "$build/display" ]] && break
    kill -0 "$xvfb_pid" 2>/dev/null || { cat "$build/xvfb.log"; exit 1; }
    sleep 0.1
done
[[ -s "$build/display" ]] || { cat "$build/xvfb.log"; exit 1; }
export DISPLAY=":$(cat "$build/display")"
# No external startup commands, XWayland process or shell status commands are needed for tests.
printf '[general]\nstartup = []\n[compat]\nxwayland = false\n[bar]\nstatus_commands = []\n' > "$build/config.toml"
target/debug/anvil --nested --config "$build/config.toml" >"$build/anvil.log" 2>&1 &
compositor_pid=$!
for _ in $(seq 1 100); do
    [[ -S "$XDG_RUNTIME_DIR/wayland-0" ]] && break
    kill -0 "$compositor_pid" 2>/dev/null || { cat "$build/anvil.log"; exit 1; }
    sleep 0.1
done
[[ -S "$XDG_RUNTIME_DIR/wayland-0" ]] || { cat "$build/anvil.log"; exit 1; }
export WAYLAND_DISPLAY=wayland-0
mode=default
[[ "$profile" == *layer* || "$profile" == all-features ]] && mode=layer-shell
timeout 45 "$build/client" "$mode"
# Also exercise the existing wlr-screencopy consumer, including cropped capture.
timeout 15 grim -o winit "$build/desktop.png"
timeout 15 grim -g '0,0 100x100' "$build/region.png"
if [[ "$mode" == layer-shell ]]; then
    printf '{"layer":"top","position":"bottom","height":32,"modules-right":["clock"]}\n' > "$build/waybar.json"
    printf '* { font-family: sans-serif; font-size: 14px; } window#waybar { background: #0000ff; color: #ffffff; }\n' > "$build/waybar.css"
    waybar --config "$build/waybar.json" --style "$build/waybar.css" >"$build/waybar.log" 2>&1 &
    waybar_pid=$!
    for _ in $(seq 1 50); do
        rg -q 'Bar configured' "$build/waybar.log" && break
        kill -0 "$waybar_pid" 2>/dev/null || { cat "$build/waybar.log"; exit 1; }
        sleep 0.1
    done
    rg -q 'Bar configured' "$build/waybar.log"
    timeout 15 grim -o winit "$build/waybar.png"
fi
python3 - "$build" <<'PY'
import pathlib, struct, sys
folder=pathlib.Path(sys.argv[1])
for name in ['desktop.png', 'region.png']:
    data=(folder/name).read_bytes()
    assert data[:8] == b'\x89PNG\r\n\x1a\n'
    width,height=struct.unpack('>II',data[16:24])
    assert width > 0 and height > 0
    if name == 'region.png': assert (width,height) == (100,100)
PY
kill -0 "$compositor_pid"
if rg -i 'panicked|protocol error|capture failed' "$build/anvil.log"; then exit 1; fi
printf 'PASS: %s nested compositor, modern capture, grim and optional Waybar\n' "$profile"
