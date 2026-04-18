# Vessel

**Vessel** is a high-performance OCI container engine built in Rust. It is daemonless by default, giving containerized workloads a "pay-as-you-go" resource model with no standing background process when nothing is running.

Unlike traditional runtimes that require a persistent service, Vessel can execute containers directly from the CLI. When you want a long-lived control plane, it also offers an optional local daemon that the CLI can talk to with `--remote`. Under the hood, Vessel uses native Linux namespaces and macOS-specific microVM technology (**libkrun**) to provide secure, isolated environments with minimal overhead.

## 🚀 Key Features

- **Daemonless by Default:** `vessel run` executes directly with zero standing control-plane memory. An optional daemon mode is available for API-driven workflows.
- **Efficient Layered Storage:** OCI layers and blobs are cached by digest and shared across images to save disk space. Unused data can be reclaimed with `vessel gc`.
- **Cross-Platform Native:**
    - **Linux:** Uses native kernel namespaces (`unshare`), `chroot`, `mount`, and `overlayfs` for rootless execution. Outbound networking and `-p` publishing are enabled through `slirp4netns` when available.
    - **macOS:** Spawns lightweight microVMs using `libkrun` and `virtiofs`, with guest networking and host port forwarding handled inside the microVM.
- **OCI Compatible:** Pull, unpack, and execute standard images from any public registry (Docker Hub, GHCR, etc.).
- **Private Registry Ready:** Reuses credentials from standard Docker/Podman auth files so authenticated pulls work after `docker login` or `podman login`.
- **Runtime Flexibility:** Supports environment variable injection (`-e`), host volume mounting (`-v`), and host-to-guest port publishing (`-p`).
- **Resource Management:** Comprehensive suite of commands for container lifecycle (`run`, `start`, `stop`, `kill`, `rm`, `ps`, `logs`) and image management (`rmi`, `gc`).
- **Observability:** Integrated logging (`logs`) and table/JSON container listings (`ps --format table|json`).
- **Compose-Style Projects:** Launch and manage YAML-defined multi-service stacks with `vessel compose`.
- **Optional Remote Control Plane:** Start `vessel daemon` locally and use `vessel --remote ...` to route CLI operations over the Unix socket API.

## 🛠 Getting Started

### Prerequisites

- **Rust:** Vessel currently targets Rust 1.85 or newer.
- **macOS:** You must have `libkrun` installed. We recommend using Homebrew:
  ```bash
  brew install slp/krun/libkrun
  ```
- **Linux:** A kernel supporting user namespaces and overlayfs (standard on most modern distributions), plus `unshare` and `chroot` on your `PATH`.
- **Linux networking:** Install `slirp4netns` if you want outbound networking and host port publishing (`-p`) for rootless containers.

### Installation

1. **Clone the repository:**
   ```bash
   git clone https://github.com/wfinken/vessel.git
   cd vessel
   ```

2. **Build the binary:**
   ```bash
   cargo build --release
   ```

3. **Sign the binary (macOS only):**
   Vessel requires specific entitlements to manage hypervisor resources on macOS. Use the included helper task to sign the binary:
   ```bash
   cargo run -p xtask -- sign-macos target/release/vessel
   ```

4. **Add to your PATH:**
   Move `target/release/vessel` to a directory in your `$PATH` (e.g., `/usr/local/bin`).

## 📖 Usage

Vessel's CLI is designed to be familiar to users of Docker or Podman.

### Basic Examples

**Run a simple command:**
```bash
vessel run alpine -- echo "Hello from Vessel"
```

**Run a background (detached) container with environment variables:**
```bash
vessel run -d -e DB_HOST=localhost -e DB_PORT=5432 postgres:latest
```

**Publish a port to the host:**
```bash
vessel run -p 8080:80 nginx:alpine
```

**Mount a host directory into the container:**
```bash
vessel run -v $(pwd):/app alpine -- ls /app
```

**List containers as JSON:**
```bash
vessel ps --format json
```

**View logs from a background container:**
```bash
vessel logs <container_id>
```

**Pull from a private registry after logging in with Docker or Podman:**
```bash
docker login registry.example.com
vessel run registry.example.com/team/app:latest
```

Vessel checks `VESSEL_REGISTRY_AUTH_FILE`, `REGISTRY_AUTH_FILE`, Docker's `config.json`, and common Podman `auth.json` locations when resolving registry credentials.

**Clean up unused cached layers and blobs:**
```bash
vessel gc
```

**Use the optional daemon-backed control plane:**
```bash
vessel daemon start
vessel --remote ps
vessel daemon status
vessel daemon stop
```

**Start a multi-service project from YAML:**
```yaml
name: demo
services:
  db:
    image: postgres:16
    environment:
      POSTGRES_PASSWORD: secret
  api:
    image: ghcr.io/acme/api:latest
    command: ["./bin/api"]
    volumes:
      - ./app:/workspace
    ports:
      - 8080:80
    depends_on:
      - db
```

```bash
vessel compose up
vessel compose ps
vessel compose logs api
vessel compose down
```

Vessel auto-discovers `compose.yaml`, `compose.yml`, `vessel-compose.yaml`, and `vessel-compose.yml`. The current compose implementation supports `image`, `command`, `environment`, `volumes`, `ports`, `depends_on`, and an optional top-level `name`. Relative bind mounts are resolved from the compose file's directory, and you can override discovery with `vessel compose --file <path> --project-name <name> ...`.

### Lifecycle Management

- `vessel ps`: List all known containers.
- `vessel stop <id>`: Request a graceful shutdown.
- `vessel kill <id>`: Forcefully terminate a running container.
- `vessel rm <id>`: Remove a stopped container's state and logs.
- `vessel rmi <image>`: Remove a cached image and its root filesystem.
- `vessel gc`: Remove unused cached layers and blobs.

## 🤝 Contributing

We welcome contributions from the community! Whether you are fixing a bug, adding a feature, or improving documentation, your help is appreciated.

1. Fork the repository.
2. Create a new branch (`git checkout -b feature/my-new-feature`).
3. Commit your changes (`git commit -m 'Add some feature'`).
4. Push to the branch (`git push origin feature/my-new-feature`).
5. Open a Pull Request.

Please ensure your code follows the existing style and includes appropriate tests.

## ⚖️ License

Vessel is dual-licensed under:
- **Apache License, Version 2.0** ([LICENSE-APACHE](LICENSE-APACHE))
- **MIT License** ([LICENSE-MIT](LICENSE-MIT))

Choose the one that best fits your needs.
