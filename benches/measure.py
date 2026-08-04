"""Token measurement for the Kagoni benchmark.

Counts tokens with a real BPE tokenizer when one is installed, and says so
loudly when it falls back to an estimate. The distinction matters: dense JSON
tokenizes *worse* than bytes/4 and prose-like log text somewhat better, so an
estimate systematically flatters whichever side has more JSON — which is
Kagoni's. A benchmark that quietly favours its own subject is worthless.

Install a tokenizer for publishable numbers:

    pip install tiktoken
"""

import json
import subprocess
import sys

try:
    import tiktoken

    _ENC = tiktoken.get_encoding("cl100k_base")
    TOKENIZER = "tiktoken/cl100k_base"

    def count(text: str) -> int:
        return len(_ENC.encode(text, disallowed_special=()))

except ImportError:  # pragma: no cover - depends on the host
    _ENC = None
    TOKENIZER = "ESTIMATE (bytes/4) — install tiktoken for real counts"

    def count(text: str) -> int:
        return len(text.encode("utf-8")) // 4


def sh(cmd: list[str]) -> str:
    """Run a command and return stdout+stderr, as an agent would see it."""
    r = subprocess.run(cmd, capture_output=True, text=True)
    return r.stdout + r.stderr


def kagoni(binary: str, tool: str, args: dict) -> str:
    """Call one Kagoni tool over real stdio JSON-RPC and return its text result.

    Goes through the actual protocol rather than calling Rust directly, so the
    measurement includes everything the agent would really receive.
    """
    req = [
        json.dumps(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "bench", "version": "0"},
                },
            }
        ),
        json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json.dumps(
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {"name": tool, "arguments": args},
            }
        ),
    ]
    r = subprocess.run(
        [binary, "--log-level", "warn"],
        input="\n".join(req) + "\n",
        capture_output=True,
        text=True,
        timeout=120,
    )
    for line in r.stdout.splitlines():
        if not line.strip():
            continue
        msg = json.loads(line)
        if msg.get("id") == 2:
            if "error" in msg:
                raise RuntimeError(f"{tool}: {msg['error']}")
            return msg["result"]["content"][0]["text"]
    raise RuntimeError(f"{tool}: no response")


def resident_cost(binary: str) -> tuple[int, int, int]:
    """Tokens Kagoni occupies before any tool is called.

    Tool schemas and handshake instructions are always-on context. Excluding
    them would overstate the saving for anyone who loads Kagoni and doesn't use
    it, so the benchmark carries the cost explicitly.
    """
    req = [
        json.dumps(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "bench", "version": "0"},
                },
            }
        ),
        json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json.dumps({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    ]
    r = subprocess.run(
        [binary, "--log-level", "warn"],
        input="\n".join(req) + "\n",
        capture_output=True,
        text=True,
        timeout=60,
    )
    schemas = instructions = ntools = 0
    for line in r.stdout.splitlines():
        if not line.strip():
            continue
        msg = json.loads(line)
        if msg.get("id") == 1:
            instructions = count(msg["result"].get("instructions", ""))
        elif msg.get("id") == 2:
            tools = msg["result"]["tools"]
            ntools = len(tools)
            schemas = count(json.dumps(tools))
    return schemas, instructions, ntools
