# 06 Values and literals

Files: `src/parser/value.rs`, `src/parser/strings.rs`, `src/parser/cellpath.rs`,
`src/parser/collections.rs`, `src/parser/literal.rs`.

`value(st, span, hint)` turns the text of one item into an `Expr`. It is
called for every argument, operand, list element, record value, range bound,
match value and attribute argument, so it is the most exercised function in
the crate. It mirrors nu-parser's `parse_value`.

## Dispatch

```rust,ignore
pub fn value<'a>(st: St<'_, 'a>, span: Span, hint: Hint<'_, 'a>) -> PResult<Expr<'a>> {
    if let Hint::Typed(kind) = hint {
        return typed_value(st, span, kind);                // a parameter default: its declared shape
    }
    match text.as_bytes() {
        [] => Err(cut(Diagnostic::expected("value", span))),
        [b'$', ..] => cellpath::dollar(st, span),          // $var, $x.a, $.a, $"..", $'..', ranges
        [b'(', ..] => cellpath::paren(st, span, hint),     // range, signature, or (subexpr)[.members]
        [b'{', ..] => brace(st, span, hint),               // record | closure | block
        [b'[', ..] if hint == Hint::Signature => Ok(signature_placeholder(span)),
        [b'[', ..] if hint == Hint::String => strings::string(st, span),   // `[a b]` is a bare word
        [b'[', ..] if hint == Hint::Number => Err(..),
        [b'[', ..] => cellpath::full_cell_path(st, span, false),
        [b'r', b'#', ..] => literal::raw_string(st, span),
        _ => match hint {
            Hint::Number => literal::number(st, span),
            Hint::String => string_shape(st, span),        // refuses true/false/null
            Hint::MatchBody | Hint::Closure | Hint::Signature => Err(..),   // those must start with { or [
            Hint::Any | Hint::Typed(_) => any_value(st, span, text),
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
| `String` | record keys, `def`/`use`/`module` names, `record<...>` field names, completers | bare or quoted string, `$var`, `(expr)`, interpolation; `true`, `false` and `null` are refused ("`true` is a value; quote it") and `[a b]` is a bare word, as with nu's `SyntaxShape::String` |
| `Signature` | (reserved) | |
| `Typed(&TypeKind)` | the default value of a typed parameter | the value is parsed with the declared shape (below) |

Statement bodies (`if`, `for`, `def`, ...) do not go through `value` at all:
`statement::block_item` calls `block_body` directly, which rejects a leading
`|`.

`any_value` tries, in nu's order: `null`/`true`/`false`, binary (`0x[`, `0o[`,
`0b[`), range (only if `cellpath::is_range_syntax` says the text has the shape
of one), filesize, duration, datetime, int, float, and finally string. The order
matters: `1..3` must be tested as a range before `1.` could be read as a
float, and `1kb` as a filesize before `1` as an int. A matched unit with a
bad number (`1..2sec`) is an error, not a fallback, as in nu, and so is a
word with a radix prefix that is not a number (`0b2`, `0x`, `0x[13]=`): nu
commits to an int as soon as it sees `0x`/`0o`/`0b`, and `literal::binary`
only claims a bracketed literal that closes with `]`.

`typed_value` is nu's `parse_value` with a declared `SyntaxShape`, used for
`[x: int = 1]`: a `$` or `(` item is what it always is, a `{` item is a
closure only for `closure` (and refused for the literal shapes), `[` is
allowed for `any`, `table`, `list<T>` (whose items are parsed as `T`),
`string`/`path`/`glob` (a bare word) and `oneof<...>`, and a bare word must
be a literal of the shape: `x: int = abc`, `x: int = "a"`, `x: bool = 1` and
`x: list<int> = 1` are errors ("expected int"), `x: string = 1` is the string
`1`, `oneof<A, B>` takes the first shape that parses. `type_name` supplies
the word nu uses in the message.

`looks_like_value` is nu's `is_math_expression_like`: it decides whether the
first word of a command line starts a math expression instead of naming a
command. Besides the literal kinds above it consults
`literal::looks_like_binary`, so `0b[1|2]` (a pipe inside the brackets) is a
command name, as in nu.

## Literals (`literal.rs`)

Pure functions over the item text, most returning `Option`:

| Function | Accepts |
| --- | --- |
| `parse_int` | decimal, `0x`, `0o`, `0b`, `_` separators, sign; radix literals wrap like nu (`0xffffffffffffffff` is -1) |
| `parse_float` | whatever Rust's `f64::from_str` accepts after removing `_`: `1.5`, `.5`, `5.`, `1e3`, `inf`, `NaN` |
| `filesize` / `duration` | `<number><unit>`; filesize units case-insensitive (`kb`, `KiB`), duration units case-sensitive (`sec`, `µs`); the number must start with a digit, `.digit` or `-digit` and must not end with `$` (so `$x..$kb` can be a range) |
| `is_datetime` | `YYYY-MM-DD` (a real calendar date: `days_in_month` knows leap years, so `2023-02-30` is a string), optionally `Thh:mm:ss[.frac]` (seconds up to 60), optionally `Z` or `±hh:mm`, written as a winnow parser |
| `binary` | `0x[..]`, `0o[..]`, `0b[..]` ending in `]`; the interior is lexed with `BINARY`, digits are concatenated, left-padded to whole bytes and decoded |
| `looks_like_binary` | whether such a word makes the line a math expression: not when the brackets hold a pipe, redirection or assignment token |
| `unescape` | the escape table of double-quoted strings: `\" \' \\ \/ \( \) \{ \} \$ \^ \# \| \~ \  \a \b \e \f \n \r \t \0 \xHH \u{...}`; anything else is an error. It decodes into bytes and checks UTF-8 once at the end, as nu does, so `"\xC3\xA9"` is `é` and `"\xC3"` alone is an error |
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
through `unescape`. Backticks are trimmed only when the item both starts and
ends with one: `` `a`b `` is the bare word `` `a`b ``, as in nu.

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
`implicit = true` produces the `$it` paths of row conditions. A `(` head
that does not close the group at the end of its token (`(pwd)/x`) is a bare
interpolation, not a subexpression. A bare member containing `(` (`$x.a(b)`)
is refused, as nu refuses it ("expected string"). `cell_path_literal` parses
the members alone, without a head, for the `cell-path` shape of a typed
default (`[x: cell-path = a.b.0]`).

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
every bound present is number-like (`is_range_bound`: an int, a float, a `$`
expression, or a `(` group that closes, followed by anything, so `(ls).0..5`
is a range whose bound carries a cell path). `cd ..` and `a..b` fail that
test and fall through to the next literal kind, as in nu. `range()` then
parses a text that passed: it reads the operator (`..`, `..<`, `..=`) and each
bound with `value(.., Hint::Number)`. Because the shape was checked first, an
error in a bound (`1..(1 +)`) is reported as an error in the range rather than
turning the item into a string; a bound that turns out to be a bare
interpolation (`(1)abc..5`, a string for nu) is refused with "the `..`
operator does not work on a string".

## `{ ... }`: record, closure or block

`brace` reproduces nu-parser's `parse_brace_expr`:

1. If the item does not end with `}`, it is `{...}.member` → `full_cell_path`.
2. `brace_shape` lexes the first two interior tokens with `BRACE_PROBE` and
   classifies them as a `BraceShape` (a `pub` enum, because the statement
   parsers ask the same question before deciding how to parse a body):

| `BraceShape` | Probe | `brace` makes it |
| --- | --- | --- |
| `Empty` | no tokens | closure/block per hint, otherwise an empty record |
| `ClosureParams` | first token is `\|` or `\|\|` | closure, whatever the hint |
| `Record` | second token is `:` | record (`{a: 1}`), whatever the hint |
| `Spread` | first token starts with `...` followed by `{`, `$` or `(` | closure/block for those hints, otherwise a record |
| `Other` | anything else | closure/block per hint; `Hint::Any` means closure; a literal hint (`Number`, `String`, `Typed`) is an error |

So `{ print hi }` in argument position is a closure, `{}` is a record, and
`if true { }` gets a block.

Three functions parse the body:

* `block_body` is what a statement wants (`if`, `for`, `while`, `loop`,
  `export-env`, `module`, the `else` branch): it refuses `ClosureParams`
  ("blocks cannot have parameters") and `Record` ("expected block, found a
  record"), which is nu's type mismatch for `if true {a: 1}`.
* `block_unchecked` parses a block without looking at its shape. It is the
  body of a `def`: nu parses that as a closure before it knows what it
  wants, so `def f [] {a: 1}` is a call to `a:` and `def f [] {|x| }` keeps
  its parameters (`Def::body_params`, chapter 05).
* `closure` / `closure_parts` lex the body with `BLOCK`, take a leading
  `|...|` as the parameter list (parsed by `signature::parse_signature_inner`
  on the text between the pipes) or `||` as empty parameters, push a
  declaration scope and parse the rest with `parse_block`. The parameter list
  may start on a later line than the `{`.

## Lists, tables, records (`collections.rs`)

* `list_or_table`: `bracket_tokens` lexes the interior with `LIST` (comments
  recorded and dropped); if the tokens are `[..]` `;` `[..]...`, it is a
  table; otherwise `refuse_semicolon` makes any `;` an error ("unexpected
  semicolon in list", as nu) and the rest is a list. `list_or_table_typed`
  is the same with the items parsed as a declared element type
  (`[x: list<int> = [1 2]]`).
* `lite_parts` is nu's lite parse of the tokens inside the brackets, shared
  with list patterns: `|` splits the tokens into groups whose items are all
  list items (`[1 | 2]` is `[1, 2]`), a trailing `|` is an error, `||` is
  the "use `or`" error, a redirection and its target are dropped from the
  items (`[a o> b]` is `[a]`) and recorded through `st.ignore` so they show
  up in `Ast::ignored`; a redirection with nothing before it, without a
  target, or repeated for the same stream is an error. After an assignment
  operator everything is an item, so `[a = b | c]` has five items.
* `list_item`: items starting with `...` followed by `[`, `$` or `(` are
  spreads; a token that is not an item (the `=` of `[Assignment, =, Assign]`,
  and everything after it) is a bare word; the rest go through `value`.
* `table`: the header and each row go through `list_row` (a list without
  spreads); nu's checks are applied at parse time: at least one row, every
  row a list ("table item not list"), every row with exactly as many items as
  there are columns ("missing columns" / "extra columns"), and every column
  name a string, an interpolation, a variable, a cell path or a subexpression
  ("table column name not string" for `[[1 2]; [3 4]]`; the type of the last
  three is the consumer's).
* `record`: lexes entry by entry with `lex_prefix_at`: key with `RECORD_KEY`,
  the `:`, then the value with `RECORD_VALUE`. A key and a value must be
  items (`{a: =}` and `{a: o>}` are "unexpected token in record value").
  Keys are parsed with `Hint::String` (bare, quoted, `$var`, `(expr)`,
  interpolation), which refuses `true`, `false` and `null`. Spreads `...$r`
  are allowed. Comments inside are recorded. Like nu
  (`check_record_key_or_value`), a bare word or bare interpolation containing
  `:` is refused as a key or value (`{a: http://x}`, `{ :: x }`): quote it.
* `subexpression`: lex with `SUBEXPRESSION` (newlines are whitespace) and
  parse a block in a new scope.

All of these record their interior comments through `st.comment`, which is
how a formatter can put comments back inside multi-line collections.
