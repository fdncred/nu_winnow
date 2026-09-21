# 06 Values and literals

Files: `src/parser/value.rs`, `src/parser/strings.rs`, `src/parser/cellpath.rs`,
`src/parser/collections.rs`, `src/parser/literal.rs`.

`value(st, span, hint)` turns the text of one item into an `Expr`. It is
called for every argument, operand, list element, record value, range bound,
match value and attribute argument, so it is the most exercised function in
the crate. It mirrors nu-parser's `parse_value`.

## Dispatch

```rust,ignore
pub fn value<'a>(st: St<'_, 'a>, span: Span, hint: Hint) -> PResult<Expr<'a>> {
    match text.as_bytes() {
        [] => Err(cut(Diagnostic::expected("value", span))),
        [b'$', ..] => cellpath::dollar(st, span),          // $var, $x.a, $.a, $"..", $'..', ranges
        [b'(', ..] => cellpath::paren(st, span, hint),     // range, signature, or (subexpr)[.members]
        [b'{', ..] => brace(st, span, hint),               // record | closure | block
        [b'[', ..] if hint == Hint::Signature => Ok(signature_placeholder(span)),
        [b'[', ..] => cellpath::full_cell_path(st, span, false),
        [b'r', b'#', ..] => literal::raw_string(st, span),
        _ => match hint {
            Hint::Number => literal::number(st, span),
            Hint::String => strings::string(st, span),
            Hint::MatchBody | Hint::Closure | Hint::Signature => Err(..),   // those must start with { or [
            Hint::Any => any_value(st, span, text),
        },
    }
}
```

`Hint` is the small subset of nu's `SyntaxShape` that changes *parsing*
rather than typing:

| Hint | Used for | Effect |
| --- | --- | --- |
| `Any` | most arguments | full literal search |
| `Closure` | `catch`/`finally` handlers | `{}` is a closure even if it looks like a block |
| `MatchBody` | the body of a `match` arm | `{}` is a block, unless it is written as a closure (`{|x| ..}`) or a record (`{a: 1}`) |
| `Number` | range bounds | numbers only (plus `$` and `(` forms) |
| `String` | record keys, `def`/`use`/`module` names | bare or quoted string, `$var`, `(expr)`, interpolation |
| `Signature` | (reserved) | |

Statement bodies (`if`, `for`, `def`, ...) do not go through `value` at all:
`statement::block_item` calls `block_body` directly, which rejects a leading
`|`.

`any_value` tries, in nu's order: `null`/`true`/`false`, binary (`0x[`, `0o[`,
`0b[`), range (only if `cellpath::is_range_syntax` says the text has the shape
of one), filesize, duration, datetime, int, float, and finally string. The order
matters: `1..3` must be tested as a range before `1.` could be read as a
float, and `1kb` as a filesize before `1` as an int. A matched unit with a
bad number (`1..2sec`) is an error, not a fallback, as in nu.

## Literals (`literal.rs`)

Pure functions over the item text, most returning `Option`:

| Function | Accepts |
| --- | --- |
| `parse_int` | decimal, `0x`, `0o`, `0b`, `_` separators, sign; radix literals wrap like nu (`0xffffffffffffffff` is -1) |
| `parse_float` | whatever Rust's `f64::from_str` accepts after removing `_`: `1.5`, `.5`, `5.`, `1e3`, `inf`, `NaN` |
| `filesize` / `duration` | `<number><unit>`; filesize units case-insensitive (`kb`, `KiB`), duration units case-sensitive (`sec`, `µs`); the number must start with a digit, `.digit` or `-digit` and must not end with `$` (so `$x..$kb` can be a range) |
| `is_datetime` | `YYYY-MM-DD`, optionally `Thh:mm:ss[.frac]`, optionally `Z` or `±hh:mm`, written as a winnow parser |
| `binary` | `0x[..]`, `0o[..]`, `0b[..]`; the interior is lexed with `BINARY`, digits are concatenated, left-padded to whole bytes and decoded |
| `unescape` | the escape table of double-quoted strings: `\" \' \\ \/ \( \) \{ \} \$ \^ \# \| \~ \  \a \b \e \f \n \r \t \0 \xHH \u{...}`; anything else is an error |
| `raw_string` | `r#'...'#` with any number of hashes |

```rust
use nu_winnow_parser::{parse, ast::{ExprKind, DurationUnit, FilesizeUnit}};

let ast = parse("[1_000 0xff 1.5e3 2.5hr 10kib 2024-01-02T03:04:05Z 0x[de ad] r#'raw'#]").unwrap();
let items = match &ast.block.pipelines[0].elements[0].expr.kind {
    ExprKind::List(items) => items,
    other => panic!("{other:?}"),
};
let kinds: Vec<String> = items.iter().map(|i| match i {
    nu_winnow_parser::ast::ListItem::Item(e) => match &e.kind {
        ExprKind::Int(i) => format!("int {i}"),
        ExprKind::Float(f) => format!("float {f}"),
        ExprKind::Duration(d) => format!("{} {:?}", d.value, d.unit),
        ExprKind::Filesize(f) => format!("{} {:?}", f.value, f.unit),
        ExprKind::DateTime(t) => format!("datetime {t}"),
        ExprKind::Binary(b) => format!("binary {:?}", b.bytes),
        ExprKind::String(s) => format!("string {:?} {:?}", s.quote, s.value),
        other => format!("{other:?}"),
    },
    _ => unreachable!(),
}).collect();
assert_eq!(kinds, vec![
    "int 1000", "int 255", "float 1500", "2.5 Hour", "10 KiB",
    "datetime 2024-01-02T03:04:05Z", "binary [222, 173]", "string Raw(1) \"raw\"",
]);
let _ = (DurationUnit::Hour, FilesizeUnit::KiB);
```

## Strings (`strings.rs`)

`string()` decides between a raw string, a bare interpolation (a bare word
containing `(`, e.g. `foo(1 + 1)bar`) and a plain literal. `string_lit`
handles the quoting styles with nu's exact rule for embedded quotes: the
*last* quote character in the item must be its last byte, but quotes in
between are kept as text, so `"a"b"c"` is the string `a"b"c` and `"abc"def`
is an error. Single quotes and backticks have no escapes; double quotes go
through `unescape`.

`interpolation()` handles `$"..."` and `$'...'` (and bare interpolation). The
body is scanned with the same `interp_subexpr_step` the lexer uses, so `(`
opens a subexpression in which quotes and parentheses nest, `\(` is a literal
in double-quoted strings, and each `( ... )` becomes a `Subexpression`
expression. Text parts are unescaped for double quotes only.

## `$` expressions and cell paths (`cellpath.rs`)

`dollar` orders the cases as nu does: `$"`/`$'` → interpolation; `$.` → a
`CellPath` literal (`$.` alone is the empty path); a text with the shape of a
range (`is_range_syntax`) → range; otherwise `full_cell_path`.

`full_cell_path` re-lexes the item with `CELL_PATH` (so `.`, `?` and `!` are
split off) and parses the head token (`$var`, `(subexpr)`, `[list]` or
`{record}`), followed by `cell_path_members`, a small state machine copied
from nu-parser that accepts `.name`, `.0`, `."quoted"`, and the modifiers `?`
(optional) and `!` (case-insensitive) in either order. A lone `$x` with no
members is returned as a plain `Var`, not wrapped. The same function with
`implicit = true` produces the `$it` paths of row conditions.

```rust
use nu_winnow_parser::{parse, ast::{ExprKind, PathMemberKind}};

let ast = parse("$env.PATH.0?").unwrap();
match &ast.block.pipelines[0].elements[0].expr.kind {
    ExprKind::FullCellPath(p) => {
        assert!(matches!(p.head.kind, ExprKind::Var(ref v) if v.is_env()));
        assert_eq!(p.members[0].kind, PathMemberKind::String("PATH".into()));
        assert_eq!(p.members[1].kind, PathMemberKind::Int(0));
        assert!(p.members[1].optional);
    }
    other => panic!("{other:?}"),
}
```

## Ranges

Two functions share the work. `is_range_syntax(text)` decides, without
parsing, whether an item *is* a range: it finds the `..` occurrences at
parenthesis depth zero (one for `a..b`, two for `a..s..b`) and checks that
every bound present is number-like (an int, a float, a `$` expression or a
parenthesised subexpression). `cd ..` and `a..b` fail that test and fall
through to the next literal kind, as in nu. `range()` then parses a text that
passed: it reads the operator (`..`, `..<`, `..=`) and each bound with
`value(.., Hint::Number)`. Because the shape was checked first, an error in a
bound (`1..(1 +)`) is reported as an error in the range rather than turning
the item into a string.

## `{ ... }`: record, closure or block

`brace` reproduces nu-parser's `parse_brace_expr`:

1. If the item does not end with `}`, it is `{...}.member` → `full_cell_path`.
2. Lex the first two interior tokens with `BRACE_PROBE`.
3. Decide:

| Probe | Result |
| --- | --- |
| no tokens | closure/block per hint, otherwise an empty record |
| first token is `\|` or `\|\|` | closure (error if a block was required) |
| second token is `:` | record (`{a: 1}`), regardless of hint |
| first token starts with `...` followed by `{`, `$` or `(` | record (spread) |
| otherwise | closure/block per hint; `Hint::Any` means closure |

So `{ print hi }` in argument position is a closure, `{}` is a record, and
`if true { }` gets a block. `closure()` lexes the body with `BLOCK`, takes a
leading `|...|` as the parameter list (parsed by `signature::parse_signature_inner`
on the text between the pipes) or `||` as empty parameters, pushes a
declaration scope and parses the rest with `parse_block`. The parameter list
may start on a later line than the `{`.

## Lists, tables, records (`collections.rs`)

* `list_or_table`: lex with `LIST`; if the tokens are `[..]` `;` `[..]...`,
  it is a table whose rows are lists without spreads; otherwise a list.
  Items starting with `...` followed by `[`, `$` or `(` are spreads. Stray
  `|` and `;` between items and bare operator tokens (`[Assignment, =,
  Assign]`) are tolerated as nu tolerates them.
* `record`: lexes entry by entry with `lex_prefix_at`: key with `RECORD_KEY`,
  the `:`, then the value with `RECORD_VALUE`. Keys are parsed with
  `Hint::String` (bare, quoted, `$var`, `(expr)`, interpolation). Spreads
  `...$r` are allowed. Comments inside are recorded.
* `subexpression`: lex with `SUBEXPRESSION` (newlines are whitespace) and
  parse a block in a new scope.

All of these record their interior comments through `st.comment`, which is
how a formatter can put comments back inside multi-line collections.
