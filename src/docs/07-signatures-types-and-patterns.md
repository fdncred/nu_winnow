# 07 Signatures, types and patterns

Files: `src/parser/signature.rs`, `src/parser/pattern.rs`.

## Signatures

`parse_signature(st, span)` accepts a `[...]` or `(...)` item;
`parse_signature_inner(st, inner, outer)` is the shared worker also used for
closure parameter lists (`|x, y|`), where `inner` is the text between the
pipes and `outer` the span including them.

The interior is lexed with `LexOptions::SIGNATURE`: newlines are whitespace,
`:` `=` `,` are special (so `x:int=3` splits into five tokens), comments are
kept, and `<`/`>` nest so `record<a: int, b: string>` stays one token.

`parse_params` is a five-state machine copied from nu-parser:

```text
Arg ──":"──▶ Type ──token──▶ AfterType ──"="──▶ Default ──token──▶ Arg
 │                              │                                    ▲
 └──"="──▶ Default              └──","──▶ AfterCommaArg ──token──────┘
 └──","──▶ AfterCommaArg
```

In `Arg`/`AfterCommaArg`/`AfterType` a token creates a parameter:

| Token | Parameter |
| --- | --- |
| `--name`, `--name(-n)` | `ParamKind::Flag { long, short }` |
| `-n` | `ParamKind::Flag { long: None, short }` |
| `(-n)` | attaches a short form to the previous flag |
| `name?` | `ParamKind::Positional { optional: true }` |
| `...name` | `ParamKind::Rest` |
| `name` | `ParamKind::Positional { optional: false }` |

A comment token becomes the `description` of the most recent parameter. A
type token is split at the first `@` outside angle brackets into the type and
an optional completer. Names are validated with `is_identifier` (no `.[({+-*^%/=!<>&|`).

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
assert_eq!(p[0].description.unwrap().body(src), "first");
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
first `<` and requiring a trailing `>`; record and table fields are lexed in
signature mode and may be separated by commas, spaces or newlines.

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
| `$name` | `Variable` |
| `{a: pat, $b}` | `Record` (a `$var` entry binds the field of the same name) |
| `[p, ..$rest]` / `[p, ..]` | `List` with `Rest(Some)` / `Rest(None)` |
| `_` | `Wildcard` |
| anything else | `Value(value(.., Hint::Any))` — literals, ranges, `(1 + 1)` |

Or-patterns become `PatternKind::Or`. Guards and bodies are ordinary
expressions, so everything in chapter 05 applies inside them.
