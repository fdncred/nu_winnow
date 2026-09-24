//! Keyword statements and expressions: `def`, `let`, `if`, `match`, ...
//!
//! Each parser takes a [`Cursor`] over the items of one pipeline element and
//! produces the corresponding [`ExprKind`] variant.
//!
//! nu parses every keyword as a call to a command with a fixed signature, so
//! two things hold for all of them and are shared here: at the start of each
//! positional argument nu looks for flags, which for the keywords means that
//! `--help`/`-h` turns the statement into an ordinary call ([`boundary`]) and
//! any other `-x` is an unknown-flag error (`return -1` is one); and the
//! commands that are keywords in nu but ordinary calls here (`hide`, `source`,
//! `overlay use`, ...) get their positional counts and flags checked against
//! that fixed signature ([`check_fixed_signature`]).

use std::borrow::Cow;

use crate::ast::{
    Alias, Arg, Attribute, AttributeBlock, Binding, Block, Call, Def, DefFlag, Else, Export, ExportEnv, Expr, ExprKind,
    Extern, For, Handler, HandlerKind, If, ListItem, Loop, Match, Module, RedirectTarget, Redirection, Return,
    Signature, Try, Use, UseMember, UseMemberKind, Where, While,
};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut};
use crate::lexer::{AssignOp, RedirectSource, Token, TokenKind};
use crate::span::{Span, Spanned};

use super::block::RawCommand;
use super::cursor::Cursor;
use super::expr::Position;
use super::signature::{self, definition_name};
use super::value::{self, BraceShape, Hint};
use super::{St, block, cellpath, collections, expr, literal, pattern};

/// Keywords that start a statement and can only appear at the head of a
/// pipeline (they parse their own `=` and `{}` arguments).
pub fn is_statement_keyword(text: &str) -> bool {
    matches!(
        text,
        "def" | "extern" | "let" | "mut" | "const" | "for" | "alias" | "module" | "use" | "export" | "export-env"
    )
}

/// nu's `ALIASABLE_PARSER_KEYWORDS`: keywords an alias may name.
const ALIASABLE_KEYWORDS: &[&str] = &["if", "match", "try", "overlay", "overlay hide", "overlay new", "overlay use"];

/// nu's `UNALIASABLE_PARSER_KEYWORDS`.
const UNALIASABLE_KEYWORDS: &[&str] = &[
    "alias",
    "const",
    "def",
    "extern",
    "module",
    "use",
    "export",
    "export alias",
    "export const",
    "export def",
    "export extern",
    "export module",
    "export use",
    "for",
    "loop",
    "while",
    "return",
    "break",
    "continue",
    "let",
    "mut",
    "hide",
    "export-env",
    "source-env",
    "source",
    "run",
    "where",
    "plugin use",
];

/// Names that cannot be given to a definition because the parser treats them
/// specially (`nu-parser`'s aliasable and unaliasable keyword tables, the
/// multi-word entries included: `def "export def"` is refused too).
pub fn is_parser_keyword(name: &str) -> bool {
    ALIASABLE_KEYWORDS.contains(&name) || UNALIASABLE_KEYWORDS.contains(&name)
}

/// Variable names nu reserves (`NameIsBuiltinVar`).
pub fn is_reserved_variable(name: &str) -> bool {
    matches!(name, "in" | "nu" | "env" | "ans")
}

/// Refuse a reserved variable name in a declaration.
pub fn check_variable_name(name: &str, span: Span) -> PResult<()> {
    match is_reserved_variable(name) {
        true => Err(cut(Diagnostic::message(format!("`{name}` used as variable name"), span)
            .with_help(format!("`${name}` is a built-in variable and cannot be declared")))),
        false => Ok(()),
    }
}

/// Reject a `def`/`extern`/`alias` name that is a parser keyword, or that
/// nu refuses because it could never be called: one containing `#`, `^` or
/// `%`, or one that reads as a number or a filesize (`def 1kb`).
fn check_definition_name(name: &Spanned<Cow<'_, str>>, what: &str) -> PResult<()> {
    if is_parser_keyword(&name.item) {
        return Err(cut(Diagnostic::message(
            format!("cannot use parser keyword `{}` as {what} name", name.item),
            name.span,
        )
        .with_help("choose a different name; this word is parsed specially by Nushell")));
    }
    let text: &str = &name.item;
    if text.contains(['#', '^', '%'])
        || literal::parse_int(text).is_some()
        || literal::parse_float(text).is_some()
        || literal::filesize(text).is_some_and(|f| f.is_ok())
    {
        return Err(cut(Diagnostic::message(format!("{what} name not supported"), name.span)
            .with_help("a name may not contain `#`, `^` or `%`, or read as a number or filesize")));
    }
    Ok(())
}

/// The name item of a `def`/`extern`: a string literal. Like nu, a name
/// containing `[` or `(` (even quoted) is "no space between name and
/// parameters", and a `$` item is not a string.
fn command_name<'a>(st: St<'_, 'a>, tok: &Token) -> PResult<Spanned<Cow<'a, str>>> {
    let text = st.tok(tok);
    if let Some(at) = text.find(['[', '(']) {
        let span = Span::point(tok.span.start + at);
        return Err(cut(Diagnostic::message("no space between name and parameters", span)
            .with_help("consider adding a space between the command's name and its parameters")));
    }
    if text.starts_with('$') {
        return Err(cut(Diagnostic::expected("string", tok.span).with_help("the name of a definition is a string")));
    }
    definition_name(st, tok.span)
}

/// What nu does with the item at the start of a keyword's positional argument.
enum Boundary {
    /// `--help`/`-h`: the whole statement is an ordinary call showing help.
    Help,
    /// Nothing special (a `--` end-of-options marker was consumed and ignored).
    Arg,
}

/// Look at the item at a positional boundary of the keyword `kw`: `--help`
/// and `-h` are its help flag, `--` is consumed and ignored (nu keeps nothing
/// of it), any other `-x` is a flag the keyword does not have (`return -1`),
/// unless `extra` allows it.
fn boundary(st: St<'_, '_>, c: &mut Cursor<'_>, kw: &str, extra: &[&str]) -> PResult<Boundary> {
    boundary_with(st, c, kw, extra, true)
}

/// [`boundary`] with a choice about `--`: the statements nu parses by
/// position (`alias`, `module`, `let`, `mut`, `const`, `export-env`) never
/// see the end-of-options marker, so `alias -- x = ls` has no `=` where one
/// is expected.
fn boundary_with(st: St<'_, '_>, c: &mut Cursor<'_>, kw: &str, extra: &[&str], dashdash: bool) -> PResult<Boundary> {
    let Some(tok) = c.peek().filter(|t| t.kind == TokenKind::Item) else { return Ok(Boundary::Arg) };
    if dashdash && end_of_options(st, c) {
        return Ok(Boundary::Arg);
    }
    let text = st.tok(tok);
    match text {
        "--help" | "-h" => Ok(Boundary::Help),
        "--" if dashdash => {
            st.ignore(tok.span);
            c.next();
            Ok(Boundary::Arg)
        }
        "--" => Ok(Boundary::Arg),
        _ if text.starts_with('-') && text.len() > 1 && !extra.contains(&text) => {
            Err(cut(Diagnostic::message(format!("the `{kw}` command doesn't have flag `{text}`"), tok.span)
                .with_help("use `--help` to see available flags")))
        }
        _ => Ok(Boundary::Arg),
    }
}

/// Whether a `--` marker was consumed earlier in the statement: from then on
/// nu looks for no flags at all, so a second `--` is a positional (`try {}
/// -- catch {} --` has one too many) and `return -- --help` returns a string.
fn end_of_options(st: St<'_, '_>, c: &Cursor<'_>) -> bool {
    c.all()[..c.position()].iter().any(|t| t.kind == TokenKind::Item && st.tok(t) == "--")
}

/// [`boundary`] for the statements that keep parsing after `--help`: the
/// flag is consumed and remembered in `help`. nu parses every positional
/// that follows it as usual (`match 1 --help :{}` has no match block,
/// `return --help 1 2` has an extra positional) and only forgives the missing
/// ones; the statement is then an ordinary call showing help.
fn boundary_help(st: St<'_, '_>, c: &mut Cursor<'_>, kw: &str, dashdash: bool, help: &mut bool) -> PResult<()> {
    // nu takes every flag at the boundary: `extern foo --help --help`.
    loop {
        let before = c.position();
        if let Boundary::Help = boundary_with(st, c, kw, &[], dashdash)? {
            c.next();
            *help = true;
        } else if c.position() == before {
            return Ok(());
        }
    }
}

/// The next item of a statement, or nothing when the cursor is at its end
/// and `--help` forgives the missing positional (see [`boundary_help`]).
fn item_or_help(c: &mut Cursor<'_>, help: bool, what: &'static str) -> PResult<Option<Token>> {
    if help && c.at_end() {
        return Ok(None);
    }
    c.expect_item(what).map(Some)
}

/// The finished statement, unless `--help` turned it into a call.
fn done<'a>(st: St<'_, 'a>, full: Cursor<'_>, help: bool, expr: Expr<'a>) -> PResult<Expr<'a>> {
    if help { help_call(st, full) } else { Ok(expr) }
}

/// `kw --help`: the statement parsed as an ordinary call, as nu does.
fn help_call<'a>(st: St<'_, 'a>, full: Cursor<'_>) -> PResult<Expr<'a>> {
    if let Some(span) = full.span() {
        st.forget_ignored_from(span.start);
    }
    expr::parse_call(st, full)
}

/// Parse a keyword construct or, failing that, a command call.
pub fn keyword_or_call<'a>(st: St<'_, 'a>, c: Cursor<'_>) -> PResult<Expr<'a>> {
    let Some(first) = c.peek() else {
        return Err(cut(Diagnostic::expected("command", c.end_span())));
    };
    if first.kind != TokenKind::Item {
        return Err(cut(Diagnostic::expected("command", first.span)));
    }
    let head = st.tok(first);
    // `if`, `loop`, ... are ordinary commands in Nushell and may be shadowed by
    // a user definition, unlike the statement keywords.
    let head = if !is_statement_keyword(head) && st.is_declared_command(head) { "" } else { head };
    let (ctx, result) = match head {
        "def" => ("def", def_stmt(st, c)),
        "extern" => ("extern", extern_stmt(st, c)),
        "let" => ("let", binding_stmt(st, c, BindingKind::Let)),
        "mut" => ("mut", binding_stmt(st, c, BindingKind::Mut)),
        "const" => ("const", binding_stmt(st, c, BindingKind::Const)),
        "for" => ("for", for_stmt(st, c)),
        "alias" => ("alias", alias_stmt(st, c, false)),
        "module" => ("module", module_stmt(st, c)),
        "use" => ("use", use_stmt(st, c)),
        "export" => ("export", export_stmt(st, c)),
        "export-env" => ("export-env", export_env_stmt(st, c)),
        "if" => ("if", if_stmt(st, c)),
        "match" => ("match", match_stmt(st, c)),
        "while" => ("while", while_stmt(st, c)),
        "loop" => ("loop", loop_stmt(st, c)),
        "try" => ("try", try_stmt(st, c)),
        "return" => ("return", return_stmt(st, c)),
        "break" => ("break", simple_stmt(st, c, ExprKind::Break)),
        "continue" => ("continue", simple_stmt(st, c, ExprKind::Continue)),
        "where" => ("where", where_stmt(st, c)),
        _ => ("command call", expr::parse_call(st, c)),
    };
    result.map_err(|e| e.map(|d| d.with_context(ctx)))
}

/// Parse one command (a pipeline element) as collected by the block parser.
/// `in_pipeline` is set when it is one of several elements.
pub fn parse_command<'a>(
    st: St<'_, 'a>,
    raw: &RawCommand,
    in_pipeline: bool,
) -> PResult<(Expr<'a>, Option<Redirection<'a>>)> {
    let position = if in_pipeline { Position::Element } else { Position::Statement };
    let expr = match raw.attributes.as_slice() {
        [] => expr::parse_expression(st, raw.cursor(), position)?,
        attribute_lines => {
            let attributes = attribute_lines.iter().map(|a| attribute(st, a)).collect::<PResult<Vec<_>>>()?;
            let words: Vec<&str> = raw.parts.iter().take(2).map(|t| st.tok(t)).collect();
            let is_definition = matches!(words.as_slice(), ["def" | "extern", ..] | ["export", "def" | "extern"]);
            let item = match (is_definition, raw.parts.first()) {
                (true, Some(first)) if in_pipeline => {
                    return Err(cut(Diagnostic::new(ErrorKind::KeywordInPipeline(st.tok(first).to_string()), first.span)));
                }
                (true, _) => keyword_or_call(st, raw.cursor())?,
                (false, Some(first)) => {
                    return Err(cut(Diagnostic::message("attributes must be followed by a definition", first.span)
                        .with_help("only `def`, `extern`, `export def` and `export extern` take attributes")));
                }
                (false, None) => {
                    let last = attributes.last().map_or(Span::point(raw.end), |a| a.span);
                    return Err(cut(Diagnostic::message("attributes must be followed by a definition", last.past())
                        .with_help("put a `def` or `extern` on the line after the attributes")));
                }
            };
            let span = attributes[0].span.merge(item.span);
            Expr::new(ExprKind::AttributeBlock(AttributeBlock { attributes, item: Box::new(item) }), span)
        }
    };
    let redirection = build_redirection(st, raw)?;
    if let ExprKind::ExportEnv(_) = expr.kind
        && redirection.is_some()
    {
        // nu never looks at a redirection on `export-env`.
        for (op, target) in &raw.redirections {
            st.ignore(op.span);
            if let Some(t) = target {
                st.ignore(t.span);
            }
        }
        return Ok((expr, None));
    }
    if redirection.is_some() && is_redirect_forbidden(&expr) {
        let at = raw.redirections.first().map_or(expr.span, |(op, _)| op.span);
        return Err(cut(Diagnostic::message("this statement cannot be redirected", at)));
    }
    Ok((expr, redirection))
}

fn is_redirect_forbidden(expr: &Expr<'_>) -> bool {
    match &expr.kind {
        ExprKind::Def(_)
        | ExprKind::Extern(_)
        | ExprKind::Let(_)
        | ExprKind::Mut(_)
        | ExprKind::Const(_)
        | ExprKind::For(_)
        | ExprKind::Alias(_)
        | ExprKind::Module(_)
        | ExprKind::Use(_)
        | ExprKind::Export(_)
        | ExprKind::ExportEnv(_)
        | ExprKind::AttributeBlock(_) => true,
        // `overlay <anything>` is refused by name before its arguments are looked at.
        ExprKind::Call(call) => {
            call.head.name.split(' ').next() == Some("overlay")
                || fixed_signature(call).is_some_and(|s| !s.redirectable)
        }
        _ => false,
    }
}

fn build_redirection<'a>(st: St<'_, 'a>, raw: &RawCommand) -> PResult<Option<Redirection<'a>>> {
    let mut out: Option<Redirection<'a>> = None;
    for (op, target) in &raw.redirections {
        let target = match target {
            Some(tok) => RedirectTarget::File {
                op: *op,
                append: op.item.is_append(),
                path: Box::new(value::value(st, tok.span, Hint::Any)?),
            },
            None => RedirectTarget::Pipe { op: *op },
        };
        out = Some(match (out.take(), op.item.source()) {
            (None, source) => Redirection::Single { source, target },
            (Some(Redirection::Single { source: RedirectSource::Stdout, target: out_t }), RedirectSource::Stderr) => {
                Redirection::Separate { out: out_t, err: target }
            }
            (Some(Redirection::Single { source: RedirectSource::Stderr, target: err_t }), RedirectSource::Stdout) => {
                Redirection::Separate { out: target, err: err_t }
            }
            (Some(prev), _) => {
                return Err(cut(Diagnostic::message("multiple redirections of the same stream", op.span)
                    .with_help(format!("the stream is already redirected at {}", prev.span()))));
            }
        });
    }
    Ok(out)
}

/// `@name args`. The name must be non-empty; whether `attr <name>` exists is
/// the consumer's business (it may come from a `use`d module), but the
/// built-in attributes get their arguments checked like any keyword command.
fn attribute<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Attribute<'a>> {
    let end = tokens.last().map_or(0, |t| t.span.end);
    let mut c = Cursor::new(tokens, end);
    let first = c.expect_item("attribute")?;
    if st.tok(&first) == "@" {
        return Err(cut(Diagnostic::expected("attribute name after `@`", first.span)));
    }
    let head = expr::resolve_head(st, first, &mut c, "attr ");
    let name_span = Span::new(first.span.start + 1, head.span.end);
    let full = format!("attr {}", head.name);
    let args = expr::parse_args(st, c)?;
    let call = Call { head, args, sigil: None };
    check_fixed_signature_named(st, &full, &call)?;
    let Call { head, args, .. } = call;
    Ok(Attribute { span: Span::new(first.span.start, end), name: Spanned::new(head.name, name_span), args })
}

/// The `{ ... }` item that must end a statement, or an error.
fn block_item<'a>(st: St<'_, 'a>, tok: &Token, what: &'static str) -> PResult<Block<'a>> {
    if tok.kind != TokenKind::Item || !st.tok(tok).starts_with('{') {
        return Err(cut(Diagnostic::expected(what, tok.span)));
    }
    value::block_body(st, tok.span)
}

// --- def / extern -----------------------------------------------------------

/// nu's `parse_full_signature`: the items nu hands to the signature argument
/// of `def`/`extern` are everything up to the body. One item is the
/// signature; two of which the second starts with `{` is the signature and
/// an item nu drops on the floor; otherwise the input/output types follow a
/// `:` (attached to the signature or standing alone), possibly none.
fn full_signature<'a>(st: St<'_, 'a>, items: &[Token], external: bool) -> PResult<Signature<'a>> {
    let signature_item = |tok: &Token| -> PResult<Span> {
        let text = st.tok(tok);
        match tok.kind == TokenKind::Item && text.starts_with(['[', '(']) {
            true => Ok(tok.span),
            false => Err(cut(Diagnostic::expected("signature like `[param: type]`", tok.span))),
        }
    };
    let (first, rest) = match items {
        [] => return Err(cut(Diagnostic::expected("signature", Span::point(0)))),
        [only] => return signature::parse_signature(st, signature_item(only)?, external),
        [first, second] if st.tok(second).starts_with('{') => {
            st.ignore(second.span);
            return signature::parse_signature(st, signature_item(first)?, external);
        }
        [first, rest @ ..] => (first, rest),
    };
    let sig_span = signature_item(first)?;
    let (sig_span, type_items) = match st.tok(first).strip_suffix(':') {
        Some(_) => (Span::new(sig_span.start, sig_span.end - 1), rest),
        None if st.tok(&rest[0]) == ":" => (sig_span, &rest[1..]),
        None => return Err(cut(Diagnostic::expected("`:` before the input/output types", rest[0].span))),
    };
    let mut sig = signature::parse_signature(st, sig_span, external)?;
    if let (Some(first), Some(last)) = (type_items.first(), type_items.last()) {
        let span = first.span.merge(last.span);
        sig.io_types = signature::parse_io_types(st, span)?;
        sig.io_span = Some(span);
        sig.span = sig.span.merge(span);
    }
    Ok(sig)
}

fn is_help(st: St<'_, '_>, tok: &Token) -> bool {
    tok.kind == TokenKind::Item && matches!(st.tok(tok), "--help" | "-h")
}

fn def_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let full = c;
    let kw = c.expect_item("def")?;
    let mut flags = Vec::new();
    let mut help = false;
    def_flags(st, &mut c, &mut flags)?;
    boundary_help(st, &mut c, "def", true, &mut help)?;
    let Some(name_tok) = item_or_help(&mut c, help, "command name")? else {
        return help_call(st, full);
    };
    let name = command_name(st, &name_tok)?;
    check_definition_name(&name, "command")?;
    // nu also accepts the flags after the name: `def foo --env [] { }`.
    def_flags(st, &mut c, &mut flags)?;
    boundary_help(st, &mut c, "def", true, &mut help)?;
    // The signature positional takes every remaining item but the last, which
    // is the body's: `def foo [] {} --help` drops the `{}` (two-item form of
    // `full_signature`) and `def foo [] {} {} --help` has no colon.
    let rest = c.rest();
    let (sig_items, body_tok) = match rest {
        [] if help => return help_call(st, full),
        [] => return Err(cut(Diagnostic::expected("signature", c.end_span()))),
        [_] => (rest, None),
        [sig_items @ .., body_tok] => (sig_items, Some(body_tok)),
    };
    let signature = full_signature(st, sig_items, false)?;
    let (body_params, body) = match body_tok {
        Some(tok) if is_help(st, tok) && !end_of_options(st, &c) => return help_call(st, full),
        Some(tok) => closure_body(st, tok, "definition body closure { ... }")?,
        None if help => return help_call(st, full),
        None => return Err(cut(Diagnostic::expected("block", rest[0].span.past()))),
    };
    if flags.iter().any(|f| f.item == DefFlag::Wrapped) {
        check_wrapped(&signature, name.span)?;
    }
    let span = kw.span.merge(body_tok.map_or(signature.span, |t| t.span));
    let def = Expr::new(ExprKind::Def(Def { flags, name, signature, body_params, body }), span);
    done(st, full, help, def)
}

/// The body of a `def`: nu parses it as a closure without looking at its
/// shape first (`def f [] {a: 1}` calls `a:`), and drops its parameters.
fn closure_body<'a>(st: St<'_, 'a>, tok: &Token, what: &'static str) -> PResult<(Option<Signature<'a>>, Block<'a>)> {
    if tok.kind != TokenKind::Item || !st.tok(tok).starts_with('{') {
        return Err(cut(Diagnostic::expected(what, tok.span)));
    }
    match value::brace_shape(st, tok.span)? {
        BraceShape::ClosureParams => {
            let closure = value::closure_parts(st, tok.span)?;
            Ok((closure.params, closure.body))
        }
        _ => Ok((None, value::block_unchecked(st, tok.span)?)),
    }
}

/// `def --wrapped` needs a rest parameter that is untyped or a `string`.
fn check_wrapped(signature: &Signature<'_>, name_span: Span) -> PResult<()> {
    let rest = signature.params.iter().find(|p| matches!(p.kind, crate::ast::ParamKind::Rest));
    let Some(rest) = rest else {
        return Err(cut(Diagnostic::message("missing required positional argument", name_span).with_help(
            "def --wrapped must have a ...rest-like positional argument; add `...rest: string` to the signature",
        )));
    };
    match &rest.ty {
        None => Ok(()),
        Some(ty) if ty.kind == crate::ast::TypeKind::String => Ok(()),
        Some(ty) => Err(cut(Diagnostic::message("type mismatch", ty.span).with_help(format!(
            "the ...rest-like positional argument of `def --wrapped` supports only strings; change the type of ...{} to `string`",
            rest.name.item
        )))),
    }
}

/// `--env` / `--wrapped` items at the cursor.
fn def_flags(st: St<'_, '_>, c: &mut Cursor<'_>, flags: &mut Vec<Spanned<DefFlag>>) -> PResult<()> {
    while let Some(tok) = c.peek().filter(|t| t.kind == TokenKind::Item && st.tok(t).starts_with("--")) {
        let flag = match st.tok(tok) {
            "--env" => DefFlag::Env,
            "--wrapped" => DefFlag::Wrapped,
            "--help" | "--" => return Ok(()),
            other => {
                return Err(cut(Diagnostic::message(format!("the `def` command doesn't have flag `{other}`"), tok.span)
                    .with_help("`def` accepts `--env` and `--wrapped`")));
            }
        };
        flags.push(Spanned::new(flag, tok.span));
        c.next();
    }
    Ok(())
}

fn extern_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let full = c;
    let kw = c.expect_item("extern")?;
    let mut help = false;
    boundary_help(st, &mut c, "extern", true, &mut help)?;
    let Some(name_tok) = item_or_help(&mut c, help, "command name")? else {
        return help_call(st, full);
    };
    let name = command_name(st, &name_tok)?;
    check_definition_name(&name, "command")?;
    boundary_help(st, &mut c, "extern", true, &mut help)?;
    let rest = c.rest();
    if rest.is_empty() {
        if help {
            return help_call(st, full);
        }
        return Err(cut(Diagnostic::expected("signature", c.end_span())));
    }
    // The signature argument takes every remaining item, so a body after it
    // (the old `extern-wrapped`) is dropped by nu without a look.
    let signature = full_signature(st, rest, true)?;
    let span = kw.span.merge(rest.last().map_or(signature.span, |t| t.span));
    done(st, full, help, Expr::new(ExprKind::Extern(Extern { name, signature }), span))
}

// --- let / mut / const ------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum BindingKind {
    Let,
    Mut,
    Const,
}

/// `let`, `mut` and `const`: `KW name[: type] [= value...]`. Only `let` may
/// leave the value out.
fn binding_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>, kind: BindingKind) -> PResult<Expr<'a>> {
    let full = c;
    let (keyword, expr_kind): (&str, fn(Binding<'a>) -> ExprKind<'a>) = match kind {
        BindingKind::Let => ("let", ExprKind::Let),
        BindingKind::Mut => ("mut", ExprKind::Mut),
        BindingKind::Const => ("const", ExprKind::Const),
    };
    let items = c.all();
    let kw = c.expect_item(keyword)?;
    if let Boundary::Help = boundary_with(st, &mut c, keyword, &[], false)? {
        return help_call(st, full);
    }
    let name_tok = match c.peek() {
        Some(tok) if tok.kind == TokenKind::Item => *tok,
        _ => return Err(cut(Diagnostic::expected("variable name", c.here()))),
    };
    c.next();
    let (name, typed) = variable_declaration(st, &name_tok)?;
    let eq_idx = items.iter().position(|t| matches!(t.kind, TokenKind::Assign(_))).unwrap_or(items.len());
    let eq_tok = items.get(eq_idx).copied();
    if let Some(eq_tok) = eq_tok
        && eq_tok.kind != TokenKind::Assign(AssignOp::Assign)
    {
        return Err(cut(Diagnostic::expected("`=`", eq_tok.span)));
    }
    if eq_tok.is_none() && !typed {
        // `let x --help`
        if let Boundary::Help = boundary(st, &mut c, keyword, &[])? {
            return help_call(st, full);
        }
    }
    let ty = type_after_name(st, &items[c.position()..eq_idx], typed, name_tok.span.past())?;
    let (value, end) = match eq_tok {
        Some(eq_tok) => {
            let rhs = c.slice(eq_idx + 1..items.len());
            let Some(rhs_span) = rhs.span() else {
                return Err(cut(Diagnostic::expected("value after `=`", eq_tok.span.past())));
            };
            (Some(block::parse_block(st, rhs, rhs_span)), rhs_span)
        }
        None if kind != BindingKind::Let => {
            return Err(cut(Diagnostic::message("missing required positional argument", items[items.len() - 1].span.past())
                .with_help(format!("`{keyword}` needs a value: `{keyword} {} = <value>`", name.item))));
        }
        None => (None, items[items.len() - 1].span),
    };
    let binding = Binding { name, ty, eq: eq_tok.map(|t| t.span), value };
    Ok(Expr::new(expr_kind(binding), kw.span.merge(end)))
}

/// A declared variable name: `x`, `$x`, or `x:` (followed by a type).
/// Returns the name and whether a type follows.
fn variable_declaration<'a>(st: St<'_, 'a>, tok: &Token) -> PResult<(Spanned<&'a str>, bool)> {
    let text = st.tok(tok);
    let start = tok.span.start + usize::from(text.starts_with('$'));
    let text = text.strip_prefix('$').unwrap_or(text);
    let (name, typed) = match text.strip_suffix(':') {
        Some(name) => (name, true),
        None => (text, false),
    };
    if name.contains([' ', '"', '\'', '`']) || !cellpath::is_identifier(name) {
        return Err(cut(Diagnostic::expected("valid variable name", tok.span)
            .with_help("variable names may not contain spaces, quotes or `.[({+-*^%/=!<>&|`")));
    }
    let span = Span::new(start, start + name.len());
    check_variable_name(name, span)?;
    Ok((Spanned::new(name, span), typed))
}

/// The type annotation items between a declared name and `=`: `x: int`,
/// `x : int`, `x: record<a: int, b: string>` (several items, re-lexed as one).
fn type_after_name<'a>(
    st: St<'_, 'a>,
    items: &[Token],
    typed: bool,
    after_name: Span,
) -> PResult<Option<crate::ast::TypeAnnotation<'a>>> {
    // `let a : int`: like Nushell, the colon must be attached to the name.
    if let Some(first) = items.first()
        && st.tok(first) == ":"
    {
        return Err(cut(Diagnostic::new(ErrorKind::ExtraTokens, first.span)));
    }
    match (typed, items.first(), items.last()) {
        (true, Some(first), Some(last)) => Ok(Some(signature::parse_type(st, first.span.merge(last.span))?)),
        (true, ..) => Err(cut(Diagnostic::expected("type after `:`", after_name))),
        (false, Some(first), _) => Err(cut(Diagnostic::new(ErrorKind::ExtraTokens, first.span))),
        (false, ..) => Ok(None),
    }
}

// --- for / alias / module / use / export ------------------------------------

fn for_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let full = c;
    let kw = c.expect_item("for")?;
    let mut help = false;
    boundary_help(st, &mut c, "for", true, &mut help)?;
    let Some(var_tok) = item_or_help(&mut c, help, "loop variable")? else {
        return help_call(st, full);
    };
    let (var, typed) = variable_declaration(st, &var_tok)?;
    // The variable positional runs up to the `in` keyword, so a type after
    // `x:` is every item before it (`record<a: int, b: string>` is four items)
    // and `for x: int --help in [] {}` has the unknown type `int --help`.
    let ty = match typed {
        true => {
            let rest = c.rest();
            let count = rest.iter().position(|t| t.kind == TokenKind::Item && st.tok(t) == "in").unwrap_or(rest.len());
            let (Some(first), Some(last)) = (rest.first(), rest.get(count.wrapping_sub(1))) else {
                return Err(cut(Diagnostic::expected("type", var_tok.span.past())));
            };
            let ty = signature::parse_type(st, first.span.merge(last.span))?;
            for _ in 0..count {
                c.next();
            }
            Some(ty)
        }
        false => None,
    };
    boundary_help(st, &mut c, "for", true, &mut help)?;
    let Some(in_tok) = item_or_help(&mut c, help, "`in`")? else {
        return help_call(st, full);
    };
    if st.tok(&in_tok) != "in" {
        return Err(cut(Diagnostic::new(ErrorKind::ExpectedKeyword("in"), in_tok.span)));
    }
    // nu reserves the last item for the block before it parses the keyword's
    // argument, so `for x in []` and `for x --help in []` lack the argument of
    // `in` (KeywordMissingArgument), help or not.
    if c.rest().len() < 2 {
        return Err(cut(Diagnostic::message("missing argument to `in`", in_tok.span)
            .with_help("`for` needs a value to iterate and a block: `for x in [1 2] { }`")));
    }
    let iterable = value::value(st, c.expect_item("value to iterate")?.span, Hint::Any)?;
    boundary_help(st, &mut c, "for", true, &mut help)?;
    let Some(body_tok) = item_or_help(&mut c, help, "block")? else {
        return help_call(st, full);
    };
    let body = block_item(st, &body_tok, "block")?;
    boundary_help(st, &mut c, "for", true, &mut help)?;
    c.expect_end()?;
    let span = kw.span.merge(body_tok.span);
    let for_ = For { var, ty, in_keyword: in_tok.span, iterable: Box::new(iterable), body };
    done(st, full, help, Expr::new(ExprKind::For(for_), span))
}

/// `alias name = target`; `exported` for `export alias`, which nu lets go
/// without a target (`export alias x =`) because its length check counts the
/// `export` word.
fn alias_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>, exported: bool) -> PResult<Expr<'a>> {
    let full = c;
    let kw = c.expect_item("alias")?;
    // nu parses the alias call first and returns it when it is a help call;
    // otherwise the `=` must sit right after the name, so `alias --help x =
    // ls` and `alias x --help extra` are "missing sign" and `--` is a name.
    if let Boundary::Help = boundary_with(st, &mut c, "alias", &[], false)? {
        c.next();
        return if c.at_end() { help_call(st, full) } else { Err(cut(Diagnostic::expected("`=`", c.here()))) };
    }
    let name_tok = c.expect_item("alias name")?;
    if st.tok(&name_tok).starts_with('-') {
        return Err(cut(Diagnostic::message("alias name not supported", name_tok.span)
            .with_help("a bare alias name cannot start with `-`; quote it")));
    }
    let name = definition_name(st, name_tok.span)?;
    // `alias alias --help` is a help call before the name is checked.
    if let Boundary::Help = boundary_with(st, &mut c, "alias", &[], false)? {
        c.next();
        return if c.at_end() { help_call(st, full) } else { Err(cut(Diagnostic::expected("`=`", c.here()))) };
    }
    check_definition_name(&name, "alias")?;
    let eq_tok = match c.next() {
        Some(tok) if tok.kind == TokenKind::Assign(AssignOp::Assign) => *tok,
        _ => return Err(cut(Diagnostic::expected("`=`", c.here()))),
    };
    // Nushell hands everything after `=` to the call parser as plain words,
    // so `alias ll = ls | length` is `ls` with the arguments `|` and `length`,
    // and `alias x = FOO=1 ls` calls the external command `FOO=1`.
    let words: Vec<Token> = c.rest().iter().map(|t| Token { kind: TokenKind::Item, span: t.span }).collect();
    let Some(first) = words.first() else {
        if exported {
            let alias = Alias { name, eq: eq_tok.span, value: None };
            return Ok(Expr::new(ExprKind::Alias(alias), kw.span.merge(eq_tok.span)));
        }
        return Err(cut(Diagnostic::expected("command after `=`", eq_tok.span.past())));
    };
    let first_text = st.tok(first);
    if !matches!(first_text, "if" | "match") && value::looks_like_value(first_text) {
        return Err(cut(Diagnostic::message("cannot create an alias to an expression", first.span)
            .with_help("an alias names a command and its arguments, such as `alias ll = ls -l`")));
    }
    // Like nu, only the aliasable keywords (`if`, `match`, `try`, `overlay ...`)
    // may be aliased; `alias d = def` is an error.
    let target: String = words.iter().take(2).map(|t| st.tok(t)).collect::<Vec<_>>().join(" ");
    let single = first_text;
    if UNALIASABLE_KEYWORDS.contains(&target.as_str()) || UNALIASABLE_KEYWORDS.contains(&single) {
        return Err(cut(Diagnostic::message("cannot create an alias to a parser keyword", first.span)
            .with_help(format!("only {} can be aliased", ALIASABLE_KEYWORDS.join(", ")))));
    }
    // nu forgives missing positionals and flag values in an alias target
    // (`alias x = overlay new`), but not unknown flags.
    let value = expr::parse_call_with(st, Cursor::new(&words, c.end_span().start), true)?;
    let span = kw.span.merge(value.span);
    Ok(Expr::new(ExprKind::Alias(Alias { name, eq: eq_tok.span, value: Some(Box::new(value)) }), span))
}

/// The name of a `module`: a string literal, quoted or not. nu's `module`
/// takes a `string` argument and then needs a literal: a variable, a
/// subexpression or an interpolation is not one.
fn module_name<'a>(st: St<'_, 'a>, tok: &Token) -> PResult<Expr<'a>> {
    let text = st.tok(tok);
    if text.starts_with(['$', '(', '{']) {
        return Err(cut(Diagnostic::expected("string", tok.span).with_help("the name of a module must be a literal")));
    }
    let name = value::value(st, tok.span, Hint::String)?;
    match name.kind {
        ExprKind::String(_) => Ok(name),
        _ => Err(cut(Diagnostic::expected("string", tok.span).with_help("the name of a module must be a literal"))),
    }
}

fn module_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let full = c;
    let kw = c.expect_item("module")?;
    if let Boundary::Help = boundary_with(st, &mut c, "module", &[], false)? {
        return help_call(st, full);
    }
    let name_tok = c.expect_item("module name or path")?;
    // nu first parses `module` as a call to find `--help`, and that parse
    // consumes a `--`: the item after it must then be a name (`module -- {}`
    // is a type mismatch, `module --` lacks its name). The statement itself
    // then takes `--` as the name.
    if st.tok(&name_tok) == "--" {
        match c.peek() {
            None => return Err(cut(Diagnostic::expected("module name after `--`", name_tok.span.past()))),
            Some(tok) if st.tok(tok).starts_with(['{', '$', '(']) => {
                return Err(cut(Diagnostic::expected("string", tok.span).with_help("the name of a module must be a literal")));
            }
            Some(_) => {}
        }
    }
    let name = module_name(st, &name_tok)?;
    if let Boundary::Help = boundary_with(st, &mut c, "module", &[], false)? {
        return help_call(st, full);
    }
    let mut end = name_tok.span;
    let body = match c.peek() {
        Some(tok) if tok.kind == TokenKind::Item && st.tok(tok).starts_with('{') => {
            end = tok.span;
            c.next();
            st.push_scope();
            let body = value::block_body(st, tok.span);
            st.pop_scope();
            let body = body?;
            // Like nu, a module body holds declarations only.
            for pipeline in &body.pipelines {
                let first = &pipeline.elements[0].expr;
                if !is_module_item(first) {
                    return Err(cut(Diagnostic::new(
                        ErrorKind::ExpectedKeyword("def, const, extern, alias, use, module, export or export-env"),
                        first.span,
                    )
                    .with_help("a module body can only declare things; put code in `export-env` or a `def`")));
                }
            }
            Some(body)
        }
        Some(tok) => return Err(cut(Diagnostic::expected("block", tok.span))),
        None => None,
    };
    if let Boundary::Help = boundary_with(st, &mut c, "module", &[], false)? {
        return help_call(st, full);
    }
    c.expect_end()?;
    Ok(Expr::new(ExprKind::Module(Module { name: Box::new(name), body }), kw.span.merge(end)))
}

fn is_module_item(expr: &Expr<'_>) -> bool {
    match &expr.kind {
        ExprKind::Def(_)
        | ExprKind::Extern(_)
        | ExprKind::Alias(_)
        | ExprKind::Use(_)
        | ExprKind::Module(_)
        | ExprKind::Export(_)
        | ExprKind::ExportEnv(_)
        | ExprKind::Const(_) => true,
        ExprKind::AttributeBlock(a) => is_module_item(&a.item),
        _ => false,
    }
}

fn use_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let full = c;
    let kw = c.expect_item("use")?;
    // Flags are looked for before every argument of `use`.
    for tok in c.rest() {
        if is_help(st, tok) {
            return help_call(st, full);
        }
        let text = st.tok(tok);
        if tok.kind == TokenKind::Item && text.starts_with('-') && text.len() > 1 && text != "--" {
            return Err(cut(Diagnostic::message(format!("the `use` command doesn't have flag `{text}`"), tok.span)
                .with_help("use `--help` to see available flags")));
        }
    }
    boundary(st, &mut c, "use", &[])?;
    let module_tok = c.expect_item("module name or path")?;
    let module = match st.tok(&module_tok) {
        "null" => Expr::new(ExprKind::Nothing, module_tok.span),
        _ => {
            let module = value::value(st, module_tok.span, Hint::String)?;
            if let ExprKind::Record(_) = module.kind {
                return Err(cut(Diagnostic::expected("string", module_tok.span).with_help("found a record")));
            }
            module
        }
    };
    // After `use null` nu parses the members and then never looks at them.
    let noop = matches!(module.kind, ExprKind::Nothing);
    let mut members = Vec::new();
    let mut end = module_tok.span;
    while let Some(tok) = c.next() {
        if st.tok(tok) == "--" {
            st.ignore(tok.span);
            continue;
        }
        if members
            .last()
            .is_some_and(|m: &UseMember<'_>| matches!(m.kind, UseMemberKind::Glob | UseMemberKind::List(_)))
        {
            return Err(cut(Diagnostic::message(
                "a `*` or `[...]` member can only be at the end of an import pattern",
                tok.span,
            )));
        }
        members.push(use_member(st, tok, noop)?);
        end = tok.span;
    }
    Ok(Expr::new(ExprKind::Use(Use { module: Box::new(module), members }), kw.span.merge(end)))
}

/// One member of an import pattern, classified as nu does from the parsed
/// value: a string is a name (`*` the glob), a list gives its string items,
/// a variable, subexpression or `key: value` record is parsed and ignored,
/// and anything else is a "wrong import pattern" unless `noop` (after
/// `use null`) makes every member irrelevant.
fn use_member<'a>(st: St<'_, 'a>, tok: &Token, noop: bool) -> PResult<UseMember<'a>> {
    let text = st.tok(tok);
    let span = tok.span;
    let wrong = |expr: &Expr<'_>| {
        cut(Diagnostic::message("wrong import pattern structure", expr.span)
            .with_help("only strings and lists of strings can be imported"))
    };
    let kind = match text.as_bytes()[0] {
        b'*' if text == "*" => UseMemberKind::Glob,
        b'[' => return use_member_list(st, tok),
        b'$' | b'(' | b'{' => {
            let expr = value::value(st, span, Hint::Any)?;
            match &expr.kind {
                ExprKind::Var(_) | ExprKind::FullCellPath(_) | ExprKind::Subexpression(_) => {}
                ExprKind::Record(items) if matches!(items.first(), Some(crate::ast::RecordItem::Pair { .. })) => {}
                _ if noop => {}
                _ => return Err(wrong(&expr)),
            }
            UseMemberKind::Ignored(Box::new(expr))
        }
        _ => {
            let expr = value::value(st, span, Hint::Any)?;
            match expr.kind {
                ExprKind::String(s) => UseMemberKind::Name(s.value),
                _ if noop => UseMemberKind::Ignored(Box::new(expr)),
                _ => return Err(wrong(&expr)),
            }
        }
    };
    Ok(UseMember { span, kind })
}

/// The names in a `use module [a b c]` list. Items that are not strings are
/// parsed and ignored, as is a cell path after the list (`[math].x`).
fn use_member_list<'a>(st: St<'_, 'a>, tok: &Token) -> PResult<UseMember<'a>> {
    let text = st.tok(tok);
    let span = tok.span;
    let list_end = crate::lexer::group_end(text).map_or(span.end, |e| span.start + e + 1);
    let list = collections::list_or_table(st, Span::new(span.start, list_end))?;
    if list_end < span.end {
        // Parse the cell path for its errors, then drop it like nu.
        cellpath::full_cell_path(st, span, false)?;
        st.ignore(Span::new(list_end, span.end));
    }
    let ExprKind::List(items) = list.kind else {
        return Err(cut(Diagnostic::expected("list of names", span)));
    };
    let mut names = Vec::new();
    for item in items {
        match item {
            ListItem::Item(Expr { span, kind: ExprKind::String(s) }) => names.push(Spanned::new(s.value, span)),
            ListItem::Item(other) => st.ignore(other.span),
            ListItem::Spread { dots, .. } => {
                return Err(cut(Diagnostic::message("cannot spread in an import pattern", dots)));
            }
        }
    }
    Ok(UseMember { span, kind: UseMemberKind::List(names) })
}

fn export_stmt<'a>(st: St<'_, 'a>, c: Cursor<'_>) -> PResult<Expr<'a>> {
    let items = c.all();
    let kw = items[0];
    let next = match items.get(1) {
        Some(tok) if tok.kind == TokenKind::Item => tok,
        _ => {
            return Err(cut(Diagnostic::expected(
                "`def`, `extern`, `alias`, `use`, `module` or `const` after `export`",
                kw.span.past(),
            )));
        }
    };
    match st.tok(next) {
        "def" | "extern" | "alias" | "use" | "module" | "const" => {}
        "--help" | "-h" => {
            // `export` has no positionals, so `export --help x` is one too many.
            if let Some(extra) = items.get(2) {
                return Err(cut(Diagnostic::new(ErrorKind::ExtraTokens, extra.span)
                    .with_help("`export` takes no positional arguments")));
            }
            return help_call(st, c);
        }
        other => {
            return Err(cut(Diagnostic::message(format!("`export {other}` is not a valid export"), next.span)
                .with_help("expected `def`, `extern`, `alias`, `use`, `module` or `const`")));
        }
    }
    let rest = c.slice(1..items.len());
    let item = match st.tok(next) {
        "alias" => alias_stmt(st, rest, true).map_err(|e| e.map(|d| d.with_context("alias")))?,
        _ => keyword_or_call(st, rest)?,
    };
    let span = kw.span.merge(item.span);
    Ok(Expr::new(ExprKind::Export(Export { item: Box::new(item) }), span))
}

fn export_env_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let full = c;
    let kw = c.expect_item("export-env")?;
    let mut help = false;
    boundary_help(st, &mut c, "export-env", false, &mut help)?;
    let Some(body_tok) = item_or_help(&mut c, help, "block")? else {
        return help_call(st, full);
    };
    let body = block_item(st, &body_tok, "block")?;
    // nu hands `export-env` exactly one argument: anything after it is dropped.
    for tok in c.rest() {
        st.ignore(tok.span);
    }
    done(st, full, help, Expr::new(ExprKind::ExportEnv(ExportEnv { body }), kw.span.merge(body_tok.span)))
}

// --- control flow -----------------------------------------------------------

/// A `{ ... }` where nu accepts a block or any expression (the `else` branch
/// and match arms): a closure or a record parses as that value, the rest is
/// a block.
fn block_or_value<'a>(st: St<'_, 'a>, tok: &Token) -> PResult<Expr<'a>> {
    match value::brace_shape(st, tok.span)? {
        BraceShape::ClosureParams | BraceShape::Record => value::value(st, tok.span, Hint::Any),
        BraceShape::Empty | BraceShape::Spread | BraceShape::Other => {
            Ok(Expr::new(ExprKind::Block(value::block_body(st, tok.span)?), tok.span))
        }
    }
}

/// `if COND... BLOCK [else BLOCK|EXPR]`: the condition is every item before
/// the block, which is the item before `else` or the last item.
fn if_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let full = c;
    let items = c.all();
    let kw = c.expect_item("if")?;
    let mut help = false;
    boundary_help(st, &mut c, "if", true, &mut help)?;
    let start = c.position();
    if help && c.at_end() {
        return help_call(st, full);
    }
    let else_idx = items.iter().position(|t| t.kind == TokenKind::Item && st.tok(t) == "else");
    let block_idx = match else_idx {
        Some(k) if k > start => k - 1,
        Some(k) => return Err(cut(Diagnostic::expected("condition and block before `else`", items[k].span))),
        None => items.len() - 1,
    };
    if block_idx < start + 1 {
        if help {
            return help_call(st, full);
        }
        return Err(cut(Diagnostic::expected("condition", items.get(start).map_or(kw.span.past(), |t| t.span))));
    }
    let condition = expr::math_expression(st, c.slice(start..block_idx), false)?;
    let then_block = block_item(st, &items[block_idx], "block after the condition")?;
    let mut span = kw.span.merge(items[block_idx].span);
    let else_branch = match else_idx {
        None => None,
        Some(k) => {
            let else_tok = items[k];
            let rest = c.slice(k + 1..items.len());
            let body = match rest.all() {
                [] => return Err(cut(Diagnostic::expected("block or expression after `else`", else_tok.span.past()))),
                [only] if st.tok(only).starts_with('{') => block_or_value(st, only)?,
                _ => expr::parse_expression(st, rest, Position::Element)?,
            };
            span = span.merge(body.span);
            Some(Else { keyword: else_tok.span, body: Box::new(body) })
        }
    };
    let if_ = If { condition: Box::new(condition), then_block, else_branch };
    done(st, full, help, Expr::new(ExprKind::If(if_), span))
}

fn match_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let full = c;
    let kw = c.expect_item("match")?;
    let mut help = false;
    boundary_help(st, &mut c, "match", true, &mut help)?;
    let Some(value_tok) = item_or_help(&mut c, help, "value to match on")? else {
        return help_call(st, full);
    };
    let value = value::value(st, value_tok.span, Hint::Any)?;
    boundary_help(st, &mut c, "match", true, &mut help)?;
    let Some(block_tok) = item_or_help(&mut c, help, "match block")? else {
        return help_call(st, full);
    };
    // nu decides what the `{ ... }` is before it knows it wants arms: closure
    // parameters or a `key:` make it a closure or a record, which it accepts;
    // a variable or a subexpression in that position is accepted as well.
    let block_text = st.tok(&block_tok);
    let (arms, value_block) = match block_text.as_bytes().first() {
        Some(b'{') => match value::brace_shape(st, block_tok.span)? {
            BraceShape::ClosureParams | BraceShape::Record => {
                (Vec::new(), Some(Box::new(value::value(st, block_tok.span, Hint::Any)?)))
            }
            _ => (pattern::match_block(st, block_tok.span)?.1, None),
        },
        Some(b'$' | b'(') => (Vec::new(), Some(Box::new(value::value(st, block_tok.span, Hint::Any)?))),
        _ => return Err(cut(Diagnostic::expected("match block", block_tok.span))),
    };
    boundary_help(st, &mut c, "match", true, &mut help)?;
    c.expect_end()?;
    let m = Match { value: Box::new(value), block_span: block_tok.span, arms, value_block };
    done(st, full, help, Expr::new(ExprKind::Match(m), kw.span.merge(block_tok.span)))
}

fn while_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let full = c;
    let items = c.all();
    let kw = c.expect_item("while")?;
    let mut help = false;
    boundary_help(st, &mut c, "while", true, &mut help)?;
    let start = c.position();
    let Some((block_tok, _)) = items.split_last().filter(|_| items.len() >= start + 2) else {
        if help {
            return help_call(st, full);
        }
        return Err(cut(Diagnostic::expected("condition and block", c.end_span())));
    };
    let condition = expr::math_expression(st, c.slice(start..items.len() - 1), false)?;
    let body = block_item(st, block_tok, "block")?;
    let while_ = While { condition: Box::new(condition), body };
    done(st, full, help, Expr::new(ExprKind::While(while_), kw.span.merge(block_tok.span)))
}

fn loop_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let full = c;
    let kw = c.expect_item("loop")?;
    let mut help = false;
    boundary_help(st, &mut c, "loop", true, &mut help)?;
    let Some(body_tok) = item_or_help(&mut c, help, "block")? else {
        return help_call(st, full);
    };
    let body = block_item(st, &body_tok, "block")?;
    boundary_help(st, &mut c, "loop", true, &mut help)?;
    c.expect_end()?;
    done(st, full, help, Expr::new(ExprKind::Loop(Loop { body }), kw.span.merge(body_tok.span)))
}

/// A `catch`/`finally` handler: a closure, or a variable or subexpression
/// that may hold one.
fn handler_value<'a>(st: St<'_, 'a>, tok: &Token) -> PResult<Expr<'a>> {
    let text = st.tok(tok);
    match text.as_bytes().first() {
        Some(b'{') if value::brace_shape(st, tok.span)? == BraceShape::Record => {
            Err(cut(Diagnostic::expected("closure", tok.span).with_help("found a record")))
        }
        Some(b'{') => value::closure(st, tok.span),
        Some(b'$' | b'(') => value::value(st, tok.span, Hint::Any),
        _ => Err(cut(Diagnostic::expected("closure", tok.span))),
    }
}

fn try_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let full = c;
    let kw = c.expect_item("try")?;
    let mut help = false;
    boundary_help(st, &mut c, "try", true, &mut help)?;
    let Some(body_tok) = item_or_help(&mut c, help, "block")? else {
        return help_call(st, full);
    };
    let body = block_item(st, &body_tok, "block")?;
    let mut end = body_tok.span;
    let mut handlers = Vec::new();
    while !c.at_end() {
        // `try {} --`: the marker may be the last item.
        boundary_help(st, &mut c, "try", true, &mut help)?;
        if c.at_end() {
            break;
        }
        let kw_tok = c.expect_item("`catch` or `finally`")?;
        let kind = match st.tok(&kw_tok) {
            "catch" => HandlerKind::Catch,
            "finally" => HandlerKind::Finally,
            _ => return Err(cut(Diagnostic::expected("`catch` or `finally`", kw_tok.span))),
        };
        if handlers.len() == 2 {
            return Err(cut(Diagnostic::new(ErrorKind::ExtraTokens, kw_tok.span)
                .with_help("`try` takes at most two handlers (`catch` and `finally`)")));
        }
        let handler_tok = c.expect_item("closure")?;
        let handler = handler_value(st, &handler_tok)?;
        end = handler_tok.span;
        handlers.push(Handler { kind, keyword: kw_tok.span, body: Box::new(handler) });
    }
    done(st, full, help, Expr::new(ExprKind::Try(Try { body, handlers }), kw.span.merge(end)))
}

fn return_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let full = c;
    let kw = c.expect_item("return")?;
    let mut help = false;
    boundary_help(st, &mut c, "return", true, &mut help)?;
    let value = match c.at_end() {
        true => None,
        false => Some(Box::new(value::value(st, c.expect_item("value")?.span, Hint::Any)?)),
    };
    boundary_help(st, &mut c, "return", true, &mut help)?;
    c.expect_end()?;
    let span = value.as_ref().map_or(kw.span, |v| kw.span.merge(v.span));
    done(st, full, help, Expr::new(ExprKind::Return(Return { value }), span))
}

fn simple_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>, kind: ExprKind<'a>) -> PResult<Expr<'a>> {
    let full = c;
    let kw = c.expect_item("keyword")?;
    let mut help = false;
    boundary_help(st, &mut c, st.tok(&kw), true, &mut help)?;
    c.expect_end()?;
    done(st, full, help, Expr::new(kind, kw.span))
}

/// `where {closure}` or `where ROW-CONDITION...`.
fn where_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let full = c;
    let kw = c.expect_item("where")?;
    let mut help = false;
    boundary_help(st, &mut c, "where", true, &mut help)?;
    let condition = match c.rest() {
        [] if help => return help_call(st, full),
        [] => return Err(cut(Diagnostic::expected("row condition or closure", kw.span.past()))),
        [only] if st.tok(only).starts_with('{') => value::closure(st, only.span)?,
        _ => expr::math_expression(st, c.remaining(), true)?,
    };
    let span = kw.span.merge(condition.span);
    done(st, full, help, Expr::new(ExprKind::Where(Where { condition: Box::new(condition) }), span))
}

// --- fixed signatures of the keyword commands parsed as calls ---------------

/// A flag of a [`FixedSignature`]: long name, short letter, whether it takes a value.
struct FixedFlag(&'static str, Option<char>, bool);

/// nu's signature of a command that is a keyword there and a call here.
struct FixedSignature {
    required: usize,
    optional: usize,
    rest: bool,
    /// A keyword argument (`as NAME`) allowed after the positionals.
    keyword: Option<&'static str>,
    flags: &'static [FixedFlag],
    /// Unknown flags are passed through (`run`).
    unknown_flags: bool,
    /// `null` is allowed as the first positional.
    nothing_ok: bool,
    /// A redirection is allowed on the call.
    redirectable: bool,
}

fn fixed_signature_named(name: &str) -> Option<FixedSignature> {
    let sig = |required, optional, rest, keyword, flags, unknown_flags, nothing_ok, redirectable| FixedSignature {
        required,
        optional,
        rest,
        keyword,
        flags,
        unknown_flags,
        nothing_ok,
        redirectable,
    };
    Some(match name {
        "hide" => sig(1, 1, false, None, &[], false, false, false),
        "source" | "source-env" => sig(1, 0, false, None, &[], false, true, false),
        "run" => sig(1, 0, true, None, &[FixedFlag("full-reparse", None, false)], true, true, false),
        "overlay new" => sig(1, 0, false, None, &[FixedFlag("reload", Some('r'), false)], false, false, false),
        "overlay use" => sig(
            1,
            0,
            false,
            Some("as"),
            &[FixedFlag("prefix", Some('p'), false), FixedFlag("reload", Some('r'), false)],
            false,
            true,
            false,
        ),
        "overlay hide" => sig(
            0,
            1,
            false,
            None,
            &[FixedFlag("keep-custom", Some('k'), false), FixedFlag("keep-env", Some('e'), true)],
            false,
            false,
            false,
        ),
        "overlay list" => sig(0, 0, false, None, &[], false, false, false),
        "plugin use" => sig(1, 0, false, None, &[FixedFlag("plugin-config", None, true)], false, false, false),
        "attr category" => sig(1, 0, false, None, &[], false, false, true),
        "attr complete" => sig(1, 0, false, None, &[], false, false, true),
        "attr deprecated" => sig(
            0,
            1,
            false,
            None,
            &[
                FixedFlag("flag", None, true),
                FixedFlag("since", None, true),
                FixedFlag("remove", None, true),
                FixedFlag("report", None, true),
            ],
            false,
            false,
            true,
        ),
        "attr example" => sig(2, 0, false, None, &[FixedFlag("result", None, true)], false, false, true),
        "attr interactive" => sig(0, 0, false, None, &[], false, false, true),
        "attr search-terms" => sig(0, 0, true, None, &[], false, false, true),
        _ => return None,
    })
}

/// The fixed signature of `call`, if its head is one of the keyword commands.
/// With no command table configured the head of `overlay use` is `overlay`
/// with `use` as its first argument; both spellings resolve.
fn fixed_signature(call: &Call<'_>) -> Option<FixedSignature> {
    if let Some(sig) = fixed_signature_named(&call.head.name) {
        return Some(sig);
    }
    if matches!(&*call.head.name, "overlay" | "plugin")
        && let Some(Arg::Positional(first)) = call.args.first()
        && let ExprKind::String(s) = &first.kind
    {
        return fixed_signature_named(&format!("{} {}", call.head.name, s.value));
    }
    None
}

/// Check a call to a keyword command against nu's signature for it: the
/// flags it has, the values they take, the number of positionals, the `as`
/// keyword of `overlay use`, and that `-1` is a flag rather than a number.
/// With `lenient` (an alias target) missing positionals and flag values pass.
pub fn check_fixed_signature(st: St<'_, '_>, call: &Call<'_>, lenient: bool) -> PResult<()> {
    let Some(sig) = fixed_signature(call) else { return Ok(()) };
    let mut args = call.args.iter().peekable();
    // A head resolved as a single word (`overlay` + `use`): skip the subcommand word.
    if fixed_signature_named(&call.head.name).is_none() {
        args.next();
    }
    check_against(st, &call.head.name, &sig, args, lenient)
}

fn check_fixed_signature_named(st: St<'_, '_>, name: &str, call: &Call<'_>) -> PResult<()> {
    let Some(sig) = fixed_signature_named(name) else { return Ok(()) };
    check_against(st, name, &sig, call.args.iter().peekable(), false)
}

fn check_against<'x>(
    st: St<'_, '_>,
    name: &str,
    sig: &FixedSignature,
    mut args: std::iter::Peekable<impl Iterator<Item = &'x Arg<'x>>>,
    lenient: bool,
) -> PResult<()> {
    let no_flag = |flag: &str, span: Span| {
        cut(Diagnostic::message(format!("the `{name}` command doesn't have flag `{flag}`"), span)
            .with_help("use `--help` to see available flags"))
    };
    let mut positionals = 0usize;
    let mut keyword_seen = false;
    let mut end_of_options = false;
    while let Some(arg) = args.next() {
        match arg {
            Arg::EndOfOptions(_) => end_of_options = true,
            Arg::Flag(flag) if !end_of_options => {
                if flag.long && flag.name == "help" || !flag.long && flag.name == "h" {
                    return Ok(());
                }
                let known = sig.flags.iter().find(|f| match flag.long {
                    true => f.0 == flag.name,
                    false => flag.name.chars().count() == 1 && f.1 == flag.name.chars().next(),
                });
                match known {
                    None if sig.unknown_flags => {}
                    None => return Err(no_flag(&format!("{}{}", if flag.long { "--" } else { "-" }, flag.name), flag.span)),
                    Some(FixedFlag(_, _, true)) if flag.value.is_none() => match args.peek() {
                        Some(Arg::Positional(_)) => {
                            args.next();
                        }
                        _ if lenient => {}
                        _ => {
                            return Err(cut(Diagnostic::message("missing flag argument", flag.span)
                                .with_help(format!("`--{}` takes a value", flag.name))));
                        }
                    },
                    Some(_) => {}
                }
            }
            Arg::Flag(flag) => {
                let _ = flag;
                positionals += 1;
            }
            Arg::Spread { .. } => positionals = sig.required,
            Arg::Positional(expr) => {
                let text = st.text(expr.span);
                if !end_of_options && text.starts_with('-') && text.len() > 1 && !sig.unknown_flags {
                    return Err(no_flag(text, expr.span));
                }
                if let Some(kw) = sig.keyword
                    && positionals >= sig.required + sig.optional
                    && !sig.rest
                {
                    if keyword_seen {
                        return Err(cut(Diagnostic::message("extra positional argument", expr.span)));
                    }
                    if text != kw {
                        return Err(cut(Diagnostic::new(ErrorKind::ExpectedKeyword(kw), expr.span)));
                    }
                    match args.next() {
                        Some(Arg::Positional(_)) => keyword_seen = true,
                        _ => {
                            return Err(cut(Diagnostic::message(format!("missing argument to `{kw}`"), expr.span.past())));
                        }
                    }
                    continue;
                }
                if positionals == 0 && matches!(expr.kind, ExprKind::Nothing) && !sig.nothing_ok
                    || positionals == 0 && matches!(expr.kind, ExprKind::Bool(_) | ExprKind::Record(_) | ExprKind::Closure(_))
                {
                    return Err(cut(Diagnostic::expected("string", expr.span)));
                }
                positionals += 1;
                if !sig.rest && positionals > sig.required + sig.optional {
                    return Err(cut(Diagnostic::message("extra positional argument", expr.span)
                        .with_help(format!("`{name}` takes at most {} positional arguments", sig.required + sig.optional))));
                }
            }
        }
    }
    if positionals < sig.required && !lenient {
        return Err(cut(Diagnostic::message("missing required positional argument", Span::point(0))
            .with_help(format!("`{name}` takes {} positional argument(s)", sig.required))));
    }
    Ok(())
}
