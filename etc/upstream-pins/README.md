# Reth workspace pin

`reth.toml` is the source of truth for every git-based `reth-*` crate in
`[workspace.dependencies]`. Do not edit those Cargo.toml lines by hand.

## Commands

```bash
just pin-reth
just check-reth-pin
just pin-reth-test
just reth-prepare-release --upstream v2.5.1 --pr 26708 --pr 26766
```

`--pr 26708` is a PR on `--upstream-repo` (default `paradigmxyz/reth`). A
full GitHub pull URL is fetched from that repository. Allowed sources are
`--upstream-repo` and `--fork` (default `base/reth`). Use a `base/reth` URL
for Base-specific patches instead of opening them upstream.

`--line` is inferred from `releases/<line>` when you are on that branch;
otherwise pass it. `--skip-push` squashes and tags locally without
publishing the fork.

## Workflow

1. Decide the official Reth tag and the PRs to carry.
2. Run `just reth-prepare-release`. It squashes each PR onto that tag in
   the fork, publishes `vX.Y.Z-base.N`, writes this manifest, and rewrites
   Cargo.toml in the current tree.
3. Commit and open the `base/base` PR yourself.
4. After merge, `just check-reth-pin` confirms Cargo still matches this file.
5. To return to an official Reth tag, point `repository`, `reference`, and
   `rev` at `paradigmxyz/reth`, clear `[[patches]]` that landed in that
   release, and run `just pin-reth`.

A squash conflict is the only hard stop. Fix the PR against that official
tag and rerun.

`--pr` replaces the patch set. Pass every PR you still want to carry.
Protect `v*-base.*` tags on `base/reth` so they cannot be moved after
publish.
