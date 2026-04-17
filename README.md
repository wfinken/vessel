# Vessel

**Vessel** is a high-performance, daemonless OCI container engine built in Rust. It provides a "pay-as-you-go" resource model for containerized workloads, ensuring that no standing daemons or background processes consume memory when your containers aren't running.

Unlike traditional container runtimes that rely on a persistent background service (like Docker Desktop), Vessel operates as a standalone CLI. It leverages native Linux namespaces and macOS-specific microVM technology (**libkrun**) to provide secure, isolated environments with minimal overhead.

## 🚀 Key Features

- **Daemonless Operation:** Zero resident memory footprint. When the container stops, the engine stops.
- **Efficient Layered Storage:** Uses **Overlayfs** to combine immutable OCI layers with a per-container writable layer. Layers are cached by digest and shared across images to save disk space.
- **Cross-Platform Native:**
    - **Linux:** Uses native kernel namespaces (`unshare`), `chroot`, and `mount` for rootless execution.
    - **macOS:** Spawns lightweight microVMs using `libkrun` and `virtiofs` for native performance.
- **OCI Compatible:** Pull, unpack, and execute standard images from any public registry (Docker Hub, GHCR, etc.).
- **Runtime Flexibility:** Supports environment variable injection (`-e`) and host volume mounting (`-v`) with automated guest-side setup.
- **Resource Management:** Comprehensive suite of commands for container lifecycle (`run`, `start`, `stop`, `kill`, `rm`) and image management (`rmi`).
- **Observability:** Integrated logging (`logs`) to inspect stdout/stderr from background containers.

## 🛠 Getting Started

### Prerequisites

- **Rust:** You will need the latest stable version of the Rust toolchain.
- **macOS:** You must have `libkrun` installed. We recommend using Homebrew:
  ```bash
  brew install slp/krun/libkrun
  ```
- **Linux:** A kernel supporting user namespaces (standard on most modern distributions).

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

**Mount a host directory into the container:**
```bash
vessel run -v $(pwd):/app alpine -- ls /app
```

**View logs from a background container:**
```bash
vessel logs <container_id>
```

### Lifecycle Management

- `vessel ps`: List all known containers.
- `vessel stop <id>`: Request a graceful shutdown.
- `vessel rm <id>`: Remove a stopped container's state and logs.
- `vessel rmi <image>`: Remove a cached image and its root filesystem.

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
