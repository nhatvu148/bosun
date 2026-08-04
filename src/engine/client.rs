//! Thin bollard wrapper: owns the connection and the facts we learned at startup.

use bollard::{API_DEFAULT_VERSION, Docker};

use super::{DiscoveryError, Endpoint, Engine, detect_engine, resolve};

/// Connection timeout in seconds for Engine API calls.
///
/// Generous rather than tight: pulls and compose operations legitimately take a
/// while, and a spurious timeout mid-write is worse than a slow answer.
const TIMEOUT_SECS: u64 = 120;

/// A live connection plus the provenance of how we got here.
#[derive(Clone)]
pub struct EngineClient {
    docker: Docker,
    endpoint: Endpoint,
    engine: Engine,
    server_version: String,
    api_version: String,
}

impl EngineClient {
    /// Resolve an endpoint, connect, and probe the daemon for its identity.
    ///
    /// The probe is not optional: it is what turns a path guess into a verified
    /// connection, and it is where a non-Docker-API engine (Apple `container`)
    /// gets a clear error instead of a cryptic one.
    pub async fn connect(override_socket: Option<&str>) -> Result<Self, ConnectError> {
        let endpoint = resolve(override_socket)?;

        let docker = if endpoint.address.contains("://")
            && !endpoint.address.starts_with("unix://")
        {
            // tcp:// / ssh:// / npipe:// — let bollard parse the URL itself.
            Docker::connect_with_defaults().map_err(|source| ConnectError::Connect {
                address: endpoint.address.clone(),
                source,
            })?
        } else {
            let path = endpoint.address.strip_prefix("unix://").unwrap_or(&endpoint.address);
            Docker::connect_with_unix(path, TIMEOUT_SECS, API_DEFAULT_VERSION).map_err(|source| {
                ConnectError::Connect {
                    address: endpoint.address.clone(),
                    source,
                }
            })?
        };

        let version = docker.version().await.map_err(|source| {
            // A socket that exists but won't answer /version is the signature of
            // something that isn't a Docker Engine.
            tracing::debug!(%source, "version probe failed");
            ConnectError::Discovery(DiscoveryError::NotDockerApi {
                address: endpoint.address.clone(),
            })
        })?;

        let component_names: Vec<String> = version
            .components
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|c| c.name.clone())
            .collect();

        let engine = detect_engine(
            endpoint.engine_hint,
            version.platform.as_ref().map(|p| p.name.as_str()),
            &component_names,
        );

        let server_version = version.version.clone().unwrap_or_else(|| "unknown".into());
        let api_version = version.api_version.clone().unwrap_or_else(|| "unknown".into());

        tracing::info!(
            engine = engine.as_str(),
            address = %endpoint.address,
            source = endpoint.source.as_str(),
            version = %server_version,
            "connected to container engine"
        );

        Ok(Self {
            docker,
            endpoint,
            engine,
            server_version,
            api_version,
        })
    }

    pub fn docker(&self) -> &Docker {
        &self.docker
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn engine(&self) -> Engine {
        self.engine
    }

    pub fn server_version(&self) -> &str {
        &self.server_version
    }

    pub fn api_version(&self) -> &str {
        &self.api_version
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error(transparent)]
    Discovery(#[from] DiscoveryError),

    #[error("failed to connect to '{address}': {source}")]
    Connect {
        address: String,
        #[source]
        source: bollard::errors::Error,
    },
}
