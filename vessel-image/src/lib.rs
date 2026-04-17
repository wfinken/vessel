use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::Builder;
use ureq::{Agent, AgentBuilder};
use vessel_core::{ImageRef, ImageReference, VesselError, VesselPaths, host_platform};

const ACCEPT_MANIFESTS: &str = concat!(
    "application/vnd.oci.image.index.v1+json,",
    "application/vnd.docker.distribution.manifest.list.v2+json,",
    "application/vnd.oci.image.manifest.v1+json,",
    "application/vnd.docker.distribution.manifest.v2+json"
);

#[derive(Debug, Clone)]
pub struct ImageStore {
    agent: Agent,
    paths: VesselPaths,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRuntimeConfig {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub working_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PulledImage {
    pub image: ImageRef,
    pub manifest_digest: String,
    pub config_digest: String,
    pub rootfs: PathBuf,
    pub runtime: ImageRuntimeConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CachedImageMetadata {
    image: ImageRef,
    manifest_digest: String,
    config_digest: String,
    runtime: ImageRuntimeConfig,
}

#[derive(Debug, Clone, Deserialize)]
struct RegistryDescriptor {
    #[serde(rename = "mediaType")]
    media_type: Option<String>,
    digest: String,
    #[allow(dead_code)]
    size: i64,
    platform: Option<RegistryPlatform>,
}

#[derive(Debug, Clone, Deserialize)]
struct RegistryPlatform {
    architecture: Option<String>,
    os: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RegistryIndex {
    manifests: Vec<RegistryDescriptor>,
}

#[derive(Debug, Clone, Deserialize)]
struct RegistryManifest {
    config: RegistryDescriptor,
    layers: Vec<RegistryDescriptor>,
}

#[derive(Debug, Clone, Deserialize)]
struct ImageConfigEnvelope {
    config: Option<ImageConfigSection>,
}

#[derive(Debug, Clone, Deserialize)]
struct RegistryTokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ImageConfigSection {
    #[serde(rename = "Cmd")]
    cmd: Option<Vec<String>>,
    #[serde(rename = "Entrypoint")]
    entrypoint: Option<Vec<String>>,
    #[serde(rename = "Env")]
    env: Option<Vec<String>>,
    #[serde(rename = "WorkingDir")]
    working_dir: Option<String>,
}

#[derive(Debug, Clone)]
struct DownloadedObject {
    bytes: Vec<u8>,
    digest: String,
    content_type: Option<String>,
}

impl ImageStore {
    pub fn new(paths: VesselPaths) -> Self {
        let agent = AgentBuilder::new().build();
        Self { agent, paths }
    }

    pub fn pull(&self, image: &ImageRef) -> Result<PulledImage, VesselError> {
        self.paths.ensure()?;
        if let Some(cached) = self.read_alias(image)? {
            return Ok(cached);
        }

        let resolved_manifest = self.resolve_manifest(image)?;
        let rootfs_dir = self.paths.rootfs_dir.join(sanitize_for_path(&resolved_manifest.digest));
        let metadata_path = rootfs_dir.join("vessel-image.json");

        if metadata_path.exists() {
            let metadata = self.read_metadata(&metadata_path)?;
            let pulled = PulledImage {
                image: metadata.image,
                manifest_digest: metadata.manifest_digest,
                config_digest: metadata.config_digest,
                rootfs: rootfs_dir,
                runtime: metadata.runtime,
            };
            self.write_alias(&pulled)?;
            return Ok(pulled);
        }

        let config_blob_path = self.cache_blob(image, &resolved_manifest.manifest.config.digest)?;
        let config_blob = fs::read(&config_blob_path)
            .map_err(|source| VesselError::io(&config_blob_path, source))?;
        let runtime = parse_image_config(&config_blob)?;
        let config_digest = resolved_manifest.manifest.config.digest.clone();
        let temp_rootfs = self.create_temp_rootfs()?;

        for layer in &resolved_manifest.manifest.layers {
            let blob_path = self.cache_blob(image, &layer.digest)?;
            apply_layer(&blob_path, layer.media_type.as_deref(), temp_rootfs.path())?;
        }

        let metadata = CachedImageMetadata {
            image: image.clone(),
            manifest_digest: resolved_manifest.digest.clone(),
            config_digest: config_digest.clone(),
            runtime,
        };

        let metadata_bytes = serde_json::to_vec_pretty(&metadata)
            .map_err(|error| VesselError::Serialization(error.to_string()))?;
        fs::write(temp_rootfs.path().join("vessel-image.json"), metadata_bytes).map_err(
            |source| VesselError::io(temp_rootfs.path().join("vessel-image.json"), source),
        )?;

        persist_temp_dir(temp_rootfs, &rootfs_dir)?;

        let metadata = self.read_metadata(&metadata_path)?;
        let pulled = PulledImage {
            image: metadata.image,
            manifest_digest: metadata.manifest_digest,
            config_digest: metadata.config_digest,
            rootfs: rootfs_dir,
            runtime: metadata.runtime,
        };
        self.write_alias(&pulled)?;
        Ok(pulled)
    }

    fn resolve_manifest(&self, image: &ImageRef) -> Result<ResolvedManifest, VesselError> {
        let manifest_ref = match image.reference() {
            ImageReference::Tag(tag) => tag.clone(),
            ImageReference::Digest(digest) => digest.clone(),
        };

        let direct = self.fetch_manifest_object(image, &manifest_ref)?;
        if is_index_media_type(direct.content_type.as_deref(), &direct.bytes) {
            let index: RegistryIndex = serde_json::from_slice(&direct.bytes)
                .map_err(|error| VesselError::Oci(error.to_string()))?;
            let selection = select_manifest_descriptor(&index, host_platform().architecture)?;
            let selected = self.fetch_manifest_object(image, &selection.digest)?;
            return Ok(ResolvedManifest {
                digest: selected.digest,
                manifest: serde_json::from_slice(&selected.bytes)
                    .map_err(|error| VesselError::Oci(error.to_string()))?,
            });
        }

        Ok(ResolvedManifest {
            digest: direct.digest,
            manifest: serde_json::from_slice(&direct.bytes)
                .map_err(|error| VesselError::Oci(error.to_string()))?,
        })
    }

    fn fetch_manifest_object(
        &self,
        image: &ImageRef,
        reference: &str,
    ) -> Result<DownloadedObject, VesselError> {
        let url =
            format!("{}/{}/manifests/{}", image.registry_api_base(), image.repository(), reference);
        self.get_with_bearer_auth(&url, Some(ACCEPT_MANIFESTS), image.scope())
    }

    fn fetch_blob(&self, image: &ImageRef, digest: &str) -> Result<DownloadedObject, VesselError> {
        let url = format!("{}/{}/blobs/{digest}", image.registry_api_base(), image.repository());
        self.get_with_bearer_auth(&url, None, image.scope())
    }

    fn cache_blob(&self, image: &ImageRef, digest: &str) -> Result<PathBuf, VesselError> {
        let target = blob_cache_path(&self.paths.blobs_dir, digest)?;
        if target.exists() {
            return Ok(target);
        }

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|source| VesselError::io(parent, source))?;
        }

        let temp_path = target.with_extension("tmp");
        let download = self.fetch_blob(image, digest)?;
        fs::write(&temp_path, &download.bytes)
            .map_err(|source| VesselError::io(&temp_path, source))?;
        fs::rename(&temp_path, &target).map_err(|source| VesselError::io(&target, source))?;
        Ok(target)
    }

    fn get_with_bearer_auth(
        &self,
        url: &str,
        accept: Option<&str>,
        scope: String,
    ) -> Result<DownloadedObject, VesselError> {
        let mut token = None;

        loop {
            let mut request = self.agent.get(url);
            if let Some(accept) = accept {
                request = request.set("Accept", accept);
            }
            if let Some(token) = &token {
                request = request.set("Authorization", &format!("Bearer {token}"));
            }

            match request.call() {
                Ok(response) => return response_to_bytes(response),
                Err(ureq::Error::Status(401, response)) => {
                    if token.is_some() {
                        return Err(VesselError::Registry(format!(
                            "authentication failed for {url}"
                        )));
                    }

                    let challenge = response.header("WWW-Authenticate").ok_or_else(|| {
                        VesselError::Registry(
                            "registry requested auth without challenge".to_string(),
                        )
                    })?;
                    token = Some(self.fetch_bearer_token(challenge, &scope)?);
                }
                Err(ureq::Error::Status(code, response)) => {
                    let body = response.into_string().unwrap_or_default();
                    return Err(VesselError::Registry(format!(
                        "GET {url} returned {code}: {body}"
                    )));
                }
                Err(error) => return Err(VesselError::Registry(error.to_string())),
            }
        }
    }

    fn fetch_bearer_token(
        &self,
        challenge: &str,
        default_scope: &str,
    ) -> Result<String, VesselError> {
        let params = parse_bearer_challenge(challenge)?;
        let realm = params
            .get("realm")
            .ok_or_else(|| VesselError::Registry("bearer challenge missing realm".to_string()))?;
        let mut url = url::Url::parse(realm)
            .map_err(|error| VesselError::Registry(format!("invalid token endpoint: {error}")))?;
        {
            let mut query = url.query_pairs_mut();
            if let Some(service) = params.get("service") {
                query.append_pair("service", service);
            }
            query.append_pair(
                "scope",
                params.get("scope").map(String::as_str).unwrap_or(default_scope),
            );
        }

        let response = self
            .agent
            .get(url.as_str())
            .call()
            .map_err(|error| VesselError::Registry(error.to_string()))?;
        let token_response: RegistryTokenResponse =
            response.into_json().map_err(|error| VesselError::Registry(error.to_string()))?;
        token_response.token.or(token_response.access_token).ok_or_else(|| {
            VesselError::Registry("registry token response missing token".to_string())
        })
    }

    fn create_temp_rootfs(&self) -> Result<tempfile::TempDir, VesselError> {
        Builder::new()
            .prefix("rootfs-")
            .tempdir_in(&self.paths.rootfs_dir)
            .map_err(|source| VesselError::io(&self.paths.rootfs_dir, source))
    }

    fn read_metadata(&self, metadata_path: &Path) -> Result<CachedImageMetadata, VesselError> {
        let payload =
            fs::read(metadata_path).map_err(|source| VesselError::io(metadata_path, source))?;
        serde_json::from_slice(&payload)
            .map_err(|error| VesselError::Serialization(error.to_string()))
    }

    fn read_alias(&self, image: &ImageRef) -> Result<Option<PulledImage>, VesselError> {
        let alias_path = self.alias_path(image);
        if !alias_path.exists() {
            return Ok(None);
        }

        let metadata = self.read_metadata(&alias_path)?;
        let rootfs = self.paths.rootfs_dir.join(sanitize_for_path(&metadata.manifest_digest));
        if !rootfs.exists() {
            return Ok(None);
        }

        Ok(Some(PulledImage {
            image: metadata.image,
            manifest_digest: metadata.manifest_digest,
            config_digest: metadata.config_digest,
            rootfs,
            runtime: metadata.runtime,
        }))
    }

    fn write_alias(&self, image: &PulledImage) -> Result<(), VesselError> {
        let alias_path = self.alias_path(&image.image);
        if let Some(parent) = alias_path.parent() {
            fs::create_dir_all(parent).map_err(|source| VesselError::io(parent, source))?;
        }

        let metadata = CachedImageMetadata {
            image: image.image.clone(),
            manifest_digest: image.manifest_digest.clone(),
            config_digest: image.config_digest.clone(),
            runtime: image.runtime.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&metadata)
            .map_err(|error| VesselError::Serialization(error.to_string()))?;
        fs::write(&alias_path, bytes).map_err(|source| VesselError::io(&alias_path, source))
    }

    pub fn remove(&self, image: &ImageRef) -> Result<(), VesselError> {
        let alias_path = self.alias_path(image);
        if !alias_path.exists() {
            return Err(VesselError::InvalidImageReference(image.to_string()));
        }

        let metadata = self.read_metadata(&alias_path)?;
        let rootfs = self.paths.rootfs_dir.join(sanitize_for_path(&metadata.manifest_digest));
        if rootfs.exists() {
            fs::remove_dir_all(&rootfs).map_err(|source| VesselError::io(&rootfs, source))?;
        }

        fs::remove_file(&alias_path).map_err(|source| VesselError::io(&alias_path, source))
    }

    fn alias_path(&self, image: &ImageRef) -> PathBuf {
        self.paths
            .data_dir
            .join("images")
            .join(format!("{}.json", sanitize_for_path(&image.canonical_name())))
    }
}

impl PulledImage {
    pub fn resolved_command(
        &self,
        command_override: Option<&[String]>,
    ) -> Result<Vec<String>, VesselError> {
        let command = if let Some(command_override) = command_override {
            if self.runtime.entrypoint.is_empty() {
                command_override.to_vec()
            } else {
                let mut command = self.runtime.entrypoint.clone();
                command.extend(command_override.iter().cloned());
                command
            }
        } else {
            let mut command = self.runtime.entrypoint.clone();
            if command.is_empty() {
                command = self.runtime.cmd.clone();
            } else if !self.runtime.cmd.is_empty() {
                command.extend(self.runtime.cmd.clone());
            }
            command
        };

        if command.is_empty() {
            return Err(VesselError::Oci(format!(
                "image `{}` does not define an entrypoint or cmd",
                self.image
            )));
        }

        Ok(command)
    }
}

#[derive(Debug, Clone)]
struct ResolvedManifest {
    digest: String,
    manifest: RegistryManifest,
}

fn select_manifest_descriptor(
    index: &RegistryIndex,
    architecture: &str,
) -> Result<RegistryDescriptor, VesselError> {
    index
        .manifests
        .iter()
        .find(|descriptor| {
            descriptor.platform.as_ref().is_some_and(|platform| {
                platform.os.as_deref() == Some("linux")
                    && platform.architecture.as_deref() == Some(architecture)
            })
        })
        .cloned()
        .ok_or_else(|| {
            VesselError::Oci(format!("no linux/{architecture} manifest found in image index"))
        })
}

fn response_to_bytes(response: ureq::Response) -> Result<DownloadedObject, VesselError> {
    let content_type = response.header("Content-Type").map(ToOwned::to_owned);
    let digest = response.header("Docker-Content-Digest").map(ToOwned::to_owned);
    let mut reader = response.into_reader();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).map_err(|source| VesselError::Registry(source.to_string()))?;
    let computed = format!("sha256:{}", hex::encode(Sha256::digest(&bytes)));
    Ok(DownloadedObject { bytes, digest: digest.unwrap_or(computed), content_type })
}

fn parse_bearer_challenge(challenge: &str) -> Result<HashMap<String, String>, VesselError> {
    let challenge = challenge.trim();
    let Some((scheme, params)) = challenge.split_once(' ') else {
        return Err(VesselError::Registry("invalid WWW-Authenticate challenge".to_string()));
    };
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return Err(VesselError::Registry(format!("unsupported auth scheme `{scheme}`")));
    }

    let mut values = HashMap::new();
    for part in params.split(',') {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        values.insert(key.to_string(), value.trim_matches('"').to_string());
    }

    Ok(values)
}

fn parse_image_config(bytes: &[u8]) -> Result<ImageRuntimeConfig, VesselError> {
    let envelope: ImageConfigEnvelope =
        serde_json::from_slice(bytes).map_err(|error| VesselError::Oci(error.to_string()))?;
    let config = envelope.config.unwrap_or_default();
    Ok(ImageRuntimeConfig {
        entrypoint: config.entrypoint.unwrap_or_default(),
        cmd: config.cmd.unwrap_or_default(),
        env: config
            .env
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                entry.split_once('=').map(|(key, value)| (key.to_string(), value.to_string()))
            })
            .collect(),
        working_dir: config.working_dir.filter(|workdir| !workdir.is_empty()),
    })
}

fn blob_cache_path(root: &Path, digest: &str) -> Result<PathBuf, VesselError> {
    let (algorithm, encoded) = digest
        .split_once(':')
        .ok_or_else(|| VesselError::Oci(format!("unsupported digest format `{digest}`")))?;
    if algorithm != "sha256" {
        return Err(VesselError::Oci(format!("unsupported digest algorithm `{algorithm}`")));
    }
    Ok(root.join(algorithm).join(encoded))
}

fn sanitize_for_path(value: &str) -> String {
    value.chars().map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' }).collect()
}

fn persist_temp_dir(temp_dir: tempfile::TempDir, destination: &Path) -> Result<(), VesselError> {
    if destination.exists() {
        return Ok(());
    }

    let source = temp_dir.keep();
    fs::rename(&source, destination)
        .map_err(|source_error| VesselError::io(destination, source_error))
}

fn is_index_media_type(content_type: Option<&str>, bytes: &[u8]) -> bool {
    if content_type
        .is_some_and(|value| value.contains("manifest.list") || value.contains("image.index"))
    {
        return true;
    }

    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|value| value.get("manifests").cloned())
        .is_some()
}

fn apply_layer(
    blob_path: &Path,
    media_type: Option<&str>,
    rootfs: &Path,
) -> Result<(), VesselError> {
    let file = fs::File::open(blob_path).map_err(|source| VesselError::io(blob_path, source))?;
    let reader = decompressed_reader(file, media_type)?;
    let mut archive = tar::Archive::new(reader);

    for entry in archive.entries().map_err(|error| VesselError::Runtime(error.to_string()))? {
        let mut entry = entry.map_err(|error| VesselError::Runtime(error.to_string()))?;
        let path =
            entry.path().map_err(|error| VesselError::Runtime(error.to_string()))?.into_owned();

        if path.components().any(|component| matches!(component, std::path::Component::ParentDir)) {
            return Err(VesselError::Oci(format!(
                "layer `{}` attempted to escape the rootfs",
                blob_path.display()
            )));
        }

        if handle_whiteout(&path, rootfs)? {
            continue;
        }

        let target = rootfs.join(&path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|source| VesselError::io(parent, source))?;
        }

        if target.exists() && entry.header().entry_type().is_dir() {
            continue;
        }

        entry.unpack_in(rootfs).map_err(|error| VesselError::Runtime(error.to_string()))?;
    }

    Ok(())
}

fn decompressed_reader(
    file: fs::File,
    media_type: Option<&str>,
) -> Result<Box<dyn Read>, VesselError> {
    if media_type.is_some_and(|value| value.contains("gzip")) {
        return Ok(Box::new(GzDecoder::new(file)));
    }

    if media_type.is_some_and(|value| value.contains("zstd")) {
        let decoder = zstd::Decoder::new(file)
            .map_err(|error| VesselError::Runtime(format!("zstd decode failed: {error}")))?;
        return Ok(Box::new(decoder));
    }

    Ok(Box::new(file))
}

fn handle_whiteout(path: &Path, rootfs: &Path) -> Result<bool, VesselError> {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(false);
    };

    if file_name == ".wh..wh..opq" {
        if let Some(parent) = path.parent() {
            let opaque_dir = rootfs.join(parent);
            if opaque_dir.exists() {
                for child in fs::read_dir(&opaque_dir)
                    .map_err(|source| VesselError::io(&opaque_dir, source))?
                {
                    let child = child.map_err(VesselError::GenericIo)?;
                    remove_path(&child.path())?;
                }
            }
        }
        return Ok(true);
    }

    let Some(target_name) = file_name.strip_prefix(".wh.") else {
        return Ok(false);
    };
    let target = rootfs.join(path.parent().unwrap_or_else(|| Path::new(""))).join(target_name);
    if target.exists() {
        remove_path(&target)?;
    }
    Ok(true)
}

fn remove_path(path: &Path) -> Result<(), VesselError> {
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|source| VesselError::io(path, source))
    } else {
        fs::remove_file(path).map_err(|source| VesselError::io(path, source))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    use flate2::{Compression, write::GzEncoder};
    use sha2::Digest;
    use tar::Builder as TarBuilder;
    use vessel_core::{ImageRef, VesselPaths};

    use super::{ImageRuntimeConfig, ImageStore, PulledImage, sanitize_for_path};

    #[test]
    fn pulls_and_reuses_cached_rootfs() {
        let state = Arc::new(MockRegistryState::new());
        let registry = MockRegistry::spawn(state.clone());
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = VesselPaths {
            state_dir: temp.path().join("state"),
            data_dir: temp.path().join("data"),
            blobs_dir: temp.path().join("data/blobs"),
            rootfs_dir: temp.path().join("data/rootfs"),
            bundles_dir: temp.path().join("data/bundles"),
        };
        paths.ensure().expect("paths");
        let store = ImageStore::new(paths.clone());

        let image: vessel_core::ImageRef =
            format!("localhost:{}/demo/app:latest", registry.port).parse().expect("image ref");
        let pulled = store.pull(&image).expect("pull image");

        assert!(pulled.rootfs.join("bin/hello").exists());
        assert!(
            paths
                .blobs_dir
                .join("sha256")
                .join(state.config_digest.trim_start_matches("sha256:"))
                .exists()
        );
        assert!(pulled.rootfs.join("vessel-image.json").exists());

        let second = store.pull(&image).expect("pull image from cache");
        assert_eq!(pulled.rootfs, second.rootfs);
        assert_eq!(state.requests.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn path_sanitizer_is_stable() {
        assert_eq!(sanitize_for_path("sha256:dead/beef"), "sha256_dead_beef");
    }

    #[test]
    fn command_override_replaces_cmd_without_entrypoint() {
        let image: ImageRef = "docker.io/severalnines/sysbench:latest".parse().expect("image");
        let pulled = PulledImage {
            image,
            manifest_digest: "sha256:manifest".to_string(),
            config_digest: "sha256:config".to_string(),
            rootfs: std::path::PathBuf::from("/tmp/rootfs"),
            runtime: ImageRuntimeConfig {
                entrypoint: Vec::new(),
                cmd: vec!["bash".to_string()],
                env: BTreeMap::new(),
                working_dir: None,
            },
        };

        let override_command =
            vec!["sysbench".to_string(), "cpu".to_string(), "--threads=1".to_string()];
        let command = pulled.resolved_command(Some(&override_command)).expect("resolved command");
        assert_eq!(command, override_command);
    }

    #[test]
    fn command_override_appends_to_entrypoint() {
        let image: ImageRef = "docker.io/library/busybox:latest".parse().expect("image");
        let pulled = PulledImage {
            image,
            manifest_digest: "sha256:manifest".to_string(),
            config_digest: "sha256:config".to_string(),
            rootfs: std::path::PathBuf::from("/tmp/rootfs"),
            runtime: ImageRuntimeConfig {
                entrypoint: vec!["/entrypoint".to_string()],
                cmd: vec!["default".to_string()],
                env: BTreeMap::new(),
                working_dir: None,
            },
        };

        let command = pulled
            .resolved_command(Some(&["override".to_string(), "arg".to_string()]))
            .expect("resolved command");
        assert_eq!(command, vec!["/entrypoint", "override", "arg"]);
    }

    struct MockRegistry {
        port: u16,
        _thread: thread::JoinHandle<()>,
    }

    impl MockRegistry {
        fn spawn(state: Arc<MockRegistryState>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let thread = thread::spawn(move || {
                for stream in listener.incoming().take(3) {
                    let mut stream = stream.expect("stream");
                    let mut request = [0_u8; 4096];
                    let count = stream.read(&mut request).expect("read request");
                    let request = String::from_utf8_lossy(&request[..count]);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .expect("path");
                    let (body, content_type, digest) = state.response(path);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {}\r\nDocker-Content-Digest: {}\r\nConnection: close\r\n\r\n",
                        body.len(),
                        content_type,
                        digest
                    );
                    stream.write_all(response.as_bytes()).expect("headers");
                    stream.write_all(&body).expect("body");
                }
            });

            Self { port, _thread: thread }
        }
    }

    struct MockRegistryState {
        config_digest: String,
        layer_digest: String,
        manifest_digest: String,
        requests: AtomicUsize,
        config_payload: Vec<u8>,
        manifest_payload: Vec<u8>,
        layer_payload: Vec<u8>,
    }

    impl MockRegistryState {
        fn new() -> Self {
            let layer_payload = gzip_layer_payload();
            let layer_digest = digest_of(&layer_payload);
            let config_payload = serde_json::json!({
                "config": {
                    "Cmd": ["/bin/hello"],
                    "Env": ["PATH=/bin:/usr/bin"],
                    "WorkingDir": "/"
                }
            })
            .to_string()
            .into_bytes();
            let config_digest = digest_of(&config_payload);
            let manifest_payload = serde_json::json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {
                    "mediaType": "application/vnd.oci.image.config.v1+json",
                    "digest": config_digest,
                    "size": config_payload.len()
                },
                "layers": [{
                    "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                    "digest": layer_digest,
                    "size": layer_payload.len()
                }]
            })
            .to_string()
            .into_bytes();
            let manifest_digest = digest_of(&manifest_payload);

            Self {
                config_digest,
                layer_digest,
                manifest_digest,
                requests: AtomicUsize::new(0),
                config_payload,
                manifest_payload,
                layer_payload,
            }
        }

        fn response(&self, path: &str) -> (Vec<u8>, &'static str, String) {
            self.requests.fetch_add(1, Ordering::Relaxed);
            match path {
                "/v2/demo/app/manifests/latest" => (
                    self.manifest_payload.clone(),
                    "application/vnd.oci.image.manifest.v1+json",
                    self.manifest_digest.clone(),
                ),
                value if value.ends_with(&format!("/blobs/{}", self.config_digest)) => (
                    self.config_payload.clone(),
                    "application/vnd.oci.image.config.v1+json",
                    self.config_digest.clone(),
                ),
                value if value.ends_with(&format!("/blobs/{}", self.layer_digest)) => (
                    self.layer_payload.clone(),
                    "application/vnd.oci.image.layer.v1.tar+gzip",
                    self.layer_digest.clone(),
                ),
                other => panic!("unexpected path: {other}"),
            }
        }
    }

    fn digest_of(bytes: &[u8]) -> String {
        format!("sha256:{}", hex::encode(sha2::Sha256::digest(bytes)))
    }

    fn gzip_layer_payload() -> Vec<u8> {
        let tar_bytes = {
            let mut tar_buffer = Vec::new();
            {
                let mut builder = TarBuilder::new(&mut tar_buffer);
                let content = b"#!/bin/sh\necho vessel\n";
                let mut header = tar::Header::new_gnu();
                header.set_mode(0o755);
                header.set_size(content.len() as u64);
                header.set_cksum();
                builder.append_data(&mut header, "bin/hello", &content[..]).expect("append");
                builder.finish().expect("finish tar");
            }
            tar_buffer
        };

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_bytes).expect("gzip write");
        encoder.finish().expect("gzip finish")
    }
}
