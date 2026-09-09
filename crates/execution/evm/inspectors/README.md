# `base-execution-evm-inspectors`

EVM tracing, access lists, transaction transfer inspection, opcode inspection, and debugging.
This is the locally maintained REVM inspector implementation, originally developed in Reth.

The crate consumes the execution runtime and produces shared RPC trace schemas. JavaScript
tracers remain optional behind `js-tracer`; their scripts, test fixtures, and benchmark fixtures
are retained alongside the implementation.

```sh
cargo test -p base-execution-evm-inspectors --features js-tracer
```

#### License

<sup>
Licensed under either of <a href="LICENSE-APACHE">Apache License, Version
2.0</a> or <a href="LICENSE-MIT">MIT license</a> at your option.
</sup>

<br>

<sub>
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in these crates by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.
</sub>
