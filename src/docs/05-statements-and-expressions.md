# 05 Statements and expressions

Files: `src/parser/statement.rs`, `src/parser/expr.rs`.

The input to this layer is a `Cursor` over the items of one command
(`RawCommand::cursor()`) and its position. The output is one `Expr`.

## Dispatch order (`expr::parse_expression`)

```rust,ignore
pub enum Position { Statement, Element }

pub fn parse_expression<'a>(st: St<'_, 'a>, mut c: Cursor<'_>, position: Position) -> PResult<Expr<'a>> {
    // 1. In statement position, statement keywords parse their own `=` and `{}` arguments.
    if position == Position::Statement && is_statement_keyword(first_text) { return statement::keyword_or_call(st, c); }
    // 2. `NAME=value` prefixes.
    let vars = env_shorthand_prefix(st, &mut c)?;
    // 3. After shorthand, or as a pipeline element, the head is checked against nu's table.
    if !vars.is_empty() || position == Position::Element { check_element_head(st, &c)?; }
    // 4. Any assignment operator among the items -> assignment.
    let inner = if c.rest().iter().any(|t| matches!(t.kind, TokenKind::Assign(_))) {
        assignment(st, c.remaining())?
    // 5. A first item that looks like a value -> math expression.
    } else if looks_like_value(st.tok(first)) {
        math_expression(st, c.remaining(), false)?
    // 6. Otherwise a keyword expression (if, match, ...) or a call.
    } else {
        statement::keyword_or_call(st, c.remaining())?
    };
    /* wrap `inner` in EnvShorthand if `vars` is not empty */
}
```

This is nu-parser's `parse_expression` in the same order, with nu's
distinction between a command that is a whole pipeline (nu's
`parse_builtin_commands`, `Position::Statement`) and one that is an element
of a longer pipeline, the left side of an assignment, an `else` or a match
arm (nu's `parse_expression`, `Position::Element`). `pipeline()` collects
every command before parsing any (chapter 04) so that it knows which case
applies. Details that are easy to miss:

* `looks_like_value` is nu's `is_math_expression_like`: true for `true`,
  `false`, `null`, `not`, `if`, `match`, items starting with `( { [ $ " ' -`
  or `r#`, and items whose text is a number, unit, datetime, binary or range.
  It is a pure function of the text (`value.rs`), so `-1 | math abs` is math,
  `"ls"` is a string not a call, and `1 + 1` at a pipeline head is
  arithmetic, all without parsing anything twice.
* Statement keywords (`def`, `let`, `mut`, `const`, `for`, `alias`, `module`,
  `use`, `export`, `export-env`, `extern`) are checked *before* the assignment
  test because `let x = 1` contains an `Assign` token.
* `check_element_head` is nu's head table for an element: the declaration
  keywords, `for`, `module`, `use`, `source`, `hide`, `export`, `export-env`
  (`BuiltinCommandInPipeline`), `const` and `mut` (`AssignInPipeline`),
  `overlay` unless the second item of the command is `list`, and `plugin`
  when the second item is `use` are errors there. So `FOO=1 def x [] {}`
  and `hide ls | length` are refused, while `ls | overlay list` parses. The
  second item is counted from the start of the command, shorthand included,
  as nu counts its spans. `let` is not in the table: `ls | let x` parses
  (nu accepts it too), and `ls | let x = 1` is refused as an assignment
  whose left side is not a variable, because after a `|` the keyword is not
  a statement.

## Keyword forms (`statement.rs`)

`keyword_or_call` matches the head text and calls one function per keyword;
anything else is a call. A non-statement keyword (`if`, `loop`, `where`, ...)
that a user command shadows is a call. The match also attaches the grammar
context (`while parsing for`) to any error. Each keyword function follows
the same pattern: consume items with `expect_item`, delegate items to
`value::value` with the right `Hint` (or to `block_body` for a block), check
the positional boundaries the way nu does (next section), and finish with
`expect_end`.

```rust,ignore
fn for_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let full = c;
    let kw = c.expect_item("for")?;
    let mut help = false;
    boundary_help(st, &mut c, "for", true, &mut help)?;            // `--help`, `--`, `-x` before the variable
    let Some(var_tok) = item_or_help(&mut c, help, "loop variable")? else { return help_call(st, full) };
    let (var, typed) = variable_declaration(st, &var_tok)?;         // `x`, `$x` or `x:` (typed)
    let ty = match typed { true => Some(/* every item before `in`, as one span */), false => None };
    boundary_help(st, &mut c, "for", true, &mut help)?;
    let Some(in_tok) = item_or_help(&mut c, help, "`in`")? else { return help_call(st, full) };
    if st.tok(&in_tok) != "in" { return Err(cut(Diagnostic::new(ErrorKind::ExpectedKeyword("in"), in_tok.span))); }
    if c.rest().len() < 2 { return Err(/* missing argument to `in`: the last item is the block's */); }
    let iterable = value::value(st, c.expect_item("value to iterate")?.span, Hint::Any)?;   // never forgiven
    boundary_help(st, &mut c, "for", true, &mut help)?;
    let Some(body_tok) = item_or_help(&mut c, help, "block")? else { return help_call(st, full) };
    let body = block_item(st, &body_tok, "block")?;                 // must start with `{`
    boundary_help(st, &mut c, "for", true, &mut help)?;
    c.expect_end()?;
    done(st, full, help, Expr::new(ExprKind::For(For { .. }), kw.span.merge(body_tok.span)))
}
```

### Positional boundaries: `--help`, `--` and `-x`

Every keyword is a command with a fixed signature in nu, so at the start of
each of its positional arguments `parse_internal_call` looks for flags. The
helpers `boundary`, `boundary_with`, `boundary_help`, `end_of_options`,
`item_or_help`, `done` and `help_call` reproduce that:

* `--help` or `-h` at a boundary makes the whole statement an ordinary
  call showing help (`help_call` re-parses the items with `parse_call`, so
  `def foo [] --help` is a `Call` to `def` with three arguments). nu keeps
  parsing the positionals that follow the flag and only forgives the
  *missing* ones, so `boundary_help` records the flag and parsing goes on:
  `match 1 --help :{}` still has no match block, `return --help 1 2` and
  `loop --help {} y` have an extra positional, `def foo [] {} {} --help` has
  no colon, but `match 1 --help` and `def foo --help` are fine.
  `item_or_help` is the "missing positional is forgiven" step; `done` turns
  the finished statement into the help call. The argument of the `in`
  keyword is never forgiven: nu reserves the last item for the block before
  it reads the keyword's argument, so `for x in []` and `for x --help in []`
  are both "missing argument to `in`". A typed loop variable takes every
  item before `in` as its type (`for x: record<a: int, b: string> in [] {}`
  parses; `for x: int --help in [] {}` has the unknown type `int --help`).
* The first `--` at a boundary is consumed, recorded in `Ast::ignored` and
  otherwise dropped (`return -- 1` returns `1`). After it nu looks for no
  flags at all (`end_of_options`): a second `--` is a positional (`try {}
  -- catch {} --` has one too many) and `return -- --help` returns the string
  `--help`.
* Any other `-x` at a boundary is "the `for` command doesn't have flag
  `-x`": `return -1`, `match -1 {}`, `if -1 > 0 {}` and `where -1 > 0` are
  errors, as they are in nu.
* Where a positional spans several items (a condition, a signature, an alias
  target) only its first item is a boundary.
* The statements nu parses by position rather than through
  `parse_internal_call` never see `--` (`boundary_with(.., dashdash =
  false)`): `alias -- x = ls`, `alias x -- = ls`, `module x -- {}`, `let --
  x = 1` and `export-env -- {}` are errors. An `alias` help call must be the
  whole statement (`alias --help`, `alias x --help`); `alias --help x = ls`
  and `alias x --help extra` are "missing sign" (expected `=`). `export
  --help x` has an extra positional. `use` looks for its flags before every
  argument and consumes any number of `--`.

The table below is the contract each keyword implements. "items" are
whitespace-delimited items; "rest" means everything to the end of the line
(because of `=` absorption, this can contain pipes).

| Keyword | Shape | Notes |
| --- | --- | --- |
| `def` | `def [--env] [--wrapped] NAME SIG[:] [TYPES] BODY` | `NAME` must be a string item (`command_name`: `def $x` is "expected string", a name containing `[` or `(`, even quoted, is "no space between name and parameters"); parser keywords, names with `#`/`^`/`%` and names that read as numbers are rejected (`check_definition_name`). The flags may also follow the name. The signature argument takes every remaining item but the last (`full_signature`): one item is the signature, two of which the second starts with `{` is the signature plus an item nu drops (`def foo [] {} {}`; the dropped one is `Ast::ignored`), otherwise `TYPES` follow a `:` attached to the signature or standing alone (`def foo [] : {}` is fine) and are re-lexed together in signature mode. `BODY` is parsed as a closure without a probe (`closure_body`): `{|x| }` keeps its parameters as `Def::body_params`, `{a: 1}` is a call to `a:`. `--wrapped` needs a rest parameter that is untyped or `string` (`check_wrapped`). |
| `extern` | `extern NAME SIG[:] [TYPES]` | The signature argument takes *every* remaining item, so a body (`extern foo [] {}`) is the dropped second item, never parsed; default values in an extern signature are not parsed either (chapter 07). |
| `let`/`mut`/`const` | `KW NAME[:] [TYPE...] [= rest]` | `NAME` may start with `$`; reserved names (`in`, `nu`, `env`, `ans`) are refused; the type tokens are re-lexed together; the value is the rest, parsed as a *block* (a pipeline). Only `let x` may leave the `=` out; `mut x` and `const x` are "missing required positional argument". |
| `for` | `for VAR[:] [TYPE] in ITEM BODY` | |
| `alias` | `alias NAME = rest` | The `=` must be the item right after the name (`alias x=y`, `alias x`, `alias x =`, `alias = x` are errors; `export alias x =` is accepted, a nu quirk whose length check counts the `export` word, and gives `Alias::value == None`). A bare name starting with `-` is "alias name not supported". The rest is handed to `parse_call_with(.., lenient = true)` as plain words (a pipe becomes a word, `alias x = FOO=1 ls` calls the external `FOO=1`), after two checks nu makes first: a target that `looks_like_value` (a literal, `$x`, `(..)`) is "cannot create an alias to an expression", and a target naming an unaliasable keyword (`def`, `let`, `for`, `export def`, ...) is "cannot create an alias to a parser keyword"; `if`, `match`, `try` and `overlay *` may be aliased. Lenient means a keyword-command target may miss positionals and flag values (`alias x = overlay new`). |
| `module` | `module NAME [BODY]` | `NAME` is a literal string (`module_name`): a record is a type error, `$x`, `(..)` and `$"..."` are "not a string". A body holds declarations only. |
| `use` | `use MODULE [MEMBER...]` | `MODULE` is a string or `null` (a no-op after which the members are parsed and then ignored). Members are names, `*`, or a `[a b]` list; only the last may be `*` or a list. A member that is a variable, subexpression or `key: value` record is parsed and kept as `UseMemberKind::Ignored`, as nu ignores it; non-string list items and a cell path after a list (`[math].x`) are `Ast::ignored`; anything else is "wrong import pattern structure". |
| `export` | `export def|extern|alias|use|module|const ...` | Wraps the inner statement. |
| `export-env` | `export-env BODY [ITEM...]` | nu hands `export-env` exactly one argument: extra items (`export-env {} extra`) and a redirection on it are `Ast::ignored`. A closure or record body is refused. |
| `if` | `if COND... BLOCK [else BLOCK|EXPR]` | The condition is every item before the block; the block is the item before `else` or the last item. A `key:` record where the block should be is "expected block" (`block_item`). The else branch is a block or a whole expression (so `else if ...` recurses and `else {|x| }` is a closure). |
| `match` | `match ITEM BLOCK` | See chapter 07 for the block. nu decides what the `{ ... }` is before it knows it wants arms, so a `{|x| }` closure, a `{a: 1}` record, a `$x` or a `(..)` in that position is accepted and kept as `Match::value_block` (the arms are empty); `match 1 [a]` and `match 1 foo` are errors. |
| `while` | `while COND... BLOCK` | |
| `loop` | `loop BLOCK` | |
| `try` | `try BLOCK [catch|finally CLOSURE]{0,2}` | Handlers are kept in source order; nu allows two of either kind. A handler is a closure, or a `$x`/`(..)` that may hold one (`handler_value`); a record is refused. |
| `return` | `return [ITEM]` | One item only, as in nu. |
| `break`/`continue` | keyword alone | |
| `where` | `where {closure}` or `where COND...` | A row condition: `math_expression(.., row = true)`. `where --help` is a help call. |

How the extent of a multi-item argument is found deserves a note. nu-parser
computes it from the signature ("the condition of `if` gets all spans up to
the ones needed by the remaining required positionals"). Here that rule is
written out per keyword: the `if` condition is `c.slice(1..block_index)`
where `block_index` is the index before `else` or the last index, and the
`def` signature is every item but the last. This is the same result without
needing signatures.

### Redirections and attributes on statements

`parse_command` builds the element's `Redirection` from the raw command and
refuses one on the statements nu refuses it on: `def`, `extern`, `let`,
`mut`, `const`, `for`, `alias`, `module`, `use`, `export`, an attribute
block, any call whose head word is `overlay` (refused by name before the
arguments are looked at) and the calls whose fixed signature (below) is not
redirectable (`hide`, `source`, `run`, `plugin use`). A redirection on
`export-env` is dropped without a look, as nu drops it, and recorded in
`Ast::ignored`.

Attribute lines (`@name args`, collected by chapter 04) are parsed by
`attribute()` as calls to `attr name`: the name must be non-empty, whether
`attr name` exists is the consumer's business (it may come from a `use`d
module), and the built-in attributes (`category`, `complete`, `deprecated`,
`example`, `interactive`, `search-terms`) get their arguments checked
against their fixed signatures. Attributes may only precede `def`, `extern`,
`export def` and `export extern`; before anything else (`export alias`, a
call) they are "attributes must be followed by a definition".

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

`assignment` splits the items at the first `Assign` token. The left side is
parsed in `Position::Element` and must be what nu's `parse_full_cell_path`
produces: a `Var`, a `Subexpression`, a `FullCellPath`, a `List`, a `Table`
or a `key: value` `Record`, with or without a cell path (nu accepts `(1) =
2` and `[1].0 = 2` at parse time and fails at run time); anything else is
"assignment requires a variable". The right side (everything absorbed to
the end of the line) is parsed with `parse_block` and stored as a `Block`,
matching nu, where `$x = ls | length` assigns the pipeline's result. Since
nu 0.97 an external command at the start of the value must be written with
a caret: when a command table is configured (`ParseConfig`), an unknown bare
`Call` as the first element of the rhs is "external command calls must be
explicit in assignments" (`$x = git` is refused, `$x = ^git` is not); with no
table the parser cannot tell and lets it through.

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
is left to the consumer; the bridge in `tools/` shows how (chapter 09). The
one exception is the commands that are keywords in nu-parser and have a
fixed signature there: `hide`, `source`, `source-env`, `run`, `overlay new`,
`overlay use`, `overlay hide`, `overlay list`, `plugin use` and the
built-in `attr *` attributes. `parse_call_with(st, c, lenient)` finishes by
calling `statement::check_fixed_signature`, whose `FixedSignature` table
(required and optional positional counts, a rest, a keyword argument such
as `overlay use`'s `as`, the flags and whether each takes a value, whether
`null` is allowed as the first positional, and whether the call may be
redirected) reproduces nu's checks: unknown flags, a flag without its value,
too many positionals, `as` without its name, `-1` as a flag rather than a
number, and `null` or `true` where a string is required (`hide null`). With
no command table the head of `overlay use x` resolves as `overlay` with
`use` as its first argument; both spellings are found. `lenient` is set for
an alias target, where nu forgives missing positionals and flag values.
Whether the named module, file or plugin exists is still the consumer's.

The `%` sigil (`percent_call`) forces the built-in command even when a custom
command or alias shadows its name. `%ls` and `% ls` give a `Call` whose
`sigil` is the span of the `%`; `%$cmd` and `%(expr)` give a `DynamicCall`
whose head is the `$` expression or subexpression. Like nu, a quoted or
otherwise non-bare name after `%` is an error, and a bare name that is not in
the configured command set is rejected with "percent sigil requires a
built-in command" (skipped when the `ParseConfig` knows no commands at all).

## External calls and environment shorthand

`^cmd args`: the head after `^` is a string, `$var` or `(subexpr)`; each
argument is `$..`/`(..)`/`[..]`/`{..}` parsed as a value, `...x` as a spread,
and everything else as an **external string**. A `[..]` argument must end
with `]` (`check_external_list`): nu hands it to its list parser alone, so
`^cmd [a].x` and `^cmd ...[a].x` are "unclosed delimiter" while
`^cmd (ls).name` parses. An unknown bare head is an external command for nu
too, so `parse_call_with` applies the same check to `cmd [a].0` when a
command table is configured. `external_string` reproduces
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
