# Language fixtures

One Nushell snippet per file, covering every construct the parser knows, in
every spelling the grammar allows. `tests/fixtures.rs` turns each file into
its own test with `rstest`'s `#[files]`, and `tools/scripts/fixtures-compare.nu`
runs the same files through Nushell itself.

```text
accept/<area>/<name>.nu    must parse; <name>.ast pins the tree (pretty::dump)
reject/<area>/<name>.nu    must not parse; <name>.err pins the rendered error
```

Areas mirror the grammar chapters in `src/docs/`: `pipelines`, `comments`,
`literals`, `strings`, `interpolation`, `variables`, `cellpaths`, `ranges`,
`lists`, `tables`, `records`, `closures`, `subexpressions`, `operators`,
`calls`, `externals`, `env-shorthand`, `redirections`, `assignments`,
`bindings`, `def`, `extern`, `alias`, `modules`, `attributes`, `signatures`,
`if`, `match`, `loops`, `try`, `where`, `lexer` and `misc`. Many snippets are
taken from Nushell's own `crates/nu-parser/tests` and `tests/repl` suites so
that the two parsers are checked against the same text.

## Rules for a fixture

* It is plain Nushell, exactly as a user would write it, with a trailing
  newline. Keep it small: one construct, or one interaction between two.
* An `accept` fixture should be something `nu-check` accepts too, so define
  the variables and commands it uses. Where that is impossible (a `use` of a
  file that does not exist, a plugin) the comparison script lists it under
  "nu disagrees where ours agrees" and the reason is semantic, not syntax.
* A `reject` fixture should be rejected by `nu-check` as well. If nu accepts
  it, either the fixture belongs in `accept/` or the parser has found a real
  difference; run the comparison script to find out which.
* Golden files are generated, never written by hand:

  ```text
  UPDATE_FIXTURES=1 cargo test --test fixtures
  ```

  Review the diff of the `.ast`/`.err` files before committing; a changed
  golden file is a changed parse.

## Checking against Nushell

```text
cargo build --release --example parse
(cd tools/nushell-harness; cargo build --release --bin nu-parser-check)
nu tools/scripts/fixtures-compare.nu
```

The report shows, per fixture, the verdict of this parser (`ours`), of the
`nu` binary on `PATH` (`nu`, through `nu-check`) and of `nu-parser` in the
local Nushell checkout (`main`, with its first error message). Rows where
`ours` and `nu` differ are parser differences and worth a fixture change or a
parser fix; rows where only `nu`/`main` disagree with the expected verdict
are semantic checks the parser does not do (types, unknown flags, missing
files) or differences between the two Nushell versions.
