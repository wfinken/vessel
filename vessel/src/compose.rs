use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use vessel_core::{ContainerId, VesselError, VesselPaths};

const DEFAULT_COMPOSE_FILES: &[&str] =
    &["compose.yaml", "compose.yml", "vessel-compose.yaml", "vessel-compose.yml"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposeProject {
    pub name: String,
    pub file: PathBuf,
    pub services: BTreeMap<String, ComposeServiceSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposeServiceSpec {
    pub image: String,
    pub command: Option<Vec<String>>,
    pub environment: BTreeMap<String, String>,
    pub mounts: BTreeMap<String, String>,
    pub ports: BTreeMap<u16, u16>,
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposeProjectState {
    pub project_name: String,
    pub source_file: PathBuf,
    pub services: BTreeMap<String, ManagedServiceState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedServiceState {
    pub container_id: ContainerId,
    pub spec: ComposeServiceSpec,
}

#[derive(Debug, Clone, Deserialize)]
struct ComposeFile {
    name: Option<String>,
    services: BTreeMap<String, ComposeService>,
}

#[derive(Debug, Clone, Deserialize)]
struct ComposeService {
    image: String,
    #[serde(default)]
    command: Option<ComposeCommand>,
    #[serde(default)]
    environment: ComposeEnvironment,
    #[serde(default)]
    volumes: Vec<String>,
    #[serde(default)]
    ports: Vec<String>,
    #[serde(default)]
    depends_on: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ComposeCommand {
    Shell(String),
    Exec(Vec<String>),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
enum ComposeEnvironment {
    #[default]
    Empty,
    Mapping(BTreeMap<String, String>),
    List(Vec<String>),
}

impl ComposeEnvironment {
    fn into_map(self) -> Result<BTreeMap<String, String>, VesselError> {
        match self {
            Self::Empty => Ok(BTreeMap::new()),
            Self::Mapping(values) => Ok(values),
            Self::List(values) => {
                let mut environment = BTreeMap::new();
                for entry in values {
                    if let Some((key, value)) = entry.split_once('=') {
                        environment.insert(key.to_string(), value.to_string());
                    } else {
                        environment.insert(entry, String::new());
                    }
                }
                Ok(environment)
            }
        }
    }
}

impl ComposeCommand {
    fn into_argv(self) -> Result<Vec<String>, VesselError> {
        match self {
            Self::Shell(command) => {
                let command = command.trim();
                if command.is_empty() {
                    return Err(VesselError::Usage(
                        "compose service command cannot be empty".to_string(),
                    ));
                }
                Ok(vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()])
            }
            Self::Exec(command) => {
                if command.is_empty() {
                    return Err(VesselError::Usage(
                        "compose service command list cannot be empty".to_string(),
                    ));
                }
                Ok(command)
            }
        }
    }
}

pub fn discover_compose_file(cwd: &Path) -> Result<PathBuf, VesselError> {
    for candidate in DEFAULT_COMPOSE_FILES {
        let path = cwd.join(candidate);
        if path.exists() {
            return Ok(path);
        }
    }

    Err(VesselError::Usage(
        "no compose file found; pass --file or create compose.yaml/compose.yml".to_string(),
    ))
}

pub fn load_project(
    file: &Path,
    project_name_override: Option<&str>,
) -> Result<ComposeProject, VesselError> {
    let payload = fs::read_to_string(file).map_err(|source| VesselError::io(file, source))?;
    let parsed: ComposeFile = serde_yaml::from_str(&payload)
        .map_err(|error| VesselError::Usage(format!("invalid compose YAML: {error}")))?;

    if parsed.services.is_empty() {
        return Err(VesselError::Usage(
            "compose file must define at least one service".to_string(),
        ));
    }

    let project_name = match project_name_override {
        Some(name) => normalize_project_name(name)?,
        None => match parsed.name {
            Some(name) => normalize_project_name(&name)?,
            None => default_project_name(file)?,
        },
    };

    let base_dir = file.parent().unwrap_or_else(|| Path::new("."));
    let mut services = BTreeMap::new();

    for (service_name, service) in parsed.services {
        let image = service.image.trim();
        if image.is_empty() {
            return Err(VesselError::Usage(format!(
                "service `{service_name}` must define an image"
            )));
        }

        let mut mounts = BTreeMap::new();
        for entry in service.volumes {
            let (host, guest) = entry.split_once(':').ok_or_else(|| {
                VesselError::Usage(format!(
                    "service `{service_name}` has invalid volume mapping `{entry}`; expected host_path:guest_path"
                ))
            })?;
            let host_path = resolve_host_volume(base_dir, host)?;
            mounts.insert(host_path.to_string_lossy().to_string(), guest.to_string());
        }

        let mut ports = BTreeMap::new();
        for entry in service.ports {
            let (host, guest) = entry.split_once(':').ok_or_else(|| {
                VesselError::Usage(format!(
                    "service `{service_name}` has invalid port mapping `{entry}`; expected host_port:guest_port"
                ))
            })?;
            if let (Ok(host_port), Ok(guest_port)) = (host.parse::<u16>(), guest.parse::<u16>()) {
                ports.insert(host_port, guest_port);
            } else {
                return Err(VesselError::Usage(format!(
                    "service `{service_name}` has invalid port mapping `{entry}`; expected host_port:guest_port with valid port numbers"
                )));
            }
        }

        services.insert(
            service_name.clone(),
            ComposeServiceSpec {
                image: image.to_string(),
                command: service.command.map(ComposeCommand::into_argv).transpose()?,
                environment: service.environment.into_map()?,
                mounts,
                ports,
                depends_on: service.depends_on,
            },
        );
    }

    validate_dependencies(&services)?;

    Ok(ComposeProject { name: project_name, file: file.to_path_buf(), services })
}

pub fn services_in_dependency_order(
    project: &ComposeProject,
) -> Result<Vec<(String, ComposeServiceSpec)>, VesselError> {
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut ordered = Vec::new();

    for service_name in project.services.keys() {
        visit_service(service_name, &project.services, &mut visiting, &mut visited, &mut ordered)?;
    }

    Ok(ordered
        .into_iter()
        .map(|name| {
            let spec = project
                .services
                .get(&name)
                .cloned()
                .expect("topological sort returned an unknown service");
            (name, spec)
        })
        .collect())
}

pub fn services_in_shutdown_order(
    project: &ComposeProject,
) -> Result<Vec<(String, ComposeServiceSpec)>, VesselError> {
    let mut ordered = services_in_dependency_order(project)?;
    ordered.reverse();
    Ok(ordered)
}

pub fn state_path(paths: &VesselPaths, project_name: &str) -> PathBuf {
    paths.data_dir.join("compose").join(format!("{}.json", sanitize_name(project_name)))
}

pub fn load_state(
    paths: &VesselPaths,
    project_name: &str,
) -> Result<Option<ComposeProjectState>, VesselError> {
    let path = state_path(paths, project_name);
    if !path.exists() {
        return Ok(None);
    }

    let payload = fs::read(&path).map_err(|source| VesselError::io(&path, source))?;
    let state = serde_json::from_slice(&payload)
        .map_err(|error| VesselError::Serialization(error.to_string()))?;
    Ok(Some(state))
}

pub fn save_state(paths: &VesselPaths, state: &ComposeProjectState) -> Result<(), VesselError> {
    let path = state_path(paths, &state.project_name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| VesselError::io(parent, source))?;
    }

    let payload = serde_json::to_vec_pretty(state)
        .map_err(|error| VesselError::Serialization(error.to_string()))?;
    fs::write(&path, payload).map_err(|source| VesselError::io(&path, source))
}

pub fn delete_state(paths: &VesselPaths, project_name: &str) -> Result<(), VesselError> {
    let path = state_path(paths, project_name);
    if path.exists() {
        fs::remove_file(&path).map_err(|source| VesselError::io(&path, source))?;
    }
    Ok(())
}

fn default_project_name(file: &Path) -> Result<String, VesselError> {
    let stem = file
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| VesselError::Usage("compose file name is invalid".to_string()))?;
    normalize_project_name(stem)
}

fn normalize_project_name(value: &str) -> Result<String, VesselError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(VesselError::Usage("compose project name cannot be empty".to_string()));
    }
    Ok(value.to_string())
}

fn resolve_host_volume(base_dir: &Path, host: &str) -> Result<PathBuf, VesselError> {
    let host = Path::new(host);
    let resolved = if host.is_absolute() { host.to_path_buf() } else { base_dir.join(host) };
    fs::canonicalize(&resolved).map_err(|source| VesselError::io(&resolved, source))
}

fn validate_dependencies(
    services: &BTreeMap<String, ComposeServiceSpec>,
) -> Result<(), VesselError> {
    for (service_name, service) in services {
        for dependency in &service.depends_on {
            if !services.contains_key(dependency) {
                return Err(VesselError::Usage(format!(
                    "service `{service_name}` depends on unknown service `{dependency}`"
                )));
            }
        }
    }
    Ok(())
}

fn visit_service(
    service_name: &str,
    services: &BTreeMap<String, ComposeServiceSpec>,
    visiting: &mut BTreeSet<String>,
    visited: &mut BTreeSet<String>,
    ordered: &mut Vec<String>,
) -> Result<(), VesselError> {
    if visited.contains(service_name) {
        return Ok(());
    }
    if !visiting.insert(service_name.to_string()) {
        return Err(VesselError::Usage(format!(
            "compose services contain a dependency cycle involving `{service_name}`"
        )));
    }

    let service = services
        .get(service_name)
        .ok_or_else(|| VesselError::Usage(format!("unknown service `{service_name}`")))?;
    for dependency in &service.depends_on {
        visit_service(dependency, services, visiting, visited, ordered)?;
    }

    visiting.remove(service_name);
    visited.insert(service_name.to_string());
    ordered.push(service_name.to_string());
    Ok(())
}

fn sanitize_name(value: &str) -> String {
    value.chars().map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' }).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        ComposeProjectState, ManagedServiceState, discover_compose_file, load_project, load_state,
        save_state, services_in_dependency_order, services_in_shutdown_order, state_path,
    };
    use std::collections::BTreeMap;
    use std::fs;

    use tempfile::tempdir;
    use vessel_core::{ContainerId, VesselPaths};

    #[test]
    fn discovers_default_compose_file_names() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("compose.yaml");
        fs::write(&path, "services: {}\n").expect("write compose");
        assert_eq!(discover_compose_file(dir.path()).expect("discover"), path);
    }

    #[test]
    fn parses_yaml_and_resolves_relative_mounts() {
        let dir = tempdir().expect("tempdir");
        let host_dir = dir.path().join("app");
        fs::create_dir_all(&host_dir).expect("mkdir");
        let file = dir.path().join("compose.yaml");
        fs::write(
            &file,
            r#"
name: demo
services:
  db:
    image: postgres:16
  api:
    image: ghcr.io/acme/api:latest
    command: "bundle exec rackup"
    environment:
      RACK_ENV: development
    volumes:
      - ./app:/workspace
    depends_on:
      - db
"#,
        )
        .expect("write compose");

        let project = load_project(&file, None).expect("load project");
        let api = project.services.get("api").expect("api");
        assert_eq!(
            api.command,
            Some(vec!["/bin/sh".to_string(), "-c".to_string(), "bundle exec rackup".to_string()])
        );
        assert_eq!(api.environment.get("RACK_ENV").map(String::as_str), Some("development"));
        assert_eq!(
            api.mounts
                .get(&host_dir.canonicalize().expect("canonical").to_string_lossy().to_string())
                .map(String::as_str),
            Some("/workspace")
        );
    }

    #[test]
    fn orders_services_by_dependencies() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("compose.yaml");
        fs::write(
            &file,
            r#"
services:
  db:
    image: postgres:16
  cache:
    image: redis:7
  api:
    image: ghcr.io/acme/api:latest
    depends_on: [db, cache]
"#,
        )
        .expect("write compose");

        let project = load_project(&file, None).expect("load project");
        let start_order = services_in_dependency_order(&project).expect("order");
        let stop_order = services_in_shutdown_order(&project).expect("reverse order");
        let start_names = start_order.into_iter().map(|(name, _)| name).collect::<Vec<_>>();
        let api_index = start_names.iter().position(|name| name == "api").expect("api present");
        let db_index = start_names.iter().position(|name| name == "db").expect("db present");
        let cache_index =
            start_names.iter().position(|name| name == "cache").expect("cache present");
        assert!(db_index < api_index);
        assert!(cache_index < api_index);
        assert_eq!(stop_order[0].0, "api");
    }

    #[test]
    fn rejects_dependency_cycles() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("compose.yaml");
        fs::write(
            &file,
            r#"
services:
  a:
    image: alpine
    depends_on: [b]
  b:
    image: alpine
    depends_on: [a]
"#,
        )
        .expect("write compose");

        let project = load_project(&file, None).expect("load project");
        let error = services_in_dependency_order(&project).expect_err("cycle should fail");
        assert!(error.to_string().contains("dependency cycle"));
    }

    #[test]
    fn saves_and_loads_project_state() {
        let dir = tempdir().expect("tempdir");
        let paths = VesselPaths {
            state_dir: dir.path().join("state"),
            data_dir: dir.path().join("data"),
            blobs_dir: dir.path().join("data/blobs"),
            rootfs_dir: dir.path().join("data/rootfs"),
            bundles_dir: dir.path().join("data/bundles"),
        };
        paths.ensure().expect("paths");

        let state = ComposeProjectState {
            project_name: "demo".to_string(),
            source_file: dir.path().join("compose.yaml"),
            services: BTreeMap::from([(
                "api".to_string(),
                ManagedServiceState {
                    container_id: ContainerId::generate(),
                    spec: super::ComposeServiceSpec {
                        image: "alpine".to_string(),
                        command: None,
                        environment: BTreeMap::new(),
                        mounts: BTreeMap::new(),
                        ports: BTreeMap::new(),
                        depends_on: Vec::new(),
                    },
                },
            )]),
        };

        save_state(&paths, &state).expect("save");
        let loaded = load_state(&paths, "demo").expect("load").expect("state");
        assert_eq!(loaded, state);
        assert!(state_path(&paths, "demo").exists());
    }
}
