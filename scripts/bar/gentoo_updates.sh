#!/usr/bin/env bash
# Read the existing Portage metadata only. Never sync repositories or sudo from the bar.
set -o pipefail
if ! command -v emerge >/dev/null 2>&1; then
    printf '󰏗 ?\n'
    exit 0
fi
if output=$(LC_ALL=C emerge --pretend --update --deep --newuse --color=n @world 2>/dev/null); then
    count=$(printf '%s\n' "$output" | awk '/^[[:space:]]*\[ebuild/ { count++ } END { print count+0 }')
    if (( count > 0 )); then
        printf '󰏖 %s\n' "$count"
    else
        printf '󰏗\n'
    fi
else
    # Anvil keeps the last successful cached value on a non-zero exit.
    exit 1
fi
