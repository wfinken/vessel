use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    io::Read,
    path::{Path, PathBuf},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::Builder;
use ureq::{Agent, AgentBuilder};
use vessel_core::{
    ContainerStore, ImageRef, ImageReference, VesselError, VesselPaths, host_platform,
};

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
    auth_files: Vec<PathBuf>,
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
    pub layers: Vec<PathBuf>,
    pub runtime: ImageRuntimeConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedImageSummary {
    pub name: String,
    pub layers: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GarbageCollectionSummary {
    pub removed_layers: usize,
    pub removed_blobs: usize,
    pub reclaimed_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CachedImageMetadata {
    image: ImageRef,
    manifest_digest: String,
    config_digest: String,
    layers: Vec<PathBuf>,
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
struct RegistryAuthFile {
    #[serde(default)]
    auths: HashMap<String, RegistryAuthEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RegistryAuthEntry {
    auth: Option<String>,
    username: Option<String>,
    password: Option<String>,
    #[serde(alias = "identitytoken")]
    identity_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RegistryCredentials {
    username: Option<String>,
    password: Option<String>,
    identity_token: Option<String>,
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
        Self { agent, paths, auth_files: default_registry_auth_files() }
    }

    pub fn pull(&self, image: &ImageRef) -> Result<PulledImage, VesselError> {
        self.paths.ensure()?;
        if let Some(cached) = self.read_alias(image)? {
            return Ok(cached);
        }

        let credentials = self.registry_credentials(image)?;
        let resolved_manifest = self.resolve_manifest(image, credentials.as_ref())?;
        let config_blob_path = self.cache_blob(
            image,
            &resolved_manifest.manifest.config.digest,
            credentials.as_ref(),
        )?;
        let config_blob = fs::read(&config_blob_path)
            .map_err(|source| VesselError::io(&config_blob_path, source))?;
        let runtime = parse_image_config(&config_blob)?;
        let config_digest = resolved_manifest.manifest.config.digest.clone();

        let mut layers = Vec::new();
        for layer in &resolved_manifest.manifest.layers {
            let layer_dir = self.paths.rootfs_dir.join(sanitize_for_path(&layer.digest));
            if !layer_dir.exists() {
                let blob_path = self.cache_blob(image, &layer.digest, credentials.as_ref())?;
                let temp_layer = self.create_temp_rootfs()?;
                apply_layer(&blob_path, layer.media_type.as_deref(), temp_layer.path())?;
                persist_temp_dir(temp_layer, &layer_dir)?;
            }
            layers.push(layer_dir);
        }

        let pulled = PulledImage {
            image: image.clone(),
            manifest_digest: resolved_manifest.digest,
            config_digest,
            layers,
            runtime,
        };

        self.write_alias(&pulled)?;
        Ok(pulled)
    }

    #[cfg(test)]
    fn with_auth_files(paths: VesselPaths, auth_files: Vec<PathBuf>) -> Self {
        let agent = AgentBuilder::new().build();
        Self { agent, paths, auth_files }
    }

    fn resolve_manifest(
        &self,
        image: &ImageRef,
        credentials: Option<&RegistryCredentials>,
    ) -> Result<ResolvedManifest, VesselError> {
        let manifest_ref = match image.reference() {
            ImageReference::Tag(tag) => tag.clone(),
            ImageReference::Digest(digest) => digest.clone(),
        };

        let direct = self.fetch_manifest_object(image, &manifest_ref, credentials)?;
        if is_index_media_type(direct.content_type.as_deref(), &direct.bytes) {
            let index: RegistryIndex = serde_json::from_slice(&direct.bytes)
                .map_err(|error| VesselError::Oci(error.to_string()))?;
            let selection = select_manifest_descriptor(&index, host_platform().architecture)?;
            let selected = self.fetch_manifest_object(image, &selection.digest, credentials)?;
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
        credentials: Option<&RegistryCredentials>,
    ) -> Result<DownloadedObject, VesselError> {
        let url =
            format!("{}/{}/manifests/{}", image.registry_api_base(), image.repository(), reference);
        self.get_with_registry_auth(&url, Some(ACCEPT_MANIFESTS), image.scope(), credentials)
    }

    fn fetch_blob(
        &self,
        image: &ImageRef,
        digest: &str,
        credentials: Option<&RegistryCredentials>,
    ) -> Result<DownloadedObject, VesselError> {
        let url = format!("{}/{}/blobs/{digest}", image.registry_api_base(), image.repository());
        self.get_with_registry_auth(&url, None, image.scope(), credentials)
    }

    fn cache_blob(
        &self,
        image: &ImageRef,
        digest: &str,
        credentials: Option<&RegistryCredentials>,
    ) -> Result<PathBuf, VesselError> {
        let target = blob_cache_path(&self.paths.blobs_dir, digest)?;
        if target.exists() {
            return Ok(target);
        }

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|source| VesselError::io(parent, source))?;
        }

        let temp_path = target.with_extension("tmp");
        let download = self.fetch_blob(image, digest, credentials)?;
        fs::write(&temp_path, &download.bytes)
            .map_err(|source| VesselError::io(&temp_path, source))?;
        fs::rename(&temp_path, &target).map_err(|source| VesselError::io(&target, source))?;
        Ok(target)
    }

    fn get_with_registry_auth(
        &self,
        url: &str,
        accept: Option<&str>,
        scope: String,
        credentials: Option<&RegistryCredentials>,
    ) -> Result<DownloadedObject, VesselError> {
        let basic_auth = credentials.and_then(RegistryCredentials::basic_authorization);
        let mut bearer_token =
            credentials.and_then(|credentials| credentials.identity_token.clone());
        let mut fetched_bearer_token = false;

        loop {
            let mut request = self.agent.get(url);
            if let Some(accept) = accept {
                request = request.set("Accept", accept);
            }
            if let Some(token) = &bearer_token {
                request = request.set("Authorization", &format!("Bearer {token}"));
            } else if let Some(basic_auth) = &basic_auth {
                request = request.set("Authorization", basic_auth);
            }

            match request.call() {
                Ok(response) => return response_to_bytes(response),
                Err(ureq::Error::Status(401, response)) => {
                    let challenge = response.header("WWW-Authenticate").ok_or_else(|| {
                        VesselError::Registry(
                            "registry requested auth without challenge".to_string(),
                        )
                    })?;
                    match parse_auth_challenge(challenge)? {
                        RegistryAuthChallenge::Basic => {
                            return Err(registry_auth_error(url, basic_auth.is_some()));
                        }
                        RegistryAuthChallenge::Bearer(params) => {
                            if fetched_bearer_token {
                                return Err(registry_auth_error(url, basic_auth.is_some()));
                            }
                            bearer_token = Some(self.fetch_bearer_token(
                                &params,
                                &scope,
                                basic_auth.as_deref(),
                            )?);
                            fetched_bearer_token = true;
                        }
                    }
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
        challenge: &HashMap<String, String>,
        default_scope: &str,
        basic_auth: Option<&str>,
    ) -> Result<String, VesselError> {
        let realm = challenge
            .get("realm")
            .ok_or_else(|| VesselError::Registry("bearer challenge missing realm".to_string()))?;
        let mut url = url::Url::parse(realm)
            .map_err(|error| VesselError::Registry(format!("invalid token endpoint: {error}")))?;
        {
            let mut query = url.query_pairs_mut();
            if let Some(service) = challenge.get("service") {
                query.append_pair("service", service);
            }
            query.append_pair(
                "scope",
                challenge.get("scope").map(String::as_str).unwrap_or(default_scope),
            );
        }

        let mut request = self.agent.get(url.as_str());
        if let Some(basic_auth) = basic_auth {
            request = request.set("Authorization", basic_auth);
        }
        let response = request.call().map_err(|error| VesselError::Registry(error.to_string()))?;
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

    fn registry_credentials(
        &self,
        image: &ImageRef,
    ) -> Result<Option<RegistryCredentials>, VesselError> {
        let target = normalize_registry_key(image.registry());

        for path in &self.auth_files {
            if !path.exists() {
                continue;
            }

            let payload = fs::read(path).map_err(|source| VesselError::io(path, source))?;
            let auth_file: RegistryAuthFile = serde_json::from_slice(&payload)
                .map_err(|error| VesselError::Serialization(error.to_string()))?;

            for (registry, entry) in auth_file.auths {
                if normalize_registry_key(&registry) == target {
                    return entry.into_credentials();
                }
            }
        }

        Ok(None)
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
        for layer in &metadata.layers {
            if !layer.exists() {
                return Ok(None);
            }
        }

        Ok(Some(PulledImage {
            image: metadata.image,
            manifest_digest: metadata.manifest_digest,
            config_digest: metadata.config_digest,
            layers: metadata.layers,
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
            layers: image.layers.clone(),
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

        // For now, only remove the alias. Individual layers are shared and should be
        // removed by a separate garbage collection mechanism.
        fs::remove_file(&alias_path).map_err(|source| VesselError::io(&alias_path, source))
    }

    pub fn list(&self) -> Result<Vec<CachedImageSummary>, VesselError> {
        let images_dir = self.paths.data_dir.join("images");
        if !images_dir.exists() {
            return Ok(Vec::new());
        }

        let mut images = Vec::new();
        for entry in
            fs::read_dir(&images_dir).map_err(|source| VesselError::io(&images_dir, source))?
        {
            let entry = entry.map_err(VesselError::GenericIo)?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }

            let metadata = self.read_metadata(&path)?;
            images.push(CachedImageSummary {
                name: metadata.image.to_string(),
                layers: metadata.layers.len(),
            });
        }

        images.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(images)
    }

    pub fn garbage_collect(&self) -> Result<GarbageCollectionSummary, VesselError> {
        self.paths.ensure()?;

        let referenced_layers = self.referenced_layers()?;
        let referenced_blobs = self.referenced_blobs()?;
        let mut summary = GarbageCollectionSummary::default();

        for entry in fs::read_dir(&self.paths.rootfs_dir)
            .map_err(|source| VesselError::io(&self.paths.rootfs_dir, source))?
        {
            let entry = entry.map_err(VesselError::GenericIo)?;
            let path = entry.path();
            if referenced_layers.contains(&path) {
                continue;
            }

            summary.reclaimed_bytes += path_size(&path)?;
            remove_path(&path)?;
            summary.removed_layers += 1;
        }

        collect_stale_blobs(&self.paths.blobs_dir, &referenced_blobs, &mut summary)?;
        remove_empty_directories(&self.paths.blobs_dir)?;

        Ok(summary)
    }

    fn alias_path(&self, image: &ImageRef) -> PathBuf {
        self.paths
            .data_dir
            .join("images")
            .join(format!("{}.json", sanitize_for_path(&image.canonical_name())))
    }

    fn alias_metadata(&self) -> Result<Vec<CachedImageMetadata>, VesselError> {
        let images_dir = self.paths.data_dir.join("images");
        if !images_dir.exists() {
            return Ok(Vec::new());
        }

        let mut metadata = Vec::new();
        for entry in
            fs::read_dir(&images_dir).map_err(|source| VesselError::io(&images_dir, source))?
        {
            let entry = entry.map_err(VesselError::GenericIo)?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            metadata.push(self.read_metadata(&path)?);
        }
        Ok(metadata)
    }

    fn referenced_layers(&self) -> Result<HashSet<PathBuf>, VesselError> {
        let mut layers = HashSet::new();

        for metadata in self.alias_metadata()? {
            layers.extend(metadata.layers);
        }

        let store = ContainerStore::new(self.paths.state_dir.clone());
        for record in store.list()? {
            layers.extend(record.layers);
        }

        Ok(layers)
    }

    fn referenced_blobs(&self) -> Result<HashSet<PathBuf>, VesselError> {
        let mut blobs = HashSet::new();

        for metadata in self.alias_metadata()? {
            blobs.insert(blob_cache_path(&self.paths.blobs_dir, &metadata.manifest_digest)?);
            blobs.insert(blob_cache_path(&self.paths.blobs_dir, &metadata.config_digest)?);
        }

        Ok(blobs)
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum RegistryAuthChallenge {
    Basic,
    Bearer(HashMap<String, String>),
}

fn default_registry_auth_files() -> Vec<PathBuf> {
    let mut files = Vec::new();

    if let Ok(path) = env::var("VESSEL_REGISTRY_AUTH_FILE") {
        push_auth_file(&mut files, PathBuf::from(path));
    }
    if let Ok(path) = env::var("REGISTRY_AUTH_FILE") {
        push_auth_file(&mut files, PathBuf::from(path));
    }
    if let Ok(path) = env::var("DOCKER_CONFIG") {
        push_auth_file(&mut files, PathBuf::from(path).join("config.json"));
    }
    if let Ok(home) = env::var("HOME") {
        push_auth_file(&mut files, PathBuf::from(&home).join(".docker/config.json"));
    }
    if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
        push_auth_file(&mut files, PathBuf::from(runtime_dir).join("containers/auth.json"));
    }
    if let Ok(config_dir) = env::var("XDG_CONFIG_HOME") {
        push_auth_file(&mut files, PathBuf::from(config_dir).join("containers/auth.json"));
    }
    if let Ok(home) = env::var("HOME") {
        push_auth_file(&mut files, PathBuf::from(home).join(".config/containers/auth.json"));
    }

    files
}

fn push_auth_file(files: &mut Vec<PathBuf>, path: PathBuf) {
    if files.iter().all(|candidate| candidate != &path) {
        files.push(path);
    }
}

impl RegistryAuthEntry {
    fn into_credentials(self) -> Result<Option<RegistryCredentials>, VesselError> {
        let mut username = self.username;
        let mut password = self.password;

        if (username.is_none() || password.is_none()) && self.auth.is_some() {
            let auth = self
                .auth
                .as_deref()
                .ok_or_else(|| VesselError::Registry("missing auth payload".to_string()))?;
            let (decoded_username, decoded_password) = decode_registry_auth(auth)?;
            username.get_or_insert(decoded_username);
            password.get_or_insert(decoded_password);
        }

        if username.is_none() && password.is_none() && self.identity_token.is_none() {
            return Ok(None);
        }

        Ok(Some(RegistryCredentials { username, password, identity_token: self.identity_token }))
    }
}

impl RegistryCredentials {
    fn basic_authorization(&self) -> Option<String> {
        let username = self.username.as_ref()?;
        let password = self.password.as_deref().unwrap_or_default();
        Some(format_basic_auth(username, password))
    }
}

fn decode_registry_auth(auth: &str) -> Result<(String, String), VesselError> {
    let decoded = STANDARD
        .decode(auth)
        .map_err(|error| VesselError::Registry(format!("invalid registry auth entry: {error}")))?;
    let decoded = String::from_utf8(decoded)
        .map_err(|error| VesselError::Registry(format!("invalid registry auth entry: {error}")))?;
    let (username, password) = decoded.split_once(':').ok_or_else(|| {
        VesselError::Registry("registry auth entry missing username/password".to_string())
    })?;
    Ok((username.to_string(), password.to_string()))
}

fn format_basic_auth(username: &str, password: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{username}:{password}")))
}

fn normalize_registry_key(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    let candidate =
        if value.contains("://") { value.to_string() } else { format!("https://{value}") };
    let url = url::Url::parse(&candidate).ok()?;
    let host = url.host_str()?;
    let host = match host {
        "registry-1.docker.io" | "index.docker.io" => "docker.io",
        other => other,
    };

    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

fn registry_auth_error(url: &str, has_credentials: bool) -> VesselError {
    if has_credentials {
        VesselError::Registry(format!("authentication failed for {url}"))
    } else {
        VesselError::Registry(format!(
            "authentication required for {url}; configure registry credentials first"
        ))
    }
}

fn parse_auth_challenge(challenge: &str) -> Result<RegistryAuthChallenge, VesselError> {
    let challenge = challenge.trim();
    let Some((scheme, params)) = challenge.split_once(' ') else {
        return if challenge.eq_ignore_ascii_case("basic") {
            Ok(RegistryAuthChallenge::Basic)
        } else {
            Err(VesselError::Registry("invalid WWW-Authenticate challenge".to_string()))
        };
    };
    if scheme.eq_ignore_ascii_case("basic") {
        return Ok(RegistryAuthChallenge::Basic);
    }
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(VesselError::Registry(format!("unsupported auth scheme `{scheme}`")));
    }

    let mut values = HashMap::new();
    for part in params.split(',') {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        values.insert(key.to_string(), value.trim_matches('"').to_string());
    }

    Ok(RegistryAuthChallenge::Bearer(values))
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

fn path_size(path: &Path) -> Result<u64, VesselError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| VesselError::io(path, source))?;
    if metadata.is_file() {
        return Ok(metadata.len());
    }

    if metadata.is_dir() {
        let mut size = 0;
        for entry in fs::read_dir(path).map_err(|source| VesselError::io(path, source))? {
            let entry = entry.map_err(VesselError::GenericIo)?;
            size += path_size(&entry.path())?;
        }
        return Ok(size);
    }

    Ok(0)
}

fn collect_stale_blobs(
    root: &Path,
    referenced: &HashSet<PathBuf>,
    summary: &mut GarbageCollectionSummary,
) -> Result<(), VesselError> {
    if !root.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(root).map_err(|source| VesselError::io(root, source))? {
        let entry = entry.map_err(VesselError::GenericIo)?;
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| VesselError::io(&path, source))?;

        if metadata.is_dir() {
            collect_stale_blobs(&path, referenced, summary)?;
            continue;
        }

        if referenced.contains(&path) {
            continue;
        }

        summary.reclaimed_bytes += metadata.len();
        fs::remove_file(&path).map_err(|source| VesselError::io(&path, source))?;
        summary.removed_blobs += 1;
    }

    Ok(())
}

fn remove_empty_directories(root: &Path) -> Result<bool, VesselError> {
    if !root.exists() {
        return Ok(true);
    }
    if !root.is_dir() {
        return Ok(false);
    }

    let mut empty = true;
    for entry in fs::read_dir(root).map_err(|source| VesselError::io(root, source))? {
        let entry = entry.map_err(VesselError::GenericIo)?;
        let path = entry.path();
        if path.is_dir() {
            if !remove_empty_directories(&path)? {
                empty = false;
            }
        } else {
            empty = false;
        }
    }

    if empty {
        fs::remove_dir(root).map_err(|source| VesselError::io(root, source))?;
    }

    Ok(empty)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        io::{Read, Write},
        net::TcpListener,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicU16, AtomicUsize, Ordering},
        },
        thread,
    };

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use flate2::{Compression, write::GzEncoder};
    use sha2::Digest;
    use tar::Builder as TarBuilder;
    use vessel_core::{ContainerId, ContainerRecord, ContainerStore, ImageRef, VesselPaths};

    use super::{
        GarbageCollectionSummary, ImageRuntimeConfig, ImageStore, PulledImage, blob_cache_path,
        normalize_registry_key, sanitize_for_path,
    };

    #[test]
    fn pulls_and_reuses_cached_rootfs() {
        let state = Arc::new(MockRegistryState::new(AuthMode::Anonymous));
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

        assert!(pulled.layers.last().unwrap().join("bin/hello").exists());
        assert!(
            paths
                .blobs_dir
                .join("sha256")
                .join(state.config_digest.trim_start_matches("sha256:"))
                .exists()
        );

        let second = store.pull(&image).expect("pull image from cache");
        assert_eq!(pulled.layers, second.layers);
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
            layers: Vec::new(),
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
            layers: Vec::new(),
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

    #[test]
    fn garbage_collect_reclaims_unreferenced_layers_and_blobs() {
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

        let alias_layer = paths.rootfs_dir.join("sha256_alias");
        let container_layer = paths.rootfs_dir.join("sha256_container");
        let stale_layer = paths.rootfs_dir.join("sha256_stale");
        fs::create_dir_all(&alias_layer).expect("alias layer");
        fs::create_dir_all(&container_layer).expect("container layer");
        fs::create_dir_all(&stale_layer).expect("stale layer");
        fs::write(alias_layer.join("keep.txt"), b"keep").expect("write alias");
        fs::write(container_layer.join("keep.txt"), b"keep").expect("write container");
        fs::write(stale_layer.join("drop.txt"), b"drop").expect("write stale");

        let manifest_digest =
            "sha256:1111111111111111111111111111111111111111111111111111111111111111";
        let config_digest =
            "sha256:2222222222222222222222222222222222222222222222222222222222222222";
        let stale_blob_digest =
            "sha256:3333333333333333333333333333333333333333333333333333333333333333";

        let manifest_blob = blob_cache_path(&paths.blobs_dir, manifest_digest).expect("manifest");
        let config_blob = blob_cache_path(&paths.blobs_dir, config_digest).expect("config");
        let stale_blob = blob_cache_path(&paths.blobs_dir, stale_blob_digest).expect("stale");
        fs::create_dir_all(manifest_blob.parent().unwrap()).expect("blob dir");
        fs::write(&manifest_blob, b"manifest").expect("manifest blob");
        fs::write(&config_blob, b"config").expect("config blob");
        fs::write(&stale_blob, b"stale").expect("stale blob");

        let image: ImageRef = "docker.io/library/alpine:latest".parse().expect("image");
        store
            .write_alias(&PulledImage {
                image,
                manifest_digest: manifest_digest.to_string(),
                config_digest: config_digest.to_string(),
                layers: vec![alias_layer.clone()],
                runtime: ImageRuntimeConfig {
                    entrypoint: Vec::new(),
                    cmd: vec!["/bin/sh".to_string()],
                    env: BTreeMap::new(),
                    working_dir: None,
                },
            })
            .expect("alias");

        let container_store = ContainerStore::new(paths.state_dir.clone());
        let record = ContainerRecord::new(
            ContainerId::generate(),
            "docker.io/library/alpine:latest".parse::<ImageRef>().expect("image"),
            vec!["/bin/sh".to_string()],
            None,
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            vec![container_layer.clone()],
        );
        container_store.save(&record).expect("save");

        let summary = store.garbage_collect().expect("gc");
        assert_eq!(
            summary,
            GarbageCollectionSummary { removed_layers: 1, removed_blobs: 1, reclaimed_bytes: 9 }
        );
        assert!(alias_layer.exists());
        assert!(container_layer.exists());
        assert!(!stale_layer.exists());
        assert!(manifest_blob.exists());
        assert!(config_blob.exists());
        assert!(!stale_blob.exists());
    }

    #[test]
    fn pulls_private_image_with_basic_auth_file() {
        let state = Arc::new(MockRegistryState::new(AuthMode::Basic));
        let registry = MockRegistry::spawn(state.clone());
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(temp.path());
        paths.ensure().expect("paths");

        let auth_file = write_auth_file(
            temp.path().join("auth.json"),
            &format!("localhost:{}", registry.port),
            "captain",
            "vessel",
        );
        let store = ImageStore::with_auth_files(paths, vec![auth_file]);
        let image: ImageRef =
            format!("localhost:{}/demo/app:latest", registry.port).parse().expect("image ref");

        let pulled = store.pull(&image).expect("pull private image");
        assert!(pulled.layers.last().unwrap().join("bin/hello").exists());
        assert_eq!(state.authenticated_requests.load(Ordering::Relaxed), 3);
        assert_eq!(state.token_requests.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn pulls_private_image_with_bearer_token_service_using_basic_credentials() {
        let state = Arc::new(MockRegistryState::new(AuthMode::Bearer));
        let registry = MockRegistry::spawn(state.clone());
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(temp.path());
        paths.ensure().expect("paths");

        let auth_file = write_auth_file(
            temp.path().join("auth.json"),
            &format!("localhost:{}", registry.port),
            "captain",
            "vessel",
        );
        let store = ImageStore::with_auth_files(paths, vec![auth_file]);
        let image: ImageRef =
            format!("localhost:{}/demo/app:latest", registry.port).parse().expect("image ref");

        let pulled = store.pull(&image).expect("pull bearer-protected image");
        assert!(pulled.layers.last().unwrap().join("bin/hello").exists());
        assert_eq!(state.authenticated_requests.load(Ordering::Relaxed), 3);
        assert_eq!(state.token_requests.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn normalizes_docker_registry_auth_aliases() {
        assert_eq!(normalize_registry_key("docker.io").as_deref(), Some("docker.io"));
        assert_eq!(
            normalize_registry_key("https://index.docker.io/v1/").as_deref(),
            Some("docker.io")
        );
        assert_eq!(
            normalize_registry_key("https://registry-1.docker.io/v2/").as_deref(),
            Some("docker.io")
        );
    }

    struct MockRegistry {
        port: u16,
        _thread: thread::JoinHandle<()>,
    }

    impl MockRegistry {
        fn spawn(state: Arc<MockRegistryState>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            state.port.store(port, Ordering::Relaxed);
            let thread = thread::spawn(move || {
                for stream in listener.incoming().take(16) {
                    let mut stream = stream.expect("stream");
                    let mut request = [0_u8; 4096];
                    let count = stream.read(&mut request).expect("read request");
                    let request = String::from_utf8_lossy(&request[..count]);
                    let first_line = request.lines().next().expect("request line");
                    let path = first_line.split_whitespace().nth(1).expect("path");
                    let authorization = request.lines().find_map(|line| {
                        line.strip_prefix("Authorization: ")
                            .or_else(|| line.strip_prefix("authorization: "))
                    });
                    let response = state.response(path, authorization);
                    write_response(&mut stream, response);
                }
            });

            Self { port, _thread: thread }
        }
    }

    struct MockRegistryState {
        port: AtomicU16,
        config_digest: String,
        layer_digest: String,
        manifest_digest: String,
        auth_mode: AuthMode,
        requests: AtomicUsize,
        authenticated_requests: AtomicUsize,
        token_requests: AtomicUsize,
        config_payload: Vec<u8>,
        manifest_payload: Vec<u8>,
        layer_payload: Vec<u8>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum AuthMode {
        Anonymous,
        Basic,
        Bearer,
    }

    struct MockResponse {
        status: &'static str,
        body: Vec<u8>,
        content_type: &'static str,
        digest: Option<String>,
        www_authenticate: Option<String>,
    }

    impl MockRegistryState {
        fn new(auth_mode: AuthMode) -> Self {
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
                port: AtomicU16::new(0),
                config_digest,
                layer_digest,
                manifest_digest,
                auth_mode,
                requests: AtomicUsize::new(0),
                authenticated_requests: AtomicUsize::new(0),
                token_requests: AtomicUsize::new(0),
                config_payload,
                manifest_payload,
                layer_payload,
            }
        }

        fn response(&self, path: &str, authorization: Option<&str>) -> MockResponse {
            self.requests.fetch_add(1, Ordering::Relaxed);
            if path == "/token" || path.starts_with("/token?") {
                return self.token_response(authorization);
            }

            if let Some(response) = self.authorize_request(authorization) {
                return response;
            }

            match path {
                "/v2/demo/app/manifests/latest" => MockResponse {
                    status: "200 OK",
                    body: self.manifest_payload.clone(),
                    content_type: "application/vnd.oci.image.manifest.v1+json",
                    digest: Some(self.manifest_digest.clone()),
                    www_authenticate: None,
                },
                value if value.ends_with(&format!("/blobs/{}", self.config_digest)) => {
                    MockResponse {
                        status: "200 OK",
                        body: self.config_payload.clone(),
                        content_type: "application/vnd.oci.image.config.v1+json",
                        digest: Some(self.config_digest.clone()),
                        www_authenticate: None,
                    }
                }
                value if value.ends_with(&format!("/blobs/{}", self.layer_digest)) => {
                    MockResponse {
                        status: "200 OK",
                        body: self.layer_payload.clone(),
                        content_type: "application/vnd.oci.image.layer.v1.tar+gzip",
                        digest: Some(self.layer_digest.clone()),
                        www_authenticate: None,
                    }
                }
                other => panic!("unexpected path: {other}"),
            }
        }

        fn authorize_request(&self, authorization: Option<&str>) -> Option<MockResponse> {
            match self.auth_mode {
                AuthMode::Anonymous => None,
                AuthMode::Basic => {
                    if authorization == Some(expected_basic_auth()) {
                        self.authenticated_requests.fetch_add(1, Ordering::Relaxed);
                        None
                    } else {
                        Some(MockResponse {
                            status: "401 Unauthorized",
                            body: b"basic auth required".to_vec(),
                            content_type: "text/plain",
                            digest: None,
                            www_authenticate: Some("Basic realm=\"vessel\"".to_string()),
                        })
                    }
                }
                AuthMode::Bearer => {
                    if authorization == Some(expected_bearer_auth()) {
                        self.authenticated_requests.fetch_add(1, Ordering::Relaxed);
                        None
                    } else {
                        Some(MockResponse {
                            status: "401 Unauthorized",
                            body: b"bearer auth required".to_vec(),
                            content_type: "text/plain",
                            digest: None,
                            www_authenticate: Some(format!(
                                "Bearer realm=\"http://127.0.0.1:{}/token\",service=\"mock-registry\",scope=\"repository:demo/app:pull\"",
                                self.port.load(Ordering::Relaxed)
                            )),
                        })
                    }
                }
            }
        }

        fn token_response(&self, authorization: Option<&str>) -> MockResponse {
            self.token_requests.fetch_add(1, Ordering::Relaxed);
            if authorization != Some(expected_basic_auth()) {
                return MockResponse {
                    status: "401 Unauthorized",
                    body: b"token auth required".to_vec(),
                    content_type: "text/plain",
                    digest: None,
                    www_authenticate: Some("Basic realm=\"token\"".to_string()),
                };
            }

            MockResponse {
                status: "200 OK",
                body: serde_json::json!({ "token": expected_bearer_token() })
                    .to_string()
                    .into_bytes(),
                content_type: "application/json",
                digest: None,
                www_authenticate: None,
            }
        }
    }

    fn write_response(stream: &mut impl Write, response: MockResponse) {
        let mut headers = format!(
            "HTTP/1.1 {}\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n",
            response.status,
            response.body.len(),
            response.content_type,
        );
        if let Some(digest) = response.digest {
            headers.push_str(&format!("Docker-Content-Digest: {digest}\r\n"));
        }
        if let Some(challenge) = response.www_authenticate {
            headers.push_str(&format!("WWW-Authenticate: {challenge}\r\n"));
        }
        headers.push_str("\r\n");

        stream.write_all(headers.as_bytes()).expect("headers");
        stream.write_all(&response.body).expect("body");
    }

    fn test_paths(root: &Path) -> VesselPaths {
        VesselPaths {
            state_dir: root.join("state"),
            data_dir: root.join("data"),
            blobs_dir: root.join("data/blobs"),
            rootfs_dir: root.join("data/rootfs"),
            bundles_dir: root.join("data/bundles"),
        }
    }

    fn write_auth_file(path: PathBuf, registry: &str, username: &str, password: &str) -> PathBuf {
        let payload = serde_json::json!({
            "auths": {
                registry: {
                    "auth": STANDARD.encode(format!("{username}:{password}"))
                }
            }
        });
        fs::write(&path, payload.to_string()).expect("write auth file");
        path
    }

    fn expected_basic_auth() -> &'static str {
        "Basic Y2FwdGFpbjp2ZXNzZWw="
    }

    fn expected_bearer_auth() -> &'static str {
        "Bearer vessel-registry-token"
    }

    fn expected_bearer_token() -> &'static str {
        "vessel-registry-token"
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
