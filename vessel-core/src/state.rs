use std::{fs, path::PathBuf};

use crate::{ContainerId, ContainerRecord, ContainerStatus, VesselError, now_timestamp};

#[derive(Debug, Clone)]
pub struct ContainerStore {
    root: PathBuf,
}

impl ContainerStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn ensure(&self) -> Result<(), VesselError> {
        fs::create_dir_all(&self.root).map_err(|source| VesselError::io(&self.root, source))
    }

    pub fn save(&self, record: &ContainerRecord) -> Result<(), VesselError> {
        self.ensure()?;
        let path = self.path_for(record.id());
        let payload = serde_json::to_vec_pretty(record)
            .map_err(|error| VesselError::Serialization(error.to_string()))?;
        fs::write(&path, payload).map_err(|source| VesselError::io(path, source))
    }

    pub fn load(&self, id: &ContainerId) -> Result<ContainerRecord, VesselError> {
        let path = self.path_for(id.as_str());
        let payload = fs::read(&path).map_err(|source| VesselError::io(&path, source))?;
        let record: ContainerRecord = serde_json::from_slice(&payload)
            .map_err(|error| VesselError::Serialization(error.to_string()))?;
        self.reconcile(record)
    }

    pub fn list(&self) -> Result<Vec<ContainerRecord>, VesselError> {
        self.ensure()?;
        let mut records = Vec::new();

        for entry in
            fs::read_dir(&self.root).map_err(|source| VesselError::io(&self.root, source))?
        {
            let entry = entry.map_err(VesselError::GenericIo)?;
            if entry
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext == "json")
            {
                let payload = fs::read(entry.path())
                    .map_err(|source| VesselError::io(entry.path(), source))?;
                let record: ContainerRecord = serde_json::from_slice(&payload)
                    .map_err(|error| VesselError::Serialization(error.to_string()))?;
                let record = self.reconcile(record)?;
                records.push(record);
            }
        }

        records.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        Ok(records)
    }

    pub fn update_status(
        &self,
        id: &ContainerId,
        status: ContainerStatus,
        pid: Option<u32>,
        finished_at: Option<String>,
    ) -> Result<ContainerRecord, VesselError> {
        let mut record = self.load(id)?;
        record.status = status;
        record.pid = pid;
        record.finished_at = finished_at;
        if record.finished_at.is_none() && !matches!(record.status, ContainerStatus::Running) {
            record.finished_at = Some(now_timestamp());
        }
        self.save(&record)?;
        Ok(record)
    }

    pub fn remove(&self, id: &ContainerId) -> Result<(), VesselError> {
        let path = self.path_for(id.as_str());
        if !path.exists() {
            return Err(VesselError::ContainerNotFound(id.to_string()));
        }
        fs::remove_file(&path).map_err(|source| VesselError::io(path, source))
    }

    fn reconcile(&self, record: ContainerRecord) -> Result<ContainerRecord, VesselError> {
        #[cfg(target_os = "linux")]
        {
            return self.reconcile_linux(record);
        }

        #[cfg(target_os = "macos")]
        {
            self.reconcile_macos(record)
        }

        #[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
        {
            Ok(record)
        }
    }

    #[cfg(target_os = "linux")]
    fn reconcile_linux(&self, mut record: ContainerRecord) -> Result<ContainerRecord, VesselError> {
        #[cfg(target_os = "linux")]
        if matches!(record.status, ContainerStatus::Running) {
            if let Some(pid) = record.pid {
                let proc_path = std::path::Path::new("/proc").join(pid.to_string());
                if !proc_path.exists() {
                    record.status = ContainerStatus::Stopped;
                    record.pid = None;
                    record.finished_at.get_or_insert_with(now_timestamp);
                    self.save(&record)?;
                }
            }
        }

        Ok(record)
    }

    #[cfg(target_os = "macos")]
    fn reconcile_macos(&self, mut record: ContainerRecord) -> Result<ContainerRecord, VesselError> {
        if matches!(record.status, ContainerStatus::Running) {
            if let Some(pid) = record.pid {
                let result = unsafe { libc::kill(pid as i32, 0) };
                let alive = result == 0
                    || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
                if !alive {
                    record.status = ContainerStatus::Stopped;
                    record.pid = None;
                    record.finished_at.get_or_insert_with(now_timestamp);
                    self.save(&record)?;
                }
            }
        }

        Ok(record)
    }

    fn path_for(&self, id: impl AsRef<str>) -> PathBuf {
        self.root.join(format!("{}.json", id.as_ref()))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use crate::{ContainerId, ContainerRecord, ContainerStatus, ImageRef};

    use super::ContainerStore;

    #[test]
    fn saves_and_loads_records() {
        let dir = tempdir().expect("tempdir");
        let store = ContainerStore::new(dir.path().to_path_buf());
        let record = ContainerRecord::new(
            ContainerId::generate(),
            "docker.io/library/alpine:latest".parse::<ImageRef>().expect("image"),
            vec!["/bin/echo".into(), "hello".into()],
            None,
            Default::default(),
            Default::default(),
            vec![dir.path().join("rootfs")],
        );
        store.save(&record).expect("save");

        let loaded = store.load(record.id()).expect("load");
        assert_eq!(loaded.status, ContainerStatus::Created);
        assert_eq!(loaded.command, vec!["/bin/echo".to_string(), "hello".to_string()]);
    }
}
