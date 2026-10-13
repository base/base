#!/usr/bin/env python3
"""The only tools a review agent has: read the pull request's files, and submit its answer.

`review.py` starts `amp` with this as its sole MCP server (and `amp.tools.enable` set to match), so an agent
cannot run a command, edit a file, or reach the network. It speaks the stdio MCP protocol, one JSON message
per line, and needs only the standard library.

Environment (set by review.py):
  REVIEW_ROOT    the directory the agent may read; nothing outside it is readable
  REVIEW_SCHEMA  path of the JSON schema the answer must satisfy
  REVIEW_OUT     file that receives the answer when it is accepted

`submit` is how an agent answers: it takes the answer as its arguments, checks them against the schema, and
tells the agent what is wrong if they do not fit, so the agent can fix it and call again. The first accepted
answer wins; the script reads it from REVIEW_OUT.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any

MAX_READ_CHARS = 200_000
MAX_GREP_CHARS = 100_000
MAX_GLOB_RESULTS = 500
GREP_TIMEOUT_SECONDS = 30


SUPPORTED_KEYWORDS = frozenset({"type", "enum", "required", "properties", "additionalProperties", "items",
                                "minimum", "maxItems"})

_KINDS: dict[str, tuple[type, ...]] = {
    "object": (dict,), "array": (list,), "string": (str,), "boolean": (bool,), "null": (type(None),),
    "integer": (int,), "number": (int, float),
}


def is_type(value: Any, kind: str) -> bool:
    """Whether `value` is of the JSON type `kind`. A bool is not a number, as in JSON."""
    if kind in ("integer", "number") and isinstance(value, bool):
        return False
    return isinstance(value, _KINDS[kind])


def validate(value: Any, schema: dict[str, Any], path: str = "answer") -> list[str]:
    """What is wrong with `value` under `schema`, as short messages. Empty if it fits.

    Understands the keywords in SUPPORTED_KEYWORDS, which are the ones the schemas in schemas/ use (a test
    fails if a schema starts using another). `type` may be one name or a list of names.
    """
    errors: list[str] = []
    expected = schema.get("type")
    if expected is not None:
        kinds = [expected] if isinstance(expected, str) else list(expected)
        if not any(is_type(value, kind) for kind in kinds):
            named = [f"{'an' if kind[0] in 'aeiou' else 'a'} {kind}" for kind in kinds]
            return [f"{path} must be {' or '.join(named)}"]
    if "enum" in schema and value not in schema["enum"]:
        errors.append(f"{path} must be one of {schema['enum']}")
    if "minimum" in schema and is_type(value, "number") and value < schema["minimum"]:
        errors.append(f"{path} must be at least {schema['minimum']}")
    if isinstance(value, dict):
        for key in schema.get("required", []):
            if key not in value:
                errors.append(f"{path}.{key} is required")
        properties = schema.get("properties", {})
        for key, item in value.items():
            if key in properties:
                errors += validate(item, properties[key], f"{path}.{key}")
            elif schema.get("additionalProperties") is False:
                errors.append(f"{path}.{key} is not allowed")
    if isinstance(value, list):
        if "maxItems" in schema and len(value) > schema["maxItems"]:
            errors.append(f"{path} has {len(value)} items; at most {schema['maxItems']} are allowed")
        if "items" in schema:
            for index, item in enumerate(value):
                errors += validate(item, schema["items"], f"{path}[{index}]")
    return errors


class Tools:
    """The four tools, confined to one directory."""

    def __init__(self, root: Path, schema: dict[str, Any], out: Path) -> None:
        self.root = root.resolve()
        self.schema = schema
        self.out = out
        self.submitted = False

    def resolve(self, relative: str) -> Path:
        """`relative` inside the root, or ValueError. Symlinks are followed first, so one cannot lead out."""
        target = (self.root / relative).resolve()
        if target != self.root and self.root not in target.parents:
            raise ValueError(f"{relative} is outside the repository")
        if ".git" in target.relative_to(self.root).parts:
            raise ValueError(f"{relative} is git's own data, not part of the change")
        return target

    def read_file(self, path: str) -> str:
        target = self.resolve(path)
        if not target.is_file():
            raise ValueError(f"{path} is not a file")
        text = target.read_text(errors="replace")
        if len(text) > MAX_READ_CHARS:
            return text[:MAX_READ_CHARS] + f"\n[cut off: the file is {len(text)} characters long]"
        return text

    def grep(self, pattern: str, path: str = ".") -> str:
        target = self.resolve(path)
        done = subprocess.run(
            ["git", "grep", "--untracked", "-nIE", "-e", pattern, "--", str(target.relative_to(self.root))],
            cwd=self.root, capture_output=True, text=True, timeout=GREP_TIMEOUT_SECONDS, check=False)
        if done.returncode not in (0, 1):
            raise ValueError(done.stderr.strip() or "grep failed")
        return done.stdout[:MAX_GREP_CHARS] or "(no matches)"

    def glob(self, pattern: str) -> str:
        found = sorted(p for p in self.root.glob(pattern) if p.is_file() and ".git" not in p.relative_to(self.root).parts)
        names = [str(p.relative_to(self.root)) for p in found[:MAX_GLOB_RESULTS]]
        more = f"\n[{len(found) - MAX_GLOB_RESULTS} more not shown]" if len(found) > MAX_GLOB_RESULTS else ""
        return ("\n".join(names) + more) if names else "(no files match)"

    def submit(self, **answer: Any) -> str:
        if self.submitted:
            raise ValueError("You have already submitted your answer. Do not call submit again.")
        errors = validate(answer, self.schema)
        if errors:
            raise ValueError("Your answer does not fit the required shape; fix these and call submit again:\n- "
                             + "\n- ".join(errors[:20]))
        self.out.write_text(json.dumps(answer))
        self.submitted = True
        return "Recorded. Your work is done; reply with one short sentence."

    def definitions(self) -> list[dict[str, Any]]:
        def schema(props: dict[str, Any], required: list[str]) -> dict[str, Any]:
            return {"type": "object", "properties": props, "required": required, "additionalProperties": False}

        text = {"type": "string"}
        return [
            {"name": "read_file", "description": "Read one file of the repository. Paths are relative to its root.",
             "inputSchema": schema({"path": text}, ["path"])},
            {"name": "grep", "description": "Search the repository's files for an extended regular expression; "
                                             "returns file:line:text. `path` limits the search to a directory.",
             "inputSchema": schema({"pattern": text, "path": text}, ["pattern"])},
            {"name": "glob", "description": "List the repository's files matching a glob such as crates/**/*.rs.",
             "inputSchema": schema({"pattern": text}, ["pattern"])},
            {"name": "submit", "description": "Submit your final answer. Call it exactly once, when you are done. "
                                               "The review reads nothing else you write.",
             "inputSchema": self.schema},
        ]

    def call(self, name: str, arguments: dict[str, Any]) -> tuple[str, bool]:
        """The text a tool returns and whether it is an error."""
        try:
            handler = {"read_file": self.read_file, "grep": self.grep, "glob": self.glob,
                       "submit": self.submit}.get(name)
            if handler is None:
                raise ValueError(f"unknown tool {name}")
            return handler(**arguments), False
        except (ValueError, TypeError, OSError, subprocess.TimeoutExpired) as exc:
            return f"error: {exc}", True


def serve(tools: Tools) -> None:
    def send(message: dict[str, Any]) -> None:
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()

    for line in sys.stdin:
        if not line.strip():
            continue
        request = json.loads(line)
        method, request_id = request.get("method"), request.get("id")
        if method == "initialize":
            send({"jsonrpc": "2.0", "id": request_id, "result": {
                "protocolVersion": request["params"]["protocolVersion"], "capabilities": {"tools": {}},
                "serverInfo": {"name": "review", "version": "1"}}})
        elif method == "tools/list":
            send({"jsonrpc": "2.0", "id": request_id, "result": {"tools": tools.definitions()}})
        elif method == "tools/call":
            text, is_error = tools.call(request["params"]["name"], request["params"].get("arguments") or {})
            send({"jsonrpc": "2.0", "id": request_id,
                  "result": {"content": [{"type": "text", "text": text}], "isError": is_error}})
        elif request_id is not None:
            send({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": "unknown method"}})


if __name__ == "__main__":
    serve(Tools(Path(os.environ["REVIEW_ROOT"]), json.loads(Path(os.environ["REVIEW_SCHEMA"]).read_text()),
                Path(os.environ["REVIEW_OUT"])))
