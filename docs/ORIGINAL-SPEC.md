> The spec this project was built from, written before any code existed and kept
> verbatim — including the name it had then (**Bosun**) and the decisions real use
> later reversed. Current documentation is in [../README.md](../README.md).

# Bosun — Handoff Plan

> An **engine-agnostic, agent-ergonomic Docker MCP server** in Rust.
> Not a monitoring daemon — an MCP server whose core design principle is
> *token-bounded I/O* so a human-in-the-loop agent (Claude Code) can drive
> containers without drowning in raw daemon output.

Status: greenfield. This doc is the spec + phased plan to build from inside Claude Code.
Stack decided: **Rust** (`rmcp` + `bollard`). Author: Nhat-Vu (Nhat-Vu Labs).

---

## 1. One-liner & positioning

Bosun exposes container lifecycle, logs, stats, and Compose as MCP tools — but every
read is **bounded and summarizing by default**, every destructive write is **gated**,
and the whole thing talks the plain Docker Engine socket so it drives Docker, OrbStack,
Podman, Colima, or Apple `container` interchangeably.

### Be honest about prior art (so future-you doesn't rediscover it)

This space is **not** empty. Before writing a line, know what already exists:

- **Thin CRUD MCP wrappers** — `ckreiling/mcp-server-docker`, `quantgeekdev/docker-mcp`,
  and forks. 1:1 mirrors of `docker ps` / `logs`. Saturated. Do **not** rebuild this.
- **Container Doctor** (freeCodeCamp) — a **Python monitoring daemon**: scans logs every
  10s, sends errors to Claude for JSON diagnosis (OOM, pool exhaustion, disk, timeouts),
  auto-restarts with throttling (max 3/hr). The diagnostic idea, already built — but it's
  an autonomous loop, not an MCP server.
- **Docker MCP Gateway "agentic remediation"** (Docker/DZone) — production-safe auto-remediation
  through Docker's own gateway. Also autonomous, and Docker-only.
- **Docker MCP Catalog & Toolkit** — mostly about running *other* MCP servers *inside*
  containers securely + a gateway/catalog. **Different problem.** Not daemon management.

### The actual, narrow differentiation

Bosun is worth building only if it stays true to what the above *don't* do:

1. **Interactive MCP server, not an autonomous daemon.** Human-in-the-loop via Claude Code;
   Bosun surfaces bounded facts and proposes actions, the agent+human decide.
2. **Token-budget as the primary design constraint.** Bounded/summarizing reads are the
   product, not an afterthought. This is the context-economy philosophy applied to a domain.
3. **Engine-agnostic by construction.** One tool, any backend that speaks the Docker socket.
4. **Write-safety contracts.** Destructive ops (`rm -f`, `prune`, `compose down -v`) are
   dry-run / confirm-gated so an over-eager agent can't nuke volumes.

Positioning is "**a sharper Docker control surface for my own agent workflow**," not
"first mover." Ship it as a clean personal tool; don't oversell novelty.

---

## 2. Design principles

- **Bounded by default, raw on request.** Every read tool caps output and offers an
  explicit `raw: true` / `full: true` escape hatch. The default answer fits a context window.
- **Diagnostic layer over CRUD layer.** The high-value tools encode *troubleshooting
  reasoning* (crash-loop? OOM? bad healthcheck? port conflict?), not just verbs the agent
  could barely misuse.
- **Deterministic core, LLM optional.** Bosun itself should do as much diagnosis as possible
  with plain deterministic heuristics (exit codes, `State.OOMKilled`, restart counts, log
  pattern clustering). It does **not** need to call an LLM — the *calling* agent is the LLM.
  Keep Bosun a dumb, fast, honest data source. (This mirrors the VEXAR anti-hallucination
  stance: give the model bounded ground truth, don't have the tool itself hallucinate.)
- **Write-safety is a contract, not a flag.** Destructive ops require an explicit
  confirmation token or `dry_run` first.
- **Never shell out to the `docker` CLI.** Talk the Engine API socket directly for structured
  data + streaming.

---

## 3. Architecture

```
Claude Code ──stdio(JSON-RPC)──▶ Bosun (rmcp server)
                                     │
                                     ▼
                              bollard (async Docker Engine API client)
                                     │  unix socket
                                     ▼
        DOCKER_HOST │ ~/.orbstack/run/docker.sock │ ~/.colima/default/docker.sock │ podman │ …
                                     ▼
                           whichever engine is running
```

- **Transport:** stdio first (that's how Claude Code launches local MCP servers). Add
  streamable-HTTP later only if you want a shared/remote instance.
- **`rmcp`** = official Rust MCP SDK (`modelcontextprotocol/rust-sdk`). Use its
  `#[tool]` / tool-router macros so tool schemas derive from typed structs.
- **`bollard`** = async Docker Engine API client (tokio). Handles the streaming endpoints
  (logs, events, stats) as async streams — the part that matters here.
- **Streaming is the hard part, not CRUD.** Logs / events / stats are chunked HTTP streams.
  Bosun's job is to *consume* the stream and hand back a *bounded digest*, never to relay
  the firehose.

---

## 4. Tool surface

MVP tools grouped by kind. Names are the MCP tool ids.

### Read / state — bounded
- `list_containers { all?, filter? }` → compact rows (id, name, image, state, status,
  health, ports). No raw inspect blobs.
- `inspect_container { id, fields? }` → **projected** subset by default (state, restart
  count, health, mounts, env-keys-only, ports). `full: true` returns the whole blob.
- `container_logs { id, tail=200, since?, grep?, level? }` → tail-N + an **error/warn
  cluster summary** (dedup similar lines, count them, surface first+last timestamp).
  `raw: true` streams the untrimmed tail.
- `container_stats { id }` → **single snapshot digest** (cpu %, mem used/limit, net, block io),
  not a stream.
- `list_images { dangling? }` → id, tags, size, age.
- `compose_ps { project }` → per-service state + health.

### Actions — guarded
- `container_start|stop|restart { id }` → low-risk, allowed directly.
- `container_rm { id, force?, volumes? }` → **gated** (see §6).
- `pull_image { ref }` → progress collapsed to a final summary line, not layer spam.
- `compose_up { project, detach=true }` / `compose_down { project, volumes? }` → `down --volumes` is gated.
- `container_exec { id, cmd, timeout }` → bounded stdout/stderr capture. Consider gating.

### Diagnostic — the differentiator (deterministic)
- `diagnose_container { id }` → structured verdict: `{ status, likely_cause, evidence[],
  suggested_actions[] }` computed from exit code + `State.OOMKilled` + restart count +
  healthcheck history + log-cluster signals. **No LLM call inside Bosun.**
- `explain_exit_code { code }` → decode 137/143/139/1/126/127 etc. into human meaning
  (137 = SIGKILL, usually OOM or `docker stop` timeout, …).
- `why_compose_failing { project }` → cross-service: dependency order, port conflicts,
  failed healthchecks, images that won't pull.

### Resources (read-only context the agent can pull without a tool call)
- `docker://containers` — current state snapshot.
- `docker://container/{id}` — projected inspect.
- `docker://compose/{project}` — service map.

---

## 5. The token-budget design (the core IP)

This is the thing that makes Bosun not-a-me-too. Concretely:

- **Logs:** default `tail=200`; run cluster-dedup (normalize numbers/UUIDs/timestamps, hash
  the skeleton, count occurrences) so "500 identical stacktraces" becomes one entry with a
  count. Return the N most-recent distinct clusters + counts, plus first/last seen. Hard cap
  output bytes; if exceeded, degrade to summary-only with a note.
- **Stats:** snapshot, never a stream. One JSON object.
- **Inspect:** project to a curated field set; env returns **keys only** by default (values on
  explicit request — avoids leaking secrets into context too).
- **Pull / build:** collapse layer-by-layer progress into a single terminal summary.
- **Everywhere:** every bounded tool documents its cap in its description and exposes the
  `raw`/`full` opt-out. The agent should *never* be surprised by a truncation it can't undo.

Design rule of thumb: **the default response to any Bosun tool should be safe to put in a
context window unread.** If it isn't, the tool is wrong.

---

## 6. Write-safety contract

- Classify every tool `safe` | `destructive`.
- `destructive` tools (`container_rm --volumes`, `compose_down --volumes`, `prune`, maybe
  `exec`) require either:
  - `dry_run: true` → returns *what would happen* (which containers/volumes), no action; or
  - an explicit `confirm` token echoing the target (e.g. `confirm: "<container-name>"`).
- Never auto-force. `force: true` must be caller-supplied, never defaulted.
- Log every destructive action taken (stderr/tracing) for an audit trail.

---

## 7. Engine-agnostic socket discovery

Resolve the endpoint in this order, first hit wins:

1. `DOCKER_HOST` env var if set (respect it verbatim).
2. `~/.orbstack/run/docker.sock` (OrbStack).
3. `~/.colima/default/docker.sock` (Colima default profile).
4. `/var/run/docker.sock` (Docker Desktop / standard symlink; OrbStack also symlinks here).
5. Podman: `$XDG_RUNTIME_DIR/podman/podman.sock` (rootless) — Docker-compatible API.
6. Apple `container`: **caveat** — it does *not* expose a full Docker Engine API yet, so treat
   as unsupported for now; add an adapter later if/when its API firms up. Log a clear
   "engine not Docker-API-compatible" message rather than failing cryptically.

Expose the resolved endpoint + detected engine name via a `bosun_info` / health tool so the
agent (and you) can see what it bound to. Add a `--socket` CLI override.

---

## 8. Phased milestones

- **M0 — Scaffold.** `cargo new bosun`; wire `rmcp` stdio server with one trivial tool
  (`bosun_info` returning resolved socket + engine). Confirm Claude Code can launch it and
  list the tool. *Done = handshake works.*
- **M1 — Bounded reads.** `list_containers`, `inspect_container` (projected), `container_logs`
  (tail + cluster summary), `container_stats` (snapshot). This is the meat of the value.
- **M2 — Actions + safety.** start/stop/restart, `container_rm` (gated), `pull_image`
  (collapsed). Implement the write-safety contract from §6.
- **M3 — Diagnostics.** `diagnose_container`, `explain_exit_code`, `why_compose_failing`.
  All deterministic.
- **M4 — Engine-agnostic + Compose.** Full socket discovery (§7), `compose_ps/up/down`,
  `bosun_info` reporting the engine.
- **M5 — Polish + publish.** Tracing, config, README, `cargo install` / homebrew-tap entry
  (you already have `homebrew-tap`), example Claude Code MCP config snippet.

Ship M0–M1 first and actually use it before building M3 — the diagnostic tools should be
shaped by real annoyances you hit, not guessed upfront.

---

## 9. Dependencies (checked against project-knowledge — none previously rejected)

- `rmcp` — official Rust MCP SDK. stdio transport, tool macros.
- `bollard` — async Docker Engine API client.
- `tokio` — async runtime.
- `serde` / `serde_json` — tool I/O types.
- `tracing` + `tracing-subscriber` — logs to stderr (stdout is the MCP channel — keep it clean).
- `clap` — `--socket`, `--engine`, verbosity flags.
- `anyhow` / `thiserror` — errors.
- Dev/test: `testcontainers` (Rust) or a scripted throwaway container to exercise tools;
  a deliberately crash-looping/OOM container as a diagnostic fixture.

> ⚠️ **stdout discipline:** an stdio MCP server speaks JSON-RPC on stdout. Send *all* logging
> to stderr, or you corrupt the protocol. Easy first-day footgun.

---

## 10. Suggested repo layout

```
bosun/
  Cargo.toml
  README.md
  src/
    main.rs            # CLI + server bootstrap
    server.rs          # rmcp server, tool router
    engine/
      mod.rs           # socket discovery, engine detection
      client.rs        # bollard wrapper
    tools/
      read.rs          # list/inspect/logs/stats
      actions.rs       # start/stop/rm/pull + safety
      diagnose.rs      # diagnose/explain/why_compose
      compose.rs
    bound/
      logs.rs          # tail + cluster-dedup
      project.rs       # inspect field projection
    safety.rs          # destructive classification + confirm/dry_run
  tests/
    fixtures/          # crash-loop + OOM compose files
```

---

## 11. Open questions / decisions to make early

- **Compose driver:** shell out to `docker compose` (simplest, but you said avoid CLI) vs.
  drive containers directly vs. a Compose-spec parser crate. Pragmatic call: Compose is the
  one place a thin `docker compose` shell-out may be acceptable — decide in M4.
- **`exec` policy:** expose it (powerful, risky) or leave it out of v1? Lean out for v1.
- **Secrets in inspect:** env-keys-only default is proposed — confirm you're happy with that.
- **Scope creep guard:** Kubernetes? Registries? **No** for v1. Bosun is local container ops.

---

## 12. Kickoff prompt for Claude Code

Paste this to start M0 in the `bosun` repo:

> Build M0 of Bosun, a Rust MCP server for Docker. Use `rmcp` (official Rust MCP SDK) with
> the **stdio** transport and `bollard` for the Docker Engine API. Create a `cargo` binary
> that: (1) resolves a Docker socket in this order — `DOCKER_HOST`, `~/.orbstack/run/docker.sock`,
> `~/.colima/default/docker.sock`, `/var/run/docker.sock`; (2) connects via bollard; (3) exposes
> ONE MCP tool `bosun_info` returning `{ engine, socket_path, server_version, container_count }`.
> All logging must go to **stderr** (stdout is the MCP channel). Add a `--socket` override flag
> via `clap`. Give me the `Cargo.toml`, the socket-discovery module, and the server bootstrap,
> then tell me the exact Claude Code `.mcp.json` entry to register it. Keep it minimal — just the
> handshake working. We'll add the bounded read tools in M1.

---

## 13. References

- Community CRUD MCP servers: `ckreiling/mcp-server-docker`, `quantgeekdev/docker-mcp` (GitHub).
- Container Doctor (freeCodeCamp) — the Python monitoring-daemon prior art.
- Docker MCP Catalog & Toolkit — docs.docker.com/ai/mcp-catalog-and-toolkit (different problem).
- `rmcp` — github.com/modelcontextprotocol/rust-sdk.
- `bollard` — docs.rs/bollard.
- OrbStack socket: `~/.orbstack/run/docker.sock`. Colima: `~/.colima/default/docker.sock`.
