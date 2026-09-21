# 04 Blocks and pipelines (`src/parser/block.rs`)

This layer corresponds to nu-parser's `lite_parser.rs` plus the top of its
`parse_block`. It receives a cursor over lexed tokens and produces
`Block { pipelines }`, deciding where statements start and end and which
tokens belong to which command. It never looks inside an item.

## Entry point

```rust,ignore
pub fn parse_block<'a>(st: St<'_, 'a>, c: Cursor<'_>, span: Span) -> Block<'a>
```

Every block in the language goes through this function: the file, closure and
block bodies, subexpressions, `let`/assignment values, `match` bodies written
as blocks. It:

1. runs `predeclare` (below),
2. loops over tokens, handling `Eol`, `;` and comments itself and calling
   `pipeline` for anything else,
3. recovers from errors per pipeline (chapter 03).

The block's `span` is the span of its *contents*: for a `{ ... }` body that is
the text between the braces, which is what a formatter needs to re-indent.

## Comments

Comments are attached while grouping, following nu-parser's rules:

* A comment on its own line is a **leading comment** of the next pipeline
  (`Pipeline::leading_comments`), unless a blank line separates them, in
  which case it is dropped from the attachment (it stays in `Ast::comments`).
  For a `def`, leading comments are its documentation.
* A comment after the last token of a line is a **trailing comment** of that
  pipeline (`Pipeline::trailing_comments`), as are comments between the
  elements of a multi-line pipeline.
* Every comment, attached or not, is recorded in `Ast::comments` (sorted, in
  source order). Nested constructs record theirs through `st.comment`.

```rust
use nu_winnow_parser::parse;

let src = "# doc one\n# doc two\ndef foo [] { }\n\n# detached\n\nls # trailing\n";
let ast = parse(src).unwrap();
let def = &ast.block.pipelines[0];
assert_eq!(def.leading_comments.len(), 2);
assert_eq!(def.leading_comments[0].body(src), "doc one");
let ls = &ast.block.pipelines[1];
assert!(ls.leading_comments.is_empty());
assert_eq!(ls.trailing_comments.len(), 1);
assert_eq!(ast.comments.len(), 4);
```

## Pipelines

`pipeline()` parses `command (| command)*`:

```rust,ignore
fn pipeline<'a>(st: St<'_, 'a>, c: &mut Cursor<'_>, leading_comments: Vec<Comment>) -> PResult<Pipeline<'a>> {
    let mut pipe: Option<Span> = None;
    skip_pipe_continuation(st, c, &mut pipe, &mut trailing_comments);   // `( | str join)` is allowed
    loop {
        let raw = raw_command(st, c)?;
        let (expr, redirection) = statement::parse_command(st, &raw)?;
        elements.push(PipelineElement { span, pipe, expr, redirection });
        // A `|` right after the command, or on a later line after only
        // newlines and comments, continues the pipeline.
        let Some(next_pipe) = raw.pipe_after.or_else(|| take_pipe_ahead(st, c, &mut trailing_comments)) else { break };
        pipe = Some(next_pipe);
        skip_pipe_continuation(st, c, &mut pipe, &mut trailing_comments);
    }
    let terminator = /* a following `;` */;
    Ok(Pipeline { span, elements, leading_comments, trailing_comments, terminator })
}
```

Two helpers handle the layouts Nushell allows around a pipe:

* `skip_pipe_continuation` consumes newlines, comments and repeated pipes
  *after* a `|`, which covers `a |\n b`, `a | # c\n b` and nu's tolerance of
  `a | | b`.
* `pipe_ahead` / `take_pipe_ahead` look *forward* from the end of a command:
  if the next token that is not a newline or a comment is a `|`, the pipeline
  continues on that line and the comments in between become trailing
  comments. This is the leading-pipe style:

  ```text
  ls
  # a comment
  | length
  ```

A `|` followed by end of input or `;` is an error ("expected command after
`|`").

## Commands: `RawCommand`

`raw_command()` collects one command's tokens without interpreting them:

```rust,ignore
pub struct RawCommand {
    pub parts: Vec<Token>,                                     // the items
    pub end: usize,                                            // byte offset just past the last part
    pub attributes: Vec<Vec<Token>>,                           // preceding @attribute lines
    pub redirections: Vec<(Spanned<RedirectOp>, Option<Token>)>, // `o> file`, `e>|`
    pub pipe_after: Option<Span>,                              // the `|` that ended the command
    pub comments: Vec<Comment>,
}
```

`raw.cursor()` gives the statement parsers a `Cursor` over `parts` that ends
at `end`, so "expected block" after `if $x` has a position.

The rules encoded in its loop:

* **Items** are pushed to `parts`.
* **`Assign` token** (`=`, `+=`, ...): switch to *absorbing* mode. Everything
  up to the end of the line, including pipes, redirections and comments, is
  pushed into `parts`. This is why `let x = ls | length` and `$x = ls |
  length` are single commands whose right-hand side is a whole pipeline. A
  `|` at the end of the line, or a `|` at the start of the next line (after
  any comment lines), continues absorption.
* **`Redirect` token**: a file redirection takes the next item as its target;
  a pipe redirection (`e>|`, `o+e>|`) ends the command and acts as the pipe to
  the next element. A redirection with no command before it is an error.
* **`Pipe`** ends the command. **`PipePipe`** is a `ShellSyntax` error (use
  `or`).
* **`Eol`**, **`;`**, **`Eof`** end the command (they are not consumed here).
* **Attribute lines** (`attribute_lines`): while the next item starts with
  `@`, the items up to the end of the line become one attribute; blank and
  comment lines between attributes and the definition are allowed.

## Predeclaration

Nushell resolves command names by longest match against known declarations,
including commands defined *later* in the same block. `predeclare` scans the
tokens for `def`, `export def`, `extern` and `alias` at line starts and
registers their names in the current scope before any statement is parsed:

```rust
use nu_winnow_parser::{parse, ast::ExprKind};

let src = "my cmd 1\ndef \"my cmd\" [x] { $x }";
let ast = parse(src).unwrap();
match &ast.block.pipelines[0].elements[0].expr.kind {
    ExprKind::Call(c) => {
        assert_eq!(c.head.name, "my cmd");   // resolved although defined below
        assert_eq!(c.args.len(), 1);
    }
    other => panic!("{other:?}"),
}
```

Scopes are pushed and popped by the constructs that create them (closures,
blocks, subexpressions, module bodies) in `src/parser/value.rs` and
`src/parser/statement.rs`.

## What this layer does not do

It does not know keywords, values or operators. All of that starts in
`statement::parse_command`, which receives a `RawCommand` and returns the
element's expression and redirection. Keep this separation: grouping rules
here are the ones nu-parser applies before any signature is known, and they
must stay independent of what the items mean.
