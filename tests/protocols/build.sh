#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
build="${ANVIL_PROTOCOL_BUILD_DIR:-target/protocol-tests}"
mkdir -p "$build"
cargo metadata --locked --format-version 1 > "$build/metadata.json"
python3 - "$build" <<'PY'
import json, pathlib, subprocess, sys
out = pathlib.Path(sys.argv[1])
packages = {p['name']: pathlib.Path(p['manifest_path']).parent for p in json.loads((out/'metadata.json').read_text())['packages']}
wp = packages['wayland-protocols'] / 'protocols'
wlr = packages['wayland-protocols-wlr'] / 'wlr-protocols'
protocols = {
    'xdg-shell': wp/'stable/xdg-shell/xdg-shell.xml',
    'layer-shell': wlr/'unstable/wlr-layer-shell-unstable-v1.xml',
    'image-source': wp/'staging/ext-image-capture-source/ext-image-capture-source-v1.xml',
    'image-copy': wp/'staging/ext-image-copy-capture/ext-image-copy-capture-v1.xml',
    'session-lock': wp/'staging/ext-session-lock/ext-session-lock-v1.xml',
    # Image-source XML references the foreign-toplevel interface even when unused.
    'foreign-toplevel': wp/'staging/ext-foreign-toplevel-list/ext-foreign-toplevel-list-v1.xml',
}
for name, xml in protocols.items():
    subprocess.run(['wayland-scanner', 'client-header', str(xml), str(out/(name+'-client.h'))], check=True)
    subprocess.run(['wayland-scanner', 'private-code', str(xml), str(out/(name+'-protocol.c'))], check=True)
PY
cc -std=c11 -Wall -Wextra -Werror -Wno-unused-parameter -I"$build" tests/protocols/client.c "$build"/*-protocol.c $(pkg-config --cflags --libs wayland-client) -o "$build/client"
