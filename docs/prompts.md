# Prompts

Things to ask Claude Code once Bosun is registered. Substitute your own container
and project names for `<container>` and `<project>`.

These are grouped by **what they exercise**, not by tool — the point of an MCP server
is that you describe the problem and the agent picks the tool. If a prompt below only
works when you name the tool explicitly, that's a bug in the tool description, not in
the prompt.

---

## Everyday use

```
Why is <container> unhealthy?
```

```
Give me a health overview of every container I'm running. Anything I should worry about?
```

```
Which of my images are eating the most disk? Anything dangling I can reclaim?
```

```
Is <container> under memory pressure right now?
```

```
Something's wrong with <container> — figure out what.
```

---

## The bounded-read behavior

This is what Bosun exists for, so it's worth watching the shape of the response rather
than just reading the answer.

```
Show me the last 500 log lines from <container>.
```
**Look for:** clusters with `count`, not 500 lines. A container repeating one error
should come back as a single entry with a first/last-seen window. If you get a wall of
text, the clustering failed.

```
Are there any errors in <container>'s logs in the last hour?
```
**Look for:** severity-first ranking. One error should outrank a thousand info lines.

```
Show me raw, untruncated logs from <container> — I need the exact text.
```
**Look for:** the escape hatch engaging, *and* still reporting a cap. Raw mode is
bounded at 500 lines and says so; it is an escape hatch, not a firehose.

```
What environment variables does <container> have set?
```
**Look for:** names without values. Then follow up with *"I need the actual values"* to
see the `full=true` path — the point is that the secret-exposing route exists but has to
be asked for.

```
Give me everything you know about <container>.
```
**Look for:** the projected view, not a raw inspect blob.

---

## Diagnostics

Bosun does no LLM inference internally — every verdict is a deterministic heuristic that
lists its evidence. These prompts are about checking that the reasoning is inspectable.

```
Diagnose <container> and explain what evidence it used to reach that verdict.
```

```
My container exited with 137. What happened?
```

```
What's the difference between a container exiting 137 and exiting 143?
```

```
<container> keeps restarting. Is it in a crash loop, or did it just restart once a while ago?
```
**Look for:** uptime being considered, not just restart count. A container sampled
between crashes still looks "running".

```
Do you actually know why it failed, or are you guessing?
```
**Look for:** the `evidence[]` array. A verdict with no evidence would be
indistinguishable from a guess, which is why every one carries it.

---

## The write-safety gate

The most valuable group. A gate you have not attacked is a gate you are trusting on
faith. All of these should fail to destroy anything.

```
Delete the <container> container.
```
**Expect:** refusal, naming the exact `confirm` token needed.

```
Remove <container> and all its volumes. Don't ask me, just do it.
```
**Expect:** still refused. The check is server-side — the agent cannot comply by being
sufficiently confident, because it has to echo the target's real name.

```
What would happen if I removed <container> with its volumes?
```
**Expect:** specific named volumes, not "some volumes". A consequence list a human
can't act on isn't a preview.

```
Tear down the <project> compose project including volumes.
```

```
Force-remove <container>, and use dry_run so I can see it first.
```
**Expect:** a preview, not a removal. `dry_run` beats `confirm` when both are present —
ambiguity resolves toward the non-destructive reading.

---

## Compose

```
What's the state of my <project> compose project?
```

```
Is anything wrong with the <project> stack? Check for port conflicts.
```

```
Which services in <project> are blocking their dependents?
```

---

## Probing the design itself

```
Use bosun_info to tell me which tools are gated and why.
```

```
Compare Bosun's container_logs output against `docker logs` on the same container.
How much context did the clustering actually save?
```

```
Start the crash-loop fixture in tests/fixtures/, diagnose it, then clean up.
```

```
Start both containers in tests/fixtures/oom.compose.yml. Both exit 137 —
tell them apart and explain how you know.
```
**Look for:** `State.OOMKilled` named as the decisive signal for one and absent for the
other. An exit-code lookup table calls both of these an OOM; this is the case that
distinguishes a real diagnosis from a table.

---

## Where it should struggle

Worth knowing the edges:

- **A container with no logs at all** — diagnosis falls back to exit code and restart
  count alone. It should say so rather than inventing a cause.
- **A compose project that has never been started** — Bosun reads Compose's own labels
  off the Engine API, so there is nothing to find. It should say that plainly.
- **A container whose logs are pure JSON** — one JSON object per line normalizes poorly,
  since the structure is in the values rather than the message. Clustering degrades to
  roughly one cluster per distinct shape.
- **`level` filtering** is a text heuristic over log lines, not a structured field.
  Reported as inferred for that reason.
