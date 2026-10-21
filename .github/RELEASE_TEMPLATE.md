<!--
Release notes template. See docs/guides/RELEASE.md#release-notes.
- Keep the two Operator action rows in this order: Base Mainnet, then Base Sepolia. Never drop a row.
- Action is one of: Required, Recommended, Optional.
- Deadline is YYYY-MM-DD HH:MM UTC, or None.
- Delete any section that says "None." except Operator action and Breaking changes.
HTML comments are not rendered on the release page; you can leave them in.
-->

## Operator action

| Network | Action | Deadline | Why |
|---|---|---|---|
| Base Mainnet | <Required / Recommended / Optional> | <YYYY-MM-DD HH:MM UTC / None> | <one line, eg, Denim hardfork activation> |
| Base Sepolia | <Required / Recommended / Optional> | <YYYY-MM-DD HH:MM UTC / None> | <one line> |

Applies to: <all operators / nodes serving proofs / operators upgrading from vX.Y.Z or earlier>
Replaces: <vX.Y.Z, which operators should not run / None>
More info: [<Upgrade name> upgrade overview](<https://docs.base.org/upgrades/...>)

## Breaking changes

<!-- One bullet per removed flag, renamed flag or changed default, with the step operators take. Write "None." if there are none. -->
- **Removed flag:** `--flag-name` (`base-reth-node`). Remove it from your entrypoint or the node fails to start.
- **Changed default:** `--flag-name` (`base-consensus`) from <old> to <new>. Set it explicitly to keep the old behavior.

## What's changed

### base-reth-node

- <change>

### base-consensus

- <change>

**Full changelog:** https://github.com/base/base/compare/vPREVIOUS...vTHIS
