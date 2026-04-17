use std::collections::BTreeMap;
use std::fs;
use std::process;

use bpaf::{OptionParser, Parser, construct, long, positional};
use vessel_core::{
    ContainerId, ContainerRecord, ContainerStatus, ContainerStore, ImageRef, OutputFormat,
    VesselError, VesselPaths,
};
use vessel_runtime::default_runtime;

enum Command {
    Run(RunArgs),
    Start(IdArgs),
    Stop(IdArgs),
    Kill(IdArgs),
    Rm(IdArgs),
    Logs(IdArgs),
    Ps(PsArgs),
    Rmi(RmiArgs),
}

#[derive(Debug, Clone)]
struct RunArgs {
    detach: bool,
    image: String,
    env: Vec<String>,
    mounts: Vec<String>,
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

fn main() {
    if let Some(exit_code) = maybe_run_internal_helper() {
        process::exit(exit_code);
    }

    let exit_code = match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            error.exit_code()
        }
    };
    process::exit(exit_code);
}

fn run() -> Result<i32, VesselError> {
    let cli = cli().run();
    let paths = VesselPaths::discover()?;
    paths.ensure()?;
    let store = ContainerStore::new(paths.state_dir.clone());
    store.ensure()?;
    let runtime = default_runtime(paths.clone());

    match cli {
        Command::Run(args) => {
            let report = runtime.capability_report();
            if !report.supported {
                return Err(VesselError::Capability(report.missing.join(", ")));
            }

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
                    let host_path = fs::canonicalize(host).map_err(|source| VesselError::io(host, source))?;
                    mount_override.insert(host_path.to_string_lossy().to_string(), guest.to_string());
                } else {
                    return Err(VesselError::Usage(format!("invalid volume mapping `{entry}`; expected host_path:guest_path")));
                }
            }
            let mount_override = (!mount_override.is_empty()).then_some(mount_override);

            let command_override = (!args.command.is_empty()).then_some(args.command);
            let outcome = runtime.run(&store, &image, args.detach, command_override, env_override, mount_override)?;
            if args.detach {
                println!("{}", outcome.record.id());
                Ok(0)
            } else {
                Ok(outcome.exit_code.unwrap_or_default())
            }
        }
        Command::Start(args) => {
            let record = runtime.start(&store, &args.id)?;
            print_container_action(&record);
            Ok(0)
        }
        Command::Stop(args) => {
            let record = runtime.stop(&store, &args.id)?;
            print_container_action(&record);
            Ok(0)
        }
        Command::Kill(args) => {
            let record = runtime.kill(&store, &args.id)?;
            print_container_action(&record);
            Ok(0)
        }
        Command::Rm(args) => {
            runtime.remove(&store, &args.id)?;
            println!("{}", args.id);
            Ok(0)
        }
        Command::Logs(args) => {
            runtime.logs(&store, &args.id)?;
            Ok(0)
        }
        Command::Ps(args) => {
            let records = store.list()?;
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
            let records = store.list()?;
            if let Some(record) = records.iter().find(|r| r.image == image) {
                return Err(VesselError::Usage(format!("image `{}` is in use by container `{}`", image, record.id())));
            }
            let image_store = vessel_image::ImageStore::new(paths);
            image_store.remove(&image)?;
            println!("Untagged: {}", image);
            Ok(0)
        }
    }
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
            truncate(&record.image.to_string(), 36),
            status_label(&record.status),
            pid
        );
    }
}

fn status_label(status: &ContainerStatus) -> String {
    match status {
        ContainerStatus::Created => "created".to_string(),
        ContainerStatus::Running => "running".to_string(),
        ContainerStatus::Exited { code } => format!("exited({code})"),
        ContainerStatus::Stopped => "stopped".to_string(),
    }
}

fn truncate(value: &str, width: usize) -> String {
    if value.len() <= width {
        return value.to_string();
    }
    let keep = width.saturating_sub(1);
    format!("{}.", &value[..keep])
}

fn cli() -> OptionParser<Command> {
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

    construct!([run, start, stop, kill, rm, logs, ps, rmi])
        .to_options()
        .descr("Vessel daemonless container engine")
        .version(env!("CARGO_PKG_VERSION"))
}
