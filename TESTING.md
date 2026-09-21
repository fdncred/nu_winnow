# Testing nu-winnow-parser

This is the runbook: what each test does, how to run it, and what to do when
it fails. The design of the test layers is explained in
[`src/docs/09-testing-and-tools.md`](src/docs/09-testing-and-tools.md); every
tool's flags are in [`src/docs/how-to.md`](src/docs/how-to.md).

## The short version

```nushell
cargo test                      # everything that needs nothing but Rust: about 1,350 tests, a few seconds
nu tools/scripts/verify.nu      # everything that compares with Nushell itself, a few minutes, prints a scoreboard
```

Both must be green before a change is finished. `cargo test` needs no
external tools. `verify.nu` needs the checkouts listed below.

## Prerequisites for the comparison rungs

| Need | Default location | Used by |
| --- | --- | --- |
| `nu` 0.115 on `PATH` | | every script in `tools/scripts/` |
| Nushell source checkout | `~/src/nushell` (also `../nushell` for the traceability test) | `tools/nushell-harness` (links `nu-parser` by path), `extract-corpus.nu`, traceability |
| nushell.github.io checkout | `~/src/nushell.github.io` | `extract-corpus.nu` (the book's code blocks) |
| nu_scripts checkout | `~/src/nu_scripts` | `verify.nu`, `nucheck-compare.nu`, `differential` |
| nushell/nufmt checkout | `~/src/nufmt` | `nufmt-fixtures.nu` |

The harness paths are in `tools/nushell-harness/Cargo.toml`; the first build
takes several minutes because it compiles Nushell.

## `cargo test`, layer by layer

| Command | What it checks | Where |
| --- | --- | --- |
| `cargo test --lib` | Unit tests of the lexer, literal parsers, spans, errors, flatten | `src/**` (`#[cfg(test)]`) |
| `cargo test --test syntax` | One test per construct, asserting on the tree with helpers | `tests/syntax.rs` |
| `cargo test --test fixtures` | Every snippet in `tests/fixtures/accept` parses and matches its golden `.ast`; every snippet in `reject` fails and matches its golden `.err`; spans nest, `flatten` covers the source | `tests/fixtures.rs`, `tests/fixtures/` |
| `cargo test --test language` | Tables lifted from Nushell's own `test_lex.rs`, `test_parser.rs` and repl tests: values, spans, token streams, precedence, error messages | `tests/language.rs` |
| `cargo test --test examples` | Every built-in command's `Example` snippet parses with no diagnostics | `tests/examples.rs`, `tests/corpus/snippets/` |
| `cargo test --test corpus` | Real files (std modules, default config, nu_scripts samples) parse cleanly | `tests/corpus.rs`, `tests/corpus/` |
| `cargo test --test traceability` | The matrix in chapter 11 points at existing fixtures and tests, and (with a Nushell checkout) covers every upstream construct | `tests/traceability.rs`, `src/docs/11-traceability.md` |
| `cargo test --test nufmt` | The formatter is idempotent, keeps comments and preserves the tree over the corpus | `tests/nufmt.rs`, `examples/nufmt/` |
| `cargo test --doc` | The `rust` code blocks in the README and `src/docs/` | `src/docs/*.md` |
| `cargo test --all-features` | The same with the `serde` derives compiled | |

Run one test by name with `cargo test --test syntax -- if_forms`; fixture
tests are named after their path, so `cargo test --test fixtures -- ranges`
runs every range fixture.

Environment variables the tests read:

| Variable | Effect |
| --- | --- |
| `UPDATE_FIXTURES=1` | Rewrite the golden `.ast`/`.err` files instead of comparing (then review `git diff`) |
| `NU_WINNOW_CORPUS=/dir` | `corpus` also parses every `.nu` file below `/dir` |
| `NU_WINNOW_CORPUS_SKIP=a,b` | Skip corpus files whose path contains one of the substrings |
| `NU_WINNOW_NUSHELL=/dir` | The Nushell checkout for the traceability test (default `../nushell`) |
| `NU_WINNOW_COMMANDS=file` | Extra command names for the `parse` example (used by the scripts) |

## `verify.nu`, rung by rung

```nushell
nu tools/scripts/verify.nu                       # build, run every rung, print the scoreboard
nu tools/scripts/verify.nu --no-build            # reuse the release binaries
nu tools/scripts/verify.nu --quick               # skip nu_scripts, the book and mutation fuzzing
nu tools/scripts/verify.nu --save tools/scripts/verify-history.nuon
```

| Rung | Command run on its own | Good means |
| --- | --- | --- |
| cargo test | `cargo test --all-features` | 0 failures |
| fixtures-compare | `nu tools/scripts/fixtures-compare.nu` | 0 rows where `ours != expected`; rows where only `nu-check` disagrees are semantic (missing files, plugins) and listed with nu-parser's message |
| differential, originals | `cd tools/nushell-harness; cargo run --release --bin differential -- ../../tests/fixtures ../../tests/corpus ~/src/nushell/crates/nu-std ~/src/nushell/tests ~/src/nu_scripts --snippets ../../tests/corpus/snippets/nu-command-examples.json --snippets ../../tests/corpus/snippets/book.json` | 0 `ours_rejects`, 0 `nu_syntax_rejects`, 0 `panics` |
| differential, mutants | the same with `--mutants 3 --seed 1` | as few as possible; 0 panics |
| nucheck-compare | `nu tools/scripts/nucheck-compare.nu ~/src/nu_scripts ~/src/nushell/crates/nu-std` | 0 files nu accepts that we reject |
| flatcmp | `nu -c 'nu tools/scripts/flatcmp.nu --commands tools/scripts/std_commands.txt ...(glob ~/src/nushell/crates/nu-std/**/*.nu)'` | only the documented signature-dependent differences |
| nufmt-fixtures | `nu tools/scripts/nufmt-fixtures.nu` | 111 of 130 reference fixtures |

The scoreboard's `ok` column is the gate. `--save` appends the table with
the date and the Nushell commit to a NUON file, so a number that moves can be
traced to a parser change or to a Nushell change.

## Reading a disagreement

* A fixture that fails: the panic prints the fixture path, the rendered
  diagnostic (accept) or the expected and actual golden text (reject).
  Decide whether the parser or the fixture is wrong; `nu-check` on the
  snippet (`open --raw file.nu | nu-check`) is the referee.
* `fixtures-compare --details` returns a table with `ours`, `nu`, `main` and
  `main_error`; `where ours != nu` shows the rows to look at.
* `differential --details` prints every disagreement with both parsers'
  messages and the text. nu-parser's error is classified as syntax or
  semantic by its variant and message (`is_syntax_error` in
  `tools/nushell-harness/src/bin/differential.rs`); a `known` row is one of
  the documented tolerances in `KNOWN_DIFFERENCES` there.
* The traceability test names the unmapped upstream item; add a row to
  chapter 11, then a fixture and a test for it.

## Adding coverage

1. Put a snippet in `tests/fixtures/accept/<area>/<name>.nu` or
   `reject/<area>/<name>.nu`, one construct per file, valid for `nu-check`
   where possible (define the variables and commands it uses). The rules are
   in `tests/fixtures/README.md`.
2. `UPDATE_FIXTURES=1 cargo test --test fixtures` to write the golden file;
   read it, it is the tree the parser produced.
3. If the snippet pins a value or a span rather than a shape, add a case to
   the matching `#[rstest]` table in `tests/language.rs`.
4. Add the fixture pattern to the row of chapter 11 it belongs to.
5. `nu tools/scripts/fixtures-compare.nu` to confirm Nushell agrees.

## When the Nushell checkout moves

```nushell
cd tools/nushell-harness; cargo build --release --bin differential --bin nu-parser-check; cd ../..
nu tools/scripts/extract-corpus.nu       # new command examples and book blocks
cargo test --test traceability           # new SyntaxShape / keyword / ParseError variants show up here
nu tools/scripts/verify.nu --save tools/scripts/verify-history.nuon
```

A construct that Nushell added appears as an unmapped item in the
traceability test, as `nu_syntax_rejects` or `ours_rejects` in the
differential rung, or as a new command example that does not parse. The `%`
sigil was found exactly this way.

## Benchmarks

Not part of the ladder. `cargo bench` runs the criterion benchmarks in
`benches/parse.rs`; `tools/nushell-harness`'s `bench-vs-nu-parser` times
nu-parser against this crate on the same files (see `how-to.md`).
