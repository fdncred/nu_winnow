# 07 Signatures, types and patterns

Files: `src/parser/signature.rs`, `src/parser/pattern.rs`.

## Signatures

`parse_signature(st, span, external)` accepts a `[...]` or `(...)` item;
`parse_signature_inner(st, inner, outer, external)` is the shared worker also
used for closure parameter lists (`|x, y|`), where `inner` is the text between
the pipes and `outer` the span including them. `external` is set for an
`extern`, whose parameters declare no variables: nu then checks no reserved
names and never parses default values (`extern foo [x = 0x]` parses; the
default token is recorded through `st.ignore` as text nu drops).

The interior is lexed with `LexOptions::SIGNATURE`: newlines are whitespace,
`:` `=` `,` are special (so `x:int=3` splits into five tokens), comments are
kept, and `<`/`>` nest so `record<a: int, b: string>` stays one token.

`parse_params` is a five-state machine copied from nu-parser:

```text
Arg ──":"──▶ Type ──token──▶ AfterType ──"="──▶ Default ──token──▶ Arg
 │                              │                                    ▲
 └──"="──▶ Default              └──","──▶ AfterComma ──token─────────┘
 └──","──▶ AfterComma
```

Tokens that are not items (a `|`, `;` or redirection: `[o> x]`) are skipped
as nu skips them. A type or default value that arrives while no parameter
exists yet (`[: int]`, `[= 1]`) is dropped silently, which is what nu does
with it. A `:` or `=` as the *last* token is "expected type" / "expected
default value", but a comment counts as a token, so `[x: # c\n]` is a
parameter without a type and `[x = # c\n y]` gives `x` the default `y`; the
list is not checked again when the tokens run out.

In `Arg`/`AfterComma`/`AfterType` a token creates a parameter (`new_param`):

| Token | Parameter |
| --- | --- |
| `--name`, `--name(-n)` | `ParamKind::Flag { long, short }` |
| `-n` | `ParamKind::Flag { long: None, short }`; the letter must be an identifier byte (`-.` and `--` are errors) |
| `(-n)` | attaches a short form to the previous flag |
| `name?` | `ParamKind::Positional { optional: true }` |
| `...name` | `ParamKind::Rest` |
| `name` | `ParamKind::Positional { optional: false }` |

Names are validated with `is_identifier` (no `.[({+-*^%/=!<>&|`), and, since
a parameter declares a variable, with `statement::check_variable_name`: `in`,
`nu`, `env` and `ans` are reserved (`def foo [--env] {}` is an error), except
in an `extern`.

A comment token is pushed onto the `description` of the most recent
parameter; nu joins several with `\n`, so all of them are kept
(`Param::description` is a `Vec<Comment>`). A type token is split by
`parse_type_with_completer` at the *first* `@` wherever it is (so
`record<a@b: int>` is an unclosed `record<`, as in nu) into the type and an
optional completer; an empty type before the `@` is unknown. `bool` on a flag
is "type annotations are not allowed for boolean switches". A default value
is parsed with the declared shape through `Hint::Typed` (chapter 06):
`[x: int = abc]` is an error at parse time, `[x: string = 1]` is the string
`1`; a rest parameter with a default is an error. `check_completer` accepts a
string (bare or quoted: the name of a command) or a list, and refuses a
subexpression or a record; whether the command exists is the consumer's
business, since it may come from a `use`d module.

After the loop `check_params` applies nu's two post-checks: a required
positional after an optional one or one with a default
(RequiredAfterOptional) and a second `...rest` (MultipleRestParams).

```rust
use nu_winnow_parser::{parse, ast::{ExprKind, ParamKind, TypeKind}};

let src = "def f [\n  a: int  # first\n  --flag(-f): string = \"x\"\n  ...rest\n]: nothing -> string { }";
let ast = parse(src).unwrap();
let d = match &ast.block.pipelines[0].elements[0].expr.kind {
    ExprKind::Def(d) => d,
    other => panic!("{other:?}"),
};
let p = &d.signature.params;
assert!(matches!(p[0].kind, ParamKind::Positional { optional: false }));
assert!(matches!(p[0].ty.as_ref().unwrap().kind, TypeKind::Int));
assert_eq!(p[0].description[0].body(src), "first");
assert!(matches!(p[1].kind, ParamKind::Flag { long: Some(l), short: Some(s) } if l.item == "flag" && s.item == 'f'));
assert!(p[1].default.is_some());
assert!(matches!(p[2].kind, ParamKind::Rest));
assert_eq!(d.signature.io_types.len(), 1);
```

## Types

`parse_type` knows exactly the names nu-parser's `parse_shape_name` knows:
`any binary bool cell-path closure datetime directory duration error
external_arg float filesize glob int nothing number path range string`, plus
the generics `list<T>`, `record<a: T, b>`, `table<...>` and `oneof<A, B>`.
`block` is rejected with nu's help text ("use closure"), and anything else is
`ErrorKind::UnknownType`. Generic parameters are found by splitting at the
first `<` and requiring a trailing `>`; record and table fields
(`named_type_params`) are lexed in signature mode with nu's skips: stray
commas are ignored (`record<a: int,, b: int>`, `record<, a: int>`), a name
followed by `:` takes the next token as its type whatever it is, a name
without `:` has type `any`, and a token that is not an item or not a string
(`;`, `|`, `true`) is "annotation key not string".

Input/output types after a signature (`def f []: int -> string { }` or
`def f []: [int -> string, nothing -> nothing] { }`) are collected by the
`def`/`extern` parsers from the items between the signature and the body,
re-lexed as one text with `IO_TYPES` (because `record<a: int>` was split into
two items by the top-level lexer), and parsed as `type -> type` triples by
`parse_io_types`.

## Match blocks and patterns (`pattern.rs`)

`match_block(st, span)` lexes the `{ ... }` item with `LexOptions::MATCH`
(commas and newlines are whitespace; `|` still yields `Pipe` tokens) and
walks the tokens:

```text
pattern ( "|" pattern )*  [ "if" guard-items... ]  "=>"  body
```

* The guard is every item up to `=>`, parsed with `math_expression`.
* The body is **one item**: a `{ ... }` is parsed with `Hint::MatchBody` (so it is
  a block, or a record if it looks like one, or a closure if it starts with
  `|`), anything else with `parse_expression` — which means `=> print hi`
  makes `hi` the next pattern, exactly as in nu.

`parse_pattern` dispatches on the first character of the pattern item:

| Item | `PatternKind` |
| --- | --- |
| `$name` | `Variable`; the name binds a variable, so the reserved `in`, `nu`, `env`, `ans` are refused |
| `{a: pat, $b}` | `Record` (a `$var` entry binds the field of the same name) |
| `[p, ..$rest]` / `[p, ..]` | `List` with `Rest(Some)` / `Rest(None)` |
| `_` | `Wildcard` |
| anything else | `Value(value(.., Hint::Any))` — literals, ranges, `(1 + 1)`; that the value is constant is the consumer's check |

Or-patterns become `PatternKind::Or`. Guards and bodies are ordinary
expressions, so everything in chapter 05 applies inside them.

The two structured patterns follow nu's lexing exactly:

* `list_pattern` lexes the interior with `PATTERN_LIST` and hands the tokens
  to `collections::lite_parts` (chapter 06), so `[1 | 2]` is the two patterns
  `1` and `2`, a redirection token is dropped, and a `;` is an error. Within
  a group nu stops reading at `..` or `..$rest`: the items after it
  (`[1 ..$r 2]`) are recorded through `st.ignore` as text nu never looks at.
  `..foo` is not a rest pattern but the string value `..foo`.
* `record_pattern` lexes with `PATTERN_RECORD` and takes *every* token as a
  field name, verbatim: the quotes of `{"a": $x}` are kept, so that pattern
  never matches a field `a`, exactly as in nu. Each field must be followed by
  `:` and a pattern, so `{a: 1; b: 2}` is "expected record" (the field `;` is
  followed by `b`).
