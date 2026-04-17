use std::{
    ffi::OsString,
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
        })
    }

    fn spawn_from_record(
        &self,
        store: &ContainerStore,
        mut record: ContainerRecord,
        detached: bool,
    ) -> Result<RunOutcome, VesselError> {
        let tools = self.required_tools()?;
        let workdir = record.workdir.clone().unwrap_or_else(|| "/".to_string());
        let host_workdir = host_workdir(&record.rootfs, &workdir)?;
        fs::create_dir_all(&host_workdir)
            .map_err(|source| VesselError::io(&host_workdir, source))?;

        let bundle_dir = self.paths.bundles_dir.join(record.id().as_str());
        fs::create_dir_all(&bundle_dir).map_err(|source| VesselError::io(&bundle_dir, source))?;
        let log_path = bundle_dir.join("stdio.log");

        let mut command = Command::new(&tools.unshare);
        command
            .arg("--user")
            .arg("--map-root-user")
            .arg("--mount")
            .arg("--pid")
            .arg("--fork")
            .arg("--kill-child")
            .arg("--mount-proc")
            .arg(&tools.chroot)
            .arg(&record.rootfs)
            .args(&record.command)
            .current_dir(&host_workdir)
            .env_clear();

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
    ) -> Result<RunOutcome, VesselError> {
        let image_store = ImageStore::new(self.paths.clone());
        let image = image_store.pull(image)?;
        let record = ContainerRecord::new(
            ContainerId::generate(),
            image.image.clone(),
            image.resolved_command(command_override.as_deref())?,
            image.runtime.working_dir.clone(),
            image.runtime.env.clone(),
            image.rootfs.clone(),
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
        send_signal(pid, libc::SIGTERM)?;
        wait_for_exit(pid, Duration::from_secs(2))?;
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
}

#[derive(Debug, Clone)]
struct ToolPaths {
    chroot: PathBuf,
    unshare: PathBuf,
}

fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|candidate| {
        let path = candidate.join(binary);
        path.is_file().then_some(path)
    })
}

fn host_workdir(rootfs: &Path, workdir: &str) -> Result<PathBuf, VesselError> {
    let relative = workdir.trim_start_matches('/');
    let path = rootfs.join(relative);
    if !path.starts_with(rootfs) {
        return Err(VesselError::Runtime(format!("workdir `{workdir}` escapes rootfs")));
    }
    Ok(path)
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
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if !proc_path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }

    Err(VesselError::Runtime(format!("process {pid} did not exit before timeout")))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, process::Command};

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
            rootfs: rootfs.clone(),
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
            image.rootfs.clone(),
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
