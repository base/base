## Stack overflow risk in on_prepare fast-forward recursion

`crates/consensus/src/role_follower.rs:230` uses a synchronous recursive call to fast-forward through chained prepares:

```rust
return self.on_prepare(prepare.0, prepare.1, prepare.2, prepare.3);
```

This is acknowledged in the code (line 227: `// FIXME? In theory, we could have a stackoverflow if we need to catchup a lot of prepares`).

If a validator is far behind and receives many chained prepares at once (e.g. after being offline), each call adds a stack frame. Sync recursion in Rust uses the real call stack (~2MB default for async tasks), so even a few hundred frames could cause a stack overflow and crash the node.

### Suggested fix
Convert the recursive call to an iterative loop. The `on_prepare` body can be wrapped in a `loop { ... break; }` where the fast-forward path reassigns local variables and `continue`s instead of recursing.