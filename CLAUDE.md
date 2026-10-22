# Agent Instructions

## Repository Layout

- `bin/`: thin binary crates (node, builder, consensus, basectl, proposer, challenger, prover services, and others).
- `crates/`: library crates grouped as `batcher`, `builder`, `common`, `consensus`, `execution`, `infra`, `proof`, `utilities`.
- `acceptance/`, `actions/`, `etc/systems/`: real-devnet acceptance scenarios, action tests, and system tests.
- `etc/`: Docker and Compose files, `just` modules, CI and local scripts, upstream pins, and tools.
- `docs/`: cross-cutting guides and specs. Crate and binary docs are READMEs next to their source.
- Run `just` to list recipes. [docs/guides/TESTING.md](docs/guides/TESTING.md) explains what `just ci`, `just pr`, and each check cover.

## Rust Development

- Before changing or reviewing Rust code or Cargo manifests, read and follow [Rust Development for Agents](docs/guides/RUST.md). It owns the crate/API, manifest, style, tracing, testing, and Rust review rules.

## Cancellation and Process Lifetimes

- Make cancellation explicit for asynchronous operations, tasks, RPCs, queued work, and child processes, in any language. State who owns completion, whether cancellation stops work or only stops waiting, what partial effects remain, and when retrying is safe. Intentional background work still needs an owner for results, errors, and cleanup.
- Use the [reviewing-cancellation skill](.agents/skills/reviewing-cancellation/SKILL.md) when implementing or reviewing cancellation, timeouts, retries, task/process lifetimes, or shutdown. Follow its workflow even if skill loading is unavailable.

## Documentation

- Docs are READMEs next to the code they describe, plus cross-cutting guides in `docs/guides/`. Open every doc with a short paragraph that says what it covers in plain terms, naming the generic concept alongside any codename (for example, "Azul network upgrade"), so a text search finds it.
