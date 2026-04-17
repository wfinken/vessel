# `zyclonite/sysbench` on Docker vs Vessel

Benchmark timestamp: `2026-04-17T03:09:40Z`

## Summary

- Docker successfully ran `docker.io/zyclonite/sysbench:latest` natively on this Apple Silicon host.
- Vessel successfully ran the same OCI image and command natively on this Apple Silicon host.
- Docker carries measurable always-on overhead on this machine:
  - Docker Desktop Linux VM memory: `7.65 GiB`
  - Host-side Docker Desktop process RSS snapshot: `394.86 MiB`
- Vessel left no resident `vessel` process behind after execution; there was no post-run background process in the host `ps` snapshot.
- Both engines produced similar throughput (events/sec), with Vessel showing slightly higher performance in this run.

## Host System

| Field | Value |
| --- | --- |
| OS | `macOS 26.4 (25E5223i)` |
| Model | `Mac16,10` |
| CPU | `Apple M4` |
| Logical CPUs | `10` |
| Physical CPUs | `10` |
| RAM | `16.00 GiB` |
| Kernel | `Darwin 25.4.0` |

Commands used:

```bash
sw_vers
sysctl -n hw.model hw.ncpu hw.logicalcpu hw.physicalcpu hw.memsize machdep.cpu.brand_string
uname -a
```

## Image Details

Image tested:

```text
docker.io/zyclonite/sysbench:latest
```

Image config:

```text
Architecture=arm64 Os=linux Entrypoint=["sysbench"] Cmd=["--help"]
```

## Docker Environment

Docker daemon details:

```text
ServerVersion=29.2.1 OSType=linux Arch=aarch64 Kernel=6.12.68-linuxkit CPUs=10 MemTotal=8217317376
```

Interpretation:

- Docker Desktop version: `29.2.1`
- Guest kernel: `6.12.68-linuxkit`
- Docker Desktop Linux VM memory: `7.65 GiB`
- Host-side Docker Desktop RSS at snapshot time: `394.86 MiB`

## Benchmark Method

Warmup commands:

```bash
docker run --rm zyclonite/sysbench --version
VESSEL_DATA_DIR=$PWD/.bench-vessel-data \
VESSEL_STATE_DIR=$PWD/.bench-vessel-state \
VESSEL_LIBKRUN_PATH=/opt/homebrew/opt/libkrun/lib/libkrun.1.dylib \
./target/release/vessel run zyclonite/sysbench -- --version
```

Timed Docker benchmark command:

```bash
/usr/bin/time -lp docker run --rm zyclonite/sysbench \
  cpu --threads=1 --time=5 run
```

Timed Vessel benchmark command:

```bash
/usr/bin/time -lp env \
  VESSEL_DATA_DIR=$PWD/.bench-vessel-data \
  VESSEL_STATE_DIR=$PWD/.bench-vessel-state \
  VESSEL_LIBKRUN_PATH=/opt/homebrew/opt/libkrun/lib/libkrun.1.dylib \
  ./target/release/vessel run zyclonite/sysbench -- \
  cpu --threads=1 --time=5 run
```

## Results

### Docker

Sysbench version:

```text
sysbench 1.0.20
```

Three timed runs:

| Engine | Run | Result | Events/sec | Total events | `time` real | Client peak footprint |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| Docker | 1 | success | 10736.99 | 53699 | 5.33s | 13.0 MiB |
| Docker | 2 | success | 10330.96 | 51665 | 5.30s | 13.1 MiB |
| Docker | 3 | success | 10341.45 | 51764 | 5.29s | 13.0 MiB |

Average across the three Docker runs:

| Metric | Value |
| --- | --- |
| Average events/sec | `10469.80` |
| Average total events | `52376` |
| Average wall-clock (`real`) | `5.31s` |
| Average Docker CLI peak footprint | `13.0 MiB` |

### Vessel

Sysbench version:

```text
sysbench 1.0.20
```

Three timed runs:

| Engine | Run | Result | Events/sec | Total events | `time` real | Engine peak footprint |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| Vessel | 1 | success | 10744.65 | 53730 | 5.50s | 279.1 MiB |
| Vessel | 2 | success | 10772.54 | 53870 | 5.15s | 279.4 MiB |
| Vessel | 3 | success | 10394.91 | 51982 | 5.16s | 278.4 MiB |

Average across the three Vessel runs:

| Metric | Value |
| --- | --- |
| Average events/sec | `10637.37` |
| Average total events | `53194` |
| Average wall-clock (`real`) | `5.27s` |
| Average Vessel peak footprint | `279.0 MiB` |

## Interpretation

1. Both Docker and Vessel executed the `arm64` image natively.
2. Vessel's throughput (`10637.37 events/sec`) was slightly higher than Docker's (`10469.80 events/sec`) in this CPU-bound test, likely due to reduced overhead from avoiding the Docker Desktop networking/storage proxying stack, though both are running inside Linux VMs on macOS.
3. Memory Footprint Tradeoff:
   - **Docker CLI** is very light (`13.0 MiB`), but it is just a client for a massive standing backend (`7.65 GiB` VM + `394.86 MiB` host RSS).
   - **Vessel** has a higher per-invocation footprint (`279.0 MiB`) because it *is* the entire engine and Linux microVM, but it consumes **zero** resources when not running.
4. Total Startup/Teardown:
   - Docker `real` time includes client-to-daemon communication and container lifecycle management by the daemon.
   - Vessel `real` time includes booting the microVM, configuring the rootfs, and executing the workload.
   - Both engines completed the 5-second workload in roughly 5.3 seconds of wall-clock time.

## Bottom Line

For `docker.io/zyclonite/sysbench:latest` on this Apple Silicon macOS machine:

- Both engines are highly capable of running native `arm64` workloads.
- Vessel provides comparable performance to Docker with a significantly better "pay-as-you-go" resource profile for short-lived tasks, trading a small standing daemon for a self-contained microVM process.
