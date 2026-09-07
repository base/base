# revmc

Experimental [JIT] and [AOT] compiler for the [Ethereum Virtual Machine][EVM].

The compiler implementation is abstracted over an intermediate representation backend. It performs very well, as demonstrated below from our criterion benchmarks, and exposes an intuitive API via Revm.

![image](https://github.com/paradigmxyz/revmc/assets/17802178/96adf64b-8513-469d-925d-4f8d902e4e0a)

The compiler backend is abstracted behind a trait ([`revmc-backend`]), with an [LLVM] implementation ([`revmc-llvm`]) providing full test coverage.

[JIT]: https://en.wikipedia.org/wiki/Just-in-time_compilation
[AOT]: https://en.wikipedia.org/wiki/Ahead-of-time_compilation
[EVM]: https://ethereum.org/en/developers/docs/evm/
[LLVM]: https://llvm.org/
[`revmc-backend`]: /crates/revmc-backend
[`revmc-llvm`]: /crates/revmc-llvm

## Requirements

- Latest stable Rust version

### LLVM backend

- Linux or macOS, Windows is not supported
- LLVM 22
  - On Debian-based Linux distros: see [apt.llvm.org](https://apt.llvm.org/)
  - On Arch-based Linux distros: `pacman -S llvm`
  - On macOS: `brew install llvm@22`
  - The following environment variables may be required:
    ```bash
    prefix=$(llvm-config --prefix)
    # or
    #prefix=$(llvm-config-22 --prefix)
    # on macOS:
    #prefix=$(brew --prefix llvm@22)
    export LLVM_SYS_221_PREFIX=$prefix
    ```

## Usage

The compiler is implemented as a library and can be used as such through the `revmc` crate.

A minimal runtime is required to run AOT-compiled bytecodes. A default runtime implementation is
provided through symbols exported in the `revmc-builtins` crate and must be exported in the final
binary. This can be achieved with the following build script:
```rust,ignore
fn main() {
    revmc_build::emit();
}
```

You can check out the [examples](/examples) directory for example usage.

## Testing

The [Ethereum state tests](https://github.com/ethereum/tests) are included as a git submodule.
The submodule is configured with `update = none` so it won't be cloned automatically.

To check it out:

```bash
git submodule update --init --checkout --depth 1 tests/ethereum-tests
```

Then run the state tests:

```bash
# All three modes (interpreter baseline, JIT, AOT):
cargo nextest run -p revmc -E 'test(statetest::)'

# A specific mode:
cargo nextest run -p revmc -E 'test(statetest::jit)'

# A specific test subdirectory:
SUBDIR=stSelfBalance cargo nextest run -p revmc -E 'test(statetest::jit)'
```

## Credits

- [`paradigmxyz/jitevm`](https://github.com/paradigmxyz/jitevm) for inspiring the initial compiler implementation.
- [gigahorse-toolchain](https://github.com/nevillegrech/gigahorse-toolchain) for static analysis ideas.
- [Solidity](https://github.com/ethereum/solidity) and [Vyper](https://github.com/vyperlang/vyper) compilers for opcode semantics and optimization references.

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
