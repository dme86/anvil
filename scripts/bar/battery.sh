#!/usr/bin/env bash
shopt -s nullglob
for battery in /sys/class/power_supply/BAT*; do
    [[ -r "$battery/capacity" ]] || continue
    capacity=$(<"$battery/capacity")
    status=$(<"$battery/status")
    if [[ "$status" == "Charging" ]]; then
        printf '󰂄 %s%%\n' "$capacity"
    else
        printf '󰁹 %s%%\n' "$capacity"
    fi
    exit 0
done
# VM/desktop without a battery: omit the block.
