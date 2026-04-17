use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::VesselError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageReference {
    Tag(String),
    Digest(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    registry: String,
    repository: String,
    reference: ImageReference,
}

impl ImageRef {
    pub fn registry(&self) -> &str {
        &self.registry
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub fn reference(&self) -> &ImageReference {
        &self.reference
    }

    pub fn manifest_reference(&self) -> &str {
        match &self.reference {
            ImageReference::Tag(tag) => tag,
            ImageReference::Digest(digest) => digest,
        }
    }

    pub fn scope(&self) -> String {
        format!("repository:{}:pull", self.repository)
    }

    pub fn registry_api_base(&self) -> String {
        let scheme = if self.registry.starts_with("localhost")
            || self.registry.starts_with("127.0.0.1")
            || self.registry.starts_with("[::1]")
        {
            "http"
        } else {
            "https"
        };

        let host = if self.registry == "docker.io" {
            "registry-1.docker.io"
        } else {
            self.registry.as_str()
        };

        format!("{scheme}://{host}/v2")
    }

    pub fn canonical_name(&self) -> String {
        self.to_string()
    }
}

impl FromStr for ImageRef {
    type Err = VesselError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let input = input.trim();
        if input.is_empty() || input.contains("://") {
            return Err(VesselError::InvalidImageReference(input.to_owned()));
        }

        let (registry, remainder) = match input.split_once('/') {
            Some((first, rest))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_ascii_lowercase(), rest)
            }
            _ => ("docker.io".to_string(), input),
        };

        let (repository, reference) = if let Some((repository, digest)) = remainder.rsplit_once('@')
        {
            (repository.to_ascii_lowercase(), ImageReference::Digest(digest.to_ascii_lowercase()))
        } else if let Some(index) = remainder.rfind(':') {
            if remainder[index + 1..].contains('/') {
                (
                    normalize_repository(&registry, remainder),
                    ImageReference::Tag("latest".to_string()),
                )
            } else {
                (
                    normalize_repository(&registry, &remainder[..index]),
                    ImageReference::Tag(remainder[index + 1..].to_string()),
                )
            }
        } else {
            (normalize_repository(&registry, remainder), ImageReference::Tag("latest".to_string()))
        };

        if repository.is_empty() {
            return Err(VesselError::InvalidImageReference(input.to_owned()));
        }

        Ok(Self { registry, repository, reference })
    }
}

fn normalize_repository(registry: &str, repository: &str) -> String {
    let repository = repository.trim_matches('/').to_ascii_lowercase();
    if registry == "docker.io" && !repository.contains('/') {
        format!("library/{repository}")
    } else {
        repository
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.reference {
            ImageReference::Tag(tag) => write!(f, "{}/{}:{tag}", self.registry, self.repository),
            ImageReference::Digest(digest) => {
                write!(f, "{}/{}@{digest}", self.registry, self.repository)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ImageRef, ImageReference};

    #[test]
    fn normalizes_docker_hub_shorthand() {
        let image: ImageRef = "alpine".parse().expect("image ref");
        assert_eq!(image.registry(), "docker.io");
        assert_eq!(image.repository(), "library/alpine");
        assert_eq!(image.reference(), &ImageReference::Tag("latest".into()));
        assert_eq!(image.registry_api_base(), "https://registry-1.docker.io/v2");
    }

    #[test]
    fn preserves_explicit_registry() {
        let image: ImageRef = "ghcr.io/acme/widget:1.2.3".parse().expect("image ref");
        assert_eq!(image.registry(), "ghcr.io");
        assert_eq!(image.repository(), "acme/widget");
        assert_eq!(image.reference(), &ImageReference::Tag("1.2.3".into()));
    }

    #[test]
    fn parses_local_registry_over_http() {
        let image: ImageRef = "localhost:5000/demo/app:dev".parse().expect("image ref");
        assert_eq!(image.registry_api_base(), "http://localhost:5000/v2");
    }
}
