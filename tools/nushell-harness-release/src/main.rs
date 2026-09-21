//! `bench-vs-nu-parser` compiled against `nu-parser` 0.115.1 from crates.io,
//! so that two Nushell releases can be compared in one table. The source is
//! shared with `tools/nushell-harness`.

#[path = "../../nushell-harness/src/bin/bench.rs"]
mod bench;

fn main() {
    bench::main();
}
