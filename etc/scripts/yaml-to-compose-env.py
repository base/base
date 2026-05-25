#!/usr/bin/env python3
"""Render selected scalar YAML values as a Docker Compose env file."""

import argparse
import pathlib
import sys


def parse_scalar_yaml(path):
    values = {}
    stack = []

    for raw_line in path.read_text().splitlines():
        if not raw_line.strip() or raw_line.lstrip().startswith("#"):
            continue

        indent = len(raw_line) - len(raw_line.lstrip(" "))
        stripped = raw_line.strip()
        if stripped.startswith("- "):
            continue
        if ":" not in stripped:
            continue

        key, raw_value = stripped.split(":", 1)
        key = key.strip()
        value = raw_value.strip()

        while stack and stack[-1][0] >= indent:
            stack.pop()

        path_parts = [part for _, part in stack] + [key]
        if value:
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
    denied = [key for key in args.deny if yaml_values.get(key)]
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

    args.output.write_text("".join(f"{key}={value}\n" for key, value in env.items()))


if __name__ == "__main__":
    try:
        main()
    except BrokenPipeError:
        sys.exit(1)
