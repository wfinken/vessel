mod compose;

use std::collections::BTreeMap;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process;
use std::process::{Command as ProcessCommand, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use bpaf::{OptionParser, Parser, construct, long, positional, pure};
use compose::{
    ComposeProject, ComposeProjectState, ManagedServiceState, delete_state, discover_compose_file,
    load_project, load_state, save_state, services_in_dependency_order, services_in_shutdown_order,
};
use vessel_core::{
    ContainerId, ContainerRecord, ContainerStatus, ImageRef, OutputFormat, VesselError, VesselPaths,
};
use vessel_daemon::{
    Backend, LocalBackend, RemoteBackend, log_path as daemon_log_path, pid_path as daemon_pid_path,
    socket_path,
};
use vessel_image::GarbageCollectionSummary;

enum Command {
    Run(RunArgs),
    Start(IdArgs),
    Stop(IdArgs),
    Kill(IdArgs),
    Rm(IdArgs),
    Logs(IdArgs),
    Ps(PsArgs),
    Rmi(RmiArgs),
    Gc(GcArgs),
    Compose(ComposeArgs),
    Daemon(DaemonArgs),
}

#[derive(Debug, Clone)]
struct RunArgs {
    detach: bool,
    env: Vec<String>,
    mounts: Vec<String>,
    image: String,
    command: Vec<String>,
}

#[derive(Debug, Clone)]
struct IdArgs {
    id: ContainerId,
}

#[derive(Debug, Clone)]
struct PsArgs {
    format: OutputFormat,
}

#[derive(Debug, Clone)]
struct RmiArgs {
    image: String,
}

#[derive(Debug, Clone)]
struct GcArgs {
    format: OutputFormat,
}

#[derive(Debug, Clone)]
struct ComposeArgs {
    file: Option<String>,
    project_name: Option<String>,
    command: ComposeSubcommand,
}

#[derive(Debug, Clone)]
enum ComposeSubcommand {
    Up,
    Down,
    Ps,
    Logs(ComposeLogsArgs),
}

#[derive(Debug, Clone)]
struct ComposeLogsArgs {
    service: Option<String>,
}

struct GlobalArgs {
    remote: bool,
    command: Command,
}

#[derive(Clone)]
enum DaemonCommand {
    Start,
    Status,
    Stop,
}

#[derive(Clone)]
struct DaemonArgs {
    command: DaemonCommand,
}

fn main() {
    if let Some(exit_code) = maybe_run_internal_daemon() {
        process::exit(exit_code);
    }

    if let Some(exit_code) = maybe_run_internal_helper() {
        process::exit(exit_code);
    }

    let opt = cli().run();
    let exit_code = match run(opt) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            error.exit_code()
        }
    };
    process::exit(exit_code);
}

fn run(args: GlobalArgs) -> Result<i32, VesselError> {
    let paths = VesselPaths::discover()?;
    paths.ensure()?;

    let backend: Box<dyn Backend> = if args.remote {
        Box::new(RemoteBackend::new(socket_path(&paths))?)
    } else {
        Box::new(LocalBackend::new(paths.clone()))
    };

    match args.command {
        Command::Run(args) => {
            let image: ImageRef = args.image.parse()?;
            let mut env_override = BTreeMap::new();
            for entry in args.env {
                if let Some((key, value)) = entry.split_once('=') {
                    env_override.insert(key.to_string(), value.to_string());
                } else {
                    env_override.insert(entry, String::new());
                }
            }
            let env_override = (!env_override.is_empty()).then_some(env_override);

            let mut mount_override = BTreeMap::new();
            for entry in args.mounts {
                if let Some((host, guest)) = entry.split_once(':') {
                    let host_path =
                        fs::canonicalize(host).map_err(|source| VesselError::io(host, source))?;
                    mount_override
                        .insert(host_path.to_string_lossy().to_string(), guest.to_string());
                } else {
                    return Err(VesselError::Usage(format!(
                        "invalid volume mapping `{entry}`; expected host_path:guest_path"
                    )));
                }
            }
            let mount_override = (!mount_override.is_empty()).then_some(mount_override);

            let command_override = (!args.command.is_empty()).then_some(args.command);
            let outcome = backend.run_container(
                &image,
                args.detach,
                command_override,
                env_override,
                mount_override,
            )?;
            if args.detach {
                println!("{}", outcome.record.id());
                Ok(0)
            } else {
                Ok(outcome.exit_code.unwrap_or_default())
            }
        }
        Command::Start(args) => {
            let record = backend.start_container(&args.id)?;
            print_container_action(&record);
            Ok(0)
        }
        Command::Stop(args) => {
            let record = backend.stop_container(&args.id)?;
            print_container_action(&record);
            Ok(0)
        }
        Command::Kill(args) => {
            let record = backend.kill_container(&args.id)?;
            print_container_action(&record);
            Ok(0)
        }
        Command::Rm(args) => {
            backend.remove_container(&args.id)?;
            println!("{}", args.id);
            Ok(0)
        }
        Command::Logs(args) => {
            let logs = backend.get_container_logs(&args.id)?;
            print!("{logs}");
            Ok(0)
        }
        Command::Ps(args) => {
            let records = backend.list_containers()?;
            match args.format {
                OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&records)
                            .map_err(|error| VesselError::Serialization(error.to_string()))?
                    );
                }
                OutputFormat::Table => print_table(&records),
            }
            Ok(0)
        }
        Command::Rmi(args) => {
            let image: ImageRef = args.image.parse()?;
            backend.remove_image(&image)?;
            println!("Untagged: {}", image);
            Ok(0)
        }
        Command::Gc(args) => {
            let summary = backend.garbage_collect_images()?;
            match args.format {
                OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&summary)
                            .map_err(|error| VesselError::Serialization(error.to_string()))?
                    );
                }
                OutputFormat::Table => print_gc_summary(&summary),
            }
            Ok(0)
        }
        Command::Compose(args) => handle_compose(&paths, backend.as_ref(), args),
        Command::Daemon(args) => match args.command {
            DaemonCommand::Start => start_daemon(&paths),
            DaemonCommand::Status => daemon_status(&paths),
            DaemonCommand::Stop => stop_daemon(&paths),
        },
    }
}

fn handle_compose(
    paths: &VesselPaths,
    backend: &dyn Backend,
    args: ComposeArgs,
) -> Result<i32, VesselError> {
    let compose_file = resolve_compose_file(args.file.as_deref())?;
    let project = load_project(&compose_file, args.project_name.as_deref())?;

    match args.command {
        ComposeSubcommand::Up => compose_up(paths, backend, project),
        ComposeSubcommand::Down => compose_down(paths, backend, project),
        ComposeSubcommand::Ps => compose_ps(paths, backend, &project),
        ComposeSubcommand::Logs(args) => compose_logs(paths, backend, &project, args),
    }
}

fn resolve_compose_file(file: Option<&str>) -> Result<std::path::PathBuf, VesselError> {
    if let Some(file) = file {
        return fs::canonicalize(file).map_err(|source| VesselError::io(file, source));
    }

    let cwd = std::env::current_dir().map_err(VesselError::GenericIo)?;
    let discovered = discover_compose_file(&cwd)?;
    fs::canonicalize(&discovered).map_err(|source| VesselError::io(&discovered, source))
}

fn compose_up(
    paths: &VesselPaths,
    backend: &dyn Backend,
    project: ComposeProject,
) -> Result<i32, VesselError> {
    let mut state = load_state(paths, &project.name)?.unwrap_or(ComposeProjectState {
        project_name: project.name.clone(),
        source_file: project.file.clone(),
        services: BTreeMap::new(),
    });
    state.source_file = project.file.clone();

    let orphaned_services = state
        .services
        .keys()
        .filter(|service| !project.services.contains_key(*service))
        .cloned()
        .collect::<Vec<_>>();
    for service_name in orphaned_services {
        if let Some(entry) = state.services.remove(&service_name) {
            remove_managed_container(backend, &entry.container_id)?;
            println!("removed\t{}\t{}", service_name, entry.container_id);
        }
    }

    for (service_name, spec) in services_in_dependency_order(&project)? {
        let existing = state.services.get(&service_name).cloned();
        let current = match existing.as_ref() {
            Some(entry) => find_container(backend, &entry.container_id)?,
            None => None,
        };

        let action = match existing {
            Some(entry) if entry.spec == spec => {
                if current
                    .as_ref()
                    .is_some_and(|record| matches!(record.status, ContainerStatus::Running))
                {
                    "unchanged"
                } else if current.is_some() {
                    backend.start_container(&entry.container_id)?;
                    state.services.insert(service_name.clone(), entry);
                    "started"
                } else {
                    let container_id = run_compose_service(backend, &spec)?;
                    state.services.insert(
                        service_name.clone(),
                        ManagedServiceState { container_id: container_id.clone(), spec },
                    );
                    println!("created\t{}\t{}", service_name, container_id);
                    continue;
                }
            }
            Some(entry) => {
                remove_managed_container(backend, &entry.container_id)?;
                let container_id = run_compose_service(backend, &spec)?;
                state.services.insert(
                    service_name.clone(),
                    ManagedServiceState { container_id: container_id.clone(), spec },
                );
                println!("recreated\t{}\t{}", service_name, container_id);
                continue;
            }
            None => {
                let container_id = run_compose_service(backend, &spec)?;
                state.services.insert(
                    service_name.clone(),
                    ManagedServiceState { container_id: container_id.clone(), spec },
                );
                println!("created\t{}\t{}", service_name, container_id);
                continue;
            }
        };

        if let Some(entry) = state.services.get(&service_name) {
            println!("{}\t{}\t{}", action, service_name, entry.container_id);
        }
    }

    save_state(paths, &state)?;
    Ok(0)
}

fn compose_down(
    paths: &VesselPaths,
    backend: &dyn Backend,
    project: ComposeProject,
) -> Result<i32, VesselError> {
    let Some(mut state) = load_state(paths, &project.name)? else {
        println!("project `{}` is already down", project.name);
        return Ok(0);
    };

    let mut ordered = services_in_shutdown_order(&project)?
        .into_iter()
        .map(|(service, _)| service)
        .collect::<Vec<_>>();
    for service_name in state.services.keys() {
        if !ordered.contains(service_name) {
            ordered.push(service_name.clone());
        }
    }

    for service_name in ordered {
        if let Some(entry) = state.services.remove(&service_name) {
            remove_managed_container(backend, &entry.container_id)?;
            println!("removed\t{}\t{}", service_name, entry.container_id);
        }
    }

    delete_state(paths, &project.name)?;
    Ok(0)
}

fn compose_ps(
    paths: &VesselPaths,
    backend: &dyn Backend,
    project: &ComposeProject,
) -> Result<i32, VesselError> {
    let state = load_state(paths, &project.name)?;
    println!("{:<16} {:<14} {:<12} IMAGE", "SERVICE", "ID", "STATUS");

    for (service_name, spec) in &project.services {
        let (id, status) = match state.as_ref().and_then(|state| state.services.get(service_name)) {
            Some(entry) => match find_container(backend, &entry.container_id)? {
                Some(record) => (entry.container_id.to_string(), status_label(&record.status)),
                None => (entry.container_id.to_string(), "missing".to_string()),
            },
            None => ("-".to_string(), "not-created".to_string()),
        };

        println!("{:<16} {:<14} {:<12} {}", service_name, id, status, spec.image);
    }

    Ok(0)
}

fn compose_logs(
    paths: &VesselPaths,
    backend: &dyn Backend,
    project: &ComposeProject,
    args: ComposeLogsArgs,
) -> Result<i32, VesselError> {
    let state = load_state(paths, &project.name)?.ok_or_else(|| {
        VesselError::Usage(format!("project `{}` has not been started yet", project.name))
    })?;

    let services = if let Some(service) = args.service {
        let entry = state.services.get(&service).ok_or_else(|| {
            VesselError::Usage(format!(
                "service `{service}` is not running under project `{}`",
                project.name
            ))
        })?;
        vec![(service, entry.container_id.clone())]
    } else {
        state
            .services
            .iter()
            .map(|(service, entry)| (service.clone(), entry.container_id.clone()))
            .collect::<Vec<_>>()
    };

    for (index, (service_name, id)) in services.iter().enumerate() {
        if services.len() > 1 {
            if index > 0 {
                println!();
            }
            println!("==> {} <==", service_name);
        }
        let logs = backend.get_container_logs(id)?;
        print!("{logs}");
    }

    Ok(0)
}

fn run_compose_service(
    backend: &dyn Backend,
    spec: &compose::ComposeServiceSpec,
) -> Result<ContainerId, VesselError> {
    let image: ImageRef = spec.image.parse()?;
    let env_override = (!spec.environment.is_empty()).then_some(spec.environment.clone());
    let mount_override = (!spec.mounts.is_empty()).then_some(spec.mounts.clone());
    let outcome =
        backend.run_container(&image, true, spec.command.clone(), env_override, mount_override)?;
    Ok(outcome.record.id().clone())
}

fn remove_managed_container(backend: &dyn Backend, id: &ContainerId) -> Result<(), VesselError> {
    if let Some(record) = find_container(backend, id)? {
        if matches!(record.status, ContainerStatus::Running) {
            match backend.stop_container(id) {
                Ok(_) | Err(VesselError::ContainerNotRunning(_)) => {}
                Err(error) => return Err(error),
            }
        }
    }

    match backend.remove_container(id) {
        Ok(()) | Err(VesselError::ContainerNotFound(_)) => Ok(()),
        Err(error) => Err(error),
    }
}

fn find_container(
    backend: &dyn Backend,
    id: &ContainerId,
) -> Result<Option<ContainerRecord>, VesselError> {
    Ok(backend.list_containers()?.into_iter().find(|record| record.id() == id))
}

fn maybe_run_internal_daemon() -> Option<i32> {
    std::env::var_os("VESSEL_INTERNAL_DAEMON")?;

    let paths = match VesselPaths::discover() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("{error}");
            return Some(error.exit_code());
        }
    };

    if let Err(error) = paths.ensure() {
        eprintln!("{error}");
        return Some(error.exit_code());
    }

    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("{error}");
            return Some(125);
        }
    };

    Some(match runtime.block_on(vessel_daemon::run_daemon(paths.clone(), &socket_path(&paths))) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            125
        }
    })
}

fn maybe_run_internal_helper() -> Option<i32> {
    std::env::var_os("VESSEL_INTERNAL_MACOS_RUN")?;

    let paths = match VesselPaths::discover() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("{error}");
            return Some(error.exit_code());
        }
    };
    let id = match std::env::var("VESSEL_CONTAINER_ID")
        .ok()
        .and_then(|value| value.parse::<ContainerId>().ok())
    {
        Some(id) => id,
        None => {
            eprintln!("missing internal macOS helper container id");
            return Some(125);
        }
    };

    #[cfg(target_os = "macos")]
    {
        Some(match vessel_runtime::run_macos_helper(paths, &id) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("{error}");
                error.exit_code()
            }
        })
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = paths;
        let _ = id;
        return Some(125);
    }
}

fn start_daemon(paths: &VesselPaths) -> Result<i32, VesselError> {
    if let Some(pid) = running_daemon_pid(paths) {
        println!("vessel daemon already running (pid {}) on {}", pid, socket_path(paths).display());
        return Ok(0);
    }

    let current_exe = std::env::current_exe().map_err(VesselError::GenericIo)?;
    let log_path = daemon_log_path(paths);
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent).map_err(|source| VesselError::io(parent, source))?;
    }

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|source| VesselError::io(&log_path, source))?;
    let log_clone = log.try_clone().map_err(VesselError::GenericIo)?;

    let mut child = ProcessCommand::new(current_exe);
    child
        .env("VESSEL_INTERNAL_DAEMON", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_clone));
    unsafe {
        child.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = child.spawn().map_err(map_spawn_error)?;
    wait_for_daemon_ready(paths, &mut child, &log_path)?;

    let pid = read_pid_file(paths)?;
    println!("Started vessel daemon (pid {}) on {}", pid, socket_path(paths).display());
    Ok(0)
}

fn daemon_status(paths: &VesselPaths) -> Result<i32, VesselError> {
    match daemon_state(paths)? {
        DaemonState::Running(pid) => {
            println!("vessel daemon is running (pid {}) on {}", pid, socket_path(paths).display());
            Ok(0)
        }
        DaemonState::Stopped => {
            println!("vessel daemon is not running");
            Ok(0)
        }
    }
}

fn stop_daemon(paths: &VesselPaths) -> Result<i32, VesselError> {
    match daemon_state(paths)? {
        DaemonState::Running(pid) => {
            send_signal(pid, libc::SIGTERM)?;
            wait_for_pid_exit(pid, Duration::from_secs(5))?;
            cleanup_daemon_files(paths)?;
            println!("Stopped vessel daemon ({pid})");
            Ok(0)
        }
        DaemonState::Stopped => {
            cleanup_daemon_files(paths)?;
            println!("vessel daemon is not running");
            Ok(0)
        }
    }
}

fn wait_for_daemon_ready(
    paths: &VesselPaths,
    child: &mut std::process::Child,
    log_path: &Path,
) -> Result<(), VesselError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let socket = socket_path(paths);

    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().map_err(VesselError::GenericIo)? {
            return Err(VesselError::Runtime(format!(
                "daemon exited before becoming ready (status {}); see {}",
                describe_status(status),
                log_path.display()
            )));
        }

        if socket.exists() {
            let backend = RemoteBackend::new(socket.clone())?;
            if backend.list_containers().is_ok() {
                return Ok(());
            }
        }

        thread::sleep(Duration::from_millis(100));
    }

    Err(VesselError::Runtime(format!(
        "daemon did not become ready within 5s; see {}",
        log_path.display()
    )))
}

fn read_pid_file(paths: &VesselPaths) -> Result<i32, VesselError> {
    let pid_path = daemon_pid_path(paths);
    let payload =
        fs::read_to_string(&pid_path).map_err(|source| VesselError::io(&pid_path, source))?;
    payload
        .trim()
        .parse::<i32>()
        .map_err(|error| VesselError::Runtime(format!("invalid daemon pid file: {error}")))
}

fn running_daemon_pid(paths: &VesselPaths) -> Option<i32> {
    match daemon_state(paths) {
        Ok(DaemonState::Running(pid)) => Some(pid),
        _ => None,
    }
}

fn daemon_state(paths: &VesselPaths) -> Result<DaemonState, VesselError> {
    let pid_path = daemon_pid_path(paths);
    if !pid_path.exists() {
        return Ok(DaemonState::Stopped);
    }

    let pid = read_pid_file(paths)?;
    if pid_exists(pid) {
        return Ok(DaemonState::Running(pid));
    }

    cleanup_daemon_files(paths)?;
    Ok(DaemonState::Stopped)
}

fn cleanup_daemon_files(paths: &VesselPaths) -> Result<(), VesselError> {
    for path in [socket_path(paths), daemon_pid_path(paths)] {
        if path.exists() {
            fs::remove_file(&path).map_err(|source| VesselError::io(&path, source))?;
        }
    }
    Ok(())
}

fn pid_exists(pid: i32) -> bool {
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn send_signal(pid: i32, signal: i32) -> Result<(), VesselError> {
    let result = unsafe { libc::kill(pid, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(VesselError::Runtime(format!(
            "failed to signal daemon pid {pid}: {}",
            io::Error::last_os_error()
        )))
    }
}

fn wait_for_pid_exit(pid: i32, timeout: Duration) -> Result<(), VesselError> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !pid_exists(pid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }

    Err(VesselError::Runtime(format!(
        "daemon pid {pid} did not exit within {}s",
        timeout.as_secs()
    )))
}

fn describe_status(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        code.to_string()
    } else if let Some(signal) = status.signal() {
        format!("signal {signal}")
    } else {
        "unknown".to_string()
    }
}

enum DaemonState {
    Running(i32),
    Stopped,
}

impl std::str::FromStr for DaemonCommand {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "start" => Ok(Self::Start),
            "status" => Ok(Self::Status),
            "stop" => Ok(Self::Stop),
            other => Err(format!("unsupported daemon action `{other}`")),
        }
    }
}

fn map_spawn_error(source: io::Error) -> VesselError {
    VesselError::Runtime(format!("failed to spawn daemon process: {source}"))
}

fn print_container_action(record: &ContainerRecord) {
    println!("{}\t{}", record.id(), status_label(&record.status));
}

fn print_table(records: &[ContainerRecord]) {
    println!("{:<14} {:<36} {:<12} {:<8}", "ID", "IMAGE", "STATUS", "PID");
    for record in records {
        let pid = record.pid.map(|pid| pid.to_string()).unwrap_or_else(|| "-".to_string());
        println!(
            "{:<14} {:<36} {:<12} {:<8}",
            record.id(),
            format!("{}.", record.image),
            status_label(&record.status),
            pid
        );
    }
}

fn print_gc_summary(summary: &GarbageCollectionSummary) {
    println!(
        "Deleted {} unused layer directories and {} cached blob files, reclaiming {} bytes",
        summary.removed_layers, summary.removed_blobs, summary.reclaimed_bytes
    );
}

fn status_label(status: &ContainerStatus) -> String {
    match status {
        ContainerStatus::Created => "created".to_string(),
        ContainerStatus::Running => "running".to_string(),
        ContainerStatus::Exited { code } => format!("exited({code})"),
        ContainerStatus::Stopped => "stopped".to_string(),
    }
}

fn cli() -> OptionParser<GlobalArgs> {
    let remote = long("remote").short('r').switch().help("Use the vessel daemon via API");

    let detach = long("detach").short('d').switch();
    let env = long("env").short('e').argument::<String>("ENV").many();
    let mounts = long("volume").short('v').argument::<String>("VOLUME").many();
    let image = positional::<String>("IMAGE");
    let command = positional::<String>("COMMAND").many();
    let run = construct!(RunArgs { detach, env, mounts, image, command })
        .to_options()
        .descr("Pull, create, and start a container")
        .command("run")
        .map(Command::Run);

    let start = {
        let id = positional::<ContainerId>("ID");
        construct!(IdArgs { id })
            .to_options()
            .descr("Start an existing container")
            .command("start")
            .map(Command::Start)
    };
    let stop = {
        let id = positional::<ContainerId>("ID");
        construct!(IdArgs { id })
            .to_options()
            .descr("Request graceful termination for a running container")
            .command("stop")
            .map(Command::Stop)
    };
    let kill = {
        let id = positional::<ContainerId>("ID");
        construct!(IdArgs { id })
            .to_options()
            .descr("Forcefully terminate a running container")
            .command("kill")
            .map(Command::Kill)
    };
    let rm = {
        let id = positional::<ContainerId>("ID");
        construct!(IdArgs { id })
            .to_options()
            .descr("Remove a container")
            .command("rm")
            .map(Command::Rm)
    };
    let logs = {
        let id = positional::<ContainerId>("ID");
        construct!(IdArgs { id })
            .to_options()
            .descr("Fetch the logs of a container")
            .command("logs")
            .map(Command::Logs)
    };

    let format = long("format").argument::<OutputFormat>("FORMAT").fallback(OutputFormat::Table);
    let ps = construct!(PsArgs { format })
        .to_options()
        .descr("List known containers")
        .command("ps")
        .map(Command::Ps);

    let rmi = {
        let image = positional::<String>("IMAGE");
        construct!(RmiArgs { image })
            .to_options()
            .descr("Remove an image")
            .command("rmi")
            .map(Command::Rmi)
    };

    let gc = {
        let format =
            long("format").argument::<OutputFormat>("FORMAT").fallback(OutputFormat::Table);
        construct!(GcArgs { format })
            .to_options()
            .descr("Remove unused layer and blob cache data")
            .command("gc")
            .map(Command::Gc)
    };

    let compose = {
        let file = long("file").short('f').argument::<String>("FILE").optional();
        let project_name = long("project-name").argument::<String>("NAME").optional();

        let up = pure(ComposeSubcommand::Up)
            .to_options()
            .descr("Create or start the services in a compose project")
            .command("up");
        let down = pure(ComposeSubcommand::Down)
            .to_options()
            .descr("Stop and remove the services in a compose project")
            .command("down");
        let ps = pure(ComposeSubcommand::Ps)
            .to_options()
            .descr("List service containers for a compose project")
            .command("ps");
        let logs = {
            let service = positional::<String>("SERVICE").optional();
            construct!(ComposeLogsArgs { service })
                .map(ComposeSubcommand::Logs)
                .to_options()
                .descr("Fetch logs for one service or the whole compose project")
                .command("logs")
        };

        let command = construct!([up, down, ps, logs]);
        construct!(ComposeArgs { file, project_name, command })
            .to_options()
            .descr("Manage a YAML-defined multi-container project")
            .command("compose")
            .map(Command::Compose)
    };

    let daemon = {
        let command = positional::<DaemonCommand>("ACTION");
        construct!(DaemonArgs { command })
            .to_options()
            .descr("Manage the vessel daemon")
            .command("daemon")
            .map(Command::Daemon)
    };

    let command = construct!([run, start, stop, kill, rm, logs, ps, rmi, gc, compose, daemon]);

    construct!(GlobalArgs { remote, command })
        .to_options()
        .descr("Vessel container engine")
        .version(env!("CARGO_PKG_VERSION"))
}
