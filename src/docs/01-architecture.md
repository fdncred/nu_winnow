# 01 Architecture

## What kind of language Nushell is

Nushell's grammar is *whitespace-sensitive at the token level*. The unit the
reference parser works with is not a character or a classical token but an
**item**: a maximal run of non-whitespace text in which brackets and quotes are
balanced. These three inputs show why that matters:

```text
1 + 1        three items: 1, +, 1        -> math expression evaluating to 2
1+1          one item                    -> a bare word, the string "1+1"
[1 + 1]      one item; its interior has  -> a list of the three strings
             three items                    1, +, 1 (no math inside lists)
```

Consequences that shape the whole design:

* Whether `{ ... }` is a record, a closure or a block depends on where it
  appears and on its first two interior tokens.
* A nested construct (`[...]`, `{...}`, `(...)`) is one item to its parent and
  is lexed again, with different delimiter rules, when it is parsed.
* Many things need a command's signature to be resolved (does `--flag` take
  the next word as its value? is `name.0` a cell path or a string?). Those
  decisions are *not* made here; the AST records what was written and the
  consumer applies signatures (see the integration plan).

`nu-parser` handles all of this with a lexer that produces items and a
recursive parser that re-lexes item interiors. This crate keeps that
architecture on purpose: it is what defines the language, and matching it is
how the parser stays 100% compatible. The difference is that everything is
expressed with `winnow` streams and combinators, the output is a plain AST
with borrowed text and spans, and no engine state is needed.

## The phases

```text
source: &str
   │
   ▼  lexer::lex(text, base, LexOptions)                        src/lexer.rs
Vec<Token>  (Item | Pipe | Redirect | Assign | Semicolon | Eol | Comment | Eof)
   │
   ▼  parser::block::parse_block_tokens                          src/parser/block.rs
grouping into Pipeline / PipelineElement / RawCommand, comments attached,
`=` absorbs the rest of the line, redirections and @attributes collected,
error recovery per statement
   │
   ▼  parser::statement::parse_command / keyword_or_call         src/parser/statement.rs
keyword statements (def, let, if, match, ...) or
   ▼  parser::expr::parse_expression                             src/parser/expr.rs
env shorthand, assignments, math expressions with precedence, calls & args
   │
   ▼  parser::value::value(st, span, Hint)                       src/parser/value.rs
one item -> Expr: literals (src/parser/literal.rs), $vars, cell paths,
ranges, strings, interpolation, lists/tables/records, closures/blocks,
subexpressions — nested constructs re-lex their interior and recurse into
parse_block_tokens
   │
   ▼
ast::Ast { source, block, comments, shebang }                    src/ast/mod.rs
```

Every phase writes into a shared, copyable handle `St` (source text plus a
`RefCell<Shared>` holding comments, diagnostics and declared command names);
see chapter 03.

## Module map

| File | Responsibility | Read it when you want to... |
| --- | --- | --- |
| `src/lib.rs` | Public entry points `parse`, `parse_with`, `parse_lenient`; re-exports | change the API surface |
| `src/span.rs` | `Span`, `Spanned<T>`, `LineIndex`, `LineCol` | change position handling |
| `src/error.rs` | `ErrorKind`, `Diagnostic`, `ParseError`, rendering with source excerpts | add an error kind or improve messages |
| `src/input.rs` | The winnow stream types `Input` (chars) and `Tokens` (tokens); winnow error-trait impls for `Diagnostic` | change how positions or errors flow through winnow |
| `src/lexer.rs` | `Token`, `TokenKind`, `LexOptions`, `lex`, `lex_prefix`, `lex_prefix_at`, the item scanner | change what counts as an item or a delimiter |
| `src/parser/mod.rs` | `ParseConfig`, `Shared`, `St`, `parse_source` | change configuration, scopes, or the top-level driver |
| `src/parser/block.rs` | Token grouping: `parse_block_tokens`, `pipeline`, `raw_command`, `predeclare`, recovery | change statement boundaries, comments, redirections, attributes |
| `src/parser/statement.rs` | `parse_command`, `keyword_or_call`, one function per keyword (`def_stmt`, `if_stmt`, ...) | add or change a keyword statement |
| `src/parser/expr.rs` | `parse_expression`, `math_expression`, `parse_call`, `parse_args`, `resolve_head`, external calls, env shorthand; token-stream helpers | change operators, precedence, argument parsing, command-name resolution |
| `src/parser/value.rs` | `value` dispatch, `Hint`, strings and interpolation, cell paths, ranges, `brace`, lists/tables/records, closures/blocks/subexpressions | add a value form or change disambiguation |
| `src/parser/literal.rs` | Numbers, units, datetimes, binary blobs, escapes, raw strings | change a literal's syntax |
| `src/parser/signature.rs` | `[params]`, `(params)`, `|params|`, types, `: in -> out` | change signatures or types |
| `src/parser/pattern.rs` | `match { ... }` blocks and patterns | change match syntax |
| `src/ast/mod.rs` | All node types | add a node or field |
| `src/ast/visit.rs` | `Visitor` trait and `walk_*` | keep in sync when adding nodes |
| `src/flatten.rs` | Source-ordered `(Span, FlatShape)` list | keep in sync when adding nodes |
| `src/pretty.rs` | Tree dump used by the example and tests | keep in sync when adding nodes |
| `src/builtin_commands.rs` | Generated list of built-in command names | regenerate for a new Nushell release |
| `examples/parse.rs` | CLI: tree dump, `--check`, `--summary`, `--flat`, `--json` | debug a file |
| `examples/nufmt/` | A formatter over the AST | see how a consumer uses spans and comments |

## The public API in one example

```rust
use nu_winnow_parser::{parse, parse_lenient, ParseConfig, ast::ExprKind};

// Strict: any diagnostic is an error.
let ast = parse("let x = 1 + 2 | into string\nprint $x").unwrap();
assert_eq!(ast.block.pipelines.len(), 2);
let first = &ast.block.pipelines[0].elements[0].expr;
assert!(matches!(first.kind, ExprKind::Let(_)));

// Lenient: keep going after a bad statement, get every diagnostic.
let (ast, diagnostics) = parse_lenient("ls\nlet = 1\npwd", &ParseConfig::new());
assert_eq!(ast.block.pipelines.len(), 3);
assert!(ast.block.pipelines[1].elements[0].expr.is_garbage());
assert_eq!(diagnostics.len(), 1);
println!("{}", diagnostics[0].render(ast.source, Some("script.nu")));
```

`ParseConfig` carries the only piece of environment knowledge the parser
uses: the set of known command names, needed to join multi-word heads such as
`str trim`. Commands defined in the file being parsed are always recognised.

## Design constraints, and why

* **No engine state.** The parser must be usable by `nufmt`, editors and
  linters without constructing a Nushell engine. Everything that needs
  declarations is either configurable (`ParseConfig`) or deferred to the
  consumer.
* **Same acceptance as nu-parser.** Where nu-parser is lenient (a `|` inside
  a list, `alias x = a | b`), so is this crate; where nu-parser rejects
  (`"abc"def`, `&&`, keyword names for `def`), so does this crate. Chapter 09
  describes the comparison tooling that keeps this true.
* **Lossless enough for a formatter.** Every node has a span; every comment is
  kept; strings keep their quote style; operators and keywords keep their
  spans; layout can be recovered from the source through the spans.
* **Zero-copy where cheap.** Bare words, identifiers, comments and un-escaped
  string bodies borrow from the source (`&'a str` / `Cow::Borrowed`).
* **Winnow idioms.** Streams implement winnow's `Stream`/`Location`, the
  error type implements `ParserError`, combinators (`dispatch!`, `take_while`,
  `repeat`, `alt`, `opt`, `verify`) are used where they express the grammar
  naturally, and hand-written scanners are written *as* winnow parsers
  (`fn(&mut Input) -> PResult<T>`) so they compose.
