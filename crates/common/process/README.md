# base-common-process

Builders for launching local [Anvil], [Geth], and [Reth] child processes.

The node binaries are not bundled: install them on `PATH`, or select an executable explicitly with
the builder's `at` or `path` method. Prefer `try_spawn` when startup failures should be returned;
`spawn` unwraps the result and therefore panics on any startup error. `try_spawn` waits until the
node's startup output reports its RPC service ready before returning an instance handle.
Readiness output is consumed synchronously, so a live child that emits no newline can delay a
configured startup timeout.

```no_run
use base_common_process::Anvil;

# fn main() -> Result<(), base_common_process::NodeError> {
let anvil = Anvil::new().try_spawn()?;
println!("Anvil is listening at {}", anvil.endpoint());

// Dropping the handle shuts down the child process.
drop(anvil);
# Ok(())
# }
```

Keep the returned instance alive for as long as the node is needed. Anvil and Geth default their
HTTP port to zero; use the returned instance's `port` or `endpoint` method to read the OS-assigned
port.

[Anvil]: https://docs.rs/base-common-process/latest/base_common_process/struct.Anvil.html
[Geth]: https://docs.rs/base-common-process/latest/base_common_process/struct.Geth.html
[Reth]: https://docs.rs/base-common-process/latest/base_common_process/struct.Reth.html
