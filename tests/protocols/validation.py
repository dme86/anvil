#!/usr/bin/env python3
"""Stress/soak, relative resource measurements and failure recovery on a real nested instance."""
import json
import os
import pathlib
import selectors
import socket
import subprocess
import sys
import time

CLIENT, MODE, CONFIG, BUILD = sys.argv[1:]
COMPOSITOR = int(os.environ['ANVIL_COMPOSITOR_PID'])
ITERATIONS = int(os.environ.get('ANVIL_SOAK_ITERATIONS', '30'))
BATCHES = int(os.environ.get('ANVIL_SOAK_BATCHES', '4'))
assert 1 <= ITERATIONS <= 1000000 and 3 <= BATCHES <= 100
children = []


def command(argv, check=True, timeout=10):
    result = subprocess.run(argv, check=False, capture_output=True, text=True, timeout=timeout)
    if check and result.returncode != 0:
        print(result.stdout, result.stderr, file=sys.stderr, flush=True)
        result.check_returncode()
    return result


def cli(*args, check=True):
    result = command(['target/debug/anvilctl', *args], check=check)
    return result, json.loads(result.stdout)


def stats():
    return cli('debug', 'stats')[1]['stats']


def windows():
    return cli('window', 'list')[1]['windows']


def wait_for(predicate, description, timeout=5):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError(description)


def counts(clients, managed):
    wait_for(lambda: (stats()['connected_clients'], stats()['managed_windows']) == (clients, managed),
             f'client/window cleanup did not reach {clients}/{managed}')


def wait_line(child, prefix, timeout=5):
    if prefix == 'LOCKED' and getattr(child, '_locked', False):
        return 'LOCKED'
    deadline = time.monotonic() + timeout
    with selectors.DefaultSelector() as selector:
        selector.register(child.stdout, selectors.EVENT_READ)
        while time.monotonic() < deadline:
            assert child.poll() is None, 'fixture client exited'
            if selector.select(max(0, deadline-time.monotonic())):
                line = child.stdout.readline().decode().strip()
                if line == 'LOCKED':
                    child._locked = True
                if line.startswith(prefix):
                    return line
    raise AssertionError(f'fixture did not report {prefix}')


def start(mode='hold'):
    child = subprocess.Popen([CLIENT, MODE, mode], stdout=subprocess.PIPE, bufsize=0)
    children.append(child)
    wait_line(child, 'READY')
    return child


def stop(child):
    child.kill()
    child.wait(timeout=3)


def key(chord):
    command(['xdotool', 'key', '--clearmodifiers', chord])


def cpu_ticks():
    # Field 2 is parenthesized and may contain spaces; fields 14/15 are user/system CPU ticks.
    fields = pathlib.Path(f'/proc/{COMPOSITOR}/stat').read_text().rsplit(')', 1)[1].split()
    return int(fields[11]) + int(fields[12])


def idle_sample():
    # Wait for pending commits and host exposure to settle, then measure an unchanged desktop.
    time.sleep(0.5)
    before, ticks, start_time = stats(), cpu_ticks(), time.monotonic()
    time.sleep(1)
    after = stats()
    elapsed = time.monotonic() - start_time
    result = dict(after)
    result['idle_cpu_percent'] = 100 * (cpu_ticks()-ticks) / os.sysconf('SC_CLK_TCK') / elapsed
    result['idle_submissions'] = after['rendered_frames'] - before['rendered_frames']
    assert result['idle_submissions'] <= 2, f'unnecessary idle rendering: {result}'
    assert after['render_failures'] == 0, after
    return result


def stress(iterations):
    result = command([CLIENT, MODE, 'stress', str(iterations)], timeout=max(45, iterations*2))
    assert 'PASS:' in result.stdout, result.stdout
    print(result.stdout.strip(), flush=True)
    counts(0, 0)


try:
    counts(0, 0)
    host = command(['xdotool', 'search', '--name', '^Smithay$']).stdout.strip().splitlines()
    assert len(host) == 1, host
    command(['xdotool', 'windowfocus', '--sync', host[0]])
    # Exercise real keybindings, including tag movement and all three layout modes.
    first, second = start(), start()
    counts(2, 2)
    for _ in range(min(ITERATIONS, 20)):
        key('super+j')
        wait_for(lambda: windows()[0]['focused'], 'next focus did not reach first window')
        key('super+k')
        wait_for(lambda: windows()[1]['focused'], 'previous focus did not reach second window')
        key('super+2')
        wait_for(lambda: not any(w['focused'] for w in windows()), 'hidden tag retained focus')
        key('super+1')
        wait_for(lambda: windows()[0]['focused'], 'tag return did not restore focus')
        key('super+k')
        wait_for(lambda: windows()[1]['focused'], 'previous focus after tag return failed')
    key('super+shift+2')
    wait_for(lambda: any(w['tags'] == [2] for w in windows()), 'move-to-tag failed')
    key('super+2')
    wait_for(lambda: any(w['focused'] and w['tags'] == [2] for w in windows()), 'moved window inaccessible')
    key('super+shift+1')
    key('super+1')
    for _ in range(3):
        key('super+space')
    key('super+space')  # Fullscreen/monocle layout; kill a client while it is active.
    stop(second)
    counts(1, 1)
    wait_for(lambda: windows()[0]['focused'], 'fullscreen client exit lost survivor focus')
    key('super+space')
    key('super+space')  # Restore tiling.
    stop(first)
    counts(0, 0)
    print('PASS: rapid focus/tags/tag movement, layout transitions and fullscreen client exit', flush=True)

    # Warm the identical workload before establishing relative resource baselines.
    stress(ITERATIONS)
    baseline = idle_sample()
    samples = []
    for _ in range(BATCHES):
        stress(ITERATIONS)
        sample = idle_sample()
        assert sample['open_file_descriptors'] <= baseline['open_file_descriptors'] + 2, sample
        samples.append(sample)
    rss_budget = max(4*1024*1024, int(baseline['rss_bytes'] * 0.15))
    assert max(s['rss_bytes'] for s in samples) <= baseline['rss_bytes'] + rss_budget, samples
    # Check late growth as well as total growth, so repeated batches cannot quietly accumulate.
    assert samples[-1]['rss_bytes'] - samples[0]['rss_bytes'] <= rss_budget, samples
    report = {'iterations_per_batch': ITERATIONS, 'batches': BATCHES,
              'rss_growth_budget_bytes': rss_budget, 'baseline': baseline, 'samples': samples}
    pathlib.Path(BUILD, 'resources.json').write_text(json.dumps(report, indent=2)+'\n')
    print('PASS: idle rendering, measured idle CPU, bounded RSS growth and FD cleanup', flush=True)

    # Invalid syntax and invalid values must retain the last valid configuration and working IPC.
    config = pathlib.Path(CONFIG)
    original = config.read_text()
    try:
        survivor = start()
        for invalid in ('[general\n', '[general]\ntags=0\n'):
            config.write_text(invalid)
            result, response = cli('reload', check=False)
            assert result.returncode != 0 and response['status'] == 'error', response
            counts(1, 1)
            key('super+4')  # Still uses the previous valid four-tag configuration.
            wait_for(lambda: not windows()[0]['focused'], 'failed reload changed working tag configuration')
            key('super+1')
            wait_for(lambda: windows()[0]['focused'], 'failed reload broke focus')
    finally:
        config.write_text(original)
        assert cli('reload')[1]['status'] == 'reloaded'
    stop(survivor)
    counts(0, 0)
    # Malformed IPC requests and a partial request must not permanently stall input/IPC.
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(3)
        stream.connect(str(pathlib.Path(os.environ['XDG_RUNTIME_DIR'])/'anvil.sock'))
        for chunk in (b'{"command":', b'"debug_stats",', b'"version":1}\n'):
            stream.sendall(chunk)
            time.sleep(0.005)
        stream.shutdown(socket.SHUT_WR)
        assert json.loads(stream.makefile().read())['status'] == 'stats'
    for payload in (b'not json\n', b'{', b''):
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(3)
            stream.connect(str(pathlib.Path(os.environ['XDG_RUNTIME_DIR'])/'anvil.sock'))
            stream.sendall(payload)
            if payload.endswith(b'\n'):
                stream.shutdown(socket.SHUT_WR)
            response = json.loads(stream.makefile().read())
            assert response['status'] == 'error', response
        stats()
    print('PASS: invalid configuration retains working state; malformed IPC recovery', flush=True)

    # A client dies while owning selections, a popup and an active persistent pointer lock.
    owner = start('owner')
    command(['xdotool', 'mousemove', '500', '500'])
    wait_line(owner, 'LOCKED')
    for extra in ([], ['--primary']):
        value = command(['wl-paste', '--no-newline', *extra])
        assert value.stdout == 'anvil-test', value
    stop(owner)
    counts(0, 0)
    for extra in ([], ['--primary']):
        value = command(['wl-paste', '--no-newline', *extra], check=False)
        assert value.stdout != 'anvil-test', 'dead selection owner remained valid'
    survivor = start()
    command(['xdotool', 'mousemove', '300', '500'])
    first_motion = wait_line(survivor, 'MOTION')
    command(['xdotool', 'mousemove', '600', '500'])
    second_motion = wait_line(survivor, 'MOTION')
    assert first_motion != second_motion, 'dead pointer constraint still pins cursor'
    others = [start() for _ in range(3)]
    # Signal every process before waiting, so disconnects overlap rather than serialize.
    for child in [survivor, *others]:
        child.kill()
    for child in [survivor, *others]:
        child.wait(timeout=3)
    counts(0, 0)
    stress(2)  # The compositor remains usable after failures.
    print('PASS: selection owner, popup/parent, pointer-lock and simultaneous client crash cleanup', flush=True)

    # A locker crash is intentionally fail-closed. Test last: recovery requires session restart.
    locker = start('lock-owner')
    assert stats()['session_locked']
    stop(locker)
    counts(0, 0)
    assert stats()['session_locked'], 'locker death exposed desktop'
    hidden = start('locked-window')
    assert not any(w['focused'] for w in windows()), 'new client stole locked keyboard focus'
    stop(hidden)
    counts(0, 0)
    assert stats()['session_locked']
    result = command(['grim', '-o', 'winit', str(pathlib.Path(BUILD, 'locked.png'))], check=False)
    assert result.returncode != 0, 'locked session allowed capture'
    print('PASS: crashed locker remains locked and rejects desktop capture', flush=True)
finally:
    for child in children:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=3)
