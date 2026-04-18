#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod unsupported;

use std::collections::BTreeMap;

use vessel_core::{
    CapabilityReport, ContainerId, ContainerRecord, ContainerStore, ImageRef, VesselError,
    VesselPaths,
};

#[cfg(target_os = "linux")]
pub use linux::LinuxRuntime;
#[cfg(target_os = "macos")]
pub use macos::{MacOsRuntime, run_macos_helper};
pub use unsupported::UnsupportedRuntime;

pub trait Runtime {
    fn capability_report(&self) -> CapabilityReport;
    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        store: &ContainerStore,
        image: &ImageRef,
        detach: bool,
        command_override: Option<Vec<String>>,
        env_override: Option<BTreeMap<String, String>>,
        mount_override: Option<BTreeMap<String, String>>,
        port_override: Option<BTreeMap<u16, u16>>,
    ) -> Result<RunOutcome, VesselError>;
    fn start(
        &self,
        store: &ContainerStore,
        id: &ContainerId,
    ) -> Result<ContainerRecord, VesselError>;
    fn stop(
        &self,
        store: &ContainerStore,
        id: &ContainerId,
    ) -> Result<ContainerRecord, VesselError>;
    fn kill(
        &self,
        store: &ContainerStore,
        id: &ContainerId,
    ) -> Result<ContainerRecord, VesselError>;
    fn remove(&self, store: &ContainerStore, id: &ContainerId) -> Result<(), VesselError>;
    fn logs(&self, store: &ContainerStore, id: &ContainerId) -> Result<(), VesselError>;
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub record: ContainerRecord,
    pub exit_code: Option<i32>,
}

pub fn default_runtime(paths: VesselPaths) -> Box<dyn Runtime> {
    #[cfg(target_os = "linux")]
    {
        Box::new(LinuxRuntime::new(paths))
    }

    #[cfg(target_os = "macos")]
    {
        Box::new(MacOsRuntime::new(paths))
    }

    #[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
    {
        Box::new(UnsupportedRuntime::new(paths))
    }
}
