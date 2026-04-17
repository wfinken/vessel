# Architecture

Vessel is organized as a Rust workspace:

- `vessel`: CLI surface, daemon lifecycle commands, and exit-code handling.
- `vessel-core`: shared types, state management, path discovery, and formatting.
- `vessel-daemon`: optional daemon API, local backend, and remote client.
- `vessel-image`: OCI registry client, blob cache, layer unpacker, and runtime config extraction.
- `vessel-runtime`: runtime abstraction with Linux, macOS, and unsupported-platform backends.
- `vesseld`: standalone daemon entrypoint.
- `xtask`: small developer utilities such as the bootstrap cold-start harness.

## Execution model

Vessel supports two execution paths:

1. **Daemonless mode:** `vessel ...` executes container operations directly in the CLI process.
2. **Daemon-backed mode:** `vessel daemon start` launches a background daemon, and `vessel --remote ...` routes lifecycle requests to it over a Unix socket.

In daemonless mode the CLI directly orchestrates every step:

1. Parse the requested image reference.
2. Pull and cache OCI objects when they are missing.
3. Materialize a merged root filesystem from cached layers.
4. Spawn the containerized process directly on Linux, or launch a fresh `libkrun` helper process on macOS.
5. Persist container state as JSON without a daemon.
6. Provide observability and cleanup via `logs`, `rm`, and `rmi`.

In daemon-backed mode the daemon owns the same local backend and exposes it through a small Unix-socket HTTP API.

## Runtime features

Vessel aims for a minimal yet capable runtime:

- **Environment merging:** Merges CLI-passed `-e` variables with the defaults defined in the OCI image.
- **Volume mapping:** 
  - On macOS, host directories are exposed via a `virtiofs` device added to the `libkrun` context. 
  - On Linux, this will use native bind mounts.
- **Detached lifecycle:** Containers can be launched in the background. On macOS, these background processes are tracked by a per-container bundle that includes a `stdio.log` for subsequent inspection.

## State layout

- Linux state: `$XDG_RUNTIME_DIR/vessel`
- Image and rootfs cache: `${XDG_DATA_HOME:-~/.local/share}/vessel`
- Non-Linux state: platform-local app data so `ps` can still inspect metadata

Each container gets a single JSON record. `ps` reads those records directly and
reconciles stale running PIDs by checking the host process table, with Linux and
macOS backends each applying platform-specific liveness checks.
