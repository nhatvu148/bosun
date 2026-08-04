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

        // Dispatch on the address we actually resolved. An earlier version called
        // `connect_with_defaults()` here, which reads DOCKER_HOST from the
        // environment and therefore *ignored* the address entirely: passing
        // `--socket tcp://remote:2375` connected to the local daemon while
        // `bosun_info` cheerfully reported the remote one. Being wrong about
        // which daemon you are driving is the single most dangerous thing this
        // server can do, since every destructive tool acts on that answer.
        let docker = if is_remote(&endpoint.address) {
            // Without the `remote` feature bollard reports only "URI scheme is
            // not supported", which reads like the scheme is wrong rather than
            // like this build simply omits it. Name the actual fix.
            #[cfg(not(feature = "remote"))]
            if needs_remote_feature(&endpoint.address) {
                return Err(ConnectError::RemoteFeatureMissing {
                    address: endpoint.address.clone(),
                });
            }

            Docker::connect_with_host(&endpoint.address).map_err(|source| {
                ConnectError::Connect {
                    address: endpoint.address.clone(),
                    source,
                }
            })?
        } else {
            let path = endpoint
                .address
                .strip_prefix("unix://")
                .unwrap_or(&endpoint.address);
            Docker::connect_with_unix(path, TIMEOUT_SECS, API_DEFAULT_VERSION).map_err(
                |source| ConnectError::Connect {
                    address: endpoint.address.clone(),
                    source,
                },
            )?
        };

        let version = docker.version().await.map_err(|source| {
            tracing::debug!(%source, "version probe failed");
            // The same failed probe means two very different things. A *local*
            // socket that exists but won't answer is usually not a Docker Engine
            // at all. A *remote* address that won't answer is almost always
            // unreachable — wrong host, closed port, no tunnel. Telling someone
            // debugging a production connection that their engine is
            // "not Docker-API-compatible" sends them somewhere useless.
            if is_remote(&endpoint.address) {
                ConnectError::Unreachable {
                    address: endpoint.address.clone(),
                    source,
                }
            } else {
                ConnectError::Discovery(DiscoveryError::NotDockerApi {
                    address: endpoint.address.clone(),
                })
            }
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
        let api_version = version
            .api_version
            .clone()
            .unwrap_or_else(|| "unknown".into());

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

/// Schemes that only exist when the `remote` feature is compiled in.
#[cfg(not(feature = "remote"))]
fn needs_remote_feature(address: &str) -> bool {
    address.starts_with("ssh://") || address.starts_with("https://")
}

/// Is this address something other than a local unix socket path?
///
/// `tcp://`, `http://`, `https://`, `ssh://` and `npipe://` all need bollard's
/// scheme-aware constructor; a bare path or `unix://` does not.
fn is_remote(address: &str) -> bool {
    address.contains("://") && !address.starts_with("unix://")
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

    /// Only reachable in a build without `remote`; with the feature on, the
    /// scheme works and the variant would be dead code.
    #[cfg(not(feature = "remote"))]
    #[error(
        "'{address}' needs the `remote` feature, which this build does not have.\n\
         Install it with:  cargo install bosun-mcp --features remote\n\
         It is off by default because ssh/TLS support costs ~34 crates and ~1.7 MB \
         that a local-socket user never touches. Plain tcp:// works without it."
    )]
    RemoteFeatureMissing { address: String },

    #[error(
        "connected to '{address}' but it never answered /version: {source}\n\
         Check the host is reachable, the daemon is listening, and any tunnel is up. \
         For a remote host, ssh://user@host is safer than exposing tcp://:2375, which is \
         unauthenticated root access to that machine."
    )]
    Unreachable {
        address: String,
        #[source]
        source: bollard::errors::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_schemes_are_distinguished_from_local_socket_paths() {
        // This predicate decides which bollard constructor runs. Getting it
        // wrong is how `--socket tcp://remote` silently drove the local daemon.
        for remote in [
            "tcp://10.0.0.5:2375",
            "http://10.0.0.5:2375",
            "https://10.0.0.5:2376",
            "ssh://deploy@prod.example.com",
            "npipe:////./pipe/docker_engine",
        ] {
            assert!(is_remote(remote), "{remote} should be treated as remote");
        }

        for local in [
            "/var/run/docker.sock",
            "unix:///var/run/docker.sock",
            "/Users/me/.orbstack/run/docker.sock",
        ] {
            assert!(!is_remote(local), "{local} should be treated as local");
        }
    }

    #[test]
    fn an_unreachable_remote_does_not_blame_the_engine_type() {
        // A network failure reported as "your engine isn't Docker-compatible"
        // sends someone debugging a production connection nowhere useful.
        let err = ConnectError::Unreachable {
            address: "tcp://prod:2375".into(),
            source: bollard::errors::Error::UnsupportedURISchemeError { uri: "x".into() },
        };
        let msg = err.to_string();
        assert!(msg.contains("never answered /version"), "{msg}");
        assert!(msg.contains("reachable"), "{msg}");
        assert!(!msg.contains("Apple"), "must not misattribute: {msg}");
    }
}
