# Changelog

## 0.2.0

First release shaped by production use. Six live incident sessions and a hostile
fixture stack found the following; every behavioural bug here came from running
Kagoni, not from reading it.

### Breaking

- **`ssh://` and `https://` now require `--features remote`.** They cost ~34
  crates and ~1.7 MB that a local-socket user never touches. A build without the
  feature names the fix rather than reporting an unsupported scheme. Binary drops
  7.8 MB to 6.5 MB.
- **`template` is omitted from singleton clusters.** For a group of one it
  duplicates `sample`. A consumer that assumed the field was always present will
  need to handle its absence.

### Added

- **Read-only mode** (`--read-only`, `KAGONI_READ_ONLY=1`). Write tools are
  *removed from the tool list*, not refused — an agent cannot misuse a tool it
  was never told exists. 18 tools drop to 10, and the always-on schema cost from
  ~4,476 to ~2,574 tokens.
- **Deadlines on log fetching.** bollard's timeout bounds the request, not the
  draining of a streaming response, so a stalled stream could run for 1800s with
  a 120s timeout in force. `container_logs` now defaults to 15s (`timeout` param,
  max 120) and `diagnose_container` to 8s, both returning partial results with
  `timed_out: true`.
- **Batch reads.** `diagnose_container` and `container_stats` take `ids[]`, and
  `ids=["*"]` covers the whole fleet in one concurrent call. A fleet-health
  question went from ~17 tool calls to ~3.
- **A reproducible token benchmark** (`benches/`, `docs/BENCHMARK.md`) with a
  real BPE tokenizer, including the scenarios where Kagoni loses.

### Fixed

- **Kagoni connected to the address it reported.** The remote branch called
  `connect_with_defaults()`, which reads `DOCKER_HOST` and ignored the resolved
  address entirely — so `--socket tcp://remote` drove the *local* daemon while
  reporting the remote one. Every destructive tool acts on that answer.
- **Podman is discoverable on macOS.** `XDG_RUNTIME_DIR` is unset there, so the
  only Podman candidate never fired. Discovery now scans
  `$TMPDIR/podman/*-api.sock`. Verified against Podman 6.0.2.
- **ANSI escapes are stripped before clustering**, fixing unreadable templates
  and the mixed-colour grouping case.
- **Paths normalize per segment.** A hash inside a path was shredded into
  `<NUM>ff<NUM>b<NUM>…` rather than recognized, and slugs grouped inconsistently
  depending on whether they happened to contain a digit.
- **Cluster text is capped** in both `sample` and `template`. One 64 KB line
  produced a 16 KB template that was 91% of the response.
- **Diagnostic calibration.** Errors must recur to change a verdict, must be
  recent to describe the present, and a container caught between crashes is
  recognized as looping. Stale errors are reported as `UNRESOLVED` rather than
  implied fixed.
- **MSRV corrected to 1.88.** The manifest claimed 1.85 while the code used
  let-chains, so `cargo install` failed with a parse error rather than a version
  one. A CI job now builds on exactly the declared version.
