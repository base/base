"""Pure, bounded contracts for untrusted feature-document suggestions.

The supported-command filter is deliberately conservative, not a proof that
arbitrary Markdown is safe. Every accepted page still requires human review.
"""

from __future__ import annotations

import hashlib
import json
import math
import re
import shlex
import tomllib
from pathlib import PurePosixPath
from urllib.parse import urlsplit

REPOSITORY = "base/base"
FEATURES = ".agents/skills/verify-base/references/features"
INDEX = f"{FEATURES}/README.md"
PREFIX = "automation/verify-base/"
MAX_TEXT_BYTES = 65_536
MAX_EVIDENCE_TEXT_BYTES = 204_800
MAX_PAGE_BYTES = 64_000
MAX_ARTIFACT_BYTES = 1_048_576
MAX_INFERENCE_FILES = 100
MAX_PR_FILES = 3000
MAX_OPEN = 3
DAILY_LIMIT = 3
SLUG = re.compile(r"[a-z0-9](?:[a-z0-9-]{0,62}[a-z0-9])?")
SHA = re.compile(r"[0-9a-f]{40}")
BASE_FIELDS = {"decision", "rationale", "confidence"}
WRITE_FIELDS = {"slug", "title", "page_markdown", "index_expected_behavior", "index_tools", "citations"}


class ContractError(ValueError):
    """An input is incomplete, unsupported, or unsafe to publish."""


def require(condition, message):
    """Reject a violated contract without echoing untrusted input or secrets."""
    if not condition:
        raise ContractError(message)


def text(value, limit, name, multiline=True):
    """Validate a nonempty UTF-8 string with a byte bound and no controls."""
    require(isinstance(value, str) and bool(value.strip()), f"invalid {name}")
    try:
        size = len(value.encode("utf-8"))
    except UnicodeError as exc:
        raise ContractError(f"invalid UTF-8 in {name}") from exc
    require(size <= limit, f"{name} exceeds byte budget")
    require(not any((ord(c) < 32 and (not multiline or c not in "\n\t")) or ord(c) == 127
                    or 0x202A <= ord(c) <= 0x202E or 0x2066 <= ord(c) <= 0x2069 for c in value),
            f"controls in {name}")
    return value


def sha(value):
    """Require a canonical full commit SHA."""
    require(isinstance(value, str) and SHA.fullmatch(value) is not None, "invalid full SHA")
    return value


def pr_number(value):
    """Require a canonical positive decimal PR number (not bool or float)."""
    require(type(value) in (str, int) and re.fullmatch(r"[1-9][0-9]{0,14}", str(value)) is not None,
            "source_pr must be a canonical positive decimal")
    return int(value)


def path(value):
    """Reject ambiguous, control-bearing, noncanonical repository paths."""
    text(value, 1024, "path", multiline=False)
    parts = value.split("/")
    require(not value.startswith("/") and all(p not in ("", ".", "..", ".git") for p in parts)
            and "\\" not in value and ":" not in value, "unsafe repository path")
    return value


def canonical(value):
    """Serialize artifacts deterministically, rejecting non-finite JSON."""
    return (json.dumps(value, ensure_ascii=True, sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n").encode()


def digest(data):
    """Return the SHA-256 digest of exact artifact bytes."""
    return hashlib.sha256(data).hexdigest()


def parse_json(data):
    """Parse one JSON value; duplicates and NaN/Infinity are not valid inputs."""
    def pairs(items):
        result = {}
        for key, value in items:
            require(key not in result, "duplicate JSON key")
            result[key] = value
        return result

    def constant(_value):
        raise ContractError("non-finite JSON number")

    try:
        return json.loads(data, object_pairs_hook=pairs, parse_constant=constant)
    except (ValueError, UnicodeError, RecursionError) as exc:
        raise ContractError("malformed JSON") from exc


def branch(source):
    """Compute the only permitted generated branch, never model-controlled."""
    return f"{PREFIX}{pr_number(source['number'])}-{sha(source['merge_sha'])}"


def abstain(reason):
    """Build the minimal strict non-writing result."""
    return {"decision": "abstain", "rationale": reason, "confidence": 0.0}


def existing_pages(texts):
    """Validate the complete feature-page inventory and its table links."""
    require(isinstance(texts, dict) and INDEX in texts, "missing feature index")
    index = text(texts[INDEX], MAX_TEXT_BYTES, "feature index")
    require("| Feature | Expected behavior | Tools |" in index, "unrecognized feature index")
    pages = {}
    for name, content in texts.items():
        path(name)
        text(content, MAX_TEXT_BYTES, "snapshot text")
        if name.startswith(FEATURES + "/") and name != INDEX:
            stem = PurePosixPath(name).stem
            require(PurePosixPath(name).parent.as_posix() == FEATURES and name.endswith(".md")
                    and SLUG.fullmatch(stem) is not None and stem != "readme", "invalid feature page path")
            require(re.search(r"(?m)^## Before starting$", content) is not None
                    and re.search(r"(?m)^## Verify$", content) is not None, "malformed existing page")
            pages[stem] = content
    rows = re.findall(r"(?m)^\| \[([^\]\n]+)\]\(([^)\n]+)\) \| ([^|\n]+) \| ([^|\n]+) \|$", index)
    targets = [row[1] for row in rows]
    require(len(targets) == len(set(targets)) and set(targets) == {s + ".md" for s in pages},
            "missing, duplicate, or malformed feature index rows")
    require(sum(line.startswith("| [") for line in index.splitlines()) == len(rows), "malformed index row")
    return pages


def preserve_warnings(old, new):
    """Preserve the safety preamble and known caution sentences verbatim."""
    section = re.search(r"(?ms)^## Before starting\n.*?(?=^## |\Z)", old)
    require(section is not None and section.group().strip() in new, "update drops Before starting safety section")
    for sentence in re.split(r"(?<=[.!?])\s+|\n\n", old):
        if re.search(r"\b(?:never|do not|not L1 finality|not validator synchronization)\b", sentence, re.I):
            require(sentence.strip() in new, "update drops an existing safety caveat")


def local_url(value):
    """Accept literal loopback HTTP RPC URLs, never credentials or remote hosts."""
    try:
        url = urlsplit(value)
        return (url.scheme in {"http", "https"} and url.hostname in {"127.0.0.1", "localhost"}
                and url.username is None and url.password is None and url.path in {"", "/"}
                and not url.query and not url.fragment and url.port is not None)
    except ValueError:
        return False


def validate_command(command, texts):
    """Admit a small evidenced command grammar without evaluating any command."""
    # Placeholder arguments are documentation, not shell input. Remove only the
    # known placeholders before disallowing redirection and shell metacharacters.
    clean = command.replace("<block-number>", "BLOCK").replace("<transaction-hash>", "TX")
    require(not re.search(r"[;|&<>`\n\r]|\$\(|\$\{|[\x00-\x1f]", clean), "unsupported shell syntax")
    try:
        words = shlex.split(clean)
    except ValueError as exc:
        raise ContractError("malformed command quoting") from exc
    require(bool(words), "empty command")
    for variable in re.findall(r"\$([A-Za-z_][A-Za-z_0-9]*)", clean):
        require(re.fullmatch(r"ANVIL_ACCOUNT_[0-9]_(?:ADDR|KEY)", variable) is not None
                and re.search(rf"(?m)^{variable}=", texts.get("etc/docker/devnet-env", "")) is not None,
                "unsupported or unevidenced environment reference")
    require("$" not in re.sub(r"\$[A-Za-z_][A-Za-z_0-9]*", "", clean), "unsupported dollar syntax")
    if words == ["source", "etc/docker/devnet-env"]:
        require("etc/docker/devnet-env" in texts, "missing public devnet environment evidence")
        return
    if words[:2] == ["just", "devnet"]:
        require(len(words) >= 3, "missing devnet recipe")
        recipe = words[2]
        permitted = {"up-single": {()}, "down": {()}, "ps": {()}, "logs": {("base-builder",)}}
        require(recipe in permitted and tuple(words[3:]) in permitted[recipe], "unsupported devnet recipe")
        require(re.search(r"(?m)^mod devnet\s", texts.get("Justfile", "")) is not None
                and re.search(rf"(?m)^{re.escape(recipe)}(?:[ :])", texts.get("etc/docker/Justfile", "")) is not None,
                "missing just recipe evidence")
        return
    if words[:2] == ["cargo", "test"]:
        require(len(words) in {4, 5} and words[2] == "-p" and re.fullmatch(r"base-[a-z0-9-]+", words[3]) is not None,
                "unsupported cargo test arguments")
        if len(words) == 5:
            require(re.fullmatch(r"[A-Za-z_][A-Za-z_0-9:]*", words[4]) is not None, "unsupported test filter")
        packages = set()
        for name, content in texts.items():
            if name.endswith("Cargo.toml"):
                try:
                    manifest = tomllib.loads(content)
                    packages.add(manifest.get("package", {}).get("name", ""))
                except (ValueError, TypeError) as exc:
                    raise ContractError("malformed manifest evidence") from exc
        require(words[3] in packages, "missing cargo package evidence")
        return
    if words[0] == "cast":
        require(len(words) >= 4 and words[1] in {"send", "block", "receipt", "balance", "block-number"},
                "unsupported cast command")
        positional, options = [], {}
        i = 2
        while i < len(words):
            word = words[i]
            if word.startswith("--"):
                require(word not in options, "duplicate cast option")
                if word == "--json":
                    options[word] = True
                else:
                    require(word in {"--rpc-url", "--chain-id", "--private-key", "--value"} and i + 1 < len(words),
                            "unsupported cast option")
                    i += 1
                    options[word] = words[i]
            else:
                positional.append(word)
            i += 1
        require(local_url(options.get("--rpc-url", "")), "cast must target literal local RPC")
        require(options["--rpc-url"] in "\n".join(texts.values())
                or re.search(rf"(?m)^L2_BUILDER_HTTP_PORT={urlsplit(options['--rpc-url']).port}$",
                             texts.get("etc/docker/devnet-env", "")) is not None, "local RPC URL is not evidenced")
        if words[1] == "send":
            require(len(positional) == 1 and re.fullmatch(r"\$ANVIL_ACCOUNT_[0-9]_ADDR", positional[0]) is not None
                    and re.fullmatch(r"\$ANVIL_ACCOUNT_[0-9]_KEY", options.get("--private-key", "")) is not None
                    and re.fullmatch(r"0\.00[0-9]+ether", options.get("--value", "")) is not None
                    and options.get("--chain-id") == "84538453", "send must use disposable public devnet accounts")
        else:
            require(not ({"--private-key", "--value", "--chain-id"} & options.keys()), "invalid read-only cast options")
            require(len(positional) == (0 if words[1] == "block-number" else 1), "invalid cast arguments")
            require(all(re.fullmatch(r"(?:[0-9]+|0x[0-9a-fA-F]+|latest|BLOCK|TX|\$ANVIL_ACCOUNT_[0-9]_ADDR)", p)
                        for p in positional), "unsupported cast argument")
        return
    raise ContractError("unsupported command; manual documentation required")


def validate_markdown(page, texts):
    """Reject known hazardous content and unsupported proposed commands."""
    text(page, MAX_PAGE_BYTES, "page markdown")
    require(page.startswith("# ") and len(re.findall(r"(?m)^## Before starting$", page)) == 1
            and len(re.findall(r"(?m)^## Verify$", page)) == 1, "missing or duplicate page headings")
    verify = re.search(r"(?ms)^## Verify\n(.*?)(?=^## |\Z)", page)
    require(verify is not None and re.search(r"(?m)^```(?:bash|sh)$", verify[1]) is not None,
            "Verify section must contain proposed commands")
    require(not re.search(r"-----BEGIN|\b(?:gh[pousr]_[A-Za-z0-9]+|github_pat_[A-Za-z0-9_]+|sk-ant-[A-Za-z0-9_-]+)\b|\b(?:0x)?[0-9a-fA-F]{64}\b", page),
            "literal credential material is not permitted")
    require(not re.search(r"<[/!A-Za-z][^>]*>|!\[|\]\((?!https?://)[^)]*://", page.replace("<block-number>", "").replace("<transaction-hash>", "")),
            "HTML, images, or non-HTTP external links are unsupported")
    require(not re.search(r"\b(?:curl|wget|sudo|eval|exec)\b|\brm\s+-|\b(?:disable|ignore|remove|skip)\b.{0,50}\b(?:warnings?|safety|safeguards?)\b", page, re.I),
            "unsafe host command or warning-disabling instruction")
    for url in re.findall(r"https?://[^\s`\)\]\"']+", page):
        require(local_url(url.rstrip(".,")), "external URLs are not supported in proposed pages")
    blocks = re.findall(r"(?ms)^```([^\n]*)\n(.*?)^```[ \t]*$", page)
    require(bool(blocks) and page.count("```") == 2 * len(blocks) and "~~~" not in page,
            "page must use complete fenced verification commands")
    commands = []
    for language, body in blocks:
        require(language in {"bash", "sh"}, "unsupported code block language")
        body = re.sub(r"\\\n[ \t]*", " ", body)
        for line in body.splitlines():
            if line.strip():
                validate_command(line.strip(), texts)
                commands.append(line.strip())
    require(any(not c.startswith("source ") for c in commands), "no verification command supplied")
    outside = re.sub(r"(?ms)^```[^\n]*\n.*?^```[ \t]*$", "", page)
    require(not re.search(r"(?m)^(?:    |\t)\S", outside), "indented code blocks are unsupported")
    for span in re.findall(r"`([^`\n]+)`", outside):
        if " " in span:
            # Bare tool names like `cast send` describe the tool, not a runnable
            # command. Longer spans must satisfy the same grammar as fences.
            if span not in {"cast send", "cast block", "cargo test"}:
                validate_command(span, texts)
    verification_prose = re.sub(r"(?ms)^```[^\n]*\n.*?^```[ \t]*$", "", verify[1])
    require(re.search(r"\b(?:require|confirm|assert|expect)\b", verification_prose, re.I) is not None
            and re.search(r"\b(?:status|receipt|output|hash|passes?|result|block)\b", verification_prose, re.I) is not None,
            "missing observable success evidence")
    if any(c.startswith(("just devnet", "cast send")) for c in commands):
        require(all(word in page.lower() for word in ("disposable", "isolated", "public test", "never stop unrelated")),
                "missing local devnet safety guidance")


def validate_result(value, texts):
    """Validate the complete model schema, citations, and supported Markdown."""
    require(isinstance(value, dict), "model result must be an object")
    decision = value.get("decision")
    require(isinstance(decision, str) and decision in {"add", "update", "no_op", "abstain"}, "invalid decision")
    confidence = value.get("confidence")
    require(type(confidence) in (int, float) and 0 <= confidence <= 1 and math.isfinite(confidence),
            "invalid confidence")
    text(value.get("rationale"), 4000, "rationale")
    if decision in {"no_op", "abstain"}:
        require(set(value) == BASE_FIELDS, "non-writing result carries page fields")
        return value
    require(set(value) == BASE_FIELDS | WRITE_FIELDS, "unknown or missing write fields")
    slug = value["slug"]
    require(isinstance(slug, str) and SLUG.fullmatch(slug) is not None and slug != "readme", "invalid or reserved slug")
    pages = existing_pages(texts)
    require((slug in pages) == (decision == "update"), "add collision or missing update page")
    for key, bound in (("title", 200), ("index_expected_behavior", 400), ("index_tools", 400)):
        cell = text(value[key], bound, key, multiline=False)
        require(not any(c in cell for c in "[]|\\<>"), "unsafe index cell")
    validate_markdown(value["page_markdown"], texts)
    if decision == "update":
        preserve_warnings(pages[slug], value["page_markdown"])
    citations = value["citations"]
    require(isinstance(citations, list) and 0 < len(citations) <= 20, "invalid citations")
    for citation in citations:
        require(isinstance(citation, dict) and set(citation) == {"path", "lines"}, "malformed citation")
        name = citation["path"]
        require(isinstance(name, str) and name in texts, "citation outside snapshot evidence")
        lines = citation["lines"]
        require(isinstance(lines, str), "invalid citation line range")
        match = re.fullmatch(r"([1-9][0-9]{0,6})(?:-([1-9][0-9]{0,6}))?", lines)
        require(match is not None, "invalid citation line range")
        start, end = int(match[1]), int(match[2] or match[1])
        require(start <= end <= len(texts[name].splitlines()), "citation range outside supplied snapshot")
    return value


def render_content(result, texts):
    """Construct both normalized outputs without performing any filesystem I/O."""
    validate_result(result, texts)
    require(result["decision"] in {"add", "update"}, "non-writing result cannot render")
    slug = result["slug"]
    lines = texts[INDEX].splitlines()
    target = f"]({slug}.md)"
    rows = [i for i, line in enumerate(lines) if line.startswith("| [")]
    matches = [i for i in rows if target in lines[i]]
    row = f"| [{result['title']}]({slug}.md) | {result['index_expected_behavior']} | {result['index_tools']} |"
    if result["decision"] == "update":
        require(len(matches) == 1, "update requires exactly one index row")
        lines[matches[0]] = row
    else:
        require(not matches, "index collision")
        separator = lines.index("| --- | --- | --- |") if "| --- | --- | --- |" in lines else -1
        require(separator >= 0, "missing index table separator")
        lines.insert(rows[-1] + 1 if rows else separator + 1, row)
    return {f"{FEATURES}/{slug}.md": result["page_markdown"].rstrip() + "\n", INDEX: "\n".join(lines) + "\n"}
