use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("cold-start") => {
            if let Err(error) = cold_start() {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        Some("sign-macos") => {
            let binary = args.next().map(PathBuf::from).unwrap_or_else(locate_vessel_binary);
            if let Err(error) = sign_macos(binary) {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("usage: cargo run -p xtask -- <cold-start|sign-macos [binary]>");
            std::process::exit(2);
        }
    }
}

fn cold_start() -> Result<(), String> {
    let binary = locate_vessel_binary();
    if !binary.exists() {
        return Err(format!("missing `{}`; build the workspace first", binary.display()));
    }

    let state_dir = PathBuf::from("target/xtask/state");
    let data_dir = PathBuf::from("target/xtask/data");
    fs::create_dir_all(&state_dir).map_err(|error| error.to_string())?;
    fs::create_dir_all(&data_dir).map_err(|error| error.to_string())?;

    let mut samples = Vec::new();
    for _ in 0..20 {
        let started = Instant::now();
        let status = Command::new(&binary)
            .arg("ps")
            .arg("--format")
            .arg("json")
            .env("VESSEL_STATE_DIR", &state_dir)
            .env("VESSEL_DATA_DIR", &data_dir)
            .status()
            .map_err(|error| error.to_string())?;
        if !status.success() {
            return Err(format!("benchmark invocation failed with {status}"));
        }
        samples.push(started.elapsed());
    }

    let total = samples.iter().copied().fold(Duration::ZERO, |acc, next| acc + next);
    let average = total / samples.len() as u32;
    println!("cold-start average: {:.2?}", average);
    println!("samples: {}", samples.len());
    Ok(())
}

fn locate_vessel_binary() -> PathBuf {
    let mut path = PathBuf::from("target");
    path.push("debug");
    path.push(if cfg!(windows) { "vessel.exe" } else { "vessel" });
    path
}

fn sign_macos(binary: PathBuf) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("sign-macos is only available on macOS".to_string());
    }
    if !binary.exists() {
        return Err(format!("missing `{}`; build Vessel first", binary.display()));
    }

    let entitlements = PathBuf::from("vessel.entitlements");
    if !entitlements.is_file() {
        return Err(format!(
            "missing `{}`; expected the macOS entitlements file at the workspace root",
            entitlements.display()
        ));
    }

    let status = Command::new("codesign")
        .arg("--entitlements")
        .arg(&entitlements)
        .arg("--force")
        .arg("-s")
        .arg("-")
        .arg(&binary)
        .status()
        .map_err(|error| format!("failed to invoke codesign: {error}"))?;

    if !status.success() {
        return Err(format!("codesign failed with {status}"));
    }

    println!("signed {}", binary.display());
    Ok(())
}
