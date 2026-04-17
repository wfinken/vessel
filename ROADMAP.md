# Roadmap

## Current bootstrap

- Cross-platform workspace and CI.
- Public anonymous OCI pulls from Docker Hub and GHCR.
- JSON-backed container state and `ps`.
- Runtime overrides for environment variables (`-e`) and volume mounts (`-v`).
- Observability via `logs` and resource cleanup via `rm`/`rmi`.
- Rootless-first Linux execution using namespace tooling already available on the host.
- Daemonless macOS execution through a per-container `libkrun` helper process.

## Next milestones

- Finalize automated guest mounting of `virtiofs` devices on macOS.
- Overlayfs-backed layer mounting instead of merged rootfs expansion.
- Interactive `exec` support.
- User-space port forwarding (`-p`) for rootless networking.
- Outbound network egress via `slirp4netns` (Linux) or `libkrun` (macOS).
- Private registry auth, signatures, and richer OCI compatibility checks.
