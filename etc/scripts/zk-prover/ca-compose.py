#!/usr/bin/env python3
"""Render a Docker Compose override that mounts an extra CA certificate."""

import argparse
import json
import pathlib
import shlex


def render_compose(
    ca_cert_file,
    service_name,
    service_command,
    system_ca_file,
    extra_ca_file,
    combined_ca_file,
):
    """Build the Compose override document for the CA certificate mount."""
    entrypoint_script = (
        f"cat {shlex.quote(system_ca_file)} {shlex.quote(extra_ca_file)} > "
        f"{shlex.quote(combined_ca_file)} && exec {shlex.quote(service_command)} \"$@\""
    )

    return {
        "services": {
            service_name: {
                "entrypoint": [
                    "/bin/sh",
                    "-c",
                    entrypoint_script,
                    service_name,
                ],
                "environment": [f"SSL_CERT_FILE={combined_ca_file}"],
                "volumes": [
                    {
                        "type": "bind",
                        "source": str(ca_cert_file.resolve()),
                        "target": extra_ca_file,
                        "read_only": True,
                    }
                ],
            }
        }
    }


def main():
    """Parse arguments and write the Compose override as JSON."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ca_cert_file", type=pathlib.Path)
    parser.add_argument("output", type=pathlib.Path)
    parser.add_argument("--service-name", required=True)
    parser.add_argument("--service-command", required=True)
    parser.add_argument("--system-ca-file", required=True)
    parser.add_argument("--extra-ca-file", required=True)
    parser.add_argument("--combined-ca-file", required=True)
    args = parser.parse_args()

    if not args.ca_cert_file.is_file():
        raise SystemExit(f"CA certificate file not found: {args.ca_cert_file}")

    compose = render_compose(
        args.ca_cert_file,
        service_name=args.service_name,
        service_command=args.service_command,
        system_ca_file=args.system_ca_file,
        extra_ca_file=args.extra_ca_file,
        combined_ca_file=args.combined_ca_file,
    )
    args.output.write_text(json.dumps(compose, indent=2) + "\n")


if __name__ == "__main__":
    main()
