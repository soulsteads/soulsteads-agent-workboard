# Run Agent Workboard in a container

Build a Linux image from this repository with Docker or Podman. Run these
commands from the root of a Git checkout that contains `Dockerfile`. You need
a running container engine and network access for the public build dependencies.
No registry account or host language toolchain is required; the build pulls
public base images. Use `engine=podman` in place of `engine=docker` for all
commands below.

For the loopback-only port mapping in this guide, use Docker Engine 28.0.0 or
newer, or Podman. With Docker, check the server version using `docker version`.
Do not use older Docker Engine versions for this recipe: a port published to
`127.0.0.1` is not guaranteed to stay on the host. See Docker’s
[port-publishing guidance](https://docs.docker.com/engine/network/port-publishing/#publishing-ports).

```sh
engine=docker                   # or: engine=podman
image=localhost/soulsteads-agent-workboard:local
"$engine" build --tag "$image" .
"$engine" volume create soulsteads-agent-workboard-data
```

The image runs as UID/GID `10001`, without root. A fresh engine-managed volume
at `/data` inherits the image's private directory and avoids bind-mount UID and
SELinux problems. If using a bind mount instead, provision a dedicated directory
writable by the mapped container user, mode `0700`, with suitable SELinux labeling
(e.g. a private `:Z` label on Podman); do not make it world-writable. Don't mount
a host home or the container-engine socket.

## Initialize the board and run the dashboard

The default SQLite path is `/data/workboard.sqlite3`. Mount the whole directory,
not an individual file: SQLite needs to create sidecar files next to it. Use a
local volume, not a network filesystem. The commands below create a project in
a **new** database. For an existing board, use `project_list` first.

```sh
wb() {
  "$engine" run --rm --network none -v soulsteads-agent-workboard-data:/data "$image" "$@"
}
wb run '{"op":"agent_register","id":"agent","name":"Local operator"}'
wb run '{"op":"project_create","key":"WB","name":"Sample board"}'
wb run '{"op":"project_list"}'

"$engine" run -d --name soulsteads-agent-workboard \
  -v soulsteads-agent-workboard-data:/data -p 127.0.0.1:7878:7878 \
  "$image" web --listen 0.0.0.0:7878
"$engine" logs soulsteads-agent-workboard
curl -fsS http://127.0.0.1:7878/ -o /dev/null
```

The wildcard bind is confined to the container's network; the published host
port is loopback-only. Keep the container on a network with only trusted peers.
The dashboard and API require a scoped worker key for data access. Follow the
[key step](../README.md#3-issue-a-key-and-open-the-dashboard), using `wb run` in
place of `workboard run` and the worker ID you registered. Handle the one-time
key output privately. Open <http://127.0.0.1:7878/> and use the key to inspect
the intended board. Use HTTPS with a trusted reverse proxy beyond localhost;
do not publish the raw API to the network.

Filesystem access to the volume is trusted operator access. Remote agents
should use the [authenticated HTTP API](../README.md#http-api), not a copy of
the SQLite database. For MCP on the same host, the `wb` wrapper needs `-i`
immediately after `run` (no `-t`), `-e WORKBOARD_API_KEY` passed from the
client's private environment, and `mcp` as the Workboard command.

## Retain state

Before upgrading, stop the service and any local writers, then follow
[Running and operating the board](../README.md#running-and-operating-the-board). Preserve the whole
volume or take a consistent SQLite backup, and keep the previous image and a
compatible snapshot for rollback. Reuse the same volume when replacing the
container; do not delete it as part of a normal upgrade.

To stop and remove just the container (the named volume remains):

```sh
"$engine" stop --time 30 soulsteads-agent-workboard
"$engine" rm soulsteads-agent-workboard
```

Then build the desired source revision and rerun the start command with the same
volume. Reuse a stable image tag or digest and the same deployment settings when
supervising it with your container engine or service manager. First test on a
fresh disposable volume. Run the offline image smoke locally with:

```sh
sh scripts/smoke-container.sh "$engine" "$image"
```
