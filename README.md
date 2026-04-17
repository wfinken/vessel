# Vessel

Vessel is a lightweight, daemonless container engine and runtime written in Rust.
This bootstrap targets three goals:

- Build and test cleanly on macOS, Linux, and Windows.
- Execute OCI images locally without a resident daemon.
- Keep the CLI small, scriptable, and explicit about unsupported capabilities.

Current bootstrap commands:

- `vessel run [-d] [-e ENV] [-v VOLUME] <image> [command...]`
- `vessel start <id>`
- `vessel stop <id>`
- `vessel kill <id>`
- `vessel rm <id>`
- `vessel logs <id>`
- `vessel ps [--format table|json]`
- `vessel rmi <image>`

For local testing and automation, `VESSEL_DATA_DIR` and `VESSEL_STATE_DIR`
override the default cache and state locations.

## Status

This is a bootstrap release. The image pipeline supports anonymous pulls from
public registries and unpacks layered OCI images into a reusable root filesystem
cache. Linux execution uses a rootless-first launch path that relies on user and
pid namespaces via `unshare` plus `chroot`. macOS execution uses a per-container
`libkrun` microVM helper process with no resident daemon.

Vessel now supports:
- **Environment Overrides:** Pass `-e KEY=VALUE` to inject variables at runtime.
- **Volume Mounts:** Pass `-v host_path:guest_path` to expose host directories to the container (macOS uses virtiofs).
- **Resource Cleanup:** Remove containers (`rm`) and images (`rmi`) to reclaim disk space.
- **Observability:** View detached container output via `vessel logs`.

## macOS runtime

The macOS backend expects a local `libkrun` install and discovers it in common
Homebrew locations first:

- `/opt/homebrew/opt/libkrun/lib/libkrun.1.dylib`
- `/usr/local/opt/libkrun/lib/libkrun.1.dylib`

You can also point Vessel at an explicit library with
`VESSEL_LIBKRUN_PATH=/absolute/path/to/libkrun.1.dylib`.

If you have a separate kernel you want Vessel to use, set
`VESSEL_LIBKRUN_KERNEL_PATH=/absolute/path/to/kernel`. For the upstream
standalone `libkrun` flow this is optional; Vessel only uses it when present.

After `cargo build` or `cargo build --release`, sign the Vessel binary before
running containers on macOS:

```bash
cargo run -p xtask -- sign-macos
```

That signs `target/debug/vessel` using the checked-in `vessel.entitlements`
profile. To sign a different binary path, pass it explicitly:

```bash
cargo run -p xtask -- sign-macos target/release/vessel
```

Planned next steps are tracked in [ROADMAP.md](ROADMAP.md) and the system shape
is summarized in [ARCHITECTURE.md](ARCHITECTURE.md).
