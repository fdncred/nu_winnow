# Plan: integrating nu-winnow-parser into Nushell

This is a plan for replacing the lexing and syntactic layers of `nu-parser`
with this crate while keeping every observable behaviour of Nushell, and
doing it in steps that can each ship on their own. It assumes the reader knows
`crates/nu-parser` and `crates/nu-protocol`.

## Where the two parsers meet today

`nu-parser` does five jobs in one recursive pass over the source:

1. lexing items and re-lexing interiors (`lex.rs`, `lite_parser.rs`);
2. recognising syntax (`parse_*` functions);
3. resolving names: declarations (`find_decl`), variables, modules, overlays;
4. type-checking expressions and calls (`type_check.rs`, `math_result_type`);
5. compiling blocks to IR (`compile_block`) and const-evaluating where the
   language requires it (`const`, `use`, `source`, attribute values).

This crate does jobs 1 and 2 and produces a tree; `tools/nushell-harness/src/bin/bridge.rs`
demonstrates jobs 3 and 5 as a separate pass over that tree (the "lowering")
for most of the language, evaluating scripts through the real engine with
results identical to `nu-parser`. The plan is to turn that demonstration into
the real front end.

The crate is written to be read next to nu-parser. `src/lex.rs` and the files
of `src/parser/` are named after nu-parser's (`lite_parser.rs`,
`parse_pipelines.rs`, `parse_expressions.rs`, `parse_calls.rs`,
`parse_def.rs`, ...) and hold functions with nu-parser's names
(`parse_block`, `parse_value`, `parse_call`, `find_longest_decl`,
`parse_def_predecl`, ...). The state every parser function shares is a
`WorkingSet` with `StateWorkingSet`'s method names, passed as `working_set`
like nu-parser's, and the AST uses nu-protocol's type names (`Expression` and
`Expr`, `Call`, `Argument`, `PipelineRedirection`, `MatchPattern`,
`SyntaxShape`). Much of the port can therefore be reviewed as "this function
replaces its namesake".

## Target architecture

```text
source ──▶ nu_winnow_parser::parse ──▶ syntactic AST
                                          │
                                          ▼
                     nu-parser (new "lower" module): syntactic AST + StateWorkingSet
                       ─ resolve decls, variables, modules, overlays
                       ─ apply signatures to arguments
                       ─ const-eval where required
                       ─ type-check (existing code, over the lowered tree)
                       ─ compile to IR (existing code)
                                          │
                                          ▼
                             nu_protocol::ast::Block (unchanged)
```

Nothing downstream of `nu-protocol`'s AST changes: the engine, IR compiler,
completions, `ast` command and LSP keep consuming the same structures. The
syntactic AST becomes an *additional* artefact that tools (`nufmt`, the LSP
formatter, highlighters) can use without an engine.

## Principles for a seamless port

* **Behaviour first.** Every step is gated on the comparison suites already in
  this repository: accept/reject parity with `nu-check` over `nu_scripts` and
  the standard library, `ast --flatten` classification parity, the
  `bridge --demo` result parity, and Nushell's own test-suite
  (`cargo test` in the Nushell workspace) once wired in.
* **Feature-flagged, one code path at a time.** `nu-parser` gains a cargo
  feature (`winnow-frontend`) selecting the new front end. CI runs both until
  the old path is deleted.
* **No new dependency for users.** `winnow` is already a dependency of the
  Nushell workspace (via other crates); the parser crate adds nothing else.
* **Keep `nu-protocol` stable.** The lowered output is the existing
  `Block`/`Expression`/`Call`; spans and `SpanId`s are produced exactly as
  today (nu-protocol's `Expression::new`, which takes the working set).

## Steps

### Step 0 — Move the crate into the workspace

* Add `crates/nu-winnow-parser` (or vendor it as `nu-parser/src/syntax`) with
  its tests and corpus. Wire `tests/corpus.rs` to the workspace's
  `crates/nu-std` and `tests/fixtures`.
* Add the Nushell-side comparison tests as `#[ignore]`-by-default integration
  tests that require the `nu` binary, so contributors can run them locally
  (`tools/scripts/*.nu` already work this way).
* Generate `builtin_commands.rs` at build time from the engine's declarations
  instead of from `help commands`, or drop it: inside Nushell the engine's
  `StateWorkingSet` knows every command. This crate's `WorkingSet` already
  plays that part. Its methods carry `StateWorkingSet`'s names
  (`get_span_contents`, `error`, `find_decl`, `add_predecl`, `enter_scope`,
  `exit_scope`) and the parser calls them at the corresponding points:
  `parse_def_predecl` at the start of every block, as nu's `parse_block`
  does, and `enter_scope`/`exit_scope` around each nested block,
  subexpression, closure and module body (nu-parser enters a scope in more
  places, because it also tracks variables). The only thing it takes from outside is the table of known
  commands: `WorkingSet::find_decl` and `WorkingSet::is_decl_name_prefix`
  combine the names declared in the file with what the `ParseConfig` knows
  (`ParseConfig::is_known`, `ParseConfig::is_prefix`). That table should be a
  trait object (`&dyn CommandNames`) rather than a static set. Add that trait
  now:

```rust,ignore
pub trait CommandNames {
    fn is_known(&self, name: &str) -> bool;
    fn is_prefix(&self, first_word: &str) -> bool;
}
```

`ParseConfig` implements it for the standalone use; `StateWorkingSet` gets an
implementation in nu-parser (`is_known` is `find_decl(name.as_bytes()).is_some()`).
The two working sets differ in their signatures where the engine needs more
(`StateWorkingSet` works on `&[u8]`, its `find_decl` returns a `DeclId`
rather than a `DeclKind`, its `add_predecl` takes a `Box<dyn Command>`), not
in what the parser asks of them.

### Step 1 — Lowering module in `nu-parser` (behind the feature)

Port `bridge.rs` into `nu-parser` as `lower.rs`, replacing its shortcuts with
the real machinery it was standing in for. Because both ASTs use the same
type names, most of the lowering maps a node to its namesake; the code tells
them apart by path (the bridge imports this crate's AST as `w`, so
`w::Expr::Call` becomes an `Expr::Call`).

| Bridge shortcut | Real implementation |
| --- | --- |
| `find_decl(name)` only | `find_decl` with the overlay/`use` visibility rules already in `StateWorkingSet` |
| Signature application by hand (`flag.arg`, `get_positional`) | reuse `parse_internal_call`'s argument assignment, split from its lexing: it already takes a signature and a list of argument expressions conceptually; make that split explicit |
| `Pattern::Expression` for literal patterns | `eval_constant` to `Pattern::Value` as `parse_value_pattern` does |
| `let`/`mut` only | `const` via `eval_constant`; `export const` |
| No modules | `parse_module`, `parse_use`, `parse_export_in_module`, `parse_overlay_*`, `parse_source`, `parse_hide` become functions over the syntactic `Use`/`Module`/`Export` nodes instead of over spans; their bodies (module registration, import patterns, overlay stacks) do not depend on lexing and move unchanged |
| No attributes | attribute values via `eval_constant`, then `parse_def`'s existing attribute handling |
| No redirections | nu-protocol's `PipelineRedirection` from the syntactic `PipelineRedirection` (a direct mapping) |
| No env shorthand | the `with-env` wrapping in `parse_expression` (`shorthand`), a direct mapping from the syntactic `EnvShorthand` |
| Captures computed by hand | `discover_captures_in_closure` and `compile_block`, unchanged |
| Type `Any` everywhere | `math_result_type`/`type_compatible` calls at the same points `nu-parser` makes them today |

Signature-driven decisions the syntactic tree leaves open are resolved here,
and only here:

* `--flag value` (a syntactic `Argument::Named` followed by a positional) →
  named argument with value, using the signature's `Flag::arg`;
* a bare word in a `CellPath`, `Filepath`, `GlobPattern`, `Directory`,
  `Int`, ... position → the typed expression (`SyntaxShape` dispatch);
* `{ ... }` in a `Block` position → block instead of closure (the tree already
  distinguishes when the braces contain `|params|` or `key:`);
* row conditions (`where`), keyword arguments (`else`, `in`), `MathExpression`
  extents: all already shaped by the syntactic parser.

Deliverable: `nu-parser::parse` with `winnow-frontend` produces the same
`Block` as today for every fixture in Nushell's test-suite, checked by a
comparison test that parses each fixture both ways and compares the
`Block`s (span for span; `SpanId`s may be renumbered).

### Step 2 — Error compatibility

`nu-parser` produces `ParseError` variants with specific spans, and many
tests (and the LSP) depend on them. Map `Diagnostic` to `ParseError`:

* This crate's `ParseError` is only the list of `Diagnostic`s; the mapping
  goes from each diagnostic's `ErrorKind` to a variant of nu-protocol's
  `ParseError` enum.
* `ErrorKind::Unclosed` → `ParseError::Unclosed`, `Unbalanced` → `Unbalanced`,
  `ShellSyntax` → `ShellAndAnd`/`ShellErrRedirect`/..., `ExtraTokens` →
  `ExtraTokens`/`ExtraTokensAfterClosingDelimiter`, `Expected` →
  `Expected`, `UnknownType` → `UnknownType`, `KeywordInPipeline` →
  `BuiltinCommandInPipeline`, and so on. Add kinds to `ErrorKind` where
  nu-parser distinguishes cases this crate currently folds into `Message`.
* Recovery granularity: nu-parser recovers per node and produces `Garbage`
  expressions inside calls; this crate recovers per statement. Completions
  and the LSP rely on partial trees for the line being edited, so the
  syntactic parser needs *expression-level* garbage in the two places that
  matter for editing: an incomplete last argument and an unclosed delimiter
  at end of input. Both are local changes in `parse_expressions.rs`
  (`parse_value`) and `parse_calls.rs` (`parse_call_arguments`): return an
  `Expr::Garbage` node (`garbage(span)` in `parse_helpers.rs`) instead of
  cutting when the lexer reported "unclosed at end of input".

Deliverable: Nushell's parser tests pass with the feature on, with an
allow-list of tests whose exact error wording changed, reviewed one by one.

### Step 3 — Switch the LSP, completions and `ast` over

These read spans and `FlatShape`s from the lowered `Block`, so they keep
working. Two improvements become possible and should be done in the same
step so the benefit is visible:

* `nu-lsp` formatting and the `nufmt` project consume the syntactic AST
  directly (comments, quoting, layout are all there), replacing their
  byte-level reconstruction over `FlatShape`s.
* `ast --flatten` can expose comments, which the syntactic tree keeps and
  the lowered tree does not.

### Step 4 — Make the new front end the default, delete the old lexer

* Flip the feature default; keep the old path one release for opt-out.
* Delete `lex.rs`, `lite_parser.rs`, `parse_literals.rs`,
  `parse_expressions.rs`, the lexing halves of `parse_calls.rs`,
  `parse_def.rs`, `parse_module.rs`, `parse_patterns.rs`,
  `parse_signatures.rs`, `parse_shape_specs.rs`. Each has a namesake in this
  crate (`src/lex.rs`, `src/parser/lite_parser.rs`,
  `src/parser/parse_literals.rs`, ...) holding functions of the same names,
  so the deletion can be reviewed file by file against what replaces
  it. What remains of `nu-parser` is name resolution, signature application,
  type checking, const evaluation and IR compilation: the engine-facing half.
* Move the syntactic crate's fuzz/corpus comparison into Nushell's CI.

## Risks and how the plan handles them

| Risk | Mitigation |
| --- | --- |
| A syntax nu accepts that this crate rejects (or vice versa) | The comparison suites found none over 1,600 real files; the Nushell test-suite comparison in step 1 is the second net; the feature flag means a regression is one flag away from a revert. |
| Error message and span drift breaking tests and user muscle memory | Step 2 is explicitly about mapping kinds and spans; the allow-list keeps the drift visible and reviewed. |
| Performance regression from the extra pass | Measured: parsing alone is 2.5–4× faster than today's combined pass; the lowering pass is a linear walk that reuses today's resolution code. Net expected: faster, verified by `bench-vs-nu-parser` before the flag flips. |
| Two ASTs to maintain | The syntactic AST is the stable one (it changes only when syntax changes); the lowered AST is unchanged `nu-protocol`. Tools gain a stable, engine-free tree, which is the point. |
| Partial-parse behaviour for the LSP | Addressed in step 2 with expression-level garbage in the two editing hot spots; the LSP tests are the acceptance criterion. |

## What contributors can do now

* Run `tools/nushell-harness`'s `bridge --demo` and extend the lowering to
  the unsupported nodes (`use`, `module`, `export`, `alias`, `const`,
  attributes, redirections, env shorthand). Each is a self-contained mapping
  from a syntactic node to the `nu-protocol` structure that `parse_*` in
  nu-parser builds today; the harness compares results with nu-parser.
* Add the `CommandNames` trait (step 0) and the `Diagnostic → ParseError`
  mapping table (step 2) in this crate; both are independent of the Nushell
  tree.
* Extend the comparison scripts to Nushell's `tests/fixtures` directory.
