# How the parser works

These documents describe `nu-winnow-parser` from the inside, for people who
want to change it. They follow the data flow: source text goes into the lexer,
tokens are grouped into pipelines, each command's items are recognised as a
statement or an expression, and each item becomes a value. Every chapter names
the files involved and shows the code shapes you will meet there.

| Chapter | What it covers | Files |
| --- | --- | --- |
| [01 Architecture](01-architecture.md) | The pipeline of phases, the module map, the public API, the design constraints inherited from Nushell | `src/lib.rs`, `src/parser/mod.rs` |
| [02 The lexer](02-lexer.md) | Items, `LexOptions`, token kinds, the item scanner, pipe continuation | `src/lexer.rs` |
| [03 Streams, cursors, errors and state](03-streams-and-errors.md) | The winnow character stream, the token `Cursor`, `Diagnostic` as the winnow error, cut vs backtrack, `St`/`Shared`, recovery | `src/input.rs`, `src/parser/cursor.rs`, `src/error.rs`, `src/parser/mod.rs` |
| [04 Blocks and pipelines](04-blocks-and-pipelines.md) | Grouping tokens into pipelines and commands, comments, assignments, redirections, attributes, predeclaration | `src/parser/block.rs` |
| [05 Statements and expressions](05-statements-and-expressions.md) | Keyword dispatch, every keyword form, math expressions and precedence, calls, arguments, external calls | `src/parser/statement.rs`, `src/parser/expr.rs` |
| [06 Values and literals](06-values-and-literals.md) | Turning one item into an expression: `$`, `(`, `{`, `[`, literals, strings, interpolation, cell paths, ranges, collections, closures | `src/parser/value.rs`, `src/parser/strings.rs`, `src/parser/cellpath.rs`, `src/parser/collections.rs`, `src/parser/literal.rs` |
| [07 Signatures, types and patterns](07-signatures-types-and-patterns.md) | Parameter lists, type annotations, input/output types, `match` patterns | `src/parser/signature.rs`, `src/parser/pattern.rs` |
| [08 The AST and its consumers](08-ast-and-consumers.md) | Node catalogue, spans, comments, the visitor, `flatten`, `pretty` | `src/ast/`, `src/flatten.rs`, `src/pretty.rs` |
| [09 Testing and tools](09-testing-and-tools.md) | Unit and integration tests, the fixtures, the corpora, differential testing, comparison scripts, benchmarks, the engine harness | `tests/`, `tools/` |
| [11 Traceability](11-traceability.md) | Every construct of nu-parser mapped to the code, fixtures and tests here; checked by `tests/traceability.rs` | `tests/`, `src/` |
| [TESTING.md](../../TESTING.md) | The runbook: how to run every test and the verification ladder, how to read a disagreement, how to add coverage | `tests/`, `tools/scripts/` |
| [10 Contributing](10-contributing.md) | Step-by-step recipes for the common changes, debugging, pitfalls | everywhere |
| [How to use the tools](how-to.md) | Every command-line tool in the repository with its flags and examples: the `parse` example, `nufmt`, the engine `bridge`, the benchmarks, the comparison scripts, tests and features | `examples/`, `tools/`, `tests/` |
| [nufmt README](../../examples/nufmt/README.md) | Why the tree plus the source is lossless, with runnable examples of reconstructing, rewriting and formatting source | `examples/nufmt/` |
| [Nushell integration plan](nushell-integration-plan.md) | How to port this parser into the Nushell code base without breaking users | — |

Conventions used in the chapters:

* Code blocks marked `rust` are compiled and run as doctests against the
  public API (`cargo test --doc`). Blocks marked `rust,ignore` are excerpts of
  internal code shown for explanation; they are kept close to the source but
  may omit details.
* "nu-parser" means the parser inside the Nushell repository
  (`crates/nu-parser`), which is the reference for every behaviour here.
* Positions are byte offsets into the original source, always absolute, even
  inside nested constructs.
