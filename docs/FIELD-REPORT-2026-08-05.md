# Field report — kagoni-prod, 2026-08-05

Observations from one real incident session: an agent using `kagoni-prod`
(read-only, `ssh://root@<prod>`) to investigate a publicly-exposed Postgres,
verify a deploy, and confirm three fixes on a live host running 18 containers.

Roughly 8 tool calls. **Two of them timed out.** This is what worked, what
didn't, and what I'd change — ordered by impact.

---

## Verdict

The design thesis is right: bounded reads, explicit escape hatches, secrets
withheld by default. When the shape of the data matched what the tool expected,
it was excellent — one call replaced what would have been a multi-step SSH
session.

Two things undercut it in practice. `container_logs` timed out on the containers
that mattered most, and when it did return, its "bounded digest" was **larger
than the raw lines it was summarizing.**

---

## 1. `container_logs` timed out on 2 of 3 calls — HIGH

| call | params | result |
|---|---|---|
| `erp-backend` | `tail=100` | returned fine, ~2s |
| `erp-postgres` | `tail=5000`, `grep` | **>120s, backgrounded** |
| `erp-pgadmin` | `tail=400`, no filter | **>120s, backgrounded** |

The third is the damning one. `tail=400` with no grep, no level filter — and it
still blew the client's 120s budget. So this isn't `MAX_TAIL` being too generous
or grep being expensive. Baseline log retrieval falls off a cliff between 100 and
400 lines on some containers.

**Likely root cause, and it's not really kagoni's fault:** both slow containers
had `"LogConfig": {"Type": "json-file", "Config": {}}` — no `max-size`, no
`max-file`. 13 days of uptime, and `erp-postgres` was absorbing a brute-force
attack the whole time. The json-file on disk is plausibly enormous, and
`tail` over an SSH-tunnelled Docker socket has to drag it back.

**But kagoni owns the failure mode.** Right now it issues the request and waits
forever; the MCP client is what eventually gives up. From the agent's side that
is the worst possible outcome — 120 seconds of wall-clock spent, zero
information returned, and the call still running in the background consuming
the connection.

What I'd do:

- **Deadline the fetch.** Wrap the bollard stream in `tokio::time::timeout`,
  default ~15s, overridable. Kagoni should decide when to stop, not the client.
- **Return partial results on deadline.** The stream yields incrementally —
  cluster whatever arrived and return it with
  `{"truncated": true, "reason": "deadline", "lines_pulled": N}`. Partial data
  beats nothing, and the agent can decide whether to narrow and retry.
- **Prefer `since` over `tail` when both are plausible.** `since` is a
  server-side time filter the daemon can satisfy cheaply; a large `tail` on an
  unrotated log is the expensive path. Consider defaulting to
  `since=1h` when neither is given.
- **Surface the log size in `diagnose_container` / `inspect_container`.**
  A `log_bytes` field, plus a warning when `LogConfig.Config` is empty, would
  have told me upfront that `erp-postgres` logs were a trap — and would flag a
  real operational problem (unbounded logs filling a disk) that no current tool
  reports.

---

## 2. The cluster digest was *bigger* than the raw logs — HIGH

The one `container_logs` call that succeeded:

```
lines_pulled: 54
distinct_clusters: 34      ← 54 lines collapsed into 34 groups
clusters returned: 12
clusters_omitted: 22
```

**54 lines produced 34 distinct clusters.** A 1.6:1 compression ratio on the
tool whose entire premise is collapsing repetition. And because each cluster
carries both `template` and `sample` — near-identical strings, both bloated with
ANSI escapes — the returned payload was roughly **twice what dumping all 54 raw
lines would have cost.**

The bounded view was more expensive than the firehose it exists to prevent.

**Correction — this is not the "low repetition" case failing.** `docs/BENCHMARK.md`
already measures that scenario at **3.8×** in kagoni's favour, so the design
handles non-repetitive logs fine. The variable my session added was **ANSI
escapes** (see §3), which the benchmark fixtures don't appear to contain.

That makes the finding sharper, not broader: ANSI is what flipped a measured
3.8× win into a loss. Fixing §3 should restore benchmark behaviour without any
redesign. The mitigations below are worth doing on their own merits, but they
are *not* the fix — stripping ANSI is.

- **Omit `template` when `count == 1`.** In my output, 9 of 12 clusters had
  `count: 1`. For a singleton the template conveys nothing the sample doesn't —
  it's a redacted duplicate. Free ~40% payload cut.
- **Cap `sample` length.** One long line shouldn't be able to dominate.
  Truncate to ~200 chars with an ellipsis.
- **Add a low-repetition guard only if it still reproduces after the ANSI fix.**
  Re-measure first; the benchmark suggests it won't.

**Worth adding to `benches/`:** a fixture that emits ANSI-coloured structured
logs. The existing suite would have caught this if one container in
`bench-stack.compose.yml` logged in colour, which most real apps do.

---

## 3. ANSI escapes flow straight into clustering and output — HIGH

There is **no ANSI stripping anywhere in the codebase.** `grep -rn "ansi"` finds
only `main.rs:102 .with_ansi(false)`, which governs kagoni's own tracing, not
container output.

The Rust backend I was debugging emits colored `tracing` output, so every line
arrived as:

```
[2m2026-08-05T12:47:53.545204Z[0m [33m WARN[0m ...
```

Three separate costs:

1. **Token bloat.** Escapes roughly double each line, in `sample` *and*
   `template`.
2. **Unreadable templates.** `normalize()` splits on delimiters, so `[` breaks
   the escape apart and the digits inside get placeholdered — producing
   `[<NUM>m<NUM>-<NUM>-<NUM>T<NUM>:<NUM>:<NUM>...`. That is noise, not a
   skeleton.
3. **Broken clustering — the real damage.** Level-colored logs give INFO and
   WARN different escape sequences, so otherwise-identical messages hash to
   different skeletons and never group. This is a direct contributor to the
   34-clusters-from-54-lines result above.

**Fix:** strip ANSI in `parse_lines()`, immediately after `split_timestamp()`,
before the line reaches `normalize()` or gets stored as a sample. A small regex
(`\x1b\[[0-9;]*[a-zA-Z]`) or the `strip-ansi-escapes` crate. This is a handful
of lines and it improves payload size, readability, *and* cluster quality at
once — the highest value-per-line change on this list.

Keep the raw form only behind `raw=true`, where verbatim output is the point.

---

## 4. `inspect_container(full=true)` is all-or-nothing — MEDIUM

I needed exactly one value: whether prod's `POSTGRES_PASSWORD` matched local.
Getting it meant `full=true`, which returned the entire inspect blob — ~250
lines of `MaskedPaths`, `ReadonlyPaths`, image manifest annotations, network
sandbox keys — to read one string.

The projected default is genuinely well-judged. **Env-names-only is the right
call** and it did real work here: it let me confirm prod's env var set without
pulling secrets into a transcript at all. That's a good default and I'd keep it.

But the escape hatch is too coarse. Suggestions:

- `env=true` as a middle tier — projected view plus env *values*, nothing else.
- Or `select=["Config.Env", "NetworkSettings.Ports"]` for a dotted-path
  projection.

Either would have cut that call by ~95% and reduced how much secret material
landed in context — which matters, because `full=true` is the only door and
it opens onto everything.

---

## 5. `dev.earthly.git-sha` is not kagoni's bug, but kagoni could catch it — LOW

Both the old and new `erp-backend` images carried
`dev.earthly.git-sha: c48905267920d5599e849e8e043f801c6baab94a`. That commit
**does not exist in the repo** — `git cat-file` fails on it. Earthly is baking a
stale label.

I nearly used it to decide whether the deploy shipped my code. The thing that
actually worked was comparing the image digest against the local build:

```
prod:  sha256:5d7664076a5f9af2e0b3951e39314742d5c06734ffe120b3494f7ed2c50c15d0
local: 5d7664076a5f  (built 9 minutes ago)   ← exact match
```

`list_images` surfacing digests alongside tags, and `inspect_container`
promoting `Image` (the resolved digest) into the projected view, would make that
the obvious move rather than something I had to reach for. During an incident,
"is prod running my build?" is a top-five question and the digest is the only
honest answer.

---

## What was genuinely good

Worth keeping in mind while changing things:

- **`list_containers` ports column.** `0.0.0.0` vs `127.0.0.1:5433->5432/tcp` —
  one call answered the entire security question, before and after the fix. The
  compact row format is the right density. Nothing to change.
- **Env-names-only default.** Answered a real question without leaking a single
  secret. Exactly the right trade.
- **Read-only advertised in the server instructions.** The "those tools are
  absent, not refused — don't look for a workaround" phrasing meant I never
  wasted a call attempting a write, and never suggested one to the user.
- **The `note` field on projected responses.** Telling me the escape hatch
  exists, at the point of use, is better than hoping I remember the tool
  description.

---

## Suggested order of work

1. **Strip ANSI in `parse_lines()`** — smallest diff, fixes payload size,
   readability and cluster quality together.
2. **Deadline + partial results in `container_logs`** — turns the worst failure
   mode (120s, nothing) into a bounded, useful one.
3. **Add an ANSI-emitting container to `benches/fixtures/`** — then re-run
   `benches/run.py`. This turns the §2/§3 findings into a regression test and
   tells you whether anything beyond ANSI is actually wrong.
4. **Omit `template` when `count == 1`, cap `sample` length** — trivial, large
   payload win.
5. **`env=true` / `select=` on `inspect_container`** — narrows the only door to
   secret material.
6. **Expose image digest and log size** in the read tools.

Items 1–4 are all in `src/bound/logs.rs`, `src/tools/read.rs` and `benches/`,
and together address the two HIGH findings.

---

## Reproducing the bad cases

Neither needs prod. Both reproduce locally:

```bash
# Case A — unbounded log, slow tail
docker run -d --name noisy --log-driver json-file alpine \
  sh -c 'i=0; while true; do echo "line $i"; i=$((i+1)); done'
sleep 300   # let it grow
# then: container_logs(id="noisy", tail=400)

# Case B — ANSI + low repetition (any app with colored structured logging)
# then: container_logs(id="<app>", tail=100)
#       check distinct_clusters vs lines_scanned, and the template field
```

For Case B, `distinct_clusters / lines_scanned` approaching 1.0 is the signal
that clustering is doing nothing but adding overhead.
