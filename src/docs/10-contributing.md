# 10 Contributing: recipes and pitfalls

## Before you change the grammar

1. Find the reference behaviour. `nu -n -c '...'` on the local binary answers
   most questions in seconds; `crates/nu-parser/src/*.rs` in the Nushell
   repository answers the rest. Write down what nu does, including the error
   cases.
2. Decide the layer: lexer (item boundaries), block (statement boundaries),
   statement (keyword shapes), expr (operators, calls), value (what one item
   means), literal (text of one literal), signature/pattern.
3. Add the test in `tests/syntax.rs` first; it should fail.
4. Implement, then run `cargo test`, `cargo clippy --all-targets
   --all-features`, `cargo fmt`, and the comparison scripts if the lexer or
   `value.rs` changed.

## Recipe: add a binary operator

1. `src/ast/mod.rs`: add the variant to `Math`, `Comparison`, `Boolean` or
   `Bits`; give it a `precedence` (copy nu-protocol's table), an `as_str`, and
   a `from_spelling` arm (several spellings may map to one operator, like
   `=~` and `like`).
2. `src/parser/expr.rs::operator`: nothing to do unless it needs a "did you
   mean" hint for a common misspelling; add that to the `help` match.
3. `src/flatten.rs`: `Boolean` operators map to `FlatShape::Boolean`, others
   to `Operator`; check the match.
4. Tests: `all_operators_parse` in `tests/syntax.rs` iterates the spellings;
   add yours, and a precedence assertion if it is not obvious.

## Recipe: add a keyword statement

Suppose Nushell gains `unless COND { }`.

1. `src/ast/mod.rs`: add `pub struct Unless<'a> { condition: Box<Expr<'a>>,
   body: Block<'a> }` and `ExprKind::Unless(Unless<'a>)`, and add
   `ExprKind::Unless(_) => "unless"` to `ExprKind::keyword` so consumers can
   find the keyword's span.
2. `src/parser/statement.rs`: write `unless_stmt` modelled on `while_stmt`
   (condition = items up to the last, body = last item via `block_item`), and
   add `"unless" => ("unless", unless_stmt(st, c))` to `keyword_or_call`. If the keyword must only appear at a pipeline head
   (like `def`), add it to `is_statement_keyword`; if it can be an operand
   (like `if`), add it to `looks_like_value`'s keyword list in `value.rs`
   and to `math_expression`'s `"if" | "match"` check in `expr.rs`. If a
   `def` may not use the name, add it to `is_parser_keyword`. Every keyword
   is a command with a fixed signature in nu, so at the start of each
   positional call `boundary_help` (it takes `--help`/`-h`, the first `--`
   and refuses other flags), fetch the item with `item_or_help` (a missing
   positional is forgiven after `--help`) and finish with `done`, which
   turns the statement into the ordinary call nu makes of `kw --help`;
   `end_of_options` and `help_call` are the helpers behind them. Text that
   nu accepts and never looks at goes to `st.ignore(span)` rather than into
   the tree.
3. `src/ast/visit.rs`: descend into the condition and body in `walk_expr`.
4. `src/flatten.rs`: the keyword shape is pushed for you from
   `keyword_span()`; visit the condition and call `block_braces` for the body.
5. `src/pretty.rs`: print it.
6. `examples/nufmt/format.rs`: emit it (the formatter matches on `ExprKind`
   and has a wildcard fallback that copies the source text, so this can be
   done later, but the round-trip test will show the raw text until then).
7. Tests in `tests/syntax.rs`, and a line in `tests/corpus/kitchen_sink.nu`
   if nu accepts it.

## Recipe: add a literal form

1. `src/parser/literal.rs`: a recogniser over `&str` returning `Option` (or
   a parser over `Input` if it has structure worth expressing with winnow,
   like `is_datetime`).
2. `src/parser/value.rs::any_value`: insert the attempt at the right place in
   nu's order (binary, range, filesize, duration, datetime, int, float,
   string). Also update `looks_like_value` if the literal can start a math
   expression.
3. Add an `ExprKind` variant if needed, then visitor, flatten, pretty.
4. Unit tests in `literal.rs`, behaviour tests in `tests/syntax.rs`.

## Recipe: change how a construct is lexed

Add a `LexOptions` preset rather than modifying the scanner. If the scanner
itself must change (a new bracket kind, a new quoting form), change `item()`
and `interp_subexpr_step` together and check nu-parser's `lex_item`, because
the lexer and the interpolation parser must agree on where a string ends.

## Debugging

```text
cargo run --example parse -- file.nu            # tree with spans
cargo run --example parse -- --flat file.nu     # what flatten sees
echo 'snippet' | cargo run --example parse      # from stdin
cargo run --release --example parse -- --check ~/src/nu_scripts   # find files that fail
nu -n -c 'ast --flatten "snippet"'              # what nu makes of it
```

A failing corpus file prints a rendered diagnostic with the line and a caret;
compare with `nu-check --debug file.nu`.

## Pitfalls

* **Give a cursor its end.** `Cursor::new(tokens, end)` needs the byte offset
  after the last token so that "expected X" at the end of the input has a
  position; use `c.slice(a..b)` or `raw.cursor()` rather than building one by
  hand.
* **Spans are absolute.** When you slice an item to parse a part of it, pass
  the absolute start (`Span::new(span.start + k, ...)`, `lex(text, base, ..)`).
* **Cut, don't backtrack.** Return `cut(Diagnostic::...)` for real errors; a
  backtrack error inside `?` becomes a confusing "expected valid syntax".
* **Block recovery records errors.** A block that fails to parse still
  returns `Ok` with its errors recorded, so never parse a block to find out
  whether an item is one; decide from the text first (`value::brace_shape`,
  which classifies a `{...}` as `BraceShape::{Empty, ClosureParams, Record,
  Spread, Other}` the way nu's `parse_brace_expr` probes it, `is_range_syntax`
  and `looks_like_value` are the existing examples).
* **Comments are recorded where they are lexed.** If you re-lex an interior
  with `skip_comments: false`, call `st.comments_from(&tokens)` (or handle
  `Comment` tokens) so they are kept; if you lex the same text twice, the
  end-of-parse `dedup` protects you, but avoid it.
* **Match nu's leniencies and strictnesses.** Do not "fix" `[1 | 2]`, `alias
  x = a | b`, `$x.a.` or `"a"b"c"`; they are accepted by nu. Do not accept
  `"abc"def`, `&&`, `def if [] {}`; nu rejects them.
* **`ExprKind` is `#[non_exhaustive]`.** Inside the crate `match` must be
  exhaustive; outside (examples, tools) a wildcard arm is required.
* **Keep `ast::visit`, `flatten`, `pretty` and the formatter in step** with
  any node change; clippy will not tell you about a missing descent.
* **Performance.** The hot path is `value` on bare words: every argument goes
  through the literal attempts, and `looks_like_value` on every command head.
  Keep those attempts allocation-free on failure (`literal::filesize` checks the first bytes before uppercasing;
  `resolve_head` only joins words when the first word is a known prefix).
  `cargo bench` and the `--check` example over `nu_scripts` are the yardsticks.
