# 02 The lexer (`src/lexer.rs`)

The lexer turns a piece of text into a flat `Vec<Token>`. It is called many
times during a parse: once for the whole file, and again for the interior of
every list, record, block, closure, subexpression, signature, cell path and
match block, each time with options that suit that construct.

## Tokens

```rust,ignore
pub enum TokenKind {
    Item,                  // a word, literal, [list], {block}, (subexpr), "string", ...
    Comment,               // `# ...` to end of line
    Pipe,                  // `|`
    PipePipe,              // `||` (an error in a pipeline, closure params elsewhere)
    Semicolon,             // `;`
    Eol,                   // `\n`
    Assign(AssignOp),      // `=` `+=` `-=` `*=` `/=` `++=` standing alone
    Redirect(RedirectOp),  // `o>` `e>` `o+e>` `o>>` `e>|` ... standing alone
    Eof,                   // always the last token; its span is the end position
}

pub struct Token { pub kind: TokenKind, pub span: Span }
```

Two things are unusual compared with a classical lexer:

1. An `Item` can be very large. `[1 2 {a: (3 | 4)}]` is one item. So is
   `$"hello (1 + 1)"` or `foo"bar"`. The parser decides what to do with the
   item's text, and often lexes it again.
2. There is no separate keyword or operator token. `def`, `+`, `==` and
   `not-in` are all items; the parser classifies them by text and position.

The `Eof` token records where the text ended; the parser turns it into the
end position of its `Cursor`, so "expected X" errors have a position even
when the input ran out (see chapter 03).

## `LexOptions`

```rust,ignore
pub struct LexOptions {
    /// Extra bytes treated as whitespace. Including b'\n' suppresses Eol tokens.
    pub extra_whitespace: &'static [u8],
    /// Bytes that become single-character items when they start a token and
    /// terminate the item otherwise (`:` in records, `.` in cell paths).
    pub special: &'static [u8],
    /// Drop comments instead of emitting them.
    pub skip_comments: bool,
    /// Treat `<`/`>` as nesting brackets (type annotations such as list<int>).
    pub signature: bool,
}
```

The named presets map one-to-one onto the constructs that use them:

| Preset | Whitespace | Special | Used by |
| --- | --- | --- | --- |
| `BLOCK` | — | — | files, block bodies, closure bodies, `let` values |
| `SUBEXPRESSION` | `\n\r` | — | `( ... )`: newlines are whitespace, so a parenthesised pipeline may span lines |
| `LIST` | `\n\r,` | — | `[ ... ]` |
| `RECORD_KEY` / `RECORD_VALUE` | `\n\r,` | `:` / — | record keys (`a:1` splits) and values (`http://x` does not) |
| `CELL_PATH` | `\n\r` | `.?!` | `$x.a?.0!` |
| `SIGNATURE` | `\n\r` | `:=,` | `[a: int = 1, --flag(-f)]`, with `<>` nesting |
| `IO_TYPES` | `\n\r,` | — | `[int -> string, nothing -> nothing]`, with `<>` nesting |
| `MATCH` | ` \r\n,` | — | match arms; `|` still becomes `Pipe` for or-patterns |
| `BRACE_PROBE` | `\r\n\t` | `:` | the first two tokens of `{ ... }` to decide record/closure/block |
| `BINARY` | `,\r\n` | — | `0x[ff 00]` |
| `PATTERN_LIST` / `PATTERN_RECORD` | `\n\r,` | — / `:` | `match` list and record patterns |

A "special" byte has two effects, both copied from nu-parser: at the start of
an item it becomes its own one-byte item; anywhere else it ends the item. That
is why `a:1` lexes as `a`, `:`, `1` with `RECORD_KEY` and `$x.a` as `$x`,
`.`, `a` with `CELL_PATH`.

## The entry points

```rust
use nu_winnow_parser::lexer::{lex, lex_prefix, LexOptions, TokenKind};

let src = "ls -l | where size > 1kb # big\n";
let tokens = lex(src, 0, LexOptions::BLOCK).unwrap();
let kinds: Vec<_> = tokens.iter().map(|t| t.kind).collect();
assert_eq!(kinds, vec![
    TokenKind::Item, TokenKind::Item, TokenKind::Pipe,
    TokenKind::Item, TokenKind::Item, TokenKind::Item, TokenKind::Item,
    TokenKind::Comment, TokenKind::Eol, TokenKind::Eof,
]);
assert_eq!(tokens[6].text(src), "1kb");

// `base` makes spans absolute when lexing a slice of a larger source.
let inner = lex("a b", 10, LexOptions::BLOCK).unwrap();
assert_eq!(inner[1].span, nu_winnow_parser::Span::new(12, 13));

// Lex just the first N tokens (used to probe a `{ ... }` body).
let probe = lex_prefix("a: 1, b: 2", 0, LexOptions::BRACE_PROBE, 2).unwrap();
assert_eq!(probe.len(), 3); // two tokens plus Eof
assert_eq!(probe[1].text("a: 1, b: 2"), ":");
```

`lex_prefix_at` is the third entry point: it returns the tokens *and* how many
bytes were consumed, so a caller can continue lexing the same text with
different options. The record parser uses it to lex `key`, `:`, `value` with
`RECORD_KEY`, `RECORD_KEY`, `RECORD_VALUE` in turn.

## The top-level loop

`lex_prefix_at` is a loop: skip whitespace, then dispatch on the next
character with winnow's `dispatch!`:

```rust,ignore
fn token(i: &mut Input<'_>, opts: LexOptions) -> PResult<Option<Token>> {
    let start = pos(i);
    dispatch! {peek(any);
        '\n' => any.map(|_| Some(TokenKind::Eol)),
        '#'  => comment_body.map(move |_| if opts.skip_comments { None } else { Some(TokenKind::Comment) }),
        '|'  => preceded('|', opt('|')).map(|second| Some(if second.is_some() { TokenKind::PipePipe } else { TokenKind::Pipe })),
        ';'  => any.map(|_| Some(TokenKind::Semicolon)),
        _    => move |i: &mut Input<'_>| item(i, opts).map(Some),
    }
    .parse_next(i)
    .map(|kind| kind.map(|kind| Token { kind, span: span_from(i, start) }))
}
```

A `#` starts a comment only here, at token start. Inside an item, `#` is a
comment only when preceded by whitespace (so `foo#bar` is one word, while
`[1 # one\n 2]` has a comment inside the list item).

## The item scanner

`item()` is the heart of the lexer and a direct port of nu-parser's
`lex_item`. It walks the bytes of the remaining input with a small state
machine and stops at the first *terminator at depth zero*:

```rust,ignore
let is_terminator = |brackets: &[(Bracket, usize)], c: u8| {
    brackets.is_empty()
        && (matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'|' | b';')
            || opts.extra_whitespace.contains(&c)
            || opts.special.contains(&c))
};
```

State tracked while scanning:

* `brackets`: a stack of open `(`, `[`, `{` (and `<` in signature mode) with
  their positions. Inside brackets nothing terminates the item, and newlines,
  pipes and semicolons are just bytes.
* `quote`: the current string delimiter (`'`, `"` or `` ` ``) once one is
  seen. Backslash escapes only count inside `"`. A quote can start in the
  middle of an item (`foo"bar"` is one item; nu accepts it).
* `quote_is_interp` and `interp_level`: inside `$"..."` or `$'...'`, an
  unescaped `(` opens a subexpression in which quotes and parentheses nest
  independently of the outer string. `interp_subexpr_step` implements the same
  rules nu-parser uses, so the lexer and the interpolation parser agree on
  where the string ends.
* `in_comment`: inside brackets, `#` after whitespace starts a comment that
  runs to the end of the line.
* Raw strings `r#'...'#` are scanned by `raw_string_end`, which counts the
  hashes and finds the matching `'#...#`.
* Closing brackets go through `close_bracket`, which pops the matching
  opener or reports the mismatch.
* A `|` directly after a redirection prefix (`e>`, `o+e>`) is consumed into
  the item, giving the `e>|` tokens.

Errors from the scanner are *cut* errors (fatal for the current block): an
unclosed quote or bracket, a stray `)` or `}`, and the bash-isms `&&`, `2>`,
`2>&1`, `o>|` which get a `ShellSyntax` diagnostic with the Nushell spelling
in the help text. Note that a stray `]` at depth zero is *not* an error; nu
treats it as an ordinary character, and so does this crate.

Once the item's extent is known, `classify` turns the exact spellings of
assignment and redirection operators into their token kinds, and the input is
advanced with `i.next_slice(off)`.

`group_end(text)` reuses the same scanner to find the bracket that closes
the group a text starts with, so that `value.rs` can tell a subexpression
`(a)` from the bare interpolation `(a)/b/(c)` without a second state machine.

## Pipe continuation is not the lexer's job

A `|` at the start of a line continues the previous pipeline:

```text
ls
# a comment
| length
```

The lexer emits exactly what is there, `Item(ls) Eol Comment Eol Pipe
Item(length)`, and the block parser (chapter 04) looks ahead over newlines
and comments for the `|`. nu-parser does this in its lexer by rewriting the
token list; keeping the lexer context-free makes its output easier to reason
about and test, and the rule lives next to the other pipeline-layout rules.

## Where to look when changing the lexer

* New delimiter behaviour for a construct: add a `LexOptions` preset and use
  it from the value parser; do not special-case the scanner.
* New operator spelling that must stand alone (`o>`-like): `classify`.
* New quoting form: the quote handling in `item` and, if the parser must
  decode it, `strings::string_lit` / `literal::raw_string`.
* Anything else: write the change as a winnow parser and add a case to
  `token`'s `dispatch!`.

Tests for the lexer live at the bottom of `src/lexer.rs` and use the
`lex_debug` helper that returns `(kind, text)` pairs.
