# 08 The AST and its consumers

Files: `src/ast/mod.rs`, `src/ast/visit.rs`, `src/flatten.rs`, `src/pretty.rs`.

## Principles

* **A span on every node.** `Expr { span, kind }`, and every struct inside
  `ExprKind` records the spans of its keywords, operators and punctuation that
  a formatter might want to reproduce (`Binding::keyword`, `Binding::eq`,
  `If::keyword`, `Else::keyword`, `MatchArm::arrow`, `Range::op_span`, ...).
  Spans are absolute byte offsets into `Ast::source`; `Span::slice(source)`
  gives the original text of any node.
* **Nushell's shape.** A `Block` is a list of `Pipeline`s; a pipeline is a
  list of `PipelineElement`s; keywords are `ExprKind` variants. This mirrors
  `nu-protocol` so an engine can lower the tree mechanically (the bridge in
  `tools/` does it in about 700 lines).
* **Decoded and original.** `StringLit { value: Cow<str>, quote }` holds the
  decoded text and the quoting style; the spelling is in the span. Numbers
  hold their value; units hold the number and the unit; datetimes keep their
  text.
* **Comments.** `Ast::comments` lists all of them; pipelines carry
  `leading_comments` and `trailing_comments`; parameters carry a
  `description`.
* **Borrowing.** `Ast<'a>` borrows the source. Identifiers are `&'a str`,
  strings `Cow<'a, str>` (owned only when unescaping changed something),
  multi-word command names `Cow<'a, str>` (owned only when joined).
* **`#[non_exhaustive]`** on `ExprKind` and `ErrorKind`: consumers outside the
  crate must have a wildcard arm, so adding variants is not a breaking change.

## Node catalogue

The `ExprKind` variants, grouped as in the source:

| Group | Variants |
| --- | --- |
| Literals | `Bool`, `Nothing`, `Int`, `Float`, `String(StringLit)`, `Interpolation`, `Binary`, `Duration`, `Filesize`, `DateTime`, `Range` |
| Variables and paths | `Var`, `CellPath` (`$.a`), `FullCellPath` (head + members, `implicit_head` for `$it`) |
| Collections | `List(Vec<ListItem>)`, `Table`, `Record(Vec<RecordItem>)`, `Closure`, `Block`, `Subexpression` |
| Operators | `BinaryOp`, `UnaryNot`, `Assignment` (rhs is a `Block`) |
| Calls | `Call { head, args }`, `ExternalCall`, `EnvShorthand`, `AttributeBlock` |
| Declarations | `Let`, `Mut`, `Const` (all `Binding`), `Def`, `Extern`, `Alias`, `Use`, `Module`, `Export`, `ExportEnv` |
| Control flow | `If`, `Match`, `For`, `While`, `Loop`, `Break`, `Continue`, `Return`, `Try`, `Where` |
| Recovery | `Garbage` |

Supporting types: `Signature`/`Param`/`ParamKind`/`TypeAnnotation`/`TypeKind`/
`IoType`, `Pattern`/`PatternKind`/`MatchArm`, `Arg`/`Flag`/`ExternalArg`,
`Redirection`/`RedirectTarget`, `UseMember`, `Handler`/`HandlerKind`,
`Operator` (with `precedence`, `as_str`, `from_spelling`).

Things that are deliberately *not* decided in the tree, because nu needs a
signature for them: whether `--flag value` binds the value (a `Flag` with
`value: None` followed by a `Positional`); whether a bare word is a cell path
or a glob (it is a `String` with `Quote::Bare`); whether a bare head is
internal or external (always `Call`; only `^` gives `ExternalCall`).

## The visitor

`ast::Visitor` has a `visit_*` method per node family with default bodies that
call the matching `walk_*` function. Override what you need and call the walk
function to descend:

```rust
use nu_winnow_parser::{parse, ast::{Visitor, Expr, ExprKind, walk_expr}};

/// Collect every variable referenced in a program.
#[derive(Default)]
struct Vars<'a>(Vec<&'a str>);

impl<'a> Visitor<'a> for Vars<'a> {
    fn visit_expr(&mut self, e: &Expr<'a>) {
        if let ExprKind::Var(v) = &e.kind {
            self.0.push(v.name);
        }
        walk_expr(self, e);
    }
}

let ast = parse("let y = $x + 1; [1 2] | each {|i| $i * $y }").unwrap();
let mut vars = Vars::default();
vars.visit_block(&ast.block);
assert_eq!(vars.0, vec!["x", "i", "y"]);
```

When you add a node or a field holding an `Expr`, `Block`, `Signature`,
`Pattern` or `TypeAnnotation`, update `walk_expr` (or the relevant walker) in
`src/ast/visit.rs` in the same change; the tests in `tests/syntax.rs` walk
the whole corpus and will notice a missing descent only indirectly.

## `flatten`

`flatten(&Ast) -> Vec<(Span, FlatShape)>` produces a source-ordered,
non-overlapping list of classified spans, the representation nufmt and
syntax highlighters consume (it corresponds to nu-parser's `flatten_block`).
Leaf nodes map to one shape each; containers emit their delimiters and
punctuation as the *gaps* between their children (`Flattener::gaps`), so
the brackets of a list are `FlatShape::List` and the `:` of a record entry is
`FlatShape::Record`. Comments are added at the end.

```rust
use nu_winnow_parser::{parse, flatten::{flatten, FlatShape}};

let src = "let x = [1 2] # c";
let ast = parse(src).unwrap();
let shapes: Vec<(&str, FlatShape)> = flatten(&ast).into_iter().map(|(s, f)| (s.slice(src), f)).collect();
assert_eq!(shapes[0], ("let", FlatShape::Keyword));
assert_eq!(shapes[1], ("x", FlatShape::VarDecl));
assert_eq!(shapes[2], ("=", FlatShape::Operator));
assert_eq!(shapes[3], ("[", FlatShape::List));
assert_eq!(shapes[4], ("1", FlatShape::Int));
assert_eq!(shapes.last().unwrap(), &("# c", FlatShape::Comment));
```

## `pretty`

`pretty::dump(&Ast)` prints an indented tree with one node per line and the
span of each node; `dump_expr` does the same for one expression. It is what
`examples/parse.rs` prints by default and what `tests/nufmt.rs` uses (with
spans stripped) to prove that formatting does not change program structure.
When you add a node, add a line to `Printer::expr` so it shows up.

## Consumers in this repository

* `examples/parse.rs` — tree dump, `--summary` statistics via a `Visitor`,
  `--flat` rows, `--json` (feature `serde`), `--check` over directories.
* `examples/nufmt/format.rs` — a formatter: walks the tree, copies atoms from
  their spans, normalises whitespace, re-indents blocks and multi-line
  collections, and re-emits comments by position.
* `tools/nushell-harness/src/bin/bridge.rs` — lowers the tree into
  `nu-protocol` structures and runs it on the engine.
