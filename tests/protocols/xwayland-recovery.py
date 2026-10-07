#!/usr/bin/env python3
"""Kill the real compatibility server while X11/native windows coexist; native IPC must recover."""
import json
import os
import pathlib
import re
import selectors
import signal
import subprocess
import sys
import time

pid = int(os.environ['ANVIL_COMPOSITOR_PID'])
children = []


def snapshot(command):
    return json.loads(subprocess.check_output(['target/debug/anvilctl', *command], text=True, timeout=3))


def wait_for(predicate, description):
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError(description)


try:
    log = re.sub(r'\x1b\[[0-9;]*m', '', pathlib.Path(sys.argv[3]).read_text())
    display = re.search(r'display=(:\d+)', log)
    assert display, log
    server_pids = [int(value) for value in pathlib.Path(f'/proc/{pid}/task/{pid}/children').read_text().split()
                   if pathlib.Path(f'/proc/{value}/comm').read_text().strip().lower() == 'xwayland']
    assert len(server_pids) == 1, server_pids
    x11 = subprocess.Popen(['xmessage', '-display', display[1], '-buttons', 'OK', 'Anvil X11 crash test'])
    children.append(x11)
    wait_for(lambda: len(snapshot(['window', 'list'])['windows']) == 1, 'X11 window did not map')
    native = subprocess.Popen([sys.argv[1], sys.argv[2], 'hold'], stdout=subprocess.PIPE, bufsize=0)
    children.append(native)
    with selectors.DefaultSelector() as selector:
        selector.register(native.stdout, selectors.EVENT_READ)
        assert selector.select(5), 'native client failed to start'
        assert native.stdout.readline().strip() == b'READY'
    wait_for(lambda: len(snapshot(['window', 'list'])['windows']) == 2, 'native/X11 coexistence failed')
    host = subprocess.check_output(['xdotool', 'search', '--name', '^Smithay$'], text=True).strip()
    subprocess.run(['xdotool', 'windowfocus', '--sync', host], check=True, timeout=3)
    subprocess.run(['xdotool', 'key', '--clearmodifiers', 'super+j'], check=True, timeout=3)
    wait_for(lambda: snapshot(['window', 'list'])['windows'][0]['focused'], 'could not focus X11 client before crash')
    os.kill(server_pids[0], signal.SIGKILL)
    x11.wait(timeout=5)
    wait_for(lambda: len(snapshot(['window', 'list'])['windows']) == 1, 'dead X11 windows retained')
    wait_for(lambda: snapshot(['debug', 'stats'])['stats']['connected_clients'] == 1, 'dead XWayland client retained')
    assert snapshot(['window', 'list'])['windows'][0]['focused'], 'native focus did not survive server death'
    native.kill(); native.wait(timeout=3)
    wait_for(lambda: not snapshot(['window', 'list'])['windows'], 'native cleanup failed')
    subprocess.run([sys.argv[1], sys.argv[2], 'stress', '2'], check=True, timeout=15)
    assert snapshot(['debug', 'stats'])['stats']['render_failures'] == 0
    print('PASS: real XWayland/X11 crash cleanup; native client focus and new windows remain usable')
finally:
    for child in children:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=3)
