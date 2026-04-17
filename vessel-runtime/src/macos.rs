use std::{
    collections::BTreeMap,
    ffi::{CString, c_char, c_int, c_uint},
    fs::{self, OpenOptions},
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use libloading::Library;
use vessel_core::{
    CapabilityReport, ContainerId, ContainerRecord, ContainerStatus, ContainerStore, ImageRef,
    VesselError, VesselPaths, now_timestamp,
};
use vessel_image::ImageStore;

use crate::{RunOutcome, Runtime};

const DEFAULT_LIBKRUN_PATHS: &[&str] = &[
    "/opt/homebrew/opt/libkrun/lib/libkrun.1.dylib",
    "/opt/homebrew/opt/libkrun/lib/libkrun.dylib",
    "/usr/local/opt/libkrun/lib/libkrun.1.dylib",
    "/usr/local/opt/libkrun/lib/libkrun.dylib",
    "/opt/homebrew/lib/libkrun.1.dylib",
    "/opt/homebrew/lib/libkrun.dylib",
    "/usr/local/lib/libkrun.1.dylib",
    "/usr/local/lib/libkrun.dylib",
    "/Applications/Docker.app/Contents/Resources/linuxkit/libkrun.dylib",
    "/Applications/Docker.app/Contents/Resources/linuxkit/docker-next/libkrun.dylib",
];

#[derive(Debug, Clone)]
pub struct MacOsRuntime {
    paths: VesselPaths,
    libkrun_path: Option<PathBuf>,
}

impl MacOsRuntime {
    pub fn new(paths: VesselPaths) -> Self {
        Self { paths, libkrun_path: discover_libkrun_path() }
    }

    fn ensure_libkrun(&self) -> Result<PathBuf, VesselError> {
        self.libkrun_path.clone().ok_or_else(|| {
            VesselError::Capability(
                "libkrun was not found; set VESSEL_LIBKRUN_PATH or install a local libkrun provider"
                    .to_string(),
            )
        })
    }

    fn helper_command(
        &self,
        record: &ContainerRecord,
        detached: bool,
    ) -> Result<Command, VesselError> {
        let current_exe = std::env::current_exe().map_err(VesselError::GenericIo)?;
        let libkrun_path = self.ensure_libkrun()?;
        let library_search_path = dynamic_library_search_path(&libkrun_path)?;
        let mut command = Command::new(current_exe);
        command
            .env("VESSEL_INTERNAL_MACOS_RUN", "1")
            .env("VESSEL_CONTAINER_ID", record.id().as_str())
            .env("VESSEL_HELPER_DETACHED", if detached { "1" } else { "0" })
            .env("VESSEL_LIBKRUN_PATH", &libkrun_path)
            .env("VESSEL_STATE_DIR", &self.paths.state_dir)
            .env("VESSEL_DATA_DIR", &self.paths.data_dir)
            .env("DYLD_LIBRARY_PATH", &library_search_path)
            .env("DYLD_FALLBACK_LIBRARY_PATH", &library_search_path);

        if let Some(kernel_path) = discover_kernel_path() {
            command.env("VESSEL_LIBKRUN_KERNEL_PATH", kernel_path);
        }

        let bundle_dir = self.paths.bundles_dir.join(record.id().as_str());
        fs::create_dir_all(&bundle_dir).map_err(|source| VesselError::io(&bundle_dir, source))?;
        let stdio_log = bundle_dir.join("stdio.log");

        if detached {
            let log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&stdio_log)
                .map_err(|source| VesselError::io(&stdio_log, source))?;
            let log_clone = log.try_clone().map_err(VesselError::GenericIo)?;
            command.stdin(Stdio::null()).stdout(Stdio::from(log)).stderr(Stdio::from(log_clone));
        } else {
            command.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
        }

        Ok(command)
    }

    fn spawn_record(
        &self,
        store: &ContainerStore,
        mut record: ContainerRecord,
        detached: bool,
    ) -> Result<RunOutcome, VesselError> {
        let mut command = self.helper_command(&record, detached)?;
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

impl Runtime for MacOsRuntime {
    fn capability_report(&self) -> CapabilityReport {
        let mut missing = Vec::new();
        if self.libkrun_path.is_none() {
            missing.push(
                "libkrun was not found; install or point Vessel at a local libkrun dylib"
                    .to_string(),
            );
        }

        CapabilityReport {
            platform: "macos".to_string(),
            supported: missing.is_empty(),
            rootless: true,
            overlayfs: false,
            cgroup_v2: false,
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
    ) -> Result<RunOutcome, VesselError> {
        self.ensure_libkrun()?;
        let image_store = ImageStore::new(self.paths.clone());
        let pulled = image_store.pull(image)?;

        let mut environment = pulled.runtime.env.clone();
        if let Some(overrides) = env_override {
            for (key, value) in overrides {
                environment.insert(key, value);
            }
        }

        let record = ContainerRecord::new(
            ContainerId::generate(),
            pulled.image.clone(),
            pulled.resolved_command(command_override.as_deref())?,
            pulled.runtime.working_dir.clone(),
            environment,
            mount_override.unwrap_or_default(),
            pulled.rootfs.clone(),
        );
        store.save(&record)?;
        self.spawn_record(store, record, detach)
    }

    fn start(
        &self,
        store: &ContainerStore,
        id: &ContainerId,
    ) -> Result<ContainerRecord, VesselError> {
        self.ensure_libkrun()?;
        let record = store.load(id)?;
        if matches!(record.status, ContainerStatus::Running) && record.pid.is_some() {
            return Err(VesselError::ContainerAlreadyRunning(id.to_string()));
        }
        let outcome = self.spawn_record(store, record, true)?;
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
        wait_for_exit(pid, Duration::from_secs(5))?;
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
        wait_for_exit(pid, Duration::from_secs(5))?;
        store.update_status(id, ContainerStatus::Exited { code: 137 }, None, Some(now_timestamp()))
    }

    fn remove(
        &self,
        store: &ContainerStore,
        id: &ContainerId,
    ) -> Result<(), VesselError> {
        let record = store.load(id)?;
        if matches!(record.status, ContainerStatus::Running) {
            return Err(VesselError::ContainerAlreadyRunning(id.to_string()));
        }

        let bundle_dir = self.paths.bundles_dir.join(id.as_str());
        if bundle_dir.exists() {
            fs::remove_dir_all(&bundle_dir).map_err(|source| VesselError::io(&bundle_dir, source))?;
        }

        store.remove(id)
    }

    fn logs(
        &self,
        store: &ContainerStore,
        id: &ContainerId,
    ) -> Result<(), VesselError> {
        let _record = store.load(id)?;
        let log_path = self.paths.bundles_dir.join(id.as_str()).join("stdio.log");
        if !log_path.exists() {
            return Ok(());
        }

        let content = fs::read_to_string(&log_path).map_err(|source| VesselError::io(&log_path, source))?;
        print!("{content}");
        Ok(())
    }
}

pub fn run_macos_helper(paths: VesselPaths, id: &ContainerId) -> Result<i32, VesselError> {
    let store = ContainerStore::new(paths.state_dir.clone());
    let record = store.load(id)?;
    let bundle_dir = paths.bundles_dir.join(id.as_str());
    fs::create_dir_all(&bundle_dir).map_err(|source| VesselError::io(&bundle_dir, source))?;
    let krun_log = bundle_dir.join("krun.log");

    let lib_path = discover_libkrun_path().ok_or_else(|| {
        VesselError::Capability("libkrun was not found for the macOS helper process".to_string())
    })?;
    let kernel_path = discover_kernel_path();
    let api = KrunApi::load(&lib_path)?;
    api.init_log()?;
    raise_nofile_limit()?;
    let ctx = api.create_ctx()?;
    let _ctx_guard = KrunContextGuard { api: &api, ctx };

    api.set_vm_config(ctx, 4, 4096)?;
    api.set_root(ctx, &record.rootfs)?;
    if let Some(kernel_path) = kernel_path.as_deref() {
        api.set_kernel(
            ctx,
            kernel_path,
            "reboot=k panic=-1 panic_print=0 nomodule console=hvc0 rootfstype=virtiofs rw quiet no-kvmapf",
        )?;
    }
    if let Some(workdir) = record.workdir.as_deref() {
        api.set_workdir(ctx, workdir)?;
    }
    for (index, (host_path, _guest_path)) in record.mounts.iter().enumerate() {
        let tag = format!("mount{index}");
        api.add_virtiofs(ctx, &tag, Path::new(host_path))?;
    }
    api.set_exec(
        ctx,
        command_exec_path(&record.command)?,
        &command_argv(&record.command),
        &record.environment,
    )?;
    api.split_irqchip(ctx, false)?;
    let detached = std::env::var("VESSEL_HELPER_DETACHED").ok().is_some_and(|value| value == "1");
    if detached {
        api.set_console_output(ctx, &krun_log)?;
    }
    api.start_enter(ctx)
}

struct KrunContextGuard<'a> {
    api: &'a KrunApi,
    ctx: u32,
}

impl Drop for KrunContextGuard<'_> {
    fn drop(&mut self) {
        let _ = self.api.free_ctx(self.ctx);
    }
}

struct KrunApi {
    _library: Library,
    create_ctx: unsafe extern "C" fn() -> c_int,
    free_ctx: unsafe extern "C" fn(c_uint) -> c_int,
    init_log: unsafe extern "C" fn(c_int, c_uint, c_uint, c_uint) -> c_int,
    set_vm_config: unsafe extern "C" fn(c_uint, u8, c_uint) -> c_int,
    set_root: unsafe extern "C" fn(c_uint, *const c_char) -> c_int,
    set_kernel:
        unsafe extern "C" fn(c_uint, *const c_char, c_uint, *const c_char, *const c_char) -> c_int,
    split_irqchip: unsafe extern "C" fn(c_uint, bool) -> c_int,
    set_workdir: unsafe extern "C" fn(c_uint, *const c_char) -> c_int,
    set_exec: unsafe extern "C" fn(
        c_uint,
        *const c_char,
        *const *const c_char,
        *const *const c_char,
    ) -> c_int,
    add_virtiofs: unsafe extern "C" fn(c_uint, *const c_char, *const c_char) -> c_int,
    set_console_output: unsafe extern "C" fn(c_uint, *const c_char) -> c_int,
    start_enter: unsafe extern "C" fn(c_uint) -> c_int,
}

impl KrunApi {
    fn load(path: &Path) -> Result<Self, VesselError> {
        unsafe {
            let library = Library::new(path).map_err(|error| {
                VesselError::Runtime(format!(
                    "failed to load libkrun at {}: {error}",
                    path.display()
                ))
            })?;
            macro_rules! load_symbol {
                ($name:literal, $ty:ty) => {
                    *library.get::<$ty>($name).map_err(|error| {
                        VesselError::Runtime(format!(
                            "failed to load {} from libkrun: {error}",
                            String::from_utf8_lossy($name)
                        ))
                    })?
                };
            }

            Ok(Self {
                create_ctx: load_symbol!(b"krun_create_ctx\0", unsafe extern "C" fn() -> c_int),
                free_ctx: load_symbol!(b"krun_free_ctx\0", unsafe extern "C" fn(c_uint) -> c_int),
                init_log: load_symbol!(
                    b"krun_init_log\0",
                    unsafe extern "C" fn(c_int, c_uint, c_uint, c_uint) -> c_int
                ),
                set_vm_config: load_symbol!(
                    b"krun_set_vm_config\0",
                    unsafe extern "C" fn(c_uint, u8, c_uint) -> c_int
                ),
                set_root: load_symbol!(
                    b"krun_set_root\0",
                    unsafe extern "C" fn(c_uint, *const c_char) -> c_int
                ),
                set_kernel: load_symbol!(
                    b"krun_set_kernel\0",
                    unsafe extern "C" fn(
                        c_uint,
                        *const c_char,
                        c_uint,
                        *const c_char,
                        *const c_char,
                    ) -> c_int
                ),
                split_irqchip: load_symbol!(
                    b"krun_split_irqchip\0",
                    unsafe extern "C" fn(c_uint, bool) -> c_int
                ),
                set_workdir: load_symbol!(
                    b"krun_set_workdir\0",
                    unsafe extern "C" fn(c_uint, *const c_char) -> c_int
                ),
                set_exec: load_symbol!(
                    b"krun_set_exec\0",
                    unsafe extern "C" fn(
                        c_uint,
                        *const c_char,
                        *const *const c_char,
                        *const *const c_char,
                    ) -> c_int
                ),
                add_virtiofs: load_symbol!(
                    b"krun_add_virtiofs\0",
                    unsafe extern "C" fn(c_uint, *const c_char, *const c_char) -> c_int
                ),
                set_console_output: load_symbol!(
                    b"krun_set_console_output\0",
                    unsafe extern "C" fn(c_uint, *const c_char) -> c_int
                ),
                start_enter: load_symbol!(
                    b"krun_start_enter\0",
                    unsafe extern "C" fn(c_uint) -> c_int
                ),
                _library: library,
            })
        }
    }

    fn init_log(&self) -> Result<(), VesselError> {
        self.call(unsafe { (self.init_log)(2, 1, 2, 0) }, "krun_init_log")
    }

    fn create_ctx(&self) -> Result<u32, VesselError> {
        let value = unsafe { (self.create_ctx)() };
        if value < 0 {
            return Err(krun_errno("krun_create_ctx", value));
        }
        Ok(value as u32)
    }

    fn free_ctx(&self, ctx: u32) -> Result<(), VesselError> {
        self.call(unsafe { (self.free_ctx)(ctx) }, "krun_free_ctx")
    }

    fn set_vm_config(&self, ctx: u32, vcpus: u8, ram_mib: u32) -> Result<(), VesselError> {
        self.call(unsafe { (self.set_vm_config)(ctx, vcpus, ram_mib) }, "krun_set_vm_config")
    }

    fn set_root(&self, ctx: u32, root: &Path) -> Result<(), VesselError> {
        let root = CString::new(root.to_string_lossy().as_bytes())
            .map_err(|_| VesselError::Runtime("rootfs path contained a NUL byte".to_string()))?;
        self.call(unsafe { (self.set_root)(ctx, root.as_ptr()) }, "krun_set_root")
    }

    fn set_kernel(&self, ctx: u32, kernel: &Path, cmdline: &str) -> Result<(), VesselError> {
        let kernel = CString::new(kernel.to_string_lossy().as_bytes())
            .map_err(|_| VesselError::Runtime("kernel path contained a NUL byte".to_string()))?;
        let cmdline = CString::new(cmdline)
            .map_err(|_| VesselError::Runtime("kernel cmdline contained a NUL byte".to_string()))?;
        self.call(
            unsafe {
                (self.set_kernel)(ctx, kernel.as_ptr(), 0, std::ptr::null(), cmdline.as_ptr())
            },
            "krun_set_kernel",
        )
    }

    fn set_workdir(&self, ctx: u32, workdir: &str) -> Result<(), VesselError> {
        let workdir = CString::new(workdir)
            .map_err(|_| VesselError::Runtime("workdir contained a NUL byte".to_string()))?;
        self.call(unsafe { (self.set_workdir)(ctx, workdir.as_ptr()) }, "krun_set_workdir")
    }

    fn split_irqchip(&self, ctx: u32, enabled: bool) -> Result<(), VesselError> {
        self.call(unsafe { (self.split_irqchip)(ctx, enabled) }, "krun_split_irqchip")
    }

    fn set_exec(
        &self,
        ctx: u32,
        exec_path: String,
        argv: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<(), VesselError> {
        let exec_path = CString::new(exec_path)
            .map_err(|_| VesselError::Runtime("exec path contained a NUL byte".to_string()))?;
        let argv = CStringArray::new(argv.iter().cloned())?;
        let envp = CStringArray::new(env.iter().map(|(key, value)| format!("{key}={value}")))?;
        self.call(
            unsafe { (self.set_exec)(ctx, exec_path.as_ptr(), argv.as_ptr(), envp.as_ptr()) },
            "krun_set_exec",
        )
    }

    fn add_virtiofs(&self, ctx: u32, tag: &str, host_path: &Path) -> Result<(), VesselError> {
        let tag = CString::new(tag)
            .map_err(|_| VesselError::Runtime("virtiofs tag contained a NUL byte".to_string()))?;
        let host_path = CString::new(host_path.to_string_lossy().as_bytes())
            .map_err(|_| VesselError::Runtime("host path contained a NUL byte".to_string()))?;
        self.call(
            unsafe { (self.add_virtiofs)(ctx, tag.as_ptr(), host_path.as_ptr()) },
            "krun_add_virtiofs",
        )
    }

    fn set_console_output(&self, ctx: u32, path: &Path) -> Result<(), VesselError> {
        let path = CString::new(path.to_string_lossy().as_bytes()).map_err(|_| {
            VesselError::Runtime("console log path contained a NUL byte".to_string())
        })?;
        self.call(
            unsafe { (self.set_console_output)(ctx, path.as_ptr()) },
            "krun_set_console_output",
        )
    }

    fn start_enter(&self, ctx: u32) -> Result<i32, VesselError> {
        let value = unsafe { (self.start_enter)(ctx) };
        if value < 0 {
            return Err(krun_errno("krun_start_enter", value));
        }
        Ok(value)
    }

    fn call(&self, value: i32, operation: &str) -> Result<(), VesselError> {
        if value < 0 { Err(krun_errno(operation, value)) } else { Ok(()) }
    }
}

struct CStringArray {
    _values: Vec<CString>,
    pointers: Vec<*const c_char>,
}

impl CStringArray {
    fn new<I>(values: I) -> Result<Self, VesselError>
    where
        I: IntoIterator<Item = String>,
    {
        let values = values
            .into_iter()
            .map(|value| {
                CString::new(value)
                    .map_err(|_| VesselError::Runtime("string contained a NUL byte".to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut pointers = values.iter().map(|value| value.as_ptr()).collect::<Vec<_>>();
        pointers.push(std::ptr::null());
        Ok(Self { _values: values, pointers })
    }

    fn as_ptr(&self) -> *const *const c_char {
        self.pointers.as_ptr()
    }
}

fn command_exec_path(command: &[String]) -> Result<String, VesselError> {
    command
        .first()
        .cloned()
        .ok_or_else(|| VesselError::Runtime("empty command for macOS runtime".to_string()))
}

fn command_argv(command: &[String]) -> Vec<String> {
    command.iter().skip(1).cloned().collect()
}

#[cfg(test)]
fn normalize_guest_path(path: Option<&str>) -> Option<String> {
    path.and_then(|path| {
        let normalized = path.trim_matches('/');
        if normalized.is_empty() { None } else { Some(normalized.to_string()) }
    })
}

fn discover_libkrun_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("VESSEL_LIBKRUN_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }

    DEFAULT_LIBKRUN_PATHS.iter().map(PathBuf::from).find(|path| path.is_file())
}

fn discover_kernel_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("VESSEL_LIBKRUN_KERNEL_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }

    discover_libkrun_path().and_then(|path| {
        let candidate = path.parent()?.join("kernel");
        candidate.is_file().then_some(candidate)
    })
}

fn dynamic_library_search_path(libkrun_path: &Path) -> Result<String, VesselError> {
    let library_dir = libkrun_path.parent().ok_or_else(|| {
        VesselError::Runtime("libkrun path did not have a parent directory".to_string())
    })?;

    let mut candidates = vec![library_dir.to_path_buf()];
    if let Some(homebrew_prefix) = infer_homebrew_prefix(libkrun_path) {
        candidates.push(homebrew_prefix.join("lib"));
        candidates.push(homebrew_prefix.join("opt/libkrun/lib"));
        candidates.push(homebrew_prefix.join("opt/libkrunfw/lib"));
        candidates.push(homebrew_prefix.join("opt/libepoxy/lib"));
        candidates.push(homebrew_prefix.join("opt/virglrenderer/lib"));
    }

    let mut paths = Vec::new();
    for candidate in candidates {
        if candidate.is_dir() && !paths.contains(&candidate) {
            paths.push(candidate);
        }
    }

    if paths.is_empty() {
        return Err(VesselError::Runtime(
            "no dynamic library search paths were available for libkrun".to_string(),
        ));
    }

    let joined =
        paths.iter().map(|path| path.to_string_lossy().into_owned()).collect::<Vec<_>>().join(":");
    Ok(joined)
}

fn infer_homebrew_prefix(libkrun_path: &Path) -> Option<PathBuf> {
    let mut current = libkrun_path.parent()?;
    while let Some(parent) = current.parent() {
        if current.file_name().is_some_and(|name| name == "opt") {
            return Some(parent.to_path_buf());
        }
        current = parent;
    }
    None
}

fn exit_code_for_status(status: ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| status.signal().map(|signal| 128 + signal).unwrap_or(125))
}

fn map_spawn_error(error: std::io::Error) -> VesselError {
    match error.kind() {
        std::io::ErrorKind::NotFound => {
            VesselError::Runtime("failed to spawn macOS helper process".to_string())
        }
        std::io::ErrorKind::PermissionDenied => {
            VesselError::Runtime("permission denied while launching macOS helper".to_string())
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
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if !pid_exists(pid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }

    Err(VesselError::Runtime(format!("process {pid} did not exit before timeout")))
}

pub(crate) fn pid_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn krun_errno(operation: &str, value: i32) -> VesselError {
    let errno = (-value).max(1);
    let io = std::io::Error::from_raw_os_error(errno);
    VesselError::Runtime(format!("{operation} failed: {io}"))
}

fn raise_nofile_limit() -> Result<(), VesselError> {
    let mut rlimit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    let get_result = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlimit) };
    if get_result != 0 {
        return Err(VesselError::Runtime(format!(
            "getrlimit(RLIMIT_NOFILE) failed: {}",
            std::io::Error::last_os_error()
        )));
    }

    rlimit.rlim_cur = rlimit.rlim_max;
    let set_result = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rlimit) };
    if set_result != 0 {
        return Err(VesselError::Runtime(format!(
            "setrlimit(RLIMIT_NOFILE) failed: {}",
            std::io::Error::last_os_error()
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;
    use vessel_core::VesselPaths;

    use super::{
        MacOsRuntime, command_argv, command_exec_path, dynamic_library_search_path,
        infer_homebrew_prefix, normalize_guest_path, pid_exists,
    };
    use crate::Runtime;

    #[test]
    fn normalizes_guest_paths() {
        assert_eq!(normalize_guest_path(Some("/")), None);
        assert_eq!(
            normalize_guest_path(Some("/workspace/data")),
            Some("workspace/data".to_string())
        );
    }

    #[test]
    fn extracts_exec_path_and_argv() {
        let command = vec!["/bin/sh".to_string(), "-c".to_string(), "echo hi".to_string()];
        assert_eq!(command_exec_path(&command).expect("exec"), "/bin/sh");
        assert_eq!(command_argv(&command), vec!["-c".to_string(), "echo hi".to_string()]);
    }

    #[test]
    fn reports_missing_libkrun() {
        let temp = tempdir().expect("tempdir");
        let runtime = MacOsRuntime {
            paths: VesselPaths {
                state_dir: temp.path().join("state"),
                data_dir: temp.path().join("data"),
                blobs_dir: temp.path().join("data/blobs"),
                rootfs_dir: temp.path().join("data/rootfs"),
                bundles_dir: temp.path().join("data/bundles"),
            },
            libkrun_path: None,
        };
        let report = runtime.capability_report();
        assert!(!report.supported);
    }

    #[test]
    fn does_not_require_kernel_for_capability_report() {
        let temp = tempdir().expect("tempdir");
        let dylib = temp.path().join("libkrun.1.dylib");
        std::fs::write(&dylib, b"").expect("write dylib placeholder");
        let runtime = MacOsRuntime {
            paths: VesselPaths {
                state_dir: temp.path().join("state"),
                data_dir: temp.path().join("data"),
                blobs_dir: temp.path().join("data/blobs"),
                rootfs_dir: temp.path().join("data/rootfs"),
                bundles_dir: temp.path().join("data/bundles"),
            },
            libkrun_path: Some(dylib),
        };
        let report = runtime.capability_report();
        assert!(report.supported);
    }

    #[test]
    fn pid_probe_handles_missing_processes() {
        assert!(!pid_exists(999_999));
    }

    #[test]
    fn infers_homebrew_prefix_from_libkrun_path() {
        let prefix =
            infer_homebrew_prefix(Path::new("/opt/homebrew/opt/libkrun/lib/libkrun.1.dylib"))
                .expect("homebrew prefix");
        assert_eq!(prefix, PathBuf::from("/opt/homebrew"));
    }

    #[test]
    fn builds_library_search_path_from_homebrew_layout() {
        let temp = tempdir().expect("tempdir");
        let prefix = temp.path();
        let libkrun_dir = prefix.join("opt/libkrun/lib");
        let libkrunfw_dir = prefix.join("opt/libkrunfw/lib");
        let libepoxy_dir = prefix.join("opt/libepoxy/lib");
        let virgl_dir = prefix.join("opt/virglrenderer/lib");
        let shared_lib_dir = prefix.join("lib");
        std::fs::create_dir_all(&libkrun_dir).expect("libkrun dir");
        std::fs::create_dir_all(&libkrunfw_dir).expect("libkrunfw dir");
        std::fs::create_dir_all(&libepoxy_dir).expect("libepoxy dir");
        std::fs::create_dir_all(&virgl_dir).expect("virgl dir");
        std::fs::create_dir_all(&shared_lib_dir).expect("shared lib dir");

        let libkrun_path = libkrun_dir.join("libkrun.1.dylib");
        std::fs::write(&libkrun_path, b"").expect("placeholder dylib");

        let search_path = dynamic_library_search_path(&libkrun_path).expect("search path");
        assert!(search_path.contains(libkrun_dir.to_string_lossy().as_ref()));
        assert!(search_path.contains(libkrunfw_dir.to_string_lossy().as_ref()));
        assert!(search_path.contains(libepoxy_dir.to_string_lossy().as_ref()));
        assert!(search_path.contains(virgl_dir.to_string_lossy().as_ref()));
        assert!(search_path.contains(shared_lib_dir.to_string_lossy().as_ref()));
    }
}
