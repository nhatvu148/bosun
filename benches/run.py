"""Bosun token benchmark: Bosun vs the raw docker CLI, same question either way.

Each scenario pairs the Bosun calls an agent actually makes against the docker
commands it would otherwise run to answer the *same* question. Scenarios where
Bosun loses are included deliberately — a benchmark that only reports wins gets
taken apart the first time somebody checks it.

    docker compose -p bosun-bench -f benches/fixtures/bench-stack.compose.yml up -d
    python3 benches/run.py > docs/BENCHMARK.md
"""

import os
import subprocess
import sys
import time

# Resolve everything from this file, not the caller's cwd — the README tells
# people to run it from the repo root, and a benchmark that only works from one
# directory is a benchmark people quietly stop running.
HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)

from measure import TOKENIZER, bosun, count, resident_cost, sh  # noqa: E402

BINARY = os.path.join(ROOT, "target", "release", "bosun")
COMPOSE_FILE = os.path.join(HERE, "fixtures", "bench-stack.compose.yml")
PROJECT = "bosun-bench"
FLEET = [
    "bosun-bench-chatty",
    "bosun-bench-varied",
    "bosun-bench-crashloop",
    "bosun-bench-quiet-1",
    "bosun-bench-quiet-2",
    "bosun-bench-quiet-3",
]


def running_fixture() -> list[str]:
    out = sh(["docker", "ps", "-a", "--filter", f"label=com.docker.compose.project={PROJECT}",
              "--format", "{{.Names}}"])
    return [n for n in out.split() if n]


# Each scenario: (name, question, raw_fn, bosun_fn).
# The functions return (tokens, call_count) so we can report both — the call
# count is a latency and reliability story the token number doesn't tell.
#
# FAIRNESS: both sides must cover exactly the same containers. The first
# version of this benchmark ran an unfiltered `docker ps -a` against a
# *filtered* Bosun call, so on a machine with other containers running the raw
# side was charged for rows Bosun never returned. That inflated Bosun's numbers
# by comparing two different questions. Every raw command below is scoped to
# the fixture stack, exactly as the Bosun call is.

PS_FORMAT = "table {{.ID}}\t{{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}"
STATS_FORMAT = ("table {{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}"
                "\t{{.NetIO}}\t{{.BlockIO}}\t{{.PIDs}}")
SCOPE = ["--filter", f"label=com.docker.compose.project={PROJECT}"]


def raw_fleet(names):
    calls = [["docker", "ps", "-a", *SCOPE, "--format", PS_FORMAT]]
    calls += [["docker", "inspect", n] for n in names]
    calls += [["docker", "logs", "--tail", "200", "--timestamps", n] for n in names]
    # Explicit names, so stats covers this stack and nothing else.
    running = [n for n in names if "crashloop" not in n]
    calls += [["docker", "stats", "--no-stream", "--format", STATS_FORMAT, *running]]
    return sum(count(sh(c)) for c in calls), len(calls)


def bosun_fleet(names):
    out = [
        bosun(BINARY, "list_containers", {"all": True, "filter": "bosun-bench"}),
        bosun(BINARY, "diagnose_container", {"ids": names}),
        bosun(BINARY, "container_stats", {"ids": [n for n in names if "crashloop" not in n]}),
    ]
    return sum(count(o) for o in out), len(out)


def raw_triage(names):
    target = "bosun-bench-crashloop"
    calls = [["docker", "inspect", target],
             ["docker", "logs", "--tail", "200", "--timestamps", target]]
    return sum(count(sh(c)) for c in calls), len(calls)


def bosun_triage(names):
    out = [bosun(BINARY, "diagnose_container", {"id": "bosun-bench-crashloop"})]
    return sum(count(o) for o in out), len(out)


def raw_logs_repetitive(names):
    calls = [["docker", "logs", "--tail", "600", "--timestamps", "bosun-bench-chatty"]]
    return sum(count(sh(c)) for c in calls), len(calls)


def bosun_logs_repetitive(names):
    out = [bosun(BINARY, "container_logs", {"id": "bosun-bench-chatty", "tail": 600})]
    return sum(count(o) for o in out), len(out)


def raw_logs_varied(names):
    calls = [["docker", "logs", "--tail", "200", "--timestamps", "bosun-bench-varied"]]
    return sum(count(sh(c)) for c in calls), len(calls)


def bosun_logs_varied(names):
    out = [bosun(BINARY, "container_logs", {"id": "bosun-bench-varied", "tail": 200})]
    return sum(count(o) for o in out), len(out)


def raw_list(names):
    calls = [["docker", "ps", "-a", *SCOPE, "--format", PS_FORMAT]]
    return sum(count(sh(c)) for c in calls), len(calls)


def bosun_list(names):
    out = [bosun(BINARY, "list_containers", {"all": True, "filter": "bosun-bench"})]
    return sum(count(o) for o in out), len(out)


SCENARIOS = [
    ("Fleet health", "How is everything doing?", raw_fleet, bosun_fleet),
    ("Crash-loop triage", "Why is this container failing?", raw_triage, bosun_triage),
    ("Logs, repetitive", "600 lines, heavy repetition", raw_logs_repetitive, bosun_logs_repetitive),
    ("Logs, low repetition", "200 structurally distinct lines", raw_logs_varied, bosun_logs_varied),
    ("Container listing", "What is running?", raw_list, bosun_list),
]


def main() -> int:
    if not os.path.exists(BINARY):
        print(f"build first: cargo build --release  (missing {BINARY})", file=sys.stderr)
        return 1

    names = running_fixture()
    if len(names) < len(FLEET):
        print(
            "fixture stack is not up. Start it first:\n"
            f"  docker compose -p {PROJECT} -f {COMPOSE_FILE} up -d",
            file=sys.stderr,
        )
        return 1
    names.sort()

    schemas, instructions, ntools = resident_cost(BINARY)
    resident = schemas + instructions

    rows, totals = [], [0, 0, 0, 0]
    for label, question, raw_fn, bosun_fn in SCENARIOS:
        t0 = time.monotonic(); raw_tok, raw_calls = raw_fn(names); raw_s = time.monotonic() - t0
        t0 = time.monotonic(); bos_tok, bos_calls = bosun_fn(names); bos_s = time.monotonic() - t0
        ratio = raw_tok / bos_tok if bos_tok else 0.0
        rows.append((label, question, raw_tok, raw_calls, raw_s, bos_tok, bos_calls, bos_s, ratio))
        totals[0] += raw_tok; totals[1] += raw_calls
        totals[2] += bos_tok; totals[3] += bos_calls

    version = sh([BINARY, "--version"]).strip()
    engine = next((l.split(":", 1)[1].strip() for l in sh([BINARY, "--check"]).splitlines()
                   if l.startswith("engine:")), "unknown")

    p = print
    p("# Benchmark: Bosun vs the raw `docker` CLI")
    p("")
    p("Generated by `benches/run.py`. Every number here is reproducible — see")
    p("[Reproducing](#reproducing).")
    p("")
    p(f"- **Tokenizer:** {TOKENIZER}")
    p(f"- **Bosun:** {version} · **engine:** {engine}")
    p(f"- **Fixture:** {len(names)} containers from `benches/fixtures/bench-stack.compose.yml`")
    p("")
    p("## Per scenario")
    p("")
    p("| Scenario | Raw tokens | Raw calls | Bosun tokens | Bosun calls | Ratio |")
    p("|---|---:|---:|---:|---:|---:|")
    for label, _q, rt, rc, _rs, bt, bc, _bs, ratio in rows:
        verdict = f"**{ratio:.1f}×**" if ratio >= 1.05 else (
            f"{ratio:.2f}× ✗" if ratio < 0.95 else f"{ratio:.2f}×")
        p(f"| {label} | {rt:,} | {rc} | {bt:,} | {bc} | {verdict} |")
    p(f"| **All scenarios** | **{totals[0]:,}** | **{totals[1]}** | "
      f"**{totals[2]:,}** | **{totals[3]}** | "
      f"**{totals[0]/totals[2]:.1f}×** |")
    p("")
    p("`✗` marks a scenario where Bosun costs **more** than the CLI. Those are")
    p("here on purpose.")
    p("")
    p("## The fixed cost")
    p("")
    p(f"Bosun's {ntools} tool schemas and handshake instructions are always-on context —")
    p("present in every session whether or not a tool is called:")
    p("")
    p(f"| Tool schemas ({ntools} tools) | {schemas:,} tok |")
    p("|---|---:|")
    p(f"| Handshake instructions | {instructions:,} tok |")
    p(f"| **Resident per session** | **{resident:,} tok** |")
    p("")
    fleet_raw, fleet_bos = rows[0][2], rows[0][5]
    allin = fleet_bos + resident
    p("Against a single fleet-health question:")
    p("")
    p(f"    raw CLI                {fleet_raw:>8,} tok")
    p(f"    bosun (calls only)     {fleet_bos:>8,} tok    {fleet_raw/fleet_bos:.1f}x cheaper")
    p(f"    bosun (+ resident)     {allin:>8,} tok    {fleet_raw/allin:.1f}x cheaper, all-in")
    p("")
    p(f"Break-even is roughly **one** non-trivial container question per session. Below")
    p("that, the schemas are pure overhead — an argument for loading Bosun where you")
    p("actually use it rather than in every project.")
    p("")
    p("## Reading this honestly")
    p("")
    p("- **The win is concentrated.** Nearly all of it comes from `inspect` projection")
    p("  and log clustering. Those dominate real troubleshooting, which is why the")
    p("  headline number is large — but the small reads genuinely cost more.")
    p("- **Clustering depends on repetition.** The two log scenarios differ by an order")
    p("  of magnitude for exactly that reason. Real service logs are usually closer to")
    p("  the repetitive case; a one-shot batch job is closer to the varied one.")
    p("- **Call count matters as much as tokens.** Fewer round trips is less latency and")
    p("  fewer chances for an agent to lose the thread.")
    if TOKENIZER.startswith("ESTIMATE"):
        p("- **⚠️ These are byte-count estimates, not real tokens.** Dense JSON tokenizes")
        p("  worse than bytes/4 and log prose better, which flatters Bosun. Install")
        p("  `tiktoken` and re-run before quoting these anywhere.")
    p("")
    p("## Reproducing")
    p("")
    p("```bash")
    p("cargo build --release")
    p("pip install tiktoken   # for real token counts")
    p(f"docker compose -p {PROJECT} -f benches/fixtures/bench-stack.compose.yml up -d")
    p("sleep 10               # let the fixtures emit their logs")
    p("python3 benches/run.py > docs/BENCHMARK.md")
    p(f"docker compose -p {PROJECT} -f benches/fixtures/bench-stack.compose.yml down")
    p("```")
    return 0


if __name__ == "__main__":
    sys.exit(main())
