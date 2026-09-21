# 05 Statements and expressions

Files: `src/parser/statement.rs`, `src/parser/expr.rs`.

The input to this layer is a `Cursor` over the items of one command
(`RawCommand::cursor()`). The output is one `Expr`.

## Dispatch order (`expr::parse_expression`)

```rust,ignore
pub fn parse_expression<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    // 1. Statement keywords parse their own `=` and `{}` arguments.
    if is_statement_keyword(first_text) { return statement::keyword_or_call(st, c); }
    // 2. `NAME=value` prefixes.
    let vars = env_shorthand_prefix(st, &mut c)?;
    // 3. Any assignment operator among the items -> assignment.
    let inner = if c.rest().iter().any(|t| matches!(t.kind, TokenKind::Assign(_))) {
        assignment(st, c.remaining())?
    // 4. A first item that looks like a value -> math expression.
    } else if looks_like_value(st.tok(first)) {
        math_expression(st, c.remaining(), false)?
    // 5. Otherwise a keyword expression (if, match, ...) or a call.
    } else {
        statement::keyword_or_call(st, c.remaining())?
    };
    /* wrap `inner` in EnvShorthand if `vars` is not empty */
}
```

This is nu-parser's `parse_expression` in the same order. Two details are
easy to miss:

* `looks_like_value` is nu's `is_math_expression_like`: true for `true`,
  `false`, `null`, `not`, `if`, `match`, items starting with `( { [ $ " ' -`
  or `r#`, and items whose text is a number, unit, datetime, binary or range.
  It is a pure function of the text (`value.rs`), so `-1 | math abs` is math,
  `"ls"` is a string not a call, and `1 + 1` at a pipeline head is
  arithmetic, all without parsing anything twice.
* Statement keywords (`def`, `let`, `mut`, `const`, `for`, `alias`, `module`,
  `use`, `export`, `export-env`, `extern`) are checked *before* the assignment
  test because `let x = 1` contains an `Assign` token.

## Keyword forms (`statement.rs`)

`keyword_or_call` matches the head text and calls one function per keyword;
anything else is a call. The match also attaches the grammar context
(`while parsing for`) to any error. Each keyword function follows the same
pattern: consume items with `expect_item`, delegate items to `value::value`
with the right `Hint` (or to `block_body` for a block), and finish with
`expect_end`.

```rust,ignore
fn for_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let kw = c.expect_item("for")?;
    let var_tok = c.expect_item("loop variable")?;               // `x`, `$x` or `x:` (typed)
    let (var, typed) = variable_declaration(st, &var_tok)?;
    let ty = match typed { true => Some(signature::parse_type(st, c.expect_item("type")?.span)?), false => None };
    let in_tok = c.expect_item("`in`")?;
    if st.tok(&in_tok) != "in" { return Err(cut(Diagnostic::new(ErrorKind::ExpectedKeyword("in"), in_tok.span))); }
    let iterable = value::value(st, c.expect_item("value to iterate")?.span, Hint::Any)?;
    let body_tok = c.expect_item("block")?;
    let body = block_item(st, &body_tok, "block")?;               // must start with `{`
    c.expect_end()?;
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
written out per keyword: the `if` condition is `c.slice(1..block_index)`
where `block_index` is the index before `else` or the last index. This is
the same result without needing signatures.

The keyword itself is not stored in the tree: `ExprKind::keyword()` names it
and `Expr::keyword_span()` locates it, since it is always the first word of
the expression's span (chapter 08).

## Math expressions (`expr::math_expression`)

Operands are single items parsed with `value(.., Hint::Any)`; operators are
items whose text is an operator spelling. The algorithm is nu-parser's
expression stack, kept identical so that precedence and associativity cannot
drift: operands and operators are pushed onto two stacks, and whenever an
operator binds no tighter than the previous one, the top of the stacks is
folded into a `BinaryOp` first:

```rust,ignore
let mut exprs = vec![lhs];
let mut ops = Vec::new();
let mut last_prec = u8::MAX;
while !c.at_end() {
    let op = operator(st, &c.expect_item("operator")?)?;
    let rhs = operand(st, &mut c)?;                          // `not`* value
    let prec = op.item.precedence();
    if !op.item.is_right_associative() && prec <= last_prec {
        while ops.last().is_some_and(|prev| prev.item.precedence() >= prec) {
            fold_top(st, &mut exprs, &mut ops, row)?;         // exprs: [.., a, b] ops: [.., op]  ->  exprs: [.., a op b]
        }
    }
    last_prec = prec;
    ops.push(op);
    exprs.push(rhs);
}
while !ops.is_empty() { fold_top(st, &mut exprs, &mut ops, row)?; }
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
absorbed to the end of the line) is parsed with `parse_block` and stored as
a `Block`, matching nu, where `$x = ls | length` assigns the pipeline's
result.

## Calls (`expr::parse_call`, `resolve_head`, `parse_args`)

`resolve_head` implements nu's longest-match rule: try the first *n* words
(up to five) joined with spaces against the known command set, longest first;
`str trim --left` becomes a call to `str trim` with one flag. Two performance
details: the single-word fast path never builds a string, and a multi-word
attempt only happens if the first word is a known *prefix* of some multi-word
command (`ParseConfig` and the declaration scopes both index prefixes).

`parse_args` classifies each remaining item with one `match`:

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
parenthesised segments (the `Segment` state machine), quoted segments stay
literal, parenthesised bare segments interpolate, and all-literal words
collapse to one string (`--query='q($x)'` keeps its parentheses;
`--out=(pwd)/x` interpolates).

`FOO=bar BAZ=$x cmd`: leading items of the form `NAME=value` with an
identifier `NAME` become `EnvShorthand`; the value is a `$` expression or a
strict string. A line consisting only of shorthand is an error.

## Cursor idioms you will reuse

```rust,ignore
let kw = c.expect_item("for")?;                 // next Item, or "expected for" at the right span
if c.at_end() { .. }                            // nothing left
c.expect_end()?;                                // ExtraTokens error otherwise
let rest: Option<Span> = c.rest_span();         // consume the rest, get its span
let cond = c.slice(1..block_idx);               // a sub-cursor over some of the items
let items = c.all();                            // the underlying slice, for index arithmetic
```
