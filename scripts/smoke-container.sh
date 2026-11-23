#!/bin/sh
# Exercise the installed CLI and its state in disposable, network-disabled containers.
set -eu
engine=${1:-docker}
image=${2:-localhost/soulsteads-agent-workboard:smoke}
# Use an explicit name: Podman removes anonymous volumes with --rm.
suffix=$(od -An -N 8 -tx1 /dev/urandom | tr -d " \n")
volume=$("$engine" volume create "soulsteads-smoke-$suffix")
trap '"$engine" volume rm "$volume" >/dev/null' EXIT
run() {
  "$engine" run --rm --network none --mount "type=volume,source=$volume,target=/data" "$@"
}
run --entrypoint sh "$image" -ec 'test "$(id -u)" -eq 10001 && test -w /data'
run "$image" schema | python3 -c 'import json, sys; assert json.load(sys.stdin)'
run "$image" run '{"op":"agent_register","id":"agent","name":"Container fixture"}'
run "$image" run '{"op":"project_create","key":"QA","name":"Test"}'
run "$image" run '{"op":"project_list"}' | python3 -c '
import json, sys
response = json.load(sys.stdin)
assert response["ok"] and any(row["key"] == "QA" for row in response["result"])
'
