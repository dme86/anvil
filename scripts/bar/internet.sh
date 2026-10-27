#!/usr/bin/env bash
# Observe interface state locally; no remote network requests.
if ! command -v ip >/dev/null 2>&1; then
    printf '󰖪\n'
    exit 0
fi
interface=$(ip -o route show default 2>/dev/null | awk 'NR==1 {for (i=1;i<=NF;i++) if ($i=="dev") {print $(i+1); exit}}')
case "$interface" in
    wl*|wlan*) printf '󰖩\n' ;;
    en*|eth*) printf '󰈀\n' ;;
    *) printf '󰖪\n' ;;
esac
