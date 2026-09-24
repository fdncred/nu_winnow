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
2. runs nu's `last_non_comment_token` check once over the whole block: if the
   last token that is not part of a trailing run of comment lines is a `|`,
   the block has a pipeline with no end and "command after `|`" is reported
   at that pipe. This is what makes `ls |`, `ls | # c` and `ls |\n# c` (no
   final newline) errors while `ls |\n` and `ls |\n# c\n` are not, and it
   fires whichever command absorbed the pipe (`alias x = ls |` too),
3. loops over tokens, handling `Eol`, `;` and comments itself and calling
   `pipeline` for anything else,
4. recovers from errors per pipeline (chapter 03).

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

`pipeline()` parses `command (| command)*` in two passes, because a command
is parsed differently when it is one element of a longer pipeline (chapter
05, `Position`): first the lite pass collects every command's tokens, then
each one is parsed.

```rust,ignore
fn pipeline<'a>(st: St<'_, 'a>, c: &mut Cursor<'_>, leading_comments: Vec<Comment>)
    -> PResult<(Option<Pipeline<'a>>, Option<Span>)>
{
    let mut raws: Vec<(Option<Span>, RawCommand)> = Vec::new();   // (the `|` before it, the command)
    let mut pipe: Option<Span> = None;
    let mut dangling = None;
    'commands: loop {
        // A pipeline may start with `|` (`( | str join)`) and `a | | b` is `a | b`.
        while let Some(tok) = c.peek().filter(|t| t.kind == TokenKind::Pipe) {
            c.next();
            pipe = Some(tok.span);
            if let AfterPipe::Dangling = after_pipe(st, c, tok.span, &mut trailing_comments)? {
                dangling = pipe.take();
                break 'commands;
            }
        }
        if pipe.is_none() && !raws.is_empty() {
            // After a command the pipeline goes on only through a `|` on a later line.
            if !take_pipe_ahead(st, c, &mut trailing_comments) { break; }
            continue;
        }
        let raw = raw_command(st, c, pipe.is_none())?;                 // attributes only before the first command
        raws.push((pipe.take(), raw));
        if let Some(span) = raws.last().unwrap().1.pipe_after {        // an `e>|` ended the command
            pipe = Some(span);
            if let AfterPipe::Dangling = after_pipe(st, c, span, &mut trailing_comments)? {
                dangling = pipe.take();
                break;
            }
        }
    }
    if raws.is_empty() { return Ok((None, dangling)); }             // only pipes: nu drops the empty command
    let single = raws.len() == 1;
    for (pipe, raw) in &raws {
        let (expr, redirection) = statement::parse_command(st, raw, !single)?;
        elements.push(PipelineElement { span, pipe: *pipe, expr, redirection });
    }
    let terminator = /* a following `;` */;
    Ok((Some(Pipeline { span, elements, leading_comments, trailing_comments, terminator }), dangling))
}
```

The rules for newlines around a `|` are nu's lite parser's exactly, and they
are asymmetric:

* `after_pipe` runs after a `|` has been consumed. It takes comments on the
  same line, then *one* end of line and any number of comment lines
  (`Eol (Comment Eol)*`), and reports what comes next: `AfterPipe::Command`
  when a command follows, `AfterPipe::Dangling` when a blank line or the end
  of the block does. A dangling `|` closes the pipeline and is dropped
  silently, as nu drops it: `a |\n\n b` is two pipelines. A `;` right after
  the continuation is "command after `|`".
* `pipe_ahead` / `take_pipe_ahead` look *forward* from the end of a command:
  the pipeline continues only through exactly `Eol (Comment Eol)* Pipe`, that
  is one end of line, comment lines each on their own line, and the pipe. A
  blank line in between closes the pipeline instead (`a\n\n| b` is two
  pipelines, the second starting with a dangling pipe that yields nothing).
  The comments taken this way become trailing comments. This is the
  leading-pipe style:

  ```text
  ls
  # a comment
  | length
  ```

`pipeline()` returns the span of a dangling `|` so that `parse_block` can
refuse a `;` that follows it (nu's lexer refuses `ls |\n\n; x`), and `None`
for the pipeline when there was no command at all (a lone `|` before a blank
line). A `|` that only comments follow at the very end of a block is reported
once, by `parse_block` (above), not here.

```rust
use nu_winnow_parser::parse;

let two = parse("[1 2 3]\n\n| length\n").unwrap();     // a blank line closes the pipeline
assert_eq!(two.block.pipelines.len(), 2);
let one = parse("[1 2 3]\n# c\n| length\n").unwrap();    // a comment line does not
assert_eq!(one.block.pipelines.len(), 1);
assert!(parse("ls |\n# c").is_err());                     // no command after the pipe
assert!(parse("ls |\n# c\n").is_ok());                    // the final newline makes it a dangling pipe
```

## Commands: `RawCommand`

`raw_command(st, c, first)` collects one command's tokens without
interpreting them; `first` is set for the first command of a pipeline, the
only place attribute lines can precede it:

```rust,ignore
pub struct RawCommand {
    pub parts: Vec<Token>,                                     // the items
    pub end: usize,                                            // byte offset just past the last part
    pub attributes: Vec<Vec<Token>>,                           // preceding @attribute lines
    pub redirections: Vec<(Spanned<RedirectOp>, Option<Token>)>, // `o> file`, `e>|`
    pub pipe_after: Option<Span>,                              // the `e>|` that ended the command
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
* **`Pipe`** ends the command and is left for `pipeline()` to consume.
  **`PipePipe`** is a `ShellSyntax` error (use `or`).
* **`Eol`**, **`;`**, **`Eof`** end the command (they are not consumed here).
* **Attribute lines** (`attribute_lines`, first command only): while the
  next item starts with `@`, every token up to the end of the line or a `;`
  becomes an item word of one attribute, pipes and redirections included
  (`@search-terms a | b` hands `|` and `b` to the attribute, as nu does; an
  `@foo` after a `|` is an ordinary command head). The next attribute or the
  definition must start on the very next line; a blank or comment-only line
  in between is an error, as in nu (a trailing comment on the attribute line
  is fine).

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

`predeclare` also carries nu's duplicate check: a `def` or `extern` whose
name is followed by a signature item (`[` or `(`) is recorded, and the same
name recorded twice in one block is "duplicate command definition within a
block". An `alias` is declared for head resolution but not counted, so `def
foo` plus `alias foo` is fine, and so is the same name in a nested block.

Scopes are pushed and popped by the constructs that create them (closures,
blocks, subexpressions, module bodies) in `src/parser/value.rs` and
`src/parser/statement.rs`.

## What this layer does not do

It does not know keywords, values or operators. All of that starts in
`statement::parse_command`, which receives a `RawCommand` and whether it is
one element of a longer pipeline, and returns the element's expression and
redirection. Keep this separation: grouping rules
here are the ones nu-parser applies before any signature is known, and they
must stay independent of what the items mean.
