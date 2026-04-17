use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

use axum::{
    Json,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
#[cfg(unix)]
use axum::{
    Router,
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use tokio::net::UnixListener;
use vessel_core::{
    ContainerId, ContainerRecord, ContainerStore, ImageRef, VesselError, VesselPaths,
};
use vessel_image::{CachedImageSummary, GarbageCollectionSummary, ImageStore};
use vessel_runtime::{RunOutcome, default_runtime};

#[derive(Clone)]
pub struct DaemonState {
    pub backend: LocalBackend,
}

pub trait Backend {
    fn list_containers(&self) -> Result<Vec<ContainerRecord>, VesselError>;
    fn run_container(
        &self,
        image: &ImageRef,
        detach: bool,
        command_override: Option<Vec<String>>,
        env_override: Option<BTreeMap<String, String>>,
        mount_override: Option<BTreeMap<String, String>>,
    ) -> Result<RunContainerResponse, VesselError>;
    fn start_container(&self, id: &ContainerId) -> Result<ContainerRecord, VesselError>;
    fn stop_container(&self, id: &ContainerId) -> Result<ContainerRecord, VesselError>;
    fn kill_container(&self, id: &ContainerId) -> Result<ContainerRecord, VesselError>;
    fn remove_container(&self, id: &ContainerId) -> Result<(), VesselError>;
    fn remove_image(&self, image: &ImageRef) -> Result<(), VesselError>;
    fn garbage_collect_images(&self) -> Result<GarbageCollectionSummary, VesselError>;
    fn get_container_logs(&self, id: &ContainerId) -> Result<String, VesselError>;
}

#[derive(Debug, Clone)]
pub struct LocalBackend {
    paths: VesselPaths,
    store: ContainerStore,
}

#[derive(Debug)]
pub struct RemoteBackend {
    socket_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunContainerRequest {
    pub image: String,
    pub detach: bool,
    pub command_override: Option<Vec<String>>,
    pub env_override: Option<BTreeMap<String, String>>,
    pub mount_override: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunContainerResponse {
    pub record: ContainerRecord,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveImageRequest {
    pub image: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ApiErrorResponse {
    error: String,
}

#[derive(Debug)]
struct ApiError(VesselError);

impl LocalBackend {
    pub fn new(paths: VesselPaths) -> Self {
        let store = ContainerStore::new(paths.state_dir.clone());
        Self { paths, store }
    }

    fn runtime(&self) -> Box<dyn vessel_runtime::Runtime> {
        default_runtime(self.paths.clone())
    }

    pub fn list_images(&self) -> Result<Vec<CachedImageSummary>, VesselError> {
        ImageStore::new(self.paths.clone()).list()
    }
}

impl Backend for LocalBackend {
    fn list_containers(&self) -> Result<Vec<ContainerRecord>, VesselError> {
        self.store.list()
    }

    fn run_container(
        &self,
        image: &ImageRef,
        detach: bool,
        command_override: Option<Vec<String>>,
        env_override: Option<BTreeMap<String, String>>,
        mount_override: Option<BTreeMap<String, String>>,
    ) -> Result<RunContainerResponse, VesselError> {
        let runtime = self.runtime();
        let report = runtime.capability_report();
        if !report.supported {
            return Err(VesselError::Capability(report.missing.join(", ")));
        }

        let RunOutcome { record, exit_code } = runtime.run(
            &self.store,
            image,
            detach,
            command_override,
            env_override,
            mount_override,
        )?;
        Ok(RunContainerResponse { record, exit_code })
    }

    fn start_container(&self, id: &ContainerId) -> Result<ContainerRecord, VesselError> {
        self.runtime().start(&self.store, id)
    }

    fn stop_container(&self, id: &ContainerId) -> Result<ContainerRecord, VesselError> {
        self.runtime().stop(&self.store, id)
    }

    fn kill_container(&self, id: &ContainerId) -> Result<ContainerRecord, VesselError> {
        self.runtime().kill(&self.store, id)
    }

    fn remove_container(&self, id: &ContainerId) -> Result<(), VesselError> {
        self.runtime().remove(&self.store, id)
    }

    fn remove_image(&self, image: &ImageRef) -> Result<(), VesselError> {
        let records = self.store.list()?;
        if let Some(record) = records.iter().find(|record| &record.image == image) {
            return Err(VesselError::Usage(format!(
                "image `{}` is in use by container `{}`",
                image,
                record.id()
            )));
        }
        ImageStore::new(self.paths.clone()).remove(image)
    }

    fn garbage_collect_images(&self) -> Result<GarbageCollectionSummary, VesselError> {
        ImageStore::new(self.paths.clone()).garbage_collect()
    }

    fn get_container_logs(&self, id: &ContainerId) -> Result<String, VesselError> {
        let record = self.store.load(id)?;
        let log_path = self.paths.bundles_dir.join(record.id().as_str()).join("stdio.log");
        if !log_path.exists() {
            return Ok(String::new());
        }

        std::fs::read_to_string(&log_path).map_err(|source| VesselError::io(&log_path, source))
    }
}

impl RemoteBackend {
    pub fn new(socket_path: PathBuf) -> Result<Self, VesselError> {
        Ok(Self { socket_path })
    }

    fn execute_json<T>(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<T, VesselError>
    where
        T: serde::de::DeserializeOwned,
    {
        let response = self.execute(method, path, body)?;
        serde_json::from_slice(&response.body)
            .map_err(|error| VesselError::Serialization(error.to_string()))
    }

    fn execute_empty(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<(), VesselError> {
        self.execute(method, path, body).map(|_| ())
    }

    fn execute_text(&self, method: &str, path: &str) -> Result<String, VesselError> {
        let response = self.execute(method, path, None)?;
        String::from_utf8(response.body).map_err(|error| VesselError::Runtime(error.to_string()))
    }

    fn execute(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse, VesselError> {
        #[cfg(not(unix))]
        {
            let _ = (method, path, body);
            Err(VesselError::UnsupportedPlatform("daemon is only supported on unix".into()))
        }
        #[cfg(unix)]
        {
            let mut stream = UnixStream::connect(&self.socket_path)
                .map_err(|source| VesselError::io(&self.socket_path, source))?;

            let body = body.unwrap_or_default();
            let request = format!(
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );

            stream.write_all(request.as_bytes()).map_err(VesselError::GenericIo)?;
            if !body.is_empty() {
                stream.write_all(&body).map_err(VesselError::GenericIo)?;
            }
            stream.flush().map_err(VesselError::GenericIo)?;

            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).map_err(VesselError::GenericIo)?;
            let response = parse_http_response(&bytes)?;
            if !(200..300).contains(&response.status) {
                return Err(response_to_error(response));
            }
            Ok(response)
        }
    }
}

impl Backend for RemoteBackend {
    fn list_containers(&self) -> Result<Vec<ContainerRecord>, VesselError> {
        self.execute_json("GET", "/containers", None)
    }

    fn run_container(
        &self,
        image: &ImageRef,
        detach: bool,
        command_override: Option<Vec<String>>,
        env_override: Option<BTreeMap<String, String>>,
        mount_override: Option<BTreeMap<String, String>>,
    ) -> Result<RunContainerResponse, VesselError> {
        let body = RunContainerRequest {
            image: image.to_string(),
            detach,
            command_override,
            env_override,
            mount_override,
        };
        self.execute_json(
            "POST",
            "/containers",
            Some(
                serde_json::to_vec(&body)
                    .map_err(|error| VesselError::Serialization(error.to_string()))?,
            ),
        )
    }

    fn start_container(&self, id: &ContainerId) -> Result<ContainerRecord, VesselError> {
        self.execute_json("POST", &format!("/containers/{id}/start"), None)
    }

    fn stop_container(&self, id: &ContainerId) -> Result<ContainerRecord, VesselError> {
        self.execute_json("POST", &format!("/containers/{id}/stop"), None)
    }

    fn kill_container(&self, id: &ContainerId) -> Result<ContainerRecord, VesselError> {
        self.execute_json("POST", &format!("/containers/{id}/kill"), None)
    }

    fn remove_container(&self, id: &ContainerId) -> Result<(), VesselError> {
        self.execute_empty("DELETE", &format!("/containers/{id}"), None)
    }

    fn remove_image(&self, image: &ImageRef) -> Result<(), VesselError> {
        let body = RemoveImageRequest { image: image.to_string() };
        self.execute_empty(
            "POST",
            "/images/remove",
            Some(
                serde_json::to_vec(&body)
                    .map_err(|error| VesselError::Serialization(error.to_string()))?,
            ),
        )
    }

    fn garbage_collect_images(&self) -> Result<GarbageCollectionSummary, VesselError> {
        self.execute_json("POST", "/images/gc", None)
    }

    fn get_container_logs(&self, id: &ContainerId) -> Result<String, VesselError> {
        self.execute_text("GET", &format!("/containers/{id}/logs"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0 {
            VesselError::Usage(_) | VesselError::InvalidImageReference(_) => {
                StatusCode::BAD_REQUEST
            }
            VesselError::ContainerNotFound(_) => StatusCode::NOT_FOUND,
            VesselError::ContainerAlreadyRunning(_) | VesselError::ContainerNotRunning(_) => {
                StatusCode::CONFLICT
            }
            VesselError::UnsupportedPlatform(_) | VesselError::Capability(_) => {
                StatusCode::FAILED_DEPENDENCY
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = Json(ApiErrorResponse { error: self.0.to_string() });
        (status, body).into_response()
    }
}

pub fn socket_path(paths: &VesselPaths) -> PathBuf {
    paths.data_dir.join("vessel.sock")
}

pub fn pid_path(paths: &VesselPaths) -> PathBuf {
    paths.data_dir.join("vesseld.pid")
}

pub fn log_path(paths: &VesselPaths) -> PathBuf {
    paths.data_dir.join("vesseld.log")
}

pub async fn run_daemon(
    paths: VesselPaths,
    socket_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(not(unix))]
    {
        let _ = (paths, socket_path);
        Err("daemon is only supported on unix".into())
    }
    #[cfg(unix)]
    {
        if socket_path.exists() {
            fs::remove_file(socket_path)?;
        }
        if let Some(parent) = socket_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let pid_path = pid_path(&paths);
        if pid_path.exists() {
            fs::remove_file(&pid_path)?;
        }

        let state = DaemonState { backend: LocalBackend::new(paths) };

        let app = Router::new()
            .route("/containers", get(list_containers))
            .route("/containers", post(run_container))
            .route("/containers/{id}/start", post(start_container))
            .route("/containers/{id}/stop", post(stop_container))
            .route("/containers/{id}/kill", post(kill_container))
            .route("/containers/{id}", delete(remove_container))
            .route("/containers/{id}/logs", get(container_logs))
            .route("/images", get(list_images))
            .route("/images/gc", post(garbage_collect_images))
            .route("/images/remove", post(remove_image))
            .with_state(state);

        let listener = UnixListener::bind(socket_path)?;
        fs::write(&pid_path, std::process::id().to_string())?;
        let _guard = DaemonGuard { socket_path: socket_path.to_path_buf(), pid_path };
        axum::serve(listener, app).await?;

        Ok(())
    }
}

struct DaemonGuard {
    socket_path: PathBuf,
    pid_path: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
        let _ = fs::remove_file(&self.pid_path);
    }
}

async fn list_containers(
    State(state): State<DaemonState>,
) -> Result<Json<Vec<ContainerRecord>>, ApiError> {
    state.backend.list_containers().map(Json).map_err(ApiError)
}

async fn run_container(
    State(state): State<DaemonState>,
    Json(request): Json<RunContainerRequest>,
) -> Result<Json<RunContainerResponse>, ApiError> {
    let image: ImageRef = request.image.parse().map_err(ApiError)?;
    state
        .backend
        .run_container(
            &image,
            request.detach,
            request.command_override,
            request.env_override,
            request.mount_override,
        )
        .map(Json)
        .map_err(ApiError)
}

async fn start_container(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ContainerRecord>, ApiError> {
    let id: ContainerId = id.parse().map_err(ApiError)?;
    state.backend.start_container(&id).map(Json).map_err(ApiError)
}

async fn stop_container(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ContainerRecord>, ApiError> {
    let id: ContainerId = id.parse().map_err(ApiError)?;
    state.backend.stop_container(&id).map(Json).map_err(ApiError)
}

async fn kill_container(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ContainerRecord>, ApiError> {
    let id: ContainerId = id.parse().map_err(ApiError)?;
    state.backend.kill_container(&id).map(Json).map_err(ApiError)
}

async fn remove_container(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    let id: ContainerId = id.parse().map_err(ApiError)?;
    state.backend.remove_container(&id).map_err(ApiError)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn container_logs(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> Result<String, ApiError> {
    let id: ContainerId = id.parse().map_err(ApiError)?;
    state.backend.get_container_logs(&id).map_err(ApiError)
}

async fn list_images(
    State(state): State<DaemonState>,
) -> Result<Json<Vec<CachedImageSummary>>, ApiError> {
    state.backend.list_images().map(Json).map_err(ApiError)
}

async fn remove_image(
    State(state): State<DaemonState>,
    Json(request): Json<RemoveImageRequest>,
) -> Result<StatusCode, ApiError> {
    let image: ImageRef = request.image.parse().map_err(ApiError)?;
    state.backend.remove_image(&image).map_err(ApiError)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn garbage_collect_images(
    State(state): State<DaemonState>,
) -> Result<Json<GarbageCollectionSummary>, ApiError> {
    state.backend.garbage_collect_images().map(Json).map_err(ApiError)
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn parse_http_response(bytes: &[u8]) -> Result<HttpResponse, VesselError> {
    let Some((head, body)) =
        bytes.split_at_checked(find_header_terminator(bytes).ok_or_else(|| {
            VesselError::Runtime("daemon returned a malformed HTTP response".to_string())
        })?)
    else {
        return Err(VesselError::Runtime("daemon returned a malformed HTTP response".to_string()));
    };
    let body = body.strip_prefix(b"\r\n\r\n").unwrap_or_default().to_vec();
    let head =
        std::str::from_utf8(head).map_err(|error| VesselError::Runtime(error.to_string()))?;
    let status_line = head.lines().next().ok_or_else(|| {
        VesselError::Runtime("daemon response was missing a status line".to_string())
    })?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| {
            VesselError::Runtime("daemon response was missing a status code".to_string())
        })?
        .parse::<u16>()
        .map_err(|error| VesselError::Runtime(error.to_string()))?;

    Ok(HttpResponse { status, body })
}

fn find_header_terminator(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn response_to_error(response: HttpResponse) -> VesselError {
    let message = serde_json::from_slice::<ApiErrorResponse>(&response.body)
        .map(|error| error.error)
        .unwrap_or_else(|_| String::from_utf8_lossy(&response.body).trim().to_string());

    match response.status {
        400 => VesselError::Usage(message),
        404 => VesselError::ContainerNotFound(message),
        409 => VesselError::Runtime(message),
        424 => VesselError::Capability(message),
        _ => VesselError::Runtime(message),
    }
}
