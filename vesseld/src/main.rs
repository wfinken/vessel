use std::process;
use vessel_core::VesselPaths;

#[tokio::main]
async fn main() {
    let paths = match VesselPaths::discover() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("{error}");
            process::exit(1);
        }
    };

    if let Err(error) = paths.ensure() {
        eprintln!("{error}");
        process::exit(1);
    }

    let socket_path = vessel_daemon::socket_path(&paths);
    println!("Starting vessel daemon on {}", socket_path.display());

    if let Err(error) = vessel_daemon::run_daemon(paths, &socket_path).await {
        eprintln!("{error}");
        process::exit(1);
    }
}
