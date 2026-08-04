# Bosun

An **engine-agnostic, agent-ergonomic Docker MCP server** in Rust.

Bosun exposes container lifecycle, logs, stats and Compose as MCP tools — but every
read is **bounded and summarizing by default**, every destructive write is **gated**,
and it talks the plain Docker Engine socket, so it drives Docker, OrbStack, Podman or
Colima interchangeably.

It is not a monitoring daemon. It is a control surface for a human-in-the-loop agent.

---

## Why this exists

This space is not empty — thin CRUD MCP wrappers around `docker ps` are everywhere, and
autonomous remediation daemons already exist. Bosun is narrower than either:

| | Bosun | Thin CRUD wrappers | Autonomous daemons |
|---|---|---|---|
| Interaction | human-in-the-loop via MCP | human-in-the-loop | autonomous loop |
| Read discipline | **bounded by design** | relays raw daemon output | n/a |
| Diagnosis | deterministic, evidence-carrying | none | LLM-in-the-loop |
| Engines | any Docker-API engine | usually Docker only | usually Docker only |

The one idea worth the code is **token-budget as the primary design constraint**. A
crash-looping container emits the same stacktrace five hundred times; relaying that
verbatim spends a context window to say one thing. Bosun's job is to consume the
firehose and hand back a digest.

**Design rule:** *the default response to any Bosun tool should be safe to put in a
context window unread.* If it isn't, the tool is wrong.

---

## Install

```bash
cargo install --path .
# or, once published:
cargo install bosun
```

This puts `bosun` in `~/.cargo/bin`. If `bosun: command not found`, that directory
isn't on your `PATH` — add it, or run the binary directly from
`./target/release/bosun` after `cargo build --release`.

Verify it can find your engine before wiring it into anything:

```bash
bosun --check
```

```
engine:         orbstack
socket:         /Users/you/.orbstack/run/docker.sock
resolved from:  ~/.orbstack/run/docker.sock
server version: 29.4.0
api version:    1.54
```

## Register with Claude Code

Add to `.mcp.json` in your project (or `~/.claude.json` for all projects):

```json
{
  "mcpServers": {
    "bosun": {
      "command": "bosun",
      "args": []
    }
  }
}
```

With an explicit socket and debug logging:

```json
{
  "mcpServers": {
    "bosun": {
      "command": "bosun",
      "args": ["--socket", "/var/run/docker.sock"],
      "env": { "BOSUN_LOG": "debug" }
    }
  }
}
```

Then `/mcp` in Claude Code should list bosun with 18 tools.

See [docs/prompts.md](docs/prompts.md) for things to ask it, grouped by what each one
exercises — including a set for attacking the write-safety gate.

---

## Tools

### Reads — bounded

| Tool | Bound | Escape hatch |
|---|---|---|
| `list_containers` | running only, 100 rows | `all`, `limit`, `filter` |
| `inspect_container` | projected fields, **env keys only** | `full=true` |
| `container_logs` | tail 200 → clustered digest, 12 clusters | `raw=true`, `tail`, `max_clusters` |
| `container_stats` | one snapshot, never a stream | — |
| `list_images` | 100 rows, largest first | `limit`, `dangling` |
| `compose_ps` | per-service rows | — |

### Actions — guarded

| Tool | Risk |
|---|---|
| `container_start` / `container_stop` / `container_restart` | safe, direct |
| `pull_image` | safe; layer progress collapsed to a summary |
| `compose_up` | safe; build/pull output collapsed |
| `container_rm` | **destructive — gated** |
| `compose_down` (with `volumes=true`) | **destructive — gated** |
| `container_exec` | **destructive — gated** |

`container_exec` was originally excluded from v1. It was added after real use showed the
omission didn't prevent exec — it pushed the agent to `Bash(docker exec …)` instead, which
is unbounded, unaudited and ungated. See [Why exec exists](#why-exec-exists).

### Diagnostics — deterministic

| Tool | What it does |
|---|---|
| `diagnose_container` | verdict + `likely_cause` + `evidence[]` + `suggested_actions[]` |
| `explain_exit_code` | decodes 137/143/139/126/127/… including the 128+N signal rule |
| `why_compose_failing` | cross-service: port conflicts, failing services, OOM clusters |

### Resources

`docker://containers` · `docker://container/{id}` · `docker://compose/{project}`

---

## The two guarantees

### 1. Reads are bounded

`container_logs` normalizes each line into a skeleton — numbers, timestamps, UUIDs, hex
ids and IPs replaced by placeholders — then groups lines that share one. Real output
from a crash-looping container:

```json
{
  "lines_pulled": 16,
  "distinct_clusters": 2,
  "clusters": [
    {
      "template": "ERROR failed to connect to database at <IP>:<NUM>",
      "sample": "ERROR failed to connect to database at 10.0.0.5:5432",
      "count": 8,
      "level": "error",
      "first_seen": "2026-08-04T01:13:25.943Z",
      "last_seen":  "2026-08-04T01:13:46.708Z"
    }
  ]
}
```

Clusters are ranked **severity first, then recency** — a single error outranks a
thousand info lines when the cap forces a choice. Every bounded tool documents its cap
in its own description and exposes `raw` / `full`, so a truncation is never something
the agent can't undo. A final 24 KB byte cap backstops every response; if it trips, the
result degrades to a self-describing summary that names the knob to turn.

`inspect_container` returns environment variable **names without values**, so a secret
can't reach a context window by accident.

### 2. Destructive writes are gated

`container_rm` and `compose_down --volumes` do nothing without either `dry_run=true` or
a `confirm` token echoing the target's exact name. Echoing the name is the point: it's a
check the agent can't satisfy by pattern-matching a boolean, because it has to have
looked up what it is about to destroy.

```jsonc
// container_rm(id="my-db", force=true)   ← no authorization
{
  "refused": true,
  "reason": "This operation is destructive and was called without authorization.",
  "consequences": [
    "container 'my-db' would be removed",
    "running container 'my-db' would be KILLED first",
    "anonymous volume 'a1b2…' would be DELETED (irreversible)"
  ],
  "to_proceed": "Re-run with confirm=\"my-db\" to authorize.",
  "to_preview": "Re-run with dry_run=true to see what would happen."
}
```

`dry_run` beats `confirm` when both are passed — ambiguity resolves toward the
non-destructive reading. `force` is never defaulted on. Every authorized destructive
action is logged to stderr as an audit trail.

The classification is enforced, not documented: a test walks the live tool router and
fails if any tool lacks a `safety::risk_of` entry, or if a destructive one doesn't
declare `destructive_hint` and expose both gates in its schema. A new destructive tool
cannot ship un-gated by accident.

#### What the gate does and does not guarantee

Be precise about this, because it is easy to over-read.

**It does:** force the caller to identify its target before acting, which prevents
acting on the wrong container; make destruction impossible as an accidental side effect
of a vaguely-worded request; produce an audit line for every authorized action; and
give the caller a way to see the consequences first.

**It does not:** put a human in the loop. An agent can look up a container's name and
echo it back on its own — that is the intended flow, not a bypass. The confirm token is
an *agent-attention* mechanism, not an approval mechanism.

#### Elicitation — the check an agent genuinely cannot satisfy

When the client advertises the MCP **elicitation** capability, every destructive tool
asks the operator directly before acting, and the confirm token is no longer required —
a human said yes, so demanding a token too would be asking the same question twice:

```
Bosun wants to remove container 'my-db' (force-killing it) and its anonymous volumes.

  • container 'my-db' would be removed
  • running container 'my-db' would be KILLED first
  • anonymous volume 'a1b2…' would be DELETED (irreversible)

Approve?
```

No agent can answer that on the operator's behalf. Order of authority is:

1. `dry_run` — nothing will happen, so there is nothing to approve
2. **elicitation**, if the client supports it — a real human decision
3. the `confirm` token, as the fallback

Clients that don't implement elicitation fall back to step 3 automatically, which is
most of them today. Failing closed instead would break every such client, so the
fallback is deliberate — but note it means the strength of the gate depends on your
client. `bosun_info` reports which mode is in effect.

An elicitation that *errors* is treated as denial rather than falling back: a client
that advertised the capability and then failed to deliver has told us nothing about
consent, and that is the one place failing closed is right.

### Why exec exists

`container_exec` was excluded from v1 on the reasoning that arbitrary code execution is
too sharp an edge to hand an agent. The first real debugging session disproved it: the
agent needed to know whether an image shipped `curl`, found no Bosun tool for it, and ran

```
Bash(docker exec <container> sh -c 'command -v python3 wget nc ...')
```

**Omitting exec did not prevent exec.** It routed it somewhere Bosun couldn't bound the
output, couldn't audit the call, and couldn't gate it. The exclusion made the tool surface
smaller without making anything safer.

So exec is here, and it earns its place by being the *stricter* path: argv-only (never a
shell string, so nothing is interpreted by a shell Bosun doesn't control), output capped
at 8 000 chars per stream, a timeout that defaults to 30s and hard-caps at 300s, and
`Destructive` classification so every call passes §6. HANDOFF §6 anticipated this —
it listed "maybe `exec`" among the destructive tools from the start.

---

## What it actually saves

Full reproducible numbers: **[docs/BENCHMARK.md](docs/BENCHMARK.md)** — generated by
`benches/run.py` against a committed fixture stack, with a real BPE tokenizer, including
the scenarios where Bosun *loses*.

Headline, from that run:

<!-- BENCH:START — generated by benches/run.py, do not edit by hand -->

| Scenario | Raw CLI | Bosun | |
|---|---:|---:|---|
| Logs, repetitive | 30,420 | 380 | **80×** |
| Fleet health | 31,018 | 2,340 | **13×** |
| Logs, low repetition | 5,065 | 1,321 | **3.8×** |
| Container listing | 176 | 400 | 0.44× — *worse* |

Bosun's 18 tool schemas cost **~4,476 tokens resident per session** whether used or not,
so the all-in figure for one fleet-health question is 31,018 → 6,816, about **4.6×**.
Break-even is roughly one non-trivial container question per session.
<!-- BENCH:END -->

Earlier ad-hoc measurements on a live container, for reference:

| Call | Raw CLI | Bosun | |
|---|---|---|---|
| `container_logs` (tail 200) | ~7 780 | ~1 520 | **5.1× less** |
| `inspect_container` | ~3 020 | ~570 | **5.3× less** |
| `container_stats` | ~28 | ~98 | 3.5× *more* |
| `list_containers` | ~368 | ~554 | 1.5× *more* |

Worth being straight about: the win is concentrated entirely in `container_logs` and
`inspect_container`, the two calls that dominate a real debugging session. On the small
reads Bosun costs *more* than a `docker ... --format table`, because a compact table is
compact by being lossy — it drops block IO, PIDs, throttling, health and limits — and
because JSON with field names is inherently wordier than columns.

That's the correct trade for a tool an agent parses rather than a human skims, but it
means "Bosun always saves tokens" would be false. Across a full session the net came out
around 3–7× fewer tokens, all of it from logs and inspect.

---

## Diagnosis is deterministic

**No LLM call happens inside Bosun.** The calling agent is the LLM; Bosun's job is to be
a fast, honest data source. Every verdict carries the evidence it was built from, so the
agent can check the reasoning and disagree.

The case that shows why this is worth having — two containers, both exit code 137:

```
bosun-oom                 → "Killed by the kernel OOM killer — the process exceeded its memory limit."
                            evidence: state.oom_killed=true — this is the decisive signal

bosun-sigkill-not-oom     → "Exited with code 137: Killed by SIGKILL. Usually an out-of-memory kill,
                             sometimes a `docker stop` that timed out."
                            evidence: state.exit_code=137, restart_count=0
```

An exit-code lookup table calls both of these OOM. Checking `State.OOMKilled` tells them
apart — and when the signal genuinely is ambiguous, the answer says so rather than
guessing.

Crash-loop detection combines restart count with **uptime**, so a container sampled in
the instant it happens to be running is still recognized as looping, while one that
restarted nine times over three months is not.

---

## Engine discovery

Resolved in order, first hit wins:

1. `--socket` flag
2. `DOCKER_HOST`
3. `~/.orbstack/run/docker.sock`
4. `~/.colima/default/docker.sock`
5. `/var/run/docker.sock`
6. `$XDG_RUNTIME_DIR/podman/podman.sock`

OrbStack and Colima come before `/var/run/docker.sock` because on macOS that path is
usually a symlink into one of them — matching the real path first yields an honest
engine label. The connection is verified by probing `/version`, so a socket that exists
but doesn't speak the Engine API fails with a clear message.

Apple's `container` tool does **not** expose a Docker Engine API and is unsupported; it
reports that explicitly rather than failing cryptically.

### Remote hosts

`--socket` and `DOCKER_HOST` accept `ssh://user@host`, `tcp://host:2375` and
`https://host:2376` as well as local paths:

```bash
bosun --check --socket ssh://deploy@prod.example.com
```

**Prefer `ssh://`.** An exposed `tcp://…:2375` is unauthenticated root on that machine —
anyone who can reach the port can start a privileged container. SSH reuses the access
control you already have.

Bosun always connects to the address it reports. That sounds obvious; it was not always
true (see the note on `connect_with_defaults` in `engine/client.rs`), and it matters
because every destructive tool acts on whichever daemon you are actually bound to.
Run `bosun --check` first and read the `socket:` line.

---

## Trying it without an MCP client

`scripts/try.sh` drives the server over stdio exactly as a client would, so you can
exercise any tool from a shell:

```bash
cargo build --release

./scripts/try.sh                                     # handshake + list tools
./scripts/try.sh bosun_info
./scripts/try.sh explain_exit_code '{"code":137}'    # no daemon needed
./scripts/try.sh list_containers '{"all":true}'
./scripts/try.sh diagnose_container '{"id":"my-container"}'
./scripts/try.sh container_logs '{"id":"my-container","level":"error"}'
./scripts/try.sh container_rm '{"id":"my-container","dry_run":true}'   # previews, removes nothing
```

Tools flagged `DESTRUCTIVE` in the listing are the gated ones.

## Development

```bash
cargo test          # 82 unit tests, no daemon required
cargo build --release
bosun --check       # verify engine discovery
```

The diagnostic fixtures reproduce the failures the diagnostics are built for:

```bash
docker compose -p bosun-fixture-crash -f tests/fixtures/crash-loop.compose.yml up -d
docker compose -p bosun-fixture-oom   -f tests/fixtures/oom.compose.yml up -d
# ... exercise diagnose_container, then:
docker compose -p bosun-fixture-crash -f tests/fixtures/crash-loop.compose.yml down
docker compose -p bosun-fixture-oom   -f tests/fixtures/oom.compose.yml down -v
```

`oom.compose.yml` deliberately includes a **non**-OOM container that also exits 137, so
the discrimination above is testable rather than assumed.

### Layout

```
src/
  main.rs          CLI + bootstrap          engine/     socket discovery, bollard wrapper
  server.rs        rmcp server, resources   bound/      logs.rs (cluster-dedup), project.rs
  safety.rs        §6 gate + classification tools/      read, actions, diagnose, compose
```

> **stdout discipline:** an stdio MCP server speaks JSON-RPC on stdout. All logging goes
> to stderr — `--log-level` and `BOSUN_LOG` control it. The one thing that writes to
> stdout outside the protocol is `--check`, which exits before serving.

---

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this work by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.

© 2026 Nhat-Vu
