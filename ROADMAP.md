# Roadmap

## Current state

- Cross-platform workspace and CI.
- Public anonymous OCI pulls from Docker Hub and GHCR.
- Private registry pulls via standard Docker/Podman auth files.
- Deduplicated OCI layer/blob caching with `vessel gc` for garbage collection.
- Fully **writable root filesystem** for containers on both Linux and macOS.
- JSON-backed container state with `ps --format table|json`.
- Runtime overrides for environment variables (`-e`), volume mounts (`-v`), and published ports (`-p`) with automated setup.
- Observability via `logs` and resource cleanup via `rm`/`rmi`.
- Outbound network egress and host port publishing on Linux (`slirp4netns`) and macOS (`libkrun`).
- Rootless-first Linux execution using namespace tooling and direct mounts.
- Daemonless macOS execution through a per-container `libkrun` helper process.
- Optional daemon-backed control plane with `vessel daemon start` and `--remote` CLI routing.
- Declarative multi-container configuration via `vessel compose` and YAML project files.

## Next milestones

- Interactive `exec` support for debugging running containers.
- Broader Compose-spec compatibility beyond the current core fields.
- Expanded remote-control and daemon workflows.
- Image signatures.
