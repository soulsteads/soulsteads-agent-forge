# Run Agent Forge in a container

Build a Linux image from this repository with Docker or Podman. Run these
commands from the root of a Git checkout that contains `Dockerfile`. You need
a running container engine and network access for the public build dependencies.
No registry account or host language toolchain is required; the build pulls
public base images. Use `engine=podman` in place of `engine=docker` for all
commands below.

```sh
engine=docker                   # or: engine=podman
image=localhost/soulsteads-agent-forge:local
"$engine" build --tag "$image" .
"$engine" volume create soulsteads-agent-forge-data
```

The image runs as UID/GID `10001`, without root. A fresh engine-managed volume
at `/data` inherits the image's private directory and avoids bind-mount UID and
SELinux problems. If using a bind mount instead, provision a dedicated directory
writable by the mapped container user, mode `0700`, with suitable SELinux labeling
(e.g. a private `:Z` label on Podman); do not make it world-writable. Don't mount
a host home or the container-engine socket.

## Initialize and run Forge

Use native Linux and a dedicated, empty PostgreSQL database (PostgreSQL 18
matches the source quickstart's tested path). For a separate PostgreSQL
container, publish its port **only** on host loopback and use Podman or Docker
Engine 28.0.0 or newer (check the server version with `docker version`).
[Older Docker Engine releases](https://docs.docker.com/engine/network/port-publishing/#publishing-ports)
can expose localhost-published ports to other hosts on the same network
segment. If you can't meet this requirement, use PostgreSQL running directly
on the host with a loopback listener instead.

The Forge image does not include a database, agent harnesses, or executable
plugins. Start with the no-model workflow in the [quick start](../README.md#quick-start).

Forge refuses all non-loopback listener addresses. Use native Linux host
networking for this recipe; `-p` on a bridged container does not make its
loopback listener reachable. `--network host` shares the host's network
namespace, including its loopback services. Do not run untrusted workflows
on that basis. Keep `127.0.0.1` as the Forge bind and use the existing access
controls; Docker Desktop/Podman machine do not provide this recipe's same-host
guarantee. Check that port 8110 is free before starting.

Create a mode-`0600` `forge.env` outside the checkout with
`FORGE_DATABASE_URL` set to the dedicated database URL. For a PostgreSQL port
on the host, use the loopback address and that port (not the database
container's name). Do not put a password in a command line or tracked example.
Engine administrators can inspect runtime environment variables.

```sh
# Set FORGE_ENV to the absolute path of that private file.
"$engine" run --rm --network none -v soulsteads-agent-forge-data:/data "$image" init
"$engine" run -d --name soulsteads-agent-forge --network host \
  --env-file "$FORGE_ENV" -v soulsteads-agent-forge-data:/data "$image" \
  serve --host 127.0.0.1 --port 8110
"$engine" logs soulsteads-agent-forge
curl -fsS http://127.0.0.1:8110/ -o /dev/null
```

Open <http://127.0.0.1:8110/> and sign in with the operator token at
`actors.operator.token` in the volume's `/data/forge/config.json`. Read it
privately, e.g. copy that file with the engine's `cp` into a mode-`0700`
directory outside the checkout; do not print it in logs or share it with agents.
A dashboard response is only a listener check. To verify execution, run the
quickstart's deterministic workflow and inspect its terminal output. To use
CLI/MCP from the host, follow [Operations](operations.md#operational-cli)
and supply the token privately to the client, not in a URL.

## Retain the home and the database

Keep the same volume and PostgreSQL database together across restarts. Never
start a second Forge service for the same home or database. Stop gracefully
before replacing the image; for recovery, retain the old image and a compatible
backup of **both** persistence stores. Follow [Recovery](recovery.md) for the
coordinated backup and restore procedure. It needs matching PostgreSQL client
tools (`pg_dump`, `pg_restore`, `psql`), which the base image does not ship.
Use a derived image with the client major version matching your database for
container backup/restore, and mount a separate private backup destination.
The container is not a replacement for the harness and plugin containment
requirements documented in [Operations](operations.md).

To stop and remove just the container (the named volume remains):

```sh
"$engine" stop --time 30 soulsteads-agent-forge
"$engine" rm soulsteads-agent-forge
```

Then build the desired source revision and rerun the start command with the same
volume. Reuse a stable image tag or digest and the same deployment settings when
supervising it with your container engine or service manager. First test on a
fresh disposable volume. Run the offline image smoke locally with:

```sh
sh scripts/smoke-container.sh "$engine" "$image"
```
