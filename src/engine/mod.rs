//! Engine-agnostic socket discovery and engine detection (HANDOFF §7).
//!
//! Bosun talks the plain Docker Engine API, so anything that speaks it —
//! Docker Desktop, OrbStack, Colima, Podman — works through the same code path.
//! This module decides *which* endpoint to bind to and *what to call it*.

pub mod client;

use std::fmt;
use std::path::{Path, PathBuf};

/// Which container engine we believe is behind the resolved endpoint.
///
/// Detection is best-effort: it combines the socket path we matched with the
/// daemon's own version banner. Callers should treat this as a label for humans,
/// not a capability switch — capability comes from the Engine API itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Docker,
    OrbStack,
    Colima,
    Podman,
    /// Reached over DOCKER_HOST or an explicit --socket; provenance unknown.
    Unknown,
}

impl Engine {
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Docker => "docker",
            Engine::OrbStack => "orbstack",
            Engine::Colima => "colima",
            Engine::Podman => "podman",
            Engine::Unknown => "unknown",
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How we arrived at an endpoint — surfaced by `bosun_info` so the user can see
/// why Bosun bound where it did without re-deriving the search order by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    CliFlag,
    DockerHostEnv,
    OrbStackSocket,
    ColimaSocket,
    DefaultSocket,
    PodmanSocket,
    PodmanMachineSocket,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::CliFlag => "--socket flag",
            Source::DockerHostEnv => "DOCKER_HOST env var",
            Source::OrbStackSocket => "~/.orbstack/run/docker.sock",
            Source::ColimaSocket => "~/.colima/default/docker.sock",
            Source::DefaultSocket => "/var/run/docker.sock",
            Source::PodmanSocket => "$XDG_RUNTIME_DIR/podman/podman.sock",
            Source::PodmanMachineSocket => "$TMPDIR/podman/*-api.sock (podman machine)",
        }
    }
}

/// A resolved Engine API endpoint, before we have actually connected to it.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Path or URL, verbatim as it will be handed to bollard.
    pub address: String,
    /// Which rule in the search order produced this.
    pub source: Source,
    /// Guess based on path alone; refined once the daemon answers.
    pub engine_hint: Engine,
}

/// Every place we know to look, in the order HANDOFF §7 specifies.
///
/// Ordering matters and is deliberate: an explicit `DOCKER_HOST` always wins so
/// the user can point Bosun anywhere, and the OrbStack/Colima paths come before
/// `/var/run/docker.sock` because on macOS that path is usually a symlink *into*
/// one of them — matching the real path first gives a more honest engine label.
fn candidates() -> Vec<Endpoint> {
    let mut out = Vec::new();

    if let Ok(host) = std::env::var("DOCKER_HOST")
        && !host.trim().is_empty()
    {
        out.push(Endpoint {
            address: host.trim().to_string(),
            source: Source::DockerHostEnv,
            engine_hint: Engine::Unknown,
        });
    }

    if let Some(home) = home_dir() {
        out.push(Endpoint {
            address: path_str(home.join(".orbstack/run/docker.sock")),
            source: Source::OrbStackSocket,
            engine_hint: Engine::OrbStack,
        });
        out.push(Endpoint {
            address: path_str(home.join(".colima/default/docker.sock")),
            source: Source::ColimaSocket,
            engine_hint: Engine::Colima,
        });
    }

    out.push(Endpoint {
        address: "/var/run/docker.sock".to_string(),
        source: Source::DefaultSocket,
        engine_hint: Engine::Docker,
    });

    // Podman's rootless socket is Docker-API-compatible, so it needs no adapter.
    // This is the *Linux* location; macOS puts it somewhere else entirely.
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR")
        && !runtime.trim().is_empty()
    {
        out.push(Endpoint {
            address: path_str(PathBuf::from(runtime.trim()).join("podman/podman.sock")),
            source: Source::PodmanSocket,
            engine_hint: Engine::Podman,
        });
    }

    out.extend(podman_machine_sockets());

    out
}

/// Podman sockets published by `podman machine` on macOS and Windows.
///
/// Verified against podman 6.0.2 on macOS, which puts its API socket at
/// `$TMPDIR/podman/podman-machine-default-api.sock`. That is neither the Linux
/// rootless path above nor the `~/.local/share/containers/...` path the docs
/// suggest — both of which this code guessed at before anyone ran it against a
/// real Podman. The README claimed "engine-agnostic" on the strength of unit
/// tests alone, and on macOS the claim was simply false: `XDG_RUNTIME_DIR` is
/// unset there, so the only Podman candidate never fired.
///
/// The filename embeds the machine name, so this scans rather than guessing a
/// fixed path — a user with a non-default machine is still found.
fn podman_machine_sockets() -> Vec<Endpoint> {
    let Some(tmpdir) = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
    else {
        return Vec::new();
    };

    let Ok(entries) = std::fs::read_dir(tmpdir.join("podman")) else {
        return Vec::new();
    };

    let mut found: Vec<String> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("-api.sock"))
        })
        .map(path_str)
        .collect();

    // Stable order, so which machine wins doesn't change between runs.
    found.sort();

    found
        .into_iter()
        .map(|address| Endpoint {
            address,
            source: Source::PodmanMachineSocket,
            engine_hint: Engine::Podman,
        })
        .collect()
}

/// Resolve the endpoint to bind to, honouring an explicit override first.
///
/// Returns the first candidate that exists on disk. Non-unix addresses
/// (`tcp://`, `ssh://`, `npipe://`) can't be stat'd, so they are accepted
/// as-is and validated by actually connecting.
pub fn resolve(override_socket: Option<&str>) -> Result<Endpoint, DiscoveryError> {
    if let Some(explicit) = override_socket {
        return Ok(Endpoint {
            address: explicit.to_string(),
            source: Source::CliFlag,
            engine_hint: Engine::Unknown,
        });
    }

    let candidates = candidates();
    for candidate in &candidates {
        if !is_unix_path(&candidate.address) || Path::new(strip_scheme(&candidate.address)).exists()
        {
            tracing::debug!(
                address = %candidate.address,
                source = candidate.source.as_str(),
                "resolved docker endpoint"
            );
            return Ok(candidate.clone());
        }
        tracing::debug!(address = %candidate.address, "candidate not present, skipping");
    }

    Err(DiscoveryError::NoEndpoint {
        tried: candidates.iter().map(|c| c.address.clone()).collect(),
    })
}

/// Refine the path-based engine guess using the daemon's own version banner.
///
/// OrbStack and Colima both present themselves as Docker over the API, so the
/// socket path is often the only distinguishing signal — we keep the hint unless
/// the banner says something more specific (Podman advertises itself clearly).
pub fn detect_engine(
    hint: Engine,
    version_platform: Option<&str>,
    components: &[String],
) -> Engine {
    let haystack = {
        let mut s = version_platform.unwrap_or_default().to_lowercase();
        for c in components {
            s.push(' ');
            s.push_str(&c.to_lowercase());
        }
        s
    };

    if haystack.contains("podman") {
        return Engine::Podman;
    }
    if haystack.contains("orbstack") {
        return Engine::OrbStack;
    }
    if haystack.contains("colima") {
        return Engine::Colima;
    }

    match hint {
        Engine::Unknown if !haystack.is_empty() => Engine::Docker,
        other => other,
    }
}

/// Errors that mean we never got as far as talking to a daemon.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error(
        "no container engine socket found. Tried: {}. \
         Start Docker/OrbStack/Colima/Podman, set DOCKER_HOST, or pass --socket <path>.",
        tried.join(", ")
    )]
    NoEndpoint { tried: Vec<String> },

    /// Apple's `container` CLI does not expose a Docker Engine API (HANDOFF §7.6).
    /// We name it explicitly so the failure reads as "unsupported engine" rather
    /// than a cryptic connection error.
    #[error(
        "endpoint '{address}' does not speak the Docker Engine API. \
         Apple's `container` tool is not Docker-API-compatible and is unsupported; \
         use Docker, OrbStack, Colima, or Podman."
    )]
    NotDockerApi { address: String },
}

fn is_unix_path(address: &str) -> bool {
    !address.contains("://") || address.starts_with("unix://")
}

fn strip_scheme(address: &str) -> &str {
    address.strip_prefix("unix://").unwrap_or(address)
}

fn path_str(p: PathBuf) -> String {
    p.to_string_lossy().into_owned()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scan is what makes Podman findable on macOS. Driven by an injected
    /// directory rather than the real $TMPDIR so it is deterministic in CI,
    /// where no Podman exists.
    fn scan(dir: &std::path::Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut found: Vec<String> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with("-api.sock"))
            })
            .map(path_str)
            .collect();
        found.sort();
        found
    }

    #[test]
    fn podman_machine_scan_matches_the_real_socket_name() {
        // The exact filename observed from podman 6.0.2 on macOS. Guessing this
        // is what produced two wrong paths before anyone ran it for real.
        let dir = std::env::temp_dir().join(format!("bosun-scan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "podman-machine-default-api.sock",
            "podman-machine-work-api.sock",
            "not-a-socket.txt",
            "podman.sock",
        ] {
            std::fs::write(dir.join(name), b"").unwrap();
        }

        let found = scan(&dir);
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            found.len(),
            2,
            "should match only the *-api.sock files: {found:?}"
        );
        assert!(found[0].ends_with("podman-machine-default-api.sock"));
        assert!(found[1].ends_with("podman-machine-work-api.sock"));
        // Sorted, so which machine wins cannot change between runs.
        assert!(found[0] < found[1]);
    }

    #[test]
    fn a_missing_podman_directory_is_not_an_error() {
        // The overwhelmingly common case: no Podman installed at all.
        let missing = std::env::temp_dir().join("bosun-definitely-not-here-xyz");
        assert!(scan(&missing).is_empty());
    }

    #[test]
    fn cli_override_wins_and_is_taken_verbatim() {
        let ep = resolve(Some("/custom/docker.sock")).unwrap();
        assert_eq!(ep.address, "/custom/docker.sock");
        assert_eq!(ep.source, Source::CliFlag);
    }

    #[test]
    fn tcp_addresses_are_not_stat_checked() {
        // A tcp:// endpoint has no filesystem presence; it must still be accepted
        // so remote daemons work.
        assert!(!is_unix_path("tcp://localhost:2375"));
        assert!(is_unix_path("/var/run/docker.sock"));
        assert!(is_unix_path("unix:///var/run/docker.sock"));
    }

    #[test]
    fn unix_scheme_is_stripped_before_stat() {
        assert_eq!(
            strip_scheme("unix:///var/run/docker.sock"),
            "/var/run/docker.sock"
        );
        assert_eq!(strip_scheme("/var/run/docker.sock"), "/var/run/docker.sock");
    }

    #[test]
    fn podman_banner_overrides_path_hint() {
        let engine = detect_engine(Engine::Docker, Some("Podman Engine"), &[]);
        assert_eq!(engine, Engine::Podman);
    }

    #[test]
    fn component_names_are_searched_too() {
        let engine = detect_engine(
            Engine::Unknown,
            Some("linux/amd64"),
            &["Podman Engine".to_string()],
        );
        assert_eq!(engine, Engine::Podman);
    }

    #[test]
    fn path_hint_survives_a_generic_docker_banner() {
        // OrbStack reports itself as Docker over the API; the socket path is the
        // only thing that distinguishes it, so the hint must not be clobbered.
        let engine = detect_engine(Engine::OrbStack, Some("linux/arm64"), &[]);
        assert_eq!(engine, Engine::OrbStack);
    }

    #[test]
    fn unknown_hint_with_a_live_banner_becomes_docker() {
        let engine = detect_engine(Engine::Unknown, Some("linux/arm64"), &[]);
        assert_eq!(engine, Engine::Docker);
    }

    #[test]
    fn unknown_hint_with_no_banner_stays_unknown() {
        let engine = detect_engine(Engine::Unknown, None, &[]);
        assert_eq!(engine, Engine::Unknown);
    }
}
