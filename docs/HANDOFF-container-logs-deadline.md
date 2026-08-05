# Handoff — `container_logs` hangs without a deadline

**Status:** open. Second confirmed incident.
**Read `docs/FIELD-REPORT-2026-08-05.md` §1 first** — it diagnosed this and
proposed the fix. This document is a second data point that *refines* that
diagnosis in three ways, one of which explains why the timeout already in the
code did not help.

---

## What happened this time

Session: `kagoni-prod` (read-only, `ssh://root@<prod>`) driving 18 containers on
a 3.8 GB host, during a live incident. One call:

```
container_logs(id="nginx-proxy", grep="limiting requests",
               since="25m", tail=2000, max_clusters=5)
```

- Exceeded the client's 120s budget, was backgrounded
- Kept running and returned nothing
- Killed by the MCP client at **1800s** — *"sent no response or progress for
  1800s; aborting"*

Meanwhile the same question, asked over plain SSH, answered in seconds:

```sh
ssh root@<prod> 'docker logs --since 25m nginx-proxy 2>&1 \
  | grep "limiting requests" | sed -E "s/.../\1/" | sort | uniq -c'
```

Other `kagoni-prod` calls in the same session were fine: `list_containers`,
`inspect_container`, and `container_logs(tail=5000, raw=true)` — the last of
which returned promptly *because* it tripped the `MAX_RESPONSE_BYTES` cap and
was withheld.

---

## Three corrections to the existing diagnosis

### 1. It is not (only) unrotated log size

The field report's leading hypothesis is that both slow containers had
`LogConfig.Config == {}` and 13 days of accumulation, so `tail` had to drag a
huge file back over the SSH tunnel. Reasonable, and probably right in that
session.

**It does not explain this one.** `nginx-proxy`'s json-file had been truncated
to **0 bytes about four hours earlier** by a `copytruncate` logrotate run, and
measured **73 KB** at 14:02, shortly before the failing call. There was no
enormous file to drag.

So "unbounded log" is a contributing factor at most, not the mechanism.
Whatever the mechanism is, it can hang on a 73 KB log.

### 2. `since` does not rescue it

The report recommends *"prefer `since` over `tail` … a server-side time filter
the daemon can satisfy cheaply"* and suggests defaulting to `since=1h`.

The failing call **already passed `since="25m"`** — alongside `tail=2000`. It
still hung.

Worth checking before that recommendation is implemented: with the json-file
driver, `since` is not obviously cheap. The daemon has to locate the timestamp
cutoff, and combining it with `tail` may mean scanning rather than seeking.
`since` + `tail` together could plausibly be *worse* than either alone. This
wants measuring, not assuming — see the benchmark suggestion below.

### 3. There *is* already a 120s timeout, and it did not fire — this is the
important one

`src/engine/client.rs:50` connects remote endpoints with:

```rust
Docker::connect_with_host(&endpoint.address)
```

That looks timeout-free next to the unix branch, which passes `TIMEOUT_SECS`
explicitly. It isn't: bollard's `connect_with_host` dispatches on scheme and
passes its own `DEFAULT_TIMEOUT` (`bollard-0.21.0/src/docker.rs:99` — 120s) to
`connect_with_ssh`.

**So a 120s timeout was in force, and the call still ran for 1800s.**

The conclusion is that bollard's timeout bounds the *request*, not the draining
of a streaming response body. `container_logs` drains at
`src/tools/read.rs:306`:

```rust
let mut stream = self.engine().docker().logs(&params.id, Some(builder.build()));
let mut lines: Vec<logs::LogLine> = Vec::new();
while let Some(chunk) = stream.next().await {
```

There is no per-chunk or wall-clock deadline on that loop. If the stream stalls
between chunks — a half-open SSH tunnel, a loaded host, a daemon that has
accepted the request and gone quiet — this awaits forever, and the connection
timeout never applies because the connection is not what failed.

This makes the field report's recommendation #1 not merely an improvement but
**the actual fix**, and it explains why bumping any connect timeout would not
have helped.

---

## Contributing factor: the host was degraded

Not kagoni's fault, but it is what turned a latent bug into a 30-minute hang.
At the time of the failing call the host was recovering from `gzip` over 545 MB
of rotated container logs; load average was 3.11. Two *plain* `ssh` commands
issued from the same session in the same window also had to be killed, at 90s
and 120s.

So the trigger was environmental. The defect is that kagoni has no way to give
up, whereas every other caller of that host did.

---

## The fix

`container_exec` already solves exactly this, at `src/tools/actions.rs:550`:

```rust
// Drain under a wall-clock deadline. On timeout we keep whatever was
// captured — partial output from a hung command is usually the most
// useful thing we have.
let drained = tokio::time::timeout(Duration::from_secs(timeout), async {
    while let Some(chunk) = output.next().await { ... }
}).await;
```

Apply the same shape to `container_logs` (`src/tools/read.rs:300-315`), and to
`diagnose_container`'s log fetch (`src/tools/diagnose.rs:353`), which has the
same unbounded drain and is the tool most likely to be called *first* on a sick
container.

Specifically:

1. **Deadline the drain**, default ~15s, overridable per call. Kagoni decides
   when to stop, not the client.
2. **Return what arrived.** The stream yields incrementally, so cluster the
   partial result and return it with
   `{"truncated": true, "reason": "deadline", "lines_pulled": N}`. Partial data
   beats nothing, and the agent can narrow and retry.
3. **Say what to do next in the payload.** On a deadline hit, suggest a smaller
   `tail` and name the current one. The agent cannot see the constant.
4. **Emit MCP progress notifications** while draining. The client killed this at
   1800s only because nothing was ever sent; progress would both keep the
   channel alive and let the deadline be the thing that ends it.

---

## Worth measuring first

`benches/` already exists. Before implementing recommendation #3 from the field
report (`since` by default), measure on a remote SSH endpoint:

| case | expectation to test |
|---|---|
| `tail=400`, no `since` | field report saw >120s |
| `since=25m`, no `tail` | is `since` alone actually cheap? |
| `since=25m` + `tail=2000` | **this session's hang** — is the combination worst? |
| same three, local unix socket | isolates SSH transport from daemon cost |
| same three, 0-byte vs 250 MB log | isolates file size from everything else |

The two incidents disagree about log size, so that last row is the one that
settles whether the current hypothesis survives.

---

## Note on filtering, unrelated to the hang

`grep` and `level` are applied client-side *after* the whole tail is pulled
(`src/tools/read.rs:319-324`). That is documented behaviour and correct — the
Docker API has no such parameter — but it means a filter never reduces bytes on
the wire. Users reasonably assume `grep` makes a call cheaper. Worth a sentence
in the tool description, since the natural reaction to a slow call is to add a
filter, which does not help.
