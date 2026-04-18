use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use rustix::process;
use vessel_core::{
    CapabilityReport, ContainerId, ContainerRecord, ContainerStatus, ContainerStore, ImageRef,
    VesselError, VesselPaths, now_timestamp,
};
use vessel_image::ImageStore;

use crate::{RunOutcome, Runtime};

#[derive(Debug, Clone)]
pub struct LinuxRuntime {
    paths: VesselPaths,
}

impl LinuxRuntime {
    pub fn new(paths: VesselPaths) -> Self {
        Self { paths }
    }

    fn required_tools(&self) -> Result<ToolPaths, VesselError> {
        Ok(ToolPaths {
            chroot: find_in_path("chroot").ok_or_else(|| {
                VesselError::Capability("missing required tool `chroot`".to_string())
            })?,
            unshare: find_in_path("unshare").ok_or_else(|| {
                VesselError::Capability("missing required tool `unshare`".to_string())
            })?,
            slirp4netns: find_in_path("slirp4netns"),
        })
    }

    fn spawn_from_record(
        &self,
        store: &ContainerStore,
        mut record: ContainerRecord,
        detached: bool,
    ) -> Result<RunOutcome, VesselError> {
        let tools = self.required_tools()?;
        let bundle_dir = self.paths.bundles_dir.join(record.id().as_str());
        fs::create_dir_all(&bundle_dir).map_err(|source| VesselError::io(&bundle_dir, source))?;

        // We need a writable layer and a workdir for overlayfs.
        let upper_dir = bundle_dir.join("upper");
        let work_dir = bundle_dir.join("work");
        let merged_dir = bundle_dir.join("merged");
        fs::create_dir_all(&upper_dir).map_err(|source| VesselError::io(&upper_dir, source))?;
        fs::create_dir_all(&work_dir).map_err(|source| VesselError::io(&work_dir, source))?;
        fs::create_dir_all(&merged_dir).map_err(|source| VesselError::io(&merged_dir, source))?;

        let log_path = bundle_dir.join("stdio.log");

        let mut command = Command::new(&tools.unshare);
        command
            .arg("--user")
            .arg("--map-root-user")
            .arg("--mount")
            .arg("--pid")
            .arg("--fork")
            .arg("--kill-child")
            .arg("--mount-proc");

        if tools.slirp4netns.is_some() {
            command.arg("--net");
        }

        let (exec_path, args) = build_linux_overlay_wrapper(&record, &tools, &bundle_dir);
        command.arg(exec_path).args(args);

        command.current_dir("/").env_clear();

        if !record.environment.contains_key("PATH") {
            command.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        }
        for (key, value) in &record.environment {
            command.env(key, value);
        }

        if detached {
            let log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .map_err(|source| VesselError::io(&log_path, source))?;
            let log_clone = log.try_clone().map_err(VesselError::GenericIo)?;
            command.stdin(Stdio::null()).stdout(Stdio::from(log)).stderr(Stdio::from(log_clone));
        } else {
            command.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
        }

        let mut child = command.spawn().map_err(map_spawn_error)?;

        if let Some(slirp_path) = &tools.slirp4netns {
            let pid = child.id();
            let mut slirp_cmd = Command::new(slirp_path);
            slirp_cmd.arg("--configure").arg("--mtu").arg("65520").arg(pid.to_string()).arg("tap0");

            for (host_port, guest_port) in &record.ports {
                slirp_cmd.arg("-p").arg(format!("{host_port}:{guest_port}"));
            }

            slirp_cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

            slirp_cmd.spawn().map_err(|source| {
                VesselError::Runtime(format!("failed to start slirp4netns: {source}"))
            })?;
        }

        record.status = ContainerStatus::Running;
        record.pid = Some(child.id());
        record.started_at = Some(now_timestamp());
        record.finished_at = None;
        store.save(&record)?;

        if detached {
            return Ok(RunOutcome { record, exit_code: None });
        }

        let status = child.wait().map_err(VesselError::GenericIo)?;
        let exit_code = exit_code_for_status(status);
        record.status = ContainerStatus::Exited { code: exit_code };
        record.pid = None;
        record.finished_at = Some(now_timestamp());
        store.save(&record)?;

        Ok(RunOutcome { record, exit_code: Some(exit_code) })
    }
}

fn build_linux_overlay_wrapper(
    record: &ContainerRecord,
    tools: &ToolPaths,
    bundle_dir: &Path,
) -> (String, Vec<String>) {
    let mut script = String::from("set -e; ");

    let upper_dir = bundle_dir.join("upper");
    let work_dir = bundle_dir.join("work");
    let merged_dir = bundle_dir.join("merged");

    // 1. Mount overlayfs
    // Lowerdirs should be in order from top to bottom.
    let lower_str =
        record.layers.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>().join(":");

    script.push_str(&format!(
        "mount -t overlay overlay -o lowerdir={},upperdir={},workdir={} {}; ",
        lower_str,
        upper_dir.display(),
        work_dir.display(),
        merged_dir.display()
    ));

    if tools.slirp4netns.is_some() {
        script.push_str("if command -v ip >/dev/null; then ip link set lo up || true; fi; ");
    }

    // 2. Mount additional volumes into the merged root
    for (host_path, guest_path) in &record.mounts {
        let absolute_guest_path = merged_dir.join(guest_path.trim_start_matches('/'));
        script.push_str(&format!("mkdir -p \"{}\"; ", absolute_guest_path.display()));
        script.push_str(&format!(
            "mount --bind \"{}\" \"{}\"; ",
            host_path,
            absolute_guest_path.display()
        ));
    }

    // 3. chroot and exec
    let workdir = record.workdir.as_deref().unwrap_or("/");
    script.push_str(&format!("cd \"{}{}\"; ", merged_dir.display(), workdir));
    script.push_str(&format!(
        "exec \"{}\" \"{}\" \"$0\" \"$@\"",
        tools.chroot.display(),
        merged_dir.display()
    ));

    let mut argv = vec![String::from("-c"), script];
    argv.extend(record.command.iter().cloned());

    (String::from("/bin/sh"), argv)
}

impl Runtime for LinuxRuntime {
    fn capability_report(&self) -> CapabilityReport {
        let mut missing = Vec::new();
        let unshare = find_in_path("unshare").is_some();
        let chroot = find_in_path("chroot").is_some();
        let overlayfs = fs::read_to_string("/proc/filesystems")
            .map(|content| content.lines().any(|line| line.contains("overlay")))
            .unwrap_or(false);
        let cgroup_v2 = Path::new("/sys/fs/cgroup/cgroup.controllers").exists();
        let user_namespace = Path::new("/proc/self/ns/user").exists();

        if !unshare {
            missing.push("unshare".to_string());
        }
        if !chroot {
            missing.push("chroot".to_string());
        }
        if !user_namespace {
            missing.push("user namespaces".to_string());
        }

        CapabilityReport {
            platform: "linux".to_string(),
            supported: missing.is_empty(),
            rootless: process::geteuid().as_raw() != 0,
            overlayfs,
            cgroup_v2,
            missing,
        }
    }

    fn run(
        &self,
        store: &ContainerStore,
        image: &ImageRef,
        detach: bool,
        command_override: Option<Vec<String>>,
        env_override: Option<BTreeMap<String, String>>,
        mount_override: Option<BTreeMap<String, String>>,
        port_override: Option<BTreeMap<u16, u16>>,
    ) -> Result<RunOutcome, VesselError> {
        let image_store = ImageStore::new(self.paths.clone());
        let image = image_store.pull(image)?;

        let mut environment = image.runtime.env.clone();
        if let Some(overrides) = env_override {
            for (key, value) in overrides {
                environment.insert(key, value);
            }
        }

        let record = ContainerRecord::new(
            ContainerId::generate(),
            image.image.clone(),
            image.resolved_command(command_override.as_deref())?,
            image.runtime.working_dir.clone(),
            environment,
            mount_override.unwrap_or_default(),
            port_override.unwrap_or_default(),
            image.layers.clone(),
        );
        store.save(&record)?;
        self.spawn_from_record(store, record, detach)
    }

    fn start(
        &self,
        store: &ContainerStore,
        id: &ContainerId,
    ) -> Result<ContainerRecord, VesselError> {
        let record = store.load(id)?;
        if matches!(record.status, ContainerStatus::Running) && record.pid.is_some() {
            return Err(VesselError::ContainerAlreadyRunning(id.to_string()));
        }
        let outcome = self.spawn_from_record(store, record, true)?;
        Ok(outcome.record)
    }

    fn stop(
        &self,
        store: &ContainerStore,
        id: &ContainerId,
    ) -> Result<ContainerRecord, VesselError> {
        let record = store.load(id)?;
        let pid = record.pid.ok_or_else(|| VesselError::ContainerNotRunning(id.to_string()))?;

        // Ignore errors if the process is already dead
        let _ = send_signal(pid, libc::SIGTERM);
        if wait_for_exit(pid, Duration::from_secs(2)).is_err() {
            let _ = send_signal(pid, libc::SIGKILL);
            wait_for_exit(pid, Duration::from_secs(2))?;
        }

        store.update_status(id, ContainerStatus::Stopped, None, Some(now_timestamp()))
    }

    fn kill(
        &self,
        store: &ContainerStore,
        id: &ContainerId,
    ) -> Result<ContainerRecord, VesselError> {
        let record = store.load(id)?;
        let pid = record.pid.ok_or_else(|| VesselError::ContainerNotRunning(id.to_string()))?;
        send_signal(pid, libc::SIGKILL)?;
        wait_for_exit(pid, Duration::from_secs(2))?;
        store.update_status(id, ContainerStatus::Exited { code: 137 }, None, Some(now_timestamp()))
    }

    fn remove(&self, store: &ContainerStore, id: &ContainerId) -> Result<(), VesselError> {
        let record = store.load(id)?;
        if matches!(record.status, ContainerStatus::Running) {
            return Err(VesselError::ContainerAlreadyRunning(id.to_string()));
        }

        let bundle_dir = self.paths.bundles_dir.join(id.as_str());
        if bundle_dir.exists() {
            fs::remove_dir_all(&bundle_dir)
                .map_err(|source| VesselError::io(&bundle_dir, source))?;
        }

        store.remove(id)
    }

    fn logs(&self, store: &ContainerStore, id: &ContainerId) -> Result<(), VesselError> {
        let _record = store.load(id)?;
        let log_path = self.paths.bundles_dir.join(id.as_str()).join("stdio.log");
        if !log_path.exists() {
            return Ok(());
        }

        let content =
            fs::read_to_string(&log_path).map_err(|source| VesselError::io(&log_path, source))?;
        print!("{content}");
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct ToolPaths {
    chroot: PathBuf,
    unshare: PathBuf,
    slirp4netns: Option<PathBuf>,
}

fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|candidate| {
        let path = candidate.join(binary);
        path.is_file().then_some(path)
    })
}

fn exit_code_for_status(status: ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| status.signal().map(|signal| 128 + signal).unwrap_or(125))
}

fn map_spawn_error(error: std::io::Error) -> VesselError {
    match error.kind() {
        std::io::ErrorKind::NotFound => {
            VesselError::Runtime("failed to spawn runtime helper".to_string())
        }
        std::io::ErrorKind::PermissionDenied => {
            VesselError::Runtime("permission denied while launching container".to_string())
        }
        _ => VesselError::GenericIo(error),
    }
}

fn send_signal(pid: u32, signal: i32) -> Result<(), VesselError> {
    let result = unsafe { libc::kill(pid as i32, signal) };
    if result == 0 {
        return Ok(());
    }
    Err(VesselError::Runtime(format!(
        "failed to signal process {pid}: {}",
        std::io::Error::last_os_error()
    )))
}

fn wait_for_exit(pid: u32, timeout: Duration) -> Result<(), VesselError> {
    let proc_path = PathBuf::from(format!("/proc/{pid}"));
    let stat_path = proc_path.join("stat");
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if !proc_path.exists() {
            return Ok(());
        }

        // Also consider the process exited if it has become a zombie
        if let Ok(stat) = fs::read_to_string(&stat_path) {
            if let Some(rparen) = stat.rfind(')') {
                let after_paren = &stat[rparen + 1..];
                let parts: Vec<&str> = after_paren.split_whitespace().collect();
                if parts.first() == Some(&"Z") || parts.first() == Some(&"X") {
                    return Ok(());
                }
            }
        }

        thread::sleep(Duration::from_millis(50));
    }

    Err(VesselError::Runtime(format!("process {pid} did not exit before timeout")))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, path::Path, process::Command};

    use tempfile::tempdir;
    use vessel_core::{ContainerStore, ImageRef, VesselPaths};
    use vessel_image::{ImageRuntimeConfig, PulledImage};

    use crate::{LinuxRuntime, Runtime};

    #[test]
    fn reports_capabilities() {
        let temp = tempdir().expect("tempdir");
        let runtime = LinuxRuntime::new(VesselPaths {
            state_dir: temp.path().join("state"),
            data_dir: temp.path().join("data"),
            blobs_dir: temp.path().join("data/blobs"),
            rootfs_dir: temp.path().join("data/rootfs"),
            bundles_dir: temp.path().join("data/bundles"),
        });
        let report = runtime.capability_report();
        assert_eq!(report.platform, "linux");
    }

    #[test]
    fn run_and_stop_work_when_host_tools_are_available() {
        if super::find_in_path("unshare").is_none() || super::find_in_path("chroot").is_none() {
            return;
        }

        let temp = tempdir().expect("tempdir");
        let paths = VesselPaths {
            state_dir: temp.path().join("state"),
            data_dir: temp.path().join("data"),
            blobs_dir: temp.path().join("data/blobs"),
            rootfs_dir: temp.path().join("data/rootfs"),
            bundles_dir: temp.path().join("data/bundles"),
        };
        paths.ensure().expect("paths");
        let rootfs = temp.path().join("rootfs");
        create_shell_rootfs(&rootfs);
        let store = ContainerStore::new(paths.state_dir.clone());
        let runtime = LinuxRuntime::new(paths);
        let image = PulledImage {
            image: "docker.io/library/test:latest".parse::<ImageRef>().expect("image"),
            manifest_digest: "sha256:test".to_string(),
            config_digest: "sha256:test".to_string(),
            layers: vec![rootfs.clone()],
            runtime: ImageRuntimeConfig {
                entrypoint: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "while :; do sleep 1; done".to_string(),
                ],
                cmd: Vec::new(),
                env: BTreeMap::new(),
                working_dir: Some("/".to_string()),
            },
        };

        let record = vessel_core::ContainerRecord::new(
            vessel_core::ContainerId::generate(),
            image.image.clone(),
            image.resolved_command(None).expect("resolved command"),
            image.runtime.working_dir.clone(),
            image.runtime.env.clone(),
            BTreeMap::new(),
            BTreeMap::new(),
            image.layers.clone(),
        );
        store.save(&record).expect("save record");
        let outcome = runtime.spawn_from_record(&store, record, true).expect("run detached");
        assert!(matches!(outcome.record.status, vessel_core::ContainerStatus::Running));
        let stopped = runtime.stop(&store, outcome.record.id()).expect("stop");
        assert!(matches!(stopped.status, vessel_core::ContainerStatus::Stopped));
    }

    fn create_shell_rootfs(rootfs: &Path) {
        fs::create_dir_all(rootfs.join("bin")).expect("bin dir");
        fs::create_dir_all(rootfs.join("lib")).expect("lib dir");
        fs::create_dir_all(rootfs.join("lib64")).expect("lib64 dir");
        copy_binary_with_shared_objects(Path::new("/bin/sh"), rootfs);
    }

    fn copy_binary_with_shared_objects(binary: &Path, rootfs: &Path) {
        let destination = rootfs.join(binary.strip_prefix("/").expect("absolute path"));
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::copy(binary, &destination).expect("copy binary");

        let output = Command::new("ldd").arg(binary).output().expect("ldd output");
        if !output.status.success() {
            return;
        }

        let stdout = String::from_utf8(output.stdout).expect("utf8");
        for path in stdout.lines().filter_map(extract_ldd_path) {
            let source = Path::new(&path);
            let target = rootfs.join(source.strip_prefix("/").expect("strip prefix"));
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).expect("mkdir");
            }
            fs::copy(source, target).expect("copy shared object");
        }
    }

    fn extract_ldd_path(line: &str) -> Option<String> {
        if let Some((_, path)) = line.split_once(" => ") {
            return path.split_whitespace().next().map(ToOwned::to_owned);
        }

        line.split_whitespace().next().filter(|value| value.starts_with('/')).map(ToOwned::to_owned)
    }
}
