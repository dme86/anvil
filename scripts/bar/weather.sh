#!/usr/bin/env bash
# wttr.in resolves location from the remote IP; set ANVIL_WEATHER_LOCATION to override.
# This makes a network request, so keep the interval long.
location=${ANVIL_WEATHER_LOCATION:-}
command -v curl >/dev/null 2>&1 || exit 1
curl --fail --silent --show-error --location --max-time 8 \
    --get --data-urlencode 'format=%c %t' "https://wttr.in/${location}" 
