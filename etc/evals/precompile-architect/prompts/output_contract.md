# Output contract

Write an implementation plan for the ticket below. Do not edit any files. Read whatever you need in the repository.

End your reply with one fenced `json` block in exactly this shape. The grader reads only the last such block.

```json
{
  "verdict": "proceed",
  "approach": "new_logic_version",
  "activation_fork": "Denim",
  "versions": { "create": ["V3"], "modify": [], "frozen": ["V1", "V2"] },
  "files": {
    "create": ["crates/common/precompiles/src/b20_asset/logic/v3.rs"],
    "modify": ["crates/common/precompiles/src/b20_asset/versions.rs"],
    "must_not_modify": ["crates/common/precompiles/src/b20_asset/logic/v2.rs"]
  },
  "symmetric_modules": ["b20_asset", "b20_stablecoin"],
  "surfaces": { "gas": false, "revert_bytes": false, "storage": false, "abi": false, "events": false },
  "tests": { "existing_goldens_unchanged": true, "new": ["describe each new test"] }
}
```

## Field definitions

- **verdict**: `proceed` if the change can be made safely as requested. `reject` if doing it as requested would change how blocks that have already been produced execute. When you reject, propose a safe alternative in your prose.
- **approach**: how the change reaches the code.
  - `edit_in_place`: modify existing code directly with no fork gate. Use this for pure refactors, for edits to code that no network has live or scheduled, and for fixes that bring live code back to the behavior that network actually ran.
  - `new_logic_version`: add a new frozen `logic/vN.rs` that activates at a fork.
  - `fork_gate`: branch on the active fork without adding a logic version, for example in dispatch or in the storage layer.
  - `none`: use only with `reject`.
- **activation_fork**: the earliest fork whose blocks execute differently after this change, compared with the code at this commit. Count forks that are defined in code but not yet scheduled on any network. For a new version, this is the version's introduction fork. Use `null` only when execution at every fork is unchanged, for example in a pure refactor.
- **versions**: logic versions of the modules you change, written `V1`, `V2` and so on. `create` lists new versions. `modify` lists existing versions whose logic or behavior changes. `frozen` lists existing versions that must stay byte-for-byte unchanged. Use empty lists if the module has no versions.
- **files**: repository-relative paths. List every production and test file you would create or modify. `must_not_modify` lists files whose contents must not change at all.
- **symmetric_modules**: every precompile module whose logic or dispatch you change, from `b20_asset`, `b20_stablecoin`, `b20_factory`, `policy`, `activation`, `nonce`, `tx_context` and `b20_security`. Leave empty if you only change shared or storage-layer code.
- **surfaces**: compare execution at `activation_fork` before and after your change. Set a flag to `true` when:
  - `revert_bytes`: some call starts reverting, stops reverting, or reverts with different bytes.
  - `gas`: some call with the same outcome before and after is charged different gas. A revert path counts when the work done before the revert changes.
  - `storage`: the storage layout changes, or a call that succeeds both before and after writes different slots.
  - `abi`: dialable selectors, or error or event definitions, change.
  - `events`: a call that succeeds both before and after emits different logs.

  A call that starts reverting sets only `revert_bytes`, not `storage` or `events`. Use all `false` for a pure refactor or a reject.
- **tests.existing_goldens_unchanged**: `true` if every existing golden pin, meaning state roots, gas footprints and revert bytes, stays as it is.
- **tests.new**: one short description per new or changed test.
