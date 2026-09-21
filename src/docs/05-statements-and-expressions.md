# 05 Statements and expressions

Files: `src/parser/statement.rs`, `src/parser/expr.rs`.

The input to this layer is the item list of one command (`RawCommand::parts`,
ending with `Eof`). The output is one `Expr`.

## Dispatch order (`expr::parse_expression`)

```rust,ignore
pub fn parse_expression<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    // 1. Statement keywords parse their own `=` and `{}` arguments.
    if is_statement_keyword(first_text) { return statement::keyword_or_call(st, tokens); }
    // 2. `NAME=value` prefixes.
    let (vars, consumed) = env_shorthand_prefix(st, items)?;
    // 3. Any assignment operator among the items -> assignment.
    if items.iter().any(|t| matches!(t.kind, TokenKind::Assign(_))) { return assignment(st, tokens); }
    // 4. A first item that looks like a value -> math expression.
    if value::looks_like_value(st, first.span) { return math_expression(st, tokens, false); }
    // 5. Otherwise a keyword expression (if, match, ...) or a call.
    statement::keyword_or_call(st, tokens)
}
```

This is nu-parser's `parse_expression` in the same order. Two details are
easy to miss:

* `looks_like_value` is nu's `is_math_expression_like`: true for `true`,
  `false`, `null`, `not`, `if`, `match`, items starting with `( { [ $ " ' -`
  or `r#`, and items that parse as a number, unit, datetime, binary or range.
  So `-1 | math abs` is math, `"ls"` is a string not a call, and `1 + 1` at a
  pipeline head is arithmetic.
* Statement keywords (`def`, `let`, `mut`, `const`, `for`, `alias`, `module`,
  `use`, `export`, `export-env`, `extern`) are checked *before* the assignment
  test because `let x = 1` contains an `Assign` token.

## Keyword forms (`statement.rs`)

`keyword_or_call` matches the head text and calls one function per keyword;
anything else is a call. Each keyword function follows the same pattern:
wrap the items in a `Toks` stream, consume with `expect_item`, delegate items
to `value::value` with the right `Hint`, and finish with `expect_end`.

```rust,ignore
fn for_stmt<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let mut i = toks(st, tokens);
    let kw = expect_item(&mut i, "for")?;
    let var_tok = expect_item(&mut i, "loop variable")?;          // `x`, `$x` or `x:` (typed)
    /* ... optional type ... */
    let in_tok = expect_item(&mut i, "`in`")?;
    if st.tok(&in_tok) != "in" { return Err(cut(Diagnostic::new(ErrorKind::ExpectedKeyword("in"), in_tok.span))); }
    let iter_tok = expect_item(&mut i, "value to iterate")?;
    let iterable = value::value(st, iter_tok.span, Hint::Any)?;
    let body_tok = expect_item(&mut i, "block")?;
    let body = block_item(st, &body_tok, "block")?;               // must start with `{`
    expect_end(&mut i)?;
    Ok(Expr::new(ExprKind::For(For { .. }), kw.span.merge(body_tok.span)))
}
```

The table below is the contract each keyword implements. "items" are
whitespace-delimited items; "rest" means everything to the end of the line
(because of `=` absorption, this can contain pipes).

| Keyword | Shape | Notes |
| --- | --- | --- |
| `def` | `def [--env] [--wrapped] NAME SIG[:] [TYPES] BODY` | `NAME` is a bare or quoted string; `SIG` is a `[...]` or `(...)` item, a trailing `:` on it (or a separate `:` item) introduces `TYPES`, which are re-lexed together in signature mode; `BODY` is the last item and must start with `{`. Parser keywords are rejected as names. |
| `extern` | `extern NAME SIG[:] [TYPES]` | Same as `def` without a body. |
| `let`/`mut`/`const` | `KW NAME[:] [TYPE...] [= rest]` | `NAME` may start with `$`; the type tokens are re-lexed together; the value is the rest, parsed as a *block* (a pipeline). `let x` without `=` is valid. |
| `for` | `for VAR[:] [TYPE] in ITEM BODY` | |
| `alias` | `alias NAME = rest` | The rest is parsed as one expression; a pipe in it becomes a word (nu does this too). |
| `module` | `module NAME [BODY]` | |
| `use` | `use MODULE [MEMBER...]` | Members are names, `*`, or a `[a b]` list; only the last may be `*` or a list. `use null` is allowed. |
| `export` | `export def|extern|alias|use|module|const ...` | Wraps the inner statement. |
| `export-env` | `export-env BODY` | |
| `if` | `if COND... BLOCK [else BLOCK|EXPR]` | The condition is every item before the block; the block is the item before `else` or the last item. The else branch is a block or a whole expression (so `else if ...` recurses). |
| `match` | `match ITEM BLOCK` | See chapter 07 for the block. |
| `while` | `while COND... BLOCK` | |
| `loop` | `loop BLOCK` | |
| `try` | `try BLOCK [catch|finally CLOSURE]{0,2}` | Handlers are kept in source order; nu allows two of either kind. |
| `return` | `return [ITEM]` | One item only, as in nu. |
| `break`/`continue` | keyword alone | |
| `where` | `where {closure}` or `where COND...` | A row condition: `math_expression(.., row = true)`. |

How the extent of a multi-item argument is found deserves a note. nu-parser
computes it from the signature ("the condition of `if` gets all spans up to
the ones needed by the remaining required positionals"). Here that rule is
written out per keyword: the `if` condition is `items[1..block_index]` where
`block_index` is the index before `else` or the last index. This is the same
result without needing signatures.

## Math expressions (`expr::math_expression`)

Operands are single items parsed with `value(.., Hint::Any)`; operators are
items whose text is an operator spelling. The algorithm is nu-parser's
expression stack, kept identical so that precedence and associativity cannot
drift:

```rust,ignore
let mut stack = vec![Stacked::Expr(lhs)];
let mut last_prec = u8::MAX;
while !at_end(&i) {
    let op = operator(st, &expect_item(&mut i, "operator")?)?;
    let rhs = operand(&mut i)?;                                // `not`* value
    let left_assoc = !op.item.is_right_associative() && op.item.precedence() <= last_prec;
    while left_assoc && stack.len() > 1 {
        // pop rhs2, op2; if op2 binds looser than op, push them back and stop
        // else pop lhs2 and push BinaryOp(lhs2, op2, rhs2)
    }
    stack.push(Stacked::Op(op));
    stack.push(Stacked::Expr(rhs));
    last_prec = op.item.precedence();
}
// fold the remainder right to left
```

Precedence lives on `ast::Operator::precedence` (100 for `**` down to 40 for
`or`); `**` is the only right-associative operator. `not` is a prefix that may
repeat. `if` and `match` are allowed as operands and consume the rest of the
items (`1 + if $x { 2 } else { 3 }`). Unknown operators get targeted help
(`^` → use `**`, `%` → `mod`, `contains` → `has`, ...).

```rust
use nu_winnow_parser::{parse, ast::{ExprKind, Operator, Math}};

let ast = parse("1 + 2 * 3 ** 2 ** 1").unwrap();
let top = match &ast.block.pipelines[0].elements[0].expr.kind {
    ExprKind::BinaryOp(b) => b,
    other => panic!("{other:?}"),
};
assert_eq!(top.op.item, Operator::Math(Math::Add));         // 1 + (2 * (3 ** (2 ** 1)))
```

### Row conditions

With `row = true` (the argument of `where`), any *string* operand on the left
of an operator, or a lone operand, is re-parsed as a cell path on the implicit
`$it`: `where size > 1kb` yields `FullCellPath { head: Var it (empty span),
implicit_head: true, members: [size] }`. Only left operands are expanded, as
in nu-parser's `expand_to_cell_path`.

## Assignments

`assignment` splits the items at the first `Assign` token. The left side must
parse to a `Var` or a `FullCellPath` on a `Var`; the right side (everything
absorbed to the end of the line) is parsed with `parse_block_tokens` and
stored as a `Block`, matching nu, where `$x = ls | length` assigns the
pipeline's result.

## Calls (`expr::parse_call`, `resolve_head`, `parse_args`)

`resolve_head` implements nu's longest-match rule: try the first *n* words
(up to five) joined with spaces against the known command set, longest first;
`str trim --left` becomes a call to `str trim` with one flag. Two performance
details: the single-word fast path never builds a string, and a multi-word
attempt only happens if the first word is a known *prefix* of some multi-word
command (`ParseConfig` and the declaration scopes both index prefixes).

`parse_args` classifies each remaining item:

| Item | Result |
| --- | --- |
| `--` | `Arg::EndOfOptions` |
| `--name`, `--name=value` | `Arg::Flag { long: true, value }` (value only for the `=` form) |
| `-x`, `-abc` | `Arg::Flag { long: false, name: "abc" }` unless it looks like a negative number (`-1`, `-.5`) |
| `...$x`, `...[..]`, `...(..)`, `...{..}` | `Arg::Spread` |
| anything else | `Arg::Positional(value(.., Hint::Any))` |

Whether `--name value` binds `value` to the flag is a signature question and
is left to the consumer; the bridge in `tools/` shows how (chapter 09).

## External calls and environment shorthand

`^cmd args`: the head after `^` is a string, `$var` or `(subexpr)`; each
argument is `$..`/`(..)`/`[..]`/`{..}` parsed as a value, `...x` as a spread,
and everything else as an **external string**. `external_string` reproduces
nu-parser's segmenting: a word is split into bare, quoted, backtick and
parenthesised segments, quoted segments stay literal, parenthesised bare
segments interpolate, and all-literal words collapse to one string
(`--query='q($x)'` keeps its parentheses; `--out=(pwd)/x` interpolates).

`FOO=bar BAZ=$x cmd`: leading items of the form `NAME=value` with an
identifier `NAME` become `EnvShorthand`; the value is a `$` expression or a
strict string. A line consisting only of shorthand is an error.

## Token-stream helpers you will reuse

```rust,ignore
let mut i = toks(st, tokens);                 // wrap a token slice (must end with Eof)
let tok = expect_item(&mut i, "command name")?; // next Item, or "expected command name" at the right span
if at_end(&i) { .. }                          // only Eof remains
expect_end(&mut i)?;                          // ExtraTokens error otherwise
let rest: Option<Span> = rest_span(&mut i);   // consume the rest, get its span
let items_ = items(tokens);                   // slice without the Eof
let sub = with_eof(&items_[a..b]);            // a sub-slice as a fresh token list
```
