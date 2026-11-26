#!/usr/bin/env bash
# Loopback check for fastgraph without hardware.
#
# Creates a temporary PipeWire null sink "fastgraph_test", waits until you have picked
# "Monitor of fastgraph_test" as the input in fastgraph and pressed Listen (in Sweep or RTA
# mode), then plays each WAV into the null sink after a random 2-6 s delay, as a phone would.
# The sink is removed on exit, also on Ctrl-C. Nothing is played on a real device.
#
#   scripts/loopback-check.sh fastgraph-sweep-normal-48k.wav [more.wav ...]
#
# Drift copies (same samples, wrong declared rate):
#   cargo run -p fastgraph-core --example rehead -- IN.wav fast.wav 48014   # reads ~ -292 ppm
#   cargo run -p fastgraph-core --example rehead -- IN.wav slow.wav 47986   # reads ~ +292 ppm
set -euo pipefail

SINK=fastgraph_test

if [[ $# -eq 0 || $1 == -h || $1 == --help ]]; then
    sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
    exit $(( $# == 0 ? 2 : 0 ))
fi
for f in "$@"; do
    [[ -r $f ]] || { echo "cannot read $f" >&2; exit 2; }
done
for tool in pactl pw-play; do
    command -v "$tool" >/dev/null || { echo "$tool not found (pipewire-pulse / pipewire)" >&2; exit 2; }
done
if pactl list short sinks | awk '{print $2}' | grep -qx "$SINK"; then
    echo "a sink named $SINK already exists; unload it first:" >&2
    pactl list short modules | grep "sink_name=$SINK" >&2 || true
    exit 1
fi

default_before=$(pactl get-default-sink)
module=$(pactl load-module module-null-sink "sink_name=$SINK")
echo "loaded null sink $SINK (module $module)"

cleanup() {
    [[ -n ${module:-} ]] || return 0
    pactl unload-module "$module" && echo "unloaded null sink (module $module)"
    module=
    if [[ $(pactl get-default-sink) != "$default_before" ]]; then
        pactl set-default-sink "$default_before"
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [[ -t 0 ]]; then
    read -rp "In fastgraph pick \"Monitor of $SINK\" and press Listen, then press Enter here. "
fi

for f in "$@"; do
    delay_ms=$(( 2000 + RANDOM % 4001 ))
    printf 'playing %s in %d.%03d s\n' "$f" $(( delay_ms / 1000 )) $(( delay_ms % 1000 ))
    sleep "$(( delay_ms / 1000 )).$(printf '%03d' $(( delay_ms % 1000 )))"
    # Never fall back to the default device if the null sink is gone.
    pw-play --target "$SINK" -P '{ node.dont-fallback = true node.dont-reconnect = true }' -- "$f"
done

if [[ -t 0 ]]; then
    read -rp "Done. Press Enter to remove the sink. "
fi
