use std::{
    collections::BTreeMap,
    path::PathBuf,
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContainerId(String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContainerStatus {
    Created,
    Running,
    Exited { code: i32 },
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputFormat {
    Table,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityReport {
    pub platform: String,
    pub supported: bool,
    pub rootless: bool,
    pub overlayfs: bool,
    pub cgroup_v2: bool,
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerRecord {
    pub id: ContainerId,
    pub image: crate::ImageRef,
    pub status: ContainerStatus,
    pub pid: Option<u32>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub command: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub mounts: BTreeMap<String, String>,
    pub workdir: Option<String>,
    pub layers: Vec<PathBuf>,
}

impl ContainerId {
    pub fn generate() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        Self(format!("{:012x}", now ^ u128::from(count)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ContainerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for ContainerId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for ContainerId {
    type Err = crate::VesselError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.trim().is_empty() {
            return Err(crate::VesselError::Usage("container id cannot be empty".to_string()));
        }
        Ok(Self(value.to_string()))
    }
}

impl ContainerRecord {
    pub fn new(
        id: ContainerId,
        image: crate::ImageRef,
        command: Vec<String>,
        workdir: Option<String>,
        environment: BTreeMap<String, String>,
        mounts: BTreeMap<String, String>,
        layers: Vec<PathBuf>,
    ) -> Self {
        Self {
            id,
            image,
            status: ContainerStatus::Created,
            pid: None,
            created_at: now_timestamp(),
            started_at: None,
            finished_at: None,
            command,
            environment,
            mounts,
            workdir,
            layers,
        }
    }

    pub fn id(&self) -> &ContainerId {
        &self.id
    }
}

impl std::str::FromStr for OutputFormat {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "table" => Ok(Self::Table),
            "json" => Ok(Self::Json),
            other => Err(format!("unsupported output format `{other}`")),
        }
    }
}

pub fn now_timestamp() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}
