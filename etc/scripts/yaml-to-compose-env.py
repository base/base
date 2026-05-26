#!/usr/bin/env python3
"""Render selected scalar YAML values as a Docker Compose env file."""

import argparse
import pathlib
import sys


UNSUPPORTED_VALUE_PREFIXES = ("{", "[", "|", ">")


def parse_scalar_yaml(path):
    """Parse the simple nested scalar subset used by local Base config YAML files."""
    values = {}
    stack = []

    for line_number, raw_line in enumerate(path.read_text().splitlines(), start=1):
        if not raw_line.strip() or raw_line.lstrip().startswith("#"):
            continue

        if "\t" in raw_line:
            raise SystemExit(f"unsupported tab indentation in {path}:{line_number}")

        indent = len(raw_line) - len(raw_line.lstrip(" "))
        stripped = raw_line.strip()
        if stripped in {"---", "..."}:
            continue
        if stripped.startswith("- "):
            raise SystemExit(f"unsupported YAML list item in {path}:{line_number}")
        if ":" not in stripped:
            raise SystemExit(f"unsupported YAML line in {path}:{line_number}: {stripped}")

        key, raw_value = stripped.split(":", 1)
        key = key.strip()
        value = raw_value.strip()
        if not key:
            raise SystemExit(f"empty YAML key in {path}:{line_number}")

        while stack and stack[-1][0] >= indent:
            stack.pop()

        path_parts = [part for _, part in stack] + [key]
        if value:
            if value.startswith(UNSUPPORTED_VALUE_PREFIXES):
                raise SystemExit(
                    f"unsupported YAML value form for {'.'.join(path_parts)} in {path}:{line_number}"
                )
            values[".".join(path_parts)] = value.strip("\"'")
        else:
            stack.append((indent, key))

    return values


def split_assignment(value, flag):
    if "=" not in value:
        raise SystemExit(f"{flag} must use source=DEST syntax: {value}")
    left, right = value.split("=", 1)
    if not left or not right:
        raise SystemExit(f"{flag} must use source=DEST syntax: {value}")
    return left, right


def render_env_line(key, value):
    # Keep env files compatible with Docker Compose's simple KEY=VALUE parser.
    # The local config schema is expected to use scalar URLs, paths, names, and numbers.
    if any(char in value for char in "\r\n#") or value != value.strip():
        raise SystemExit(f"unsupported env value for {key}: values must not contain whitespace or #")
    return f"{key}={value}\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=pathlib.Path)
    parser.add_argument("output", type=pathlib.Path)
    parser.add_argument(
        "--map",
        action="append",
        default=[],
        metavar="YAML_PATH=ENV_KEY",
        help="Map a scalar YAML path into an env var. First mapping wins when aliases share ENV_KEY.",
    )
    parser.add_argument(
        "--default",
        action="append",
        default=[],
        metavar="ENV_KEY=VALUE",
        help="Default an env var when no mapping set it.",
    )
    parser.add_argument(
        "--require",
        action="append",
        default=[],
        metavar="ENV_KEY",
        help="Require an env var to be present and non-empty after mapping/defaults.",
    )
    parser.add_argument(
        "--deny",
        action="append",
        default=[],
        metavar="YAML_PATH",
        help="Reject the YAML file when this scalar path is set.",
    )
    args = parser.parse_args()

    if not args.input.exists():
        raise SystemExit(f"YAML config not found: {args.input}")

    yaml_values = parse_scalar_yaml(args.input)
    denied = [key for key in args.deny if key in yaml_values]
    if denied:
        joined = ", ".join(denied)
        raise SystemExit(f"refusing denied YAML values in {args.input}: {joined}")

    env = {}

    for mapping in args.map:
        source, dest = split_assignment(mapping, "--map")
        value = yaml_values.get(source)
        if value and dest not in env:
            env[dest] = value

    for default in args.default:
        key, value = split_assignment(default, "--default")
        env.setdefault(key, value)

    missing = [key for key in args.require if not env.get(key)]
    if missing:
        joined = ", ".join(missing)
        raise SystemExit(f"missing required env values after rendering {args.input}: {joined}")

    args.output.write_text("".join(render_env_line(key, value) for key, value in env.items()))


if __name__ == "__main__":
    try:
        main()
    except BrokenPipeError:
        sys.exit(1)
