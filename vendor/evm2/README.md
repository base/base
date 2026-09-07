# evm2

Fast, customizable EVM implementation in Rust.

## Highlights

- up to **2x faster than revm** from a tighter interpreter, static dispatch tables, and cheaper instruction plumbing.
- **Built for custom EVMs**: extend specs, opcodes, gas schedules, transactions, precompiles, environment data, and inspectors without reshaping the core.
- **Clean extension boundaries**: typed configuration keeps fork logic, transaction handling, host state, and opcode definitions separate while compiling down to a small execution path.

## Example

```rust,ignore
enum CustomSpecId {
    Custom,
    // ...
}

struct CustomTypes;

impl EvmTypes for CustomTypes {
    // ...
}

#[instruction(EvmTypes = CustomTypes)]
fn l1_blocknumber(cx: _) -> out {
    *out = Word::from(cx.state.host().block_env().ext.l1_block_number);
}

fn main() -> Result<()> {
    let spec_id = CustomSpecId::Custom;
    let mut evm = Evm::<CustomTypes>::new(spec_id, ..);
    let tx = CustomTx { .. };
    let executed = evm.transact(&tx)?;
    let result = executed.commit();
    // ...
    Ok(())
}
```

See [`crates/evm2/examples/custom_evm`](crates/evm2/examples/custom_evm) for the complete version.

## Benchmarks

```sh
cargo bench -p evm2 --bench evm
EVM2_BENCH_REVM=1 cargo bench -p evm2 --bench evm
```

## Development

```sh
cargo fmt --all
cargo cl
cargo nextest run
```

## Supported Rust Versions (MSRV)

evm2 always aims to stay up-to-date with the latest stable Rust release.

The Minimum Supported Rust Version (MSRV) may be updated at any time, so we can take advantage of new features and improvements in Rust.

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
