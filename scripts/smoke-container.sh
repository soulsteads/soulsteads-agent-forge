#!/bin/sh
# Exercise the installed CLI and its state in disposable, network-disabled containers.
set -eu
engine=${1:-docker}
image=${2:-localhost/soulsteads-agent-forge:smoke}
# Use an explicit name: Podman removes anonymous volumes with --rm.
suffix=$(od -An -N 8 -tx1 /dev/urandom | tr -d " \n")
volume=$("$engine" volume create "soulsteads-smoke-$suffix")
trap '"$engine" volume rm "$volume" >/dev/null' EXIT
run() {
  "$engine" run --rm --network none --mount "type=volume,source=$volume,target=/data" "$@"
}
run --entrypoint sh "$image" -ec 'test "$(id -u)" -eq 10001 && test -w /data'
run "$image" init
run --entrypoint sh "$image" -ec 'test -s "$FORGE_HOME/config.json" && test "$(stat -c %a "$FORGE_HOME/config.json")" = 600'
