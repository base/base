# wait-timeout

Workspace patch for `wait-timeout 0.2.1` that adds WASM support.

On unix: uses `waitpid(WNOHANG)` in a polling loop (same semantics as the upstream crate).
On all other targets (including `wasm32-*`): returns `io::ErrorKind::Unsupported`.
