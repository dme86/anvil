#!/usr/bin/env python3
"""Exercise the real CLI/IPC while independent Wayland clients connect and crash."""
import json
import os
import pathlib
import selectors
import socket
import subprocess
import sys
import time


def stats():
    result = subprocess.run(["target/debug/anvilctl", "debug", "stats"],
                            capture_output=True, text=True, check=True, timeout=3)
    response = json.loads(result.stdout)
    assert response["status"] == "stats", response
    return response["stats"]


def settle(clients, windows):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        value = stats()
        if (value["connected_clients"], value["managed_windows"]) == (clients, windows):
            return value
        time.sleep(0.05)
    raise AssertionError(f"client cleanup failed: {value}")


children = []
try:
    baseline = settle(0, 0)
    assert baseline["outputs"] == 1 and not baseline["session_locked"], baseline
    assert baseline["rss_bytes"] > 0 and baseline["open_file_descriptors"] > 0, baseline
    for count in (1, 2):
        child = subprocess.Popen([sys.argv[1], sys.argv[2], "hold"],
                                 stdout=subprocess.PIPE, text=True)
        children.append(child)
        with selectors.DefaultSelector() as selector:
            selector.register(child.stdout, selectors.EVENT_READ)
            assert selector.select(5), "client did not become ready"
            assert child.stdout.readline().strip() == "READY"
        settle(count, count)

    listing = subprocess.run(["target/debug/anvilctl", "window", "list"],
                             capture_output=True, text=True, check=True, timeout=3)
    windows = json.loads(listing.stdout)["windows"]
    assert len(windows) == 2 and sum(w["focused"] for w in windows) == 1, windows
    # Abrupt death must drop client objects and transfer focus to the survivor.
    children[-1].kill()
    children[-1].wait(timeout=3)
    settle(1, 1)
    listing = subprocess.run(["target/debug/anvilctl", "window", "list"],
                             capture_output=True, text=True, check=True, timeout=3)
    assert json.loads(listing.stdout)["windows"][0]["focused"]
    children[0].terminate()
    children[0].wait(timeout=3)
    final = settle(0, 0)
    assert final["uptime_seconds"] >= baseline["uptime_seconds"]
    assert final["requested_repaints"] > baseline["requested_repaints"]
    assert final["render_attempts"] >= final["rendered_frames"] > 0
    assert final["render_failures"] == 0
    assert final["average_render_time_ms"] >= 0
    assert final["dmabuf_imports"] >= 0 and final["dmabuf_import_failures"] >= 0
    # Adding a new command must retain the version check on the shared wire protocol.
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(3)
        stream.connect(str(pathlib.Path(os.environ["XDG_RUNTIME_DIR"]) / "anvil.sock"))
        stream.sendall(b'{"command":"debug_stats","version":999}\n')
        stream.shutdown(socket.SHUT_WR)
        response = json.loads(stream.makefile().read())
        assert response["status"] == "error" and "version" in response["message"]
    print("PASS: diagnostics, real client counts, window lifecycle, focus and crash cleanup")
finally:
    for child in children:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=3)
