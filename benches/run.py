"""Kagoni token benchmark: Kagoni vs the raw docker CLI, same question either way.

Each scenario pairs the Kagoni calls an agent actually makes against the docker
commands it would otherwise run to answer the *same* question. Scenarios where
Kagoni loses are included deliberately — a benchmark that only reports wins gets
taken apart the first time somebody checks it.

    docker compose -p kagoni-bench -f benches/fixtures/bench-stack.compose.yml up -d
    python3 benches/run.py --sync-readme > docs/BENCHMARK.md

--sync-readme also rewrites the generated headline block in README.md, so the
advertised numbers cannot drift from the artifact backing them.
"""

import os
import re
import subprocess
import sys

# Resolve everything from this file, not the caller's cwd — the README tells
# people to run it from the repo root, and a benchmark that only works from one
# directory is a benchmark people quietly stop running.
HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)

from measure import TOKENIZER, kagoni, count, resident_cost, sh  # noqa: E402

BINARY = os.path.join(ROOT, "target", "release", "kagoni")
COMPOSE_FILE = os.path.join(HERE, "fixtures", "bench-stack.compose.yml")
PROJECT = "kagoni-bench"
FLEET = [
    "kagoni-bench-chatty",
    "kagoni-bench-varied",
    "kagoni-bench-crashloop",
    "kagoni-bench-quiet-1",
    "kagoni-bench-quiet-2",
    "kagoni-bench-quiet-3",
]


def running_fixture() -> list[str]:
    out = sh(["docker", "ps", "-a", "--filter", f"label=com.docker.compose.project={PROJECT}",
              "--format", "{{.Names}}"])
    return [n for n in out.split() if n]


# Each scenario: (name, question, raw_fn, kagoni_fn).
# The functions return (tokens, call_count) so we can report both — the call
# count is a latency and reliability story the token number doesn't tell.
#
# FAIRNESS: both sides must cover exactly the same containers. The first
# version of this benchmark ran an unfiltered `docker ps -a` against a
# *filtered* Kagoni call, so on a machine with other containers running the raw
# side was charged for rows Kagoni never returned. That inflated Kagoni's numbers
# by comparing two different questions. Every raw command below is scoped to
# the fixture stack, exactly as the Kagoni call is.

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


def kagoni_fleet(names):
    out = [
        kagoni(BINARY, "list_containers", {"all": True, "filter": "kagoni-bench"}),
        kagoni(BINARY, "diagnose_container", {"ids": names}),
        kagoni(BINARY, "container_stats", {"ids": [n for n in names if "crashloop" not in n]}),
    ]
    return sum(count(o) for o in out), len(out)


def raw_triage(names):
    target = "kagoni-bench-crashloop"
    calls = [["docker", "inspect", target],
             ["docker", "logs", "--tail", "200", "--timestamps", target]]
    return sum(count(sh(c)) for c in calls), len(calls)


def kagoni_triage(names):
    out = [kagoni(BINARY, "diagnose_container", {"id": "kagoni-bench-crashloop"})]
    return sum(count(o) for o in out), len(out)


def raw_logs_repetitive(names):
    calls = [["docker", "logs", "--tail", "600", "--timestamps", "kagoni-bench-chatty"]]
    return sum(count(sh(c)) for c in calls), len(calls)


def kagoni_logs_repetitive(names):
    out = [kagoni(BINARY, "container_logs", {"id": "kagoni-bench-chatty", "tail": 600})]
    return sum(count(o) for o in out), len(out)


def raw_logs_varied(names):
    calls = [["docker", "logs", "--tail", "200", "--timestamps", "kagoni-bench-varied"]]
    return sum(count(sh(c)) for c in calls), len(calls)


def kagoni_logs_varied(names):
    out = [kagoni(BINARY, "container_logs", {"id": "kagoni-bench-varied", "tail": 200})]
    return sum(count(o) for o in out), len(out)


def raw_list(names):
    calls = [["docker", "ps", "-a", *SCOPE, "--format", PS_FORMAT]]
    return sum(count(sh(c)) for c in calls), len(calls)


def kagoni_list(names):
    out = [kagoni(BINARY, "list_containers", {"all": True, "filter": "kagoni-bench"})]
    return sum(count(o) for o in out), len(out)


SCENARIOS = [
    ("Fleet health", "How is everything doing?", raw_fleet, kagoni_fleet),
    ("Crash-loop triage", "Why is this container failing?", raw_triage, kagoni_triage),
    ("Logs, repetitive", "600 lines, heavy repetition", raw_logs_repetitive, kagoni_logs_repetitive),
    ("Logs, low repetition", "200 structurally distinct lines", raw_logs_varied, kagoni_logs_varied),
    ("Container listing", "What is running?", raw_list, kagoni_list),
]


README_START = "<!-- BENCH:START — generated by benches/run.py, do not edit by hand -->"
README_END = "<!-- BENCH:END -->"


def sync_readme(block: str) -> None:
    """Rewrite the generated block in README.md.

    The headline table used to be hand-copied from docs/BENCHMARK.md, and it
    went stale the first time the numbers were regenerated — which is exactly
    how an advertised figure ends up not matching the artifact backing it.
    Generating it from the same run removes the possibility rather than
    documenting the hazard.
    """
    path = os.path.join(ROOT, "README.md")
    with open(path) as fh:
        text = fh.read()
    if README_START not in text or README_END not in text:
        print(
            f"README.md is missing the {README_START} / {README_END} markers; "
            "cannot sync the headline table.",
            file=sys.stderr,
        )
        raise SystemExit(1)
    new = re.sub(
        re.escape(README_START) + r".*?" + re.escape(README_END),
        README_START + "\n" + block + README_END,
        text,
        flags=re.DOTALL,
    )
    with open(path, "w") as fh:
        fh.write(new)
    print("README.md headline table synced", file=sys.stderr)


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
    for label, question, raw_fn, kagoni_fn in SCENARIOS:
        raw_tok, raw_calls = raw_fn(names)
        bos_tok, bos_calls = kagoni_fn(names)

        # Reject a zero side rather than dividing around it. Either count being
        # zero means a command produced nothing — a broken harness, not a
        # measurement — and every downstream ratio would be meaningless. An
        # earlier version returned 0.0 here, which rendered as "0.00x" and read
        # like a real result. Failing loudly is the honest option, and it makes
        # the aggregate and summary divisions safe by construction.
        if raw_tok == 0 or bos_tok == 0:
            print(
                f"scenario '{label}' produced no output "
                f"(raw={raw_tok} tok, kagoni={bos_tok} tok).\n"
                "The harness is broken or the fixture stack is not healthy; "
                "refusing to publish a ratio derived from it.",
                file=sys.stderr,
            )
            return 1

        ratio = raw_tok / bos_tok
        rows.append((label, question, raw_tok, raw_calls, bos_tok, bos_calls, ratio))
        totals[0] += raw_tok; totals[1] += raw_calls
        totals[2] += bos_tok; totals[3] += bos_calls

    version = sh([BINARY, "--version"]).strip()
    engine = next((l.split(":", 1)[1].strip() for l in sh([BINARY, "--check"]).splitlines()
                   if l.startswith("engine:")), "unknown")

    p = print
    p("# Benchmark: Kagoni vs the raw `docker` CLI")
    p("")
    p("Generated by `benches/run.py`. Every number here is reproducible — see")
    p("[Reproducing](#reproducing).")
    p("")
    p(f"- **Tokenizer:** {TOKENIZER}")
    p(f"- **Kagoni:** {version} · **engine:** {engine}")
    p(f"- **Fixture:** {len(names)} containers from `benches/fixtures/bench-stack.compose.yml`")
    p("")
    p("## Per scenario")
    p("")
    p("| Scenario | Raw tokens | Raw calls | Kagoni tokens | Kagoni calls | Ratio |")
    p("|---|---:|---:|---:|---:|---:|")
    for label, _q, rt, rc, bt, bc, ratio in rows:
        verdict = f"**{ratio:.1f}×**" if ratio >= 1.05 else (
            f"{ratio:.2f}× ✗" if ratio < 0.95 else f"{ratio:.2f}×")
        p(f"| {label} | {rt:,} | {rc} | {bt:,} | {bc} | {verdict} |")
    p(f"| **All scenarios** | **{totals[0]:,}** | **{totals[1]}** | "
      f"**{totals[2]:,}** | **{totals[3]}** | "
      f"**{totals[0]/totals[2]:.1f}×** |")
    p("")
    p("`✗` marks a scenario where Kagoni costs **more** than the CLI. Those are")
    p("here on purpose.")
    p("")
    p("## The fixed cost")
    p("")
    p(f"Kagoni's {ntools} tool schemas and handshake instructions are always-on context —")
    p("present in every session whether or not a tool is called:")
    p("")
    p(f"| Tool schemas ({ntools} tools) | {schemas:,} tok |")
    p("|---|---:|")
    p(f"| Handshake instructions | {instructions:,} tok |")
    p(f"| **Resident per session** | **{resident:,} tok** |")
    p("")
    fleet_raw, fleet_bos = rows[0][2], rows[0][4]
    allin = fleet_bos + resident
    p("Against a single fleet-health question:")
    p("")
    p(f"    raw CLI                {fleet_raw:>8,} tok")
    p(f"    kagoni (calls only)     {fleet_bos:>8,} tok    {fleet_raw/fleet_bos:.1f}x cheaper")
    p(f"    kagoni (+ resident)     {allin:>8,} tok    {fleet_raw/allin:.1f}x cheaper, all-in")
    p("")
    p(f"Break-even is roughly **one** non-trivial container question per session. Below")
    p("that, the schemas are pure overhead — an argument for loading Kagoni where you")
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
    p("  fewer chances for an agent to lose the thread. Wall clock is deliberately NOT")
    p("  reported: this harness starts a fresh Kagoni process for every call, while a real")
    p("  MCP session starts the server once, so any timing here would overstate Kagoni's")
    p("  cost. Call count is the honest proxy.")
    if TOKENIZER.startswith("ESTIMATE"):
        p("- **⚠️ These are byte-count estimates, not real tokens.** Dense JSON tokenizes")
        p("  worse than bytes/4 and log prose better, which flatters Kagoni. Install")
        p("  `tiktoken` and re-run before quoting these anywhere.")
    p("")
    p("## Reproducing")
    p("")
    p("```bash")
    p("cargo build --release")
    p("pip install tiktoken   # for real token counts")
    p(f"docker compose -p {PROJECT} -f benches/fixtures/bench-stack.compose.yml up -d")
    p("sleep 10               # let the fixtures emit their logs")
    p("python3 benches/run.py --sync-readme > docs/BENCHMARK.md")
    p(f"docker compose -p {PROJECT} -f benches/fixtures/bench-stack.compose.yml down")
    p("```")

    if "--sync-readme" in sys.argv:
        by_label = {r[0]: r for r in rows}
        order = ["Logs, repetitive", "Fleet health", "Logs, low repetition", "Container listing"]
        lines = [
            "",
            "| Scenario | Raw CLI | Kagoni | |",
            "|---|---:|---:|---|",
        ]
        for label in order:
            _l, _q, rt, _rc, bt, _bc, ratio = by_label[label]
            # One decimal below 10x: rounding 3.8 to "4" nudges the honest
            # worst-case number upward, which is the one figure that must not
            # be flattered.
            if ratio < 1.05:
                verdict = f"{ratio:.2f}× — *worse*"
            elif ratio < 10:
                verdict = f"**{ratio:.1f}×**"
            else:
                verdict = f"**{ratio:.0f}×**"
            lines.append(f"| {label} | {rt:,} | {bt:,} | {verdict} |")
        lines += [
            "",
            f"Kagoni's {ntools} tool schemas cost **~{resident:,} tokens resident per session** "
            "whether used or not,",
            f"so the all-in figure for one fleet-health question is {fleet_raw:,} → {allin:,}, "
            f"about **{fleet_raw / allin:.1f}×**.",
            "Break-even is roughly one non-trivial container question per session.",
            "",
        ]
        sync_readme("\n".join(lines))

    return 0


if __name__ == "__main__":
    sys.exit(main())
