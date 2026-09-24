# nushell-harness: comparing with, and plugging into, Nushell

This crate is not part of the library. It links the real `nu-parser`,
`nu-engine` and command crates from the `main` branch of
<https://github.com/nushell/nushell> (`cargo update` here moves to the
newest commit; the commented `[patch]` block in `Cargo.toml` switches to a
checkout next to this repository) to answer three questions:

1. How fast is `nu-winnow-parser` compared to `nu-parser` on the same files?
2. Where do the two parsers disagree (`nu-parser-check`, `differential`; see
   `TESTING.md` at the repository root)?
3. How would the new parser plug into the Nushell engine?

## `bench-vs-nu-parser`

```text
cargo run --release --bin bench-vs-nu-parser -- [--iters N] [--std] FILE|DIR ...
(cd ../nushell-harness-release && cargo run --release -- [--iters N] FILE|DIR ...)   # nu-parser 0.115.1
```

Both parsers are given the same bytes. `nu-parser` runs on a fresh
`StateWorkingSet` with the full built-in command set, exactly as the `nu`
binary parses a script. Setup is outside the timed region; parsing (which for
`nu-parser` includes declaration resolution and type checking) is inside.

Results on an Apple Silicon laptop, release builds. `nu-parser` 0.115.2 is
nushell's `main` at the time; 0.115.1 is the crates.io release, built by
`../nushell-harness-release` from the same harness source:

| Corpus | Files | Bytes | `nu-parser` 0.115.1 | `nu-parser` 0.115.2 | `nu-winnow-parser` | Ratio vs 0.115.1 | Ratio vs 0.115.2 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Nushell standard library (`crates/nu-std`) | 61 | 250 kB | 31.6 ms (7.9 MB/s) | 25.2 ms (9.9 MB/s) | 7.4 ms (33 MB/s) | 4.2× | 3.4× |
| `nu_scripts` repository | 1538 | 6.9 MB | 435 ms (16 MB/s) | 301 ms (23 MB/s) | 117 ms (58 MB/s) | 3.6× | 2.6× |
| `tests/corpus` of this repository | 14 | 220 kB | 17.2 ms (13 MB/s) | 13.1 ms (17 MB/s) | 4.7 ms (47 MB/s) | 3.6× | 2.8× |

The 0.115.2 release made `nu-parser` about 20–30% faster on these files.

With `--std`, the standard library is registered in the engine first so that
`use std/log` resolves. `nu-parser` then parses the imported module sources
inside the timed region, which is what the shell does but is no longer a
parser-to-parser comparison: the standard library corpus takes 119 ms
(2.1 MB/s), 15× slower than `nu-winnow-parser`. The numbers without `--std`
are the fair comparison; on those files `nu-parser` reports "module not
found" for the `use` lines and otherwise parses everything.

Per file, the ratio is largest for small files (10–20×, fixed per-parse costs
in `nu-parser`) and settles around 2.5–3× for files above a few kilobytes.

## `bridge`

```text
cargo run --release --bin bridge -- --demo                 # 22 scripts run both ways
cargo run --release --bin bridge -- 'ls | where size > 1kb | length'
cargo run --release --bin bridge -- --compare --file script.nu
```

The bridge is a minimal viable integration:

1. `nu_winnow_parser::parse` builds the syntactic AST.
2. `Lower` walks it and produces `nu_protocol::ast` nodes inside a
   `StateWorkingSet`. This pass does what `nu-parser` does *between* lexing
   and type checking: it resolves command names to declarations and applies
   their signatures (which flags take a value, which positional is a block or
   a cell path), declares variables and custom commands, tracks closure
   captures, and registers spans and blocks.
3. `nu_engine::compile` turns the blocks into IR and `nu_engine::eval_block`
   runs them.

`--demo` evaluates a suite of scripts through both front ends and compares the
resulting values; all 22 agree, including custom commands with flags, closures
capturing outer variables, `match` destructuring, `try`/`catch`, and row
conditions.

What the bridge shows about a real integration:

* Everything `nu-parser` needs from the engine during parsing (declarations,
  signatures, variables, spans) can be supplied *after* parsing by a lowering
  pass over the finished tree. The parser itself stays engine-free and can be
  reused by tools.
* The lowering pass is small (about 700 lines for the supported subset)
  because the syntactic tree already has the right shape: one `Expr`
  variant per Nushell keyword, signatures parsed into parameters and types,
  match patterns as a tree.
* What is *not* covered yet is precisely the module system: `use`, `module`,
  `export`, `alias`, `extern`, `const` evaluation, attributes, redirections
  and environment shorthand. These need `nu-parser`'s overlay and module
  machinery and would be the next step of an integration.
