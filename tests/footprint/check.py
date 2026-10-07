#!/usr/bin/env python3
"""Measure reproducible feature footprints and annotate material regressions without noisy failures."""
import argparse
import json
import os
import pathlib
import re
import subprocess

PROFILES = {
    'minimal': ['--no-default-features'],
    'default': [],
    'all-features': ['--all-features'],
    'default-xwayland': ['--features', 'xwayland'],
}
BASELINE = pathlib.Path('tests/footprint/baseline.json')


def compare(baseline, current):
    notices = []
    same_compiler = baseline['rustc'] == current['rustc'] and baseline['target'] == current['target']
    if not same_compiler:
        notices.append('Compiler/target differs from baseline; size comparison is informational.')
    for name, measured in current['profiles'].items():
        previous = baseline['profiles'].get(name)
        if previous is None:
            notices.append(f'{name}: new profile, no size/dependency baseline.')
            continue
        growth = measured['anvil_bytes'] - previous['anvil_bytes']
        threshold = max(256*1024, previous['anvil_bytes'] * 0.12)
        if same_compiler and growth > threshold:
            notices.append(f'{name}: binary grew by {growth} bytes ({growth/previous["anvil_bytes"]:.1%}).')
        added = sorted(set(measured['dependencies']) - set(previous['dependencies']))
        removed = sorted(set(previous['dependencies']) - set(measured['dependencies']))
        if added or removed:
            notices.append(f'{name}: dependencies added: {", ".join(added) or "none"}; removed: {", ".join(removed) or "none"}.')
    return notices


def measure(selected_profiles=None):
    rustc = subprocess.check_output(['rustc', '-Vv'], text=True).strip()
    target = next(line.split(': ', 1)[1] for line in rustc.splitlines() if line.startswith('host: '))
    report = {'rustc': rustc, 'target': target, 'profiles': {}}
    for name, flags in PROFILES.items():
        if selected_profiles and name not in selected_profiles:
            continue
        # Isolate feature profiles: compiler/LTO artifacts must never contaminate another sample.
        target_dir = pathlib.Path(os.environ.get('ANVIL_FOOTPRINT_TARGET_DIR', 'target/footprint-build')) / name
        subprocess.run(['cargo', 'build', '--release', '--locked', '--target-dir', str(target_dir), *flags], check=True)
        tree = subprocess.check_output(['cargo', 'tree', '--locked', '--target', target,
                                        '-e', 'normal,build', '--prefix', 'none', '--format', '{p}', *flags], text=True)
        dependencies = set()
        for line in tree.splitlines():
            match = re.match(r'([^\s]+) v([^\s]+)', line)
            if match and match[1] != 'anvil':
                dependencies.add(f'{match[1]} {match[2]}')
        report['profiles'][name] = {'anvil_bytes': (target_dir/'release/anvil').stat().st_size,
                                    'dependencies': sorted(dependencies)}
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--record', action='store_true', help='write a new baseline for explicit review')
    parser.add_argument('--profile', choices=PROFILES, help='measure one CI matrix profile')
    args = parser.parse_args()
    if args.record and args.profile:
        parser.error('--record must measure all profiles')
    report = measure([args.profile] if args.profile else None)
    output = pathlib.Path('target/footprint')
    output.mkdir(parents=True, exist_ok=True)
    (output/'report.json').write_text(json.dumps(report, indent=2)+'\n')
    notices = []
    if args.record:
        BASELINE.write_text(json.dumps(report, indent=2)+'\n')
    else:
        notices = compare(json.loads(BASELINE.read_text()), report)
    lines = ['## Binary and dependency footprint', '',
             '| Profile | Anvil bytes | Dependencies |', '| --- | ---: | ---: |']
    for name, value in report['profiles'].items():
        lines.append(f'| {name} | {value["anvil_bytes"]} | {len(value["dependencies"])} |')
    lines += ['', 'Release builds use the checked-in LTO/strip settings. Material growth threshold: 12% and at least 256 KiB.', '']
    lines += ['- '+notice for notice in notices] or ['No material size or dependency changes.']
    summary = '\n'.join(lines)+'\n'
    (output/'summary.md').write_text(summary)
    print(summary)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as stream:
            stream.write(summary)
    for notice in notices:
        print('::warning title=Anvil footprint::'+notice.replace('%', '%25').replace('\n', '%0A').replace('\r', '%0D'))


if __name__ == '__main__':
    main()
