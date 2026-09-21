# 03 Streams, errors and shared state

Files: `src/input.rs`, `src/error.rs`, `src/parser/mod.rs`.

## Two winnow streams

winnow parsers are functions `fn(&mut Stream) -> Result<Output, ErrMode<E>>`.
This crate uses two stream types:

```rust,ignore
/// Character-level stream with absolute positions.
pub type Input<'a> = Stateful<LocatingSlice<&'a str>, Base>;

/// Token-level stream with a copyable state `S`.
pub type Tokens<'t, S> = Stateful<TokenSlice<'t, Token>, S>;
```

`Input` wraps a `&str` slice in winnow's `LocatingSlice` (which tracks the
offset within the slice) and `Stateful` (which carries `Base(usize)`, the
absolute offset of the slice's first byte). `pos(i)` adds the two, so a
literal parser working on the interior of a list nested three levels deep
still produces absolute spans:

```rust,ignore
pub fn pos(i: &Input<'_>) -> usize { i.state.0 + i.current_token_start() }
pub fn span_from(i: &Input<'_>, start: usize) -> Span { Span::new(start, pos(i)) }
```

`Tokens` wraps a `TokenSlice<Token>` (winnow's slice-of-tokens stream) and
carries the parser handle `St` as its state, so any token-level parser can
reach the source text and the shared state through `i.state`. The concrete
alias in `src/parser/expr.rs` is:

```rust,ignore
pub type Toks<'t, 's, 'a> = Tokens<'t, St<'s, 'a>>;
pub fn toks<'t, 's, 'a>(st: St<'s, 'a>, tokens: &'t [Token]) -> Toks<'t, 's, 'a> {
    Stateful { input: TokenSlice::new(tokens), state: st }
}
```

Helpers over `Toks` in `expr.rs` are the vocabulary of every statement parser:
`peek_token`, `at_end`, `expect_item(i, "what")`, `expect_end`, `rest_span`,
`items(tokens)` (strip the `Eof`), `with_eof(slice)` (copy a slice and append
an `Eof`).

## `Diagnostic` is the winnow error type

Instead of `ContextError`, every parser returns `PResult<T> = ModalResult<T,
Diagnostic>`. `Diagnostic` implements winnow's traits for both streams
(`src/input.rs`):

```rust,ignore
impl<'a> ParserError<Input<'a>> for Diagnostic {
    type Inner = Diagnostic;
    fn from_input(i: &Input<'a>) -> Self {
        Diagnostic::new(ErrorKind::Expected("valid syntax"), Span::point(pos(i)))
    }
    fn or(self, other: Self) -> Self {
        if other.span.start >= self.span.start { other } else { self }   // keep the furthest
    }
    fn into_inner(self) -> Result<Self::Inner, Self> { Ok(self) }
}

impl<'a> AddContext<Input<'a>, &'static str> for Diagnostic {
    fn add_context(mut self, _: &Input<'a>, _: &Checkpoint, ctx: &'static str) -> Self {
        self.context.push(ctx);   // `.context("closure")` builds the innermost-first stack
        self
    }
}
```

The payoff: a diagnostic created deep inside a nested item already carries
its absolute span, the grammar context (`while parsing signature`) and help
text, and it propagates through `?` and combinators like any winnow error.
`ErrMode::Backtrack` and `ErrMode::Cut` keep their normal meaning:

* `backtrack(d)` — this branch does not apply; `alt`/`opt` may try another.
  Used sparingly (e.g. `looks_like_value` probing a range).
* `cut(d)` — a real syntax error; no alternative will be tried. Almost every
  error in the parser is a cut, because Nushell's grammar is decided by the
  first character of an item, not by trial and error.

```rust
use nu_winnow_parser::{parse, ErrorKind};

let err = parse("def foo [x:] { }").unwrap_err();
let d = err.primary();
assert!(matches!(d.kind, ErrorKind::Expected("type")));
assert_eq!(d.context.first().copied(), Some("signature"));
let text = d.render("def foo [x:] { }", Some("t.nu"));
assert!(text.contains("--> t.nu:1:12"));
assert!(text.contains("while parsing signature"));
```

`ParseError` (the type returned by `parse`) is a non-empty `Vec<Diagnostic>`
sorted by position; `render` prints each with a source excerpt and caret.

## The parser handle `St` and the shared state

```rust,ignore
#[derive(Clone, Copy)]
pub struct St<'s, 'a> {
    pub src: &'a str,                 // the whole source text
    shared: &'s RefCell<Shared>,      // comments, diagnostics, declared command names
}

pub struct Shared {
    config: ParseConfig,
    comments: Vec<Comment>,
    diagnostics: Vec<Diagnostic>,
    decl_scopes: Vec<CommandSet>,     // innermost scope last
}
```

`St` is `Copy`, so parser functions take it by value and closures capture it
freely; the `RefCell` gives interior mutability without threading `&mut`
through winnow closures. Methods you will use:

| Method | Purpose |
| --- | --- |
| `st.text(span)`, `st.tok(&token)` | Borrow source text |
| `st.lex_span(span, opts)` | Lex a region of the source (the standard way to re-lex an item's interior) |
| `st.comment(span)`, `st.comments_from(&tokens)` | Record comments found while parsing nested constructs |
| `st.error(d)` | Record a recovered error (block-level recovery only) |
| `st.checkpoint()` / `st.rollback(cp)` | Snapshot and undo recorded comments and diagnostics around a speculative parse |
| `st.is_known_command(name)`, `st.is_command_prefix(word)`, `st.is_declared_command(name)`, `st.declare_command(name)` | Command-name knowledge for multi-word heads |
| `st.push_scope()` / `st.pop_scope()` | Declaration scopes for closures, blocks and subexpressions |

### Speculation must be undone

Some code paths try one parse and fall back to another: `range()` parses each
bound with `value(Hint::Number)` and gives up if any bound fails, and
`looks_like_value` probes whether an item is a range. If the attempted parse
walked into a subexpression, it may have recorded comments (or, with block
recovery, diagnostics) that must not survive the fallback. The pattern is:

```rust,ignore
let cp = st.checkpoint();
match value(st, bound_span, Hint::Number) {
    Ok(e) => e,
    Err(_) => { st.rollback(cp); return None; }
}
```

Comments are also de-duplicated at the end of the parse (sorted and
`dedup`ed), which makes double collection harmless in the rare case a region
is parsed twice.

## Error recovery

Recovery happens in exactly one place: `parse_block_tokens` in
`src/parser/block.rs`. When a pipeline fails to parse it records the
diagnostic, resets the token stream to the pipeline's start, skips to the
next `Eol`, `;` or `Eof`, and emits a pipeline whose single element is
`ExprKind::Garbage` covering the skipped span. Because every block (closure
body, `if` body, subexpression, `let` value) goes through the same function,
an error inside a nested block does not fail the enclosing statement:

```rust
use nu_winnow_parser::{parse_lenient, ParseConfig, ast::ExprKind};

let src = "def f [] {\n  1 +\n  ls\n}\npwd";
let (ast, diagnostics) = parse_lenient(src, &ParseConfig::new());
assert_eq!(diagnostics.len(), 1);
let def = &ast.block.pipelines[0].elements[0].expr;
match &def.kind {
    ExprKind::Def(d) => {
        assert_eq!(d.body.pipelines.len(), 2);              // `1 +` became Garbage, `ls` parsed
        assert!(d.body.pipelines[0].elements[0].expr.is_garbage());
    }
    other => panic!("{other:?}"),
}
```

A corollary for contributors: **never speculatively parse a block** (try
block, fall back to something else) without a checkpoint, because the block's
errors are recorded, not returned. The match-arm body parser is the one place
that does this and it uses `checkpoint`/`rollback`.

`parse` returns `Err` if any diagnostic was recorded; `parse_lenient` returns
the partial tree together with the diagnostics.
