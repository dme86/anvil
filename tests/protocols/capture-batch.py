#!/usr/bin/env python3
"""Check both the pixels and the shared-work bound on a real compositor."""
import json
import subprocess
import sys


def stats():
    return json.loads(subprocess.check_output(
        ['target/debug/anvilctl', 'debug', 'stats'], timeout=3))['stats']


before = stats()
subprocess.run([sys.argv[1]], check=True, timeout=45)
after = stats()
frames = after['capture_frames'] - before['capture_frames']
readbacks = after['capture_readbacks'] - before['capture_readbacks']
outputs = before['outputs']
# Two rounds, eight surviving requests/output, at most two cursor variants/round.
assert frames == 16 * outputs, (frames, outputs)
assert 2 * outputs <= readbacks <= 4 * outputs, (readbacks, outputs)
assert after['render_failures'] == before['render_failures'], (before, after)
print(f'PASS: {frames} capture frames shared {readbacks} output readbacks')
