# 03 Streams, cursors, errors and shared state

Files: `src/input.rs`, `src/parser/cursor.rs`, `src/error.rs`, `src/parser/mod.rs`.

## One winnow stream, one cursor

winnow parsers are functions `fn(&mut Stream) -> Result<Output, ErrMode<E>>`.
The crate uses winnow for the *character level*: the lexer and the literal
parsers work on `Input`:

```rust,ignore
/// Character-level stream with absolute positions.
pub type Input<'a> = Stateful<LocatingSlice<&'a str>, Base>;
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

The *token level* does not need combinators: a statement is a short list of
items that is walked once, left to right. That is done with a plain
`Cursor` (`src/parser/cursor.rs`), a slice of tokens, a position, and the
byte offset where the slice ends:

```rust,ignore
#[derive(Clone, Copy)]
pub struct Cursor<'t> {
    tokens: &'t [Token],
    pos: usize,
    end: usize,      // byte offset just past the last token, for errors at the end
}
```

Its methods are the vocabulary of every statement parser:

| Method | Purpose |
| --- | --- |
| `peek()`, `next()`, `at_end()`, `rest()` | Walk the tokens |
| `expect_item("what")` | The next `Item`, or `expected what` at the next token (or at `end` when the input ran out) |
| `expect_end()` | `ExtraTokens` unless everything was consumed |
| `here()`, `end_span()` | The span of the next token / the empty span at the end |
| `slice(a..b)`, `remaining()` | An independent cursor over part of the tokens, ending where the next token starts |
| `position()`, `reset(pos)` | Save and restore a position (used by the match-guard parser to skip to `=>`) |
| `rest_span()` | Consume what is left and return its span |
| `Cursor::from_lexed(&tokens)` | A cursor over lexer output, whose last token is `Eof` |

Because a cursor knows where its slice ends, "expected block" after `if $x`
points just past `$x` even though there is no token there. The `Eof` token
the lexer produces is only used to seed that end position; parsers never see
it.

## `Diagnostic` is the winnow error type

Instead of `ContextError`, every parser returns `PResult<T> = ModalResult<T,
Diagnostic>`. `Diagnostic` implements winnow's traits for `Input`
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
  Only the character-level parsers use it.
* `cut(d)` — a real syntax error; no alternative will be tried. Every error
  in the token-level parsers is a cut, because Nushell's grammar is decided
  by the first character of an item, not by trial and error.

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
through every function. Methods you will use:

| Method | Purpose |
| --- | --- |
| `st.text(span)`, `st.tok(&token)` | Borrow source text |
| `st.lex_span(span, opts)` | Lex a region of the source (the standard way to re-lex an item's interior) |
| `st.comment(span)`, `st.comments_from(&tokens)` | Record comments found while parsing nested constructs |
| `st.error(d)` | Record a recovered error (block-level recovery only) |
| `st.is_known_command(name)`, `st.is_command_prefix(word)`, `st.is_declared_command(name)`, `st.declare_command(name)` | Command-name knowledge for multi-word heads |
| `st.push_scope()` / `st.pop_scope()` | Declaration scopes for closures, blocks and subexpressions |

### No speculation

Nothing in the parser tries one parse and falls back to another. Every
decision that nu-parser makes by "try it and see" is made here by looking at
the text first: `cellpath::is_range_syntax` decides whether an item is a
range before any bound is parsed, `value::looks_like_value` decides whether a
command head starts a math expression, and the `{ ... }` probe (chapter 06)
decides record, closure or block from two tokens. The consequence is that
comments and diagnostics can be recorded as soon as they are seen, with no
snapshot to undo, and an error is always reported for the construct the user
wrote (a bad range bound says "expected number", not "unknown command").

Comments are de-duplicated at the end of the parse (sorted and `dedup`ed),
so the rare region that is lexed twice, such as a record key probed and then
parsed, is harmless.

## Error recovery

Recovery happens in exactly one place: `parse_block` in
`src/parser/block.rs`. When a pipeline fails to parse it records the
diagnostic, resets the cursor to the pipeline's start, skips to the next
`Eol` or `;`, and emits a pipeline whose single element is
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

A corollary for contributors: a block that fails to parse still returns
`Ok`, with its errors recorded in `Shared`. So decide *before* parsing a
block whether the item is a block; never parse one to find out.

`parse` returns `Err` if any diagnostic was recorded; `parse_lenient` returns
the partial tree together with the diagnostics.
