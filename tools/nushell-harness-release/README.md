# nushell-harness-release

The `bench-vs-nu-parser` benchmark from `../nushell-harness`, compiled against
the **released** `nu-parser` on crates.io (currently `=0.115.1`) instead of a
local Nushell checkout. The source is shared (`src/main.rs` includes
`../nushell-harness/src/bin/bench.rs` by path); only the dependencies differ.

Use it to put two Nushell versions side by side:

```text
cd tools/nushell-harness && cargo run --release --bin bench-vs-nu-parser -- ~/src/nushell/crates/nu-std
cd tools/nushell-harness-release && cargo run --release -- ~/src/nushell/crates/nu-std
```

To compare against a different release, change the `=0.115.1` pins in
`Cargo.toml` (all five `nu-*` crates must be the same version). The first
build compiles Nushell's command crates and takes several minutes.
