# nu-winnow-parser

A parser for the [Nushell](https://www.nushell.sh) language written with
[`winnow`](https://github.com/winnow-rs/winnow). It produces a span-preserving,
comment-preserving AST that is suitable both as a front end for a Nushell
engine and as the foundation of a formatter such as
[`nufmt`](https://github.com/nushell/nufmt).

* **Complete grammar.** Pipelines, redirections, every literal (numbers in all
  radices, durations, filesizes, datetimes, binary, ranges, all five string
  quoting styles, interpolation), lists, tables, records, closures, blocks,
  subexpressions, cell paths (`$x.a.0?.b!`, `$.a`, `(ls).name`), all operators
  with Nushell's precedence and associativity, calls with flags and spreads,
  external calls, environment shorthand, `let`/`mut`/`const`, assignments,
  `def`/`extern`/`alias`/`module`/`use`/`export`/`export-env`, attributes,
  `if`/`match`/`for`/`while`/`loop`/`try`/`return`/`break`/`continue`,
  `where` row conditions, signatures with types and completers, comments.
* **Checked against the reference.** Every `.nu` file in `nu_scripts` and
  Nushell's standard library parses; the parser's accept/reject verdicts agree
  with `nu-check` on all 1,599 files except those `nu-check` rejects for
  semantic reasons (missing modules, type mismatches, signature-dependent
  argument counts).
* **Fast.** Roughly 30–45 MB/s per file and 40 MB/s over a 7 MB corpus of
  real scripts in release mode (single-threaded, including file I/O), with
  zero-copy borrowing of identifiers, bare words and unescaped string bodies.
* **Good errors.** Diagnostics carry an absolute span, the stack of grammar
  contexts (`while parsing signature`), and help text; there is a built-in
  renderer with source excerpts, and statement-level error recovery.
* **Only one dependency:** `winnow`. `serde` support is an optional feature.

## Usage

```rust
use nu_winnow_parser::{parse, ast::ExprKind};

let ast = parse("ls | where size > 1kb | get name").unwrap();
let pipeline = &ast.block.pipelines[0];
assert_eq!(pipeline.elements.len(), 3);
assert!(matches!(pipeline.elements[1].expr.kind, ExprKind::Where(_)));
```

```rust
use nu_winnow_parser::{parse_lenient, ParseConfig};

// Error recovery: failed statements become `ExprKind::Garbage` nodes and
// parsing continues; every diagnostic is returned.
let (ast, diagnostics) = parse_lenient("ls\nlet = 1\npwd", &ParseConfig::new());
assert_eq!(ast.block.pipelines.len(), 3);
assert_eq!(diagnostics.len(), 1);
println!("{}", diagnostics[0].render(ast.source, Some("script.nu")));
```

The example binary prints the tree or checks a directory of scripts:

```text
cargo run --example parse -- script.nu            # indented tree dump with spans
cargo run --example parse -- --summary script.nu  # node counts and timing
cargo run --example parse -- --check ~/src/nu_scripts
cargo run --features serde --example parse -- --json script.nu
echo 'ls | length' | cargo run --example parse
```

## Public API

| Item | Purpose |
| --- | --- |
| `parse(&str) -> Result<Ast, ParseError>` | Parse with the default configuration. |
| `parse_with(&str, &ParseConfig)` | Parse with an explicit configuration. |
| `parse_lenient(&str, &ParseConfig) -> (Ast, Vec<Diagnostic>)` | Parse with statement-level recovery. |
| `ParseConfig` | The set of known (multi-word) command names. |
| `ast::*` | The tree. Every node has a `Span`. |
| `ast::Visitor` | A visitor with `walk_*` defaults for building tools. |
| `flatten::flatten(&Ast)` | Source-ordered `(Span, FlatShape)` pairs, like `nu-parser`'s `flatten_block`. |
| `pretty::dump(&Ast)` | A human-readable tree. |
| `lexer::lex` | The item lexer, usable on its own. |
| `Span`, `LineIndex`, `Diagnostic`, `ParseError` | Positions and errors. |

### Configuration

Nushell resolves multi-word command names (`str trim`, `into int`) by looking
them up in the engine's declarations. This crate ships the list of built-in
command names (feature `builtin-commands`, on by default) and always registers
commands defined in the file being parsed, so `str trim --left` is a call to
`str trim`. Use `ParseConfig::with_commands` or `add_commands` to supply the
names exported by modules you `use`, or `ParseConfig::empty()` to treat every
head as a single word.

## The AST

The shape follows `nu-protocol`'s AST so that an evaluator can consume it
directly, while keeping everything a formatter needs:

* `Ast { source, block, comments, shebang }` — `comments` lists every comment
  in source order, and each `Pipeline` also carries the comments attached to
  it (`leading_comments` are the doc comments of a `def`).
* `Block { pipelines }` → `Pipeline { elements, terminator }` →
  `PipelineElement { pipe, expr, redirection }`.
* `Expr { span, kind: ExprKind }`. Statement keywords are `ExprKind` variants
  (`Let`, `Def`, `If`, `Match`, ...) holding structs with a span for every
  keyword and operator, so a formatter can reproduce the source layout.
* Strings keep their decoded `value` and their `Quote` style; the original
  spelling is `span.slice(source)`. Bare words, identifiers and un-escaped
  string bodies borrow from the source (`&'a str` / `Cow::Borrowed`).
* Signatures record each parameter's kind, type annotation (as a `TypeKind`
  tree), default value, completer and description comment, plus the
  `input -> output` type pairs.
* `where` row conditions become `FullCellPath` nodes with `implicit_head`
  set and a zero-width `$it` head, matching Nushell's semantics.

Things that require a command's signature are deliberately *not* decided by
the parser, exactly as in `nu-parser` before signature lookup: whether a flag
takes the following argument as its value (`--flag value` is a `Flag` followed
by a `Positional`; `--flag=value` carries its value), and whether an unknown
head is an internal or external command (all bare heads become `Call`; `^cmd`
becomes `ExternalCall`). An evaluator applies its signatures on top.

## Design

The grammar of Nushell is whitespace-sensitive: `1+1` is a bare word while
`1 + 1` is math, and `[1 + 1]` is a three-element list. The reference parser
handles this by lexing *items* (bracket- and quote-balanced runs of text)
and re-lexing the interior of an item when it turns out to be a list, a record
or a block. This crate mirrors that design because it is what defines the
language:

1. `lexer` — a winnow parser over `LocatingSlice<&str>` that produces items,
   pipes, redirections, `;`, newlines, comments and assignment operators, with
   `LexOptions` selecting which bytes are whitespace or "special" (so `,` is
   whitespace inside a list, `:` splits record keys, `.` splits cell paths).
2. `parser::block` — winnow parsers over a `TokenSlice` that group tokens into
   pipelines and commands (comment attachment, `|` continuation across lines,
   `=` absorbing the rest of the line, redirections, attribute lines) with
   statement-level error recovery.
3. `parser::statement` and `parser::expr` — keyword statements, calls and
   math expressions (a precedence-climbing fold identical to `nu-parser`'s).
4. `parser::value` and `parser::literal` — one item becomes an expression;
   nested constructs re-lex their interior with the appropriate options.

Every parser uses `Diagnostic` as its winnow error type, so backtracking and
`cut_err` behave normally while errors carry absolute positions and context.

## Comparison with `nu-parser`

`nu-parser` is coupled to the engine: it needs a `StateWorkingSet`, resolves
declarations and variables while parsing, type-checks, and compiles blocks to
IR. This crate is a pure syntactic front end:

| | `nu-parser` | `nu-winnow-parser` |
| --- | --- | --- |
| Needs an engine state | yes | no |
| Output | `nu-protocol` AST with ids | plain AST with spans, borrowed text |
| Comments | spans on some nodes | all comments, attached and listed |
| Signature-aware argument parsing | yes | no (documented above) |
| Error recovery | per node | per statement, plus nested blocks |
| Dependencies | many | `winnow` |

Because the item lexer and the expression grammar are the same, the two
parsers accept the same programs; the only divergences are semantic checks
that need declarations or types.

### Measured against the reference

Three checks were run against Nushell 0.115.2 (see `tools/nushell-harness` and the
Nushell scripts in `tools/scripts/`):

* **Accept/reject parity.** Over `nu_scripts` and the standard library (1,599
  files) the verdicts agree with `nu-check` except where `nu-check` fails for
  semantic reasons (missing modules, type mismatches, signature-dependent
  argument counts). No file that nu accepts is rejected here.
* **Token classification.** For every standard-library file,
  `tools/scripts/flatcmp.nu` compares the output of nu's `ast --flatten` with
  this crate's `flatten()` segment by segment. The only differences are the
  documented signature-dependent ones (`get content.0` is a cell path only
  because `get` declares that shape), attribute lines (which nu's flatten
  treats as opaque) and `$.` (a cell-path literal here, a delimiter for nu).
* **Speed.** `tools/nushell-harness/bench-vs-nu-parser` times both parsers on the
  same bytes, with `nu-parser` given the full command set as in the shell.
  Two Nushell releases are shown, since 0.115.2 sped up `nu-parser`
  considerably (`tools/nushell-harness-release` builds the same harness against the
  crates.io release):

  | Corpus | `nu-parser` 0.115.1 | `nu-parser` 0.115.2 | `nu-winnow-parser` | Ratio vs 0.115.1 | Ratio vs 0.115.2 |
  | --- | ---: | ---: | ---: | ---: | ---: |
  | Standard library, 61 files, 250 kB | 31.6 ms | 25.2 ms | 7.4 ms | 4.2× | 3.4× |
  | `nu_scripts`, 1538 files, 6.9 MB | 435 ms | 301 ms | 117 ms | 3.6× | 2.6× |
  | `tests/corpus`, 14 files, 220 kB | 17.2 ms | 13.1 ms | 4.7 ms | 3.6× | 2.8× |

  If the standard library is loaded so that `use std/...` resolves, `nu-parser`
  also parses the imported modules and the gap grows to 15×; that number
  measures the shell's whole parse step rather than the parser itself.

### Plugging into Nushell

`tools/nushell-harness/src/bin/bridge.rs` is a working MVP: it parses with this
crate, lowers the tree into `nu-protocol` structures inside a
`StateWorkingSet` (resolving declarations, applying signatures, declaring
variables and custom commands, tracking closure captures), compiles to IR with
`nu-engine` and evaluates. `bridge --demo` runs 22 scripts through both front
ends and checks that the results are identical. The lowering pass is about
700 lines for the supported subset; the module system (`use`, `module`,
`export`, `alias`, `const`) is the part still tied to `nu-parser`.

### A formatter

`examples/nufmt/` is a `nufmt`-style formatter over this AST (about 600
lines): normalised spacing, indentation of blocks and multi-line collections,
comments preserved, literals copied verbatim. `tests/nufmt.rs` checks on the
whole corpus that formatting is idempotent, keeps every comment, and yields a
structurally identical tree when re-parsed.

```text
cargo run --example nufmt -- --check tests/corpus
echo 'ls|where size > 1kb' | cargo run --example nufmt
```

## Performance notes

Measured with `cargo bench` (criterion, release profile) on an Apple Silicon
laptop, single-threaded:

| Input | Size | Time | Throughput |
| --- | --- | --- | --- |
| `tests/corpus/kitchen_sink.nu` (every construct) | 2.9 kB | 99 µs | 29 MB/s |
| `std/iter/mod.nu` | 5.2 kB | 115 µs | 45 MB/s |
| `std/assert/mod.nu` | 8.6 kB | 287 µs | 30 MB/s |
| 20 copies of the two above | 275 kB | 9.2 ms | 30 MB/s |
| lexer only, kitchen sink | 2.9 kB | 14 µs | 200 MB/s |
| `ls \| where size > 1kb \| sort-by modified \| get name \| first 10` | 58 B | 2.1 µs | |

Real scripts (`nu_scripts` + the standard library, 1,613 files, 7.3 MB) take
about 185 ms including file I/O with the `--check` example, i.e. roughly 40
MB/s. For comparison, `nu-check` over the same corpus is dominated by engine
start-up and declaration resolution rather than by lexing and parsing; a
direct number is not meaningful without embedding this crate in the engine.

Where the time goes: nested constructs are lexed once per nesting level (as in
the reference implementation), token vectors are allocated per block, list
and record, and each item is tried against the literal parsers in order.
Bare words, identifiers, comments and un-escaped string bodies borrow from the
source. Single-word command heads take a fast path that avoids building
candidate names; multi-word resolution only runs when the first word is known
to start a multi-word command.

Deeply nested brackets recurse on the stack, one frame per nesting level, as
`nu-parser` does.

## Testing

* `tests/syntax.rs` — construct-by-construct assertions on the AST for every
  feature of the language, including error cases.
* `tests/corpus.rs` — parses the real-world files in `tests/corpus/`
  (standard library modules, default config, completion modules, prompts) and
  optionally every `.nu` file under `NU_WINNOW_CORPUS`.
* `tests/nufmt.rs` — idempotency and re-parse equivalence of the formatter
  example over the corpus.
* `tools/nushell-harness` — benchmark against `nu-parser` and the engine bridge
  (requires a local Nushell checkout; see its README);
  `tools/nushell-harness-release` builds the benchmark against the crates.io
  release for cross-version tables.
* `src/docs/` — how the parser works, chapter by chapter, plus the Nushell
  integration plan; also rendered by `cargo doc` under `nu_winnow_parser::docs`.
* Unit tests in each module (lexer, literals, flatten, spans, errors).

Run everything with `cargo test`; run the benchmarks with `cargo bench`.

## License

MIT, like Nushell. The command-name table and the corpus files are derived
from the Nushell project.
