use std::collections::BTreeMap;

use vessel_core::{
    CapabilityReport, ContainerId, ContainerRecord, ContainerStore, ImageRef, VesselError,
    VesselPaths,
};

use crate::{RunOutcome, Runtime};

#[derive(Debug, Clone)]
pub struct UnsupportedRuntime {
    platform: String,
    _paths: VesselPaths,
}

impl UnsupportedRuntime {
    pub fn new(paths: VesselPaths) -> Self {
        Self { platform: std::env::consts::OS.to_string(), _paths: paths }
    }

    fn reason(&self) -> String {
        if self.platform == "macos" {
            "macOS does not provide Linux namespaces, cgroups, or a Linux-compatible userspace kernel ABI, so standard OCI Linux images cannot be executed natively".to_string()
        } else if self.platform == "windows" {
            "Windows does not provide the Linux kernel features needed to execute standard OCI Linux images natively".to_string()
        } else {
            format!(
                "{} does not provide the Linux kernel features needed by the native Vessel runtime",
                self.platform
            )
        }
    }
}

impl Runtime for UnsupportedRuntime {
    fn capability_report(&self) -> CapabilityReport {
        CapabilityReport {
            platform: self.platform.clone(),
            supported: false,
            rootless: false,
            overlayfs: false,
            cgroup_v2: false,
            missing: vec![self.reason()],
        }
    }

    fn run(
        &self,
        _store: &ContainerStore,
        _image: &ImageRef,
        _detach: bool,
        _command_override: Option<Vec<String>>,
        _env_override: Option<BTreeMap<String, String>>,
        _mount_override: Option<BTreeMap<String, String>>,
        _port_override: Option<BTreeMap<u16, u16>>,
    ) -> Result<RunOutcome, VesselError> {
        Err(VesselError::UnsupportedPlatform(self.reason()))
    }

    fn start(
        &self,
        _store: &ContainerStore,
        _id: &ContainerId,
    ) -> Result<ContainerRecord, VesselError> {
        Err(VesselError::UnsupportedPlatform(self.reason()))
    }

    fn stop(
        &self,
        _store: &ContainerStore,
        _id: &ContainerId,
    ) -> Result<ContainerRecord, VesselError> {
        Err(VesselError::UnsupportedPlatform(self.reason()))
    }

    fn kill(
        &self,
        _store: &ContainerStore,
        _id: &ContainerId,
    ) -> Result<ContainerRecord, VesselError> {
        Err(VesselError::UnsupportedPlatform(self.reason()))
    }

    fn remove(&self, _store: &ContainerStore, _id: &ContainerId) -> Result<(), VesselError> {
        Err(VesselError::UnsupportedPlatform(self.reason()))
    }

    fn logs(&self, _store: &ContainerStore, _id: &ContainerId) -> Result<(), VesselError> {
        Err(VesselError::UnsupportedPlatform(self.reason()))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use vessel_core::{ContainerStore, ImageRef, VesselPaths};

    use crate::{Runtime, UnsupportedRuntime};

    #[test]
    fn reports_clear_unsupported_status() {
        let temp = tempdir().expect("tempdir");
        let runtime = UnsupportedRuntime::new(VesselPaths {
            state_dir: temp.path().join("state"),
            data_dir: temp.path().join("data"),
            blobs_dir: temp.path().join("data/blobs"),
            rootfs_dir: temp.path().join("data/rootfs"),
            bundles_dir: temp.path().join("data/bundles"),
        });
        let store = ContainerStore::new(temp.path().join("state"));
        let report = runtime.capability_report();
        assert!(!report.supported);
        assert!(!report.missing.is_empty());
        let image: ImageRef = "docker.io/library/alpine:latest".parse().expect("image");
        let error = runtime.run(&store, &image, false, None, None, None).expect_err("unsupported");
        assert_eq!(error.exit_code(), 3);
    }
}
