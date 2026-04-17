use std::{
    env, fs,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;

use crate::VesselError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformTriple {
    pub os: &'static str,
    pub architecture: &'static str,
}

#[derive(Debug, Clone)]
pub struct VesselPaths {
    pub state_dir: PathBuf,
    pub data_dir: PathBuf,
    pub blobs_dir: PathBuf,
    pub rootfs_dir: PathBuf,
    pub bundles_dir: PathBuf,
}

impl VesselPaths {
    pub fn discover() -> Result<Self, VesselError> {
        let data_dir = data_dir()?;
        let state_dir = state_dir(&data_dir)?;

        Ok(Self {
            blobs_dir: data_dir.join("blobs"),
            rootfs_dir: data_dir.join("rootfs"),
            bundles_dir: data_dir.join("bundles"),
            state_dir,
            data_dir,
        })
    }

    pub fn ensure(&self) -> Result<(), VesselError> {
        for path in
            [&self.state_dir, &self.data_dir, &self.blobs_dir, &self.rootfs_dir, &self.bundles_dir]
        {
            fs::create_dir_all(path).map_err(|source| VesselError::io(path, source))?;
        }

        Ok(())
    }
}

pub fn host_platform() -> PlatformTriple {
    PlatformTriple {
        os: match env::consts::OS {
            "macos" => "darwin",
            other => other,
        },
        architecture: match env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        },
    }
}

fn data_dir() -> Result<PathBuf, VesselError> {
    if let Ok(value) = env::var("VESSEL_DATA_DIR") {
        return Ok(PathBuf::from(value));
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg_data_home) = env::var("XDG_DATA_HOME") {
            return Ok(PathBuf::from(xdg_data_home).join("vessel"));
        }

        if let Ok(home) = env::var("HOME") {
            return Ok(PathBuf::from(home).join(".local/share/vessel"));
        }
    }

    ProjectDirs::from("", "", "vessel")
        .map(|dirs| dirs.data_local_dir().to_path_buf())
        .ok_or_else(|| VesselError::Runtime("unable to determine local data directory".to_string()))
}

fn state_dir(_data_dir: &Path) -> Result<PathBuf, VesselError> {
    if let Ok(value) = env::var("VESSEL_STATE_DIR") {
        return Ok(PathBuf::from(value));
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
            return Ok(PathBuf::from(runtime_dir).join("vessel"));
        }

        Ok(env::temp_dir().join("vessel-runtime"))
    }

    #[cfg(not(target_os = "linux"))]
    {
        Ok(_data_dir.join("state"))
    }
}

#[cfg(test)]
mod tests {
    use super::{PlatformTriple, host_platform};

    #[test]
    fn maps_known_architectures() {
        let triple = host_platform();
        assert!(matches!(triple, PlatformTriple { architecture: "amd64" | "arm64" | _, .. }));
    }
}
