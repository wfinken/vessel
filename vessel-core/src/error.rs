use std::{io, path::PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum VesselError {
    #[error("{0}")]
    Usage(String),
    #[error("unsupported runtime on this platform: {0}")]
    UnsupportedPlatform(String),
    #[error("missing or unsupported host capability: {0}")]
    Capability(String),
    #[error("invalid image reference `{0}`")]
    InvalidImageReference(String),
    #[error("container `{0}` was not found")]
    ContainerNotFound(String),
    #[error("container `{0}` is already running")]
    ContainerAlreadyRunning(String),
    #[error("container `{0}` is not running")]
    ContainerNotRunning(String),
    #[error("registry error: {0}")]
    Registry(String),
    #[error("OCI compatibility error: {0}")]
    Oci(String),
    #[error("runtime error: {0}")]
    Runtime(String),
    #[error("external tool error ({tool}): {details}")]
    ExternalTool { tool: String, details: String },
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("I/O error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    GenericIo(#[from] io::Error),
}

impl VesselError {
    pub fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io { path: path.into(), source }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 2,
            Self::UnsupportedPlatform(_) | Self::Capability(_) | Self::ExternalTool { .. } => 3,
            _ => 125,
        }
    }
}
