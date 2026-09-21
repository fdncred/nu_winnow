# 09 Testing and tools

The parser's correctness claim is "accepts and structures programs exactly
like nu-parser". Several layers of tests back that claim; know which one to
extend for a given change.

## Unit tests inside modules

`src/lexer.rs`, `src/parser/literal.rs`, `src/flatten.rs`, `src/span.rs` and
`src/error.rs` have `#[cfg(test)]` modules. They are the place for focused
behaviour of one function (a new escape sequence, a new unit, a lexer edge).

## `tests/syntax.rs`: one test per construct

Each test parses snippets and asserts on the tree with small helpers:

```rust,ignore
let ast = ok("let x = 1 + 1 | into string");          // parse or panic with a rendered diagnostic
let b = kind!(expr(&ast), ExprKind::Let(b) => b);      // match a variant or panic with the actual one
assert_eq!(text(&ast, b.eq.unwrap()), "=");
assert!(matches!(err("1 +").primary().kind, ErrorKind::Expected(_)));
```

The file is organised by chapter of the grammar (pipelines, literals,
strings, cell paths, ranges, collections, calls, declarations, control flow,
redirections, errors and recovery, spans). `spans_are_nested_and_on_char_boundaries`
walks `tests/corpus/kitchen_sink.nu` with a visitor that checks every child
span lies inside its parent and on a UTF-8 boundary. Add a test here for any
grammar change; include the nu behaviour you verified (`nu -n -c '...'`) in
a comment when it is surprising.

## `tests/corpus.rs`: real files

`tests/corpus/` holds standard-library modules, the default config files,
completion modules and prompts copied from the Nushell and `nu_scripts`
repositories, plus `kitchen_sink.nu`, which exercises most of the grammar and
is itself valid Nushell (`nu-check` passes and it runs). The test parses every
file and requires zero diagnostics. Set `NU_WINNOW_CORPUS=/path` to also parse
every `.nu` file below a directory (for example a `nu_scripts` checkout).

## `tests/nufmt.rs`: the formatter as a parser test

Formatting every corpus file must be idempotent, keep every comment, and
produce a tree whose `pretty::dump` (spans removed) equals the original's.
This catches parser regressions from a different angle: a construct that
parses but whose spans are wrong will format into something different.

## Comparing with Nushell itself (`tools/scripts/`, Nushell scripts)

* `nucheck-compare.nu DIR...` runs `nu-check` and this parser on every file
  and reports disagreements. A file nu accepts and this parser rejects is a
  bug; the other direction is usually a semantic error (missing module, type
  mismatch) and worth a look but not a bug.
* `flatcmp.nu FILE...` compares nu's `ast --flatten` classification with
  `flatten()` segment by segment after mapping both to coarse classes. Pass
  `--commands std_commands.txt` so standard-library commands resolve as
  multi-word names as they do inside nu. Residual differences are expected
  only where nu needs a signature (`get a.0` is a cell path), around
  attribute lines and `$.`.
* `gen-std-commands.nu STD_DIR` regenerates `std_commands.txt`.

Run these after any change to the lexer or to `value.rs`; they take seconds.

## Benchmarks

* `benches/parse.rs` (criterion): per-file, per-snippet and lexer-only
  throughput. `cargo bench`.
* `tools/nushell-harness` (`bench-vs-nu-parser`): times `nu-parser` and this
  crate on the same files, with `nu-parser` given the full command set.
  `tools/nushell-harness-release` builds the same harness against the
  crates.io release so two Nushell versions can be compared. Both link real
  Nushell crates (path dependencies to a checkout, crates.io respectively),
  so they are separate cargo packages and take minutes to build.

## The engine bridge

`tools/nushell-harness/src/bin/bridge.rs` lowers this AST into `nu-protocol`
and evaluates it with `nu-engine`; `bridge --demo` runs a suite of scripts
through both front ends and checks the results are identical. It is the
executable form of the integration plan and the best place to look when a
question is "what would the engine need from this node?".

## Regenerating the built-in command table

`src/builtin_commands.rs` is generated from `nu -c "help commands | get name"`
for the Nushell release the parser targets. Regenerate it when targeting a
new release; the file is a sorted `&[&str]`.
