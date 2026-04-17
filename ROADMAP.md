# Roadmap

## Current bootstrap

- Cross-platform workspace and CI.
- Public anonymous OCI pulls from Docker Hub and GHCR.
- Private registry pulls via standard Docker/Podman auth files.
- Efficient **Overlayfs-backed** storage with deduplicated layer caching.
- Fully **writable root filesystem** for containers on both Linux and macOS.
- JSON-backed container state and `ps`.
- Runtime overrides for environment variables (`-e`) and volume mounts (`-v`) with automated setup.
- Observability via `logs` and resource cleanup via `rm`/`rmi`.
- Rootless-first Linux execution using namespace tooling and direct mounts.
- Daemonless macOS execution through a per-container `libkrun` helper process.
- Optional daemon-backed control plane with `vessel daemon start` and `--remote` CLI routing.
- Image layer garbage collection via `vessel gc`.
- Declarative multi-container configuration via `vessel compose` and YAML project files.

## Next milestones

- Interactive `exec` support for debugging running containers.
- User-space port forwarding (`-p`) for rootless networking.
- Outbound network egress via `slirp4netns` (Linux) or expanded `libkrun` config (macOS).
- Image signatures.
