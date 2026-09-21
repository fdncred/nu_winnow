//! Keyword statements and expressions: `def`, `let`, `if`, `match`, ...
//!
//! Each parser takes a [`Cursor`] over the items of one pipeline element and
//! produces the corresponding [`ExprKind`] variant.

use std::borrow::Cow;

use crate::ast::{
    Alias, Attribute, AttributeBlock, Binding, Block, Def, DefFlag, Else, Export, ExportEnv, Expr, ExprKind, Extern,
    For, Handler, HandlerKind, If, ListItem, Loop, Match, Module, RedirectTarget, Redirection, Return, Signature, Try,
    Use, UseMember, UseMemberKind, Where, While,
};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut};
use crate::lexer::{AssignOp, RedirectSource, Token, TokenKind};
use crate::span::{Span, Spanned};

use super::block::RawCommand;
use super::cursor::Cursor;
use super::signature::{self, definition_name};
use super::value::{self, Hint};
use super::{St, block, cellpath, collections, expr, literal, pattern, strings};

/// Keywords that start a statement and can only appear at the head of a
/// pipeline (they parse their own `=` and `{}` arguments).
pub fn is_statement_keyword(text: &str) -> bool {
    matches!(
        text,
        "def" | "extern" | "let" | "mut" | "const" | "for" | "alias" | "module" | "use" | "export" | "export-env"
    )
}

/// Names that cannot be given to a definition because the parser treats them
/// specially (`nu-parser`'s aliasable and unaliasable keyword tables).
pub fn is_parser_keyword(name: &str) -> bool {
    matches!(
        name,
        "if" | "match"
            | "try"
            | "overlay"
            | "alias"
            | "const"
            | "def"
            | "extern"
            | "module"
            | "use"
            | "export"
            | "for"
            | "loop"
            | "while"
            | "return"
            | "break"
            | "continue"
            | "let"
            | "mut"
            | "hide"
            | "export-env"
            | "source-env"
            | "source"
            | "run"
            | "where"
    )
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
        "alias" => ("alias", alias_stmt(st, c)),
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
        "break" => ("break", simple_stmt(c, ExprKind::Break)),
        "continue" => ("continue", simple_stmt(c, ExprKind::Continue)),
        "where" => ("where", where_stmt(st, c)),
        _ => ("command call", expr::parse_call(st, c)),
    };
    result.map_err(|e| e.map(|d| d.with_context(ctx)))
}

/// Parse one command (a pipeline element) as collected by the block parser.
pub fn parse_command<'a>(st: St<'_, 'a>, raw: &RawCommand) -> PResult<(Expr<'a>, Option<Redirection<'a>>)> {
    let expr = match raw.attributes.as_slice() {
        [] => expr::parse_expression(st, raw.cursor())?,
        attribute_lines => {
            let attributes = attribute_lines.iter().map(|a| attribute(st, a)).collect::<PResult<Vec<_>>>()?;
            let item = match raw.parts.first().map(|t| st.tok(t)) {
                Some("def" | "extern" | "export") => keyword_or_call(st, raw.cursor())?,
                Some(_) => {
                    return Err(cut(Diagnostic::expected(
                        "`def`, `extern` or `export` after attributes",
                        raw.parts[0].span,
                    )));
                }
                None => {
                    let last = attributes.last().map_or(Span::point(raw.end), |a| a.span);
                    return Err(cut(Diagnostic::expected("a definition after the attributes", last.past())
                        .with_help("attributes must be followed by a `def` or `extern`")));
                }
            };
            let span = attributes[0].span.merge(item.span);
            Expr::new(ExprKind::AttributeBlock(AttributeBlock { attributes, item: Box::new(item) }), span)
        }
    };
    let redirection = build_redirection(st, raw)?;
    if redirection.is_some() && is_redirect_forbidden(&expr) {
        let at = raw.redirections.first().map_or(expr.span, |(op, _)| op.span);
        return Err(cut(Diagnostic::message("this statement cannot be redirected", at)));
    }
    Ok((expr, redirection))
}

fn is_redirect_forbidden(expr: &Expr<'_>) -> bool {
    matches!(
        expr.kind,
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
            | ExprKind::AttributeBlock(_)
    )
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

/// `@name args`.
fn attribute<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Attribute<'a>> {
    let end = tokens.last().map_or(0, |t| t.span.end);
    let mut c = Cursor::new(tokens, end);
    let first = c.expect_item("attribute")?;
    let head = expr::resolve_head(st, first, &mut c, "attr ");
    let name_span = Span::new(first.span.start + 1, head.span.end);
    let args = expr::parse_args(st, c)?;
    Ok(Attribute { span: Span::new(first.span.start, end), name: Spanned::new(head.name, name_span), args })
}

/// The `{ ... }` item that must end a statement, or an error.
fn block_item<'a>(st: St<'_, 'a>, tok: &Token, what: &'static str) -> PResult<Block<'a>> {
    if tok.kind != TokenKind::Item || !st.tok(tok).starts_with('{') {
        return Err(cut(Diagnostic::expected(what, tok.span)));
    }
    value::block_body(st, tok.span)
}

fn def_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let kw = c.expect_item("def")?;
    let mut flags = Vec::new();
    def_flags(st, &mut c, &mut flags)?;
    let name = definition_name(st, c.expect_item("command name")?.span)?;
    check_definition_name(&name, "command")?;
    // nu also accepts the flags after the name: `def foo --env [] { }`.
    def_flags(st, &mut c, &mut flags)?;
    let (mut signature, has_colon) = signature_item(st, &mut c)?;
    let Some((body_tok, type_items)) = c.rest().split_last() else {
        return Err(cut(Diagnostic::expected("block", c.end_span())));
    };
    io_types(st, &mut signature, type_items, has_colon)?;
    let body = block_item(st, body_tok, "block")?;
    let span = kw.span.merge(body_tok.span);
    Ok(Expr::new(ExprKind::Def(Def { flags, name, signature, body }), span))
}

/// `--env` / `--wrapped` items at the cursor.
fn def_flags(st: St<'_, '_>, c: &mut Cursor<'_>, flags: &mut Vec<Spanned<DefFlag>>) -> PResult<()> {
    while let Some(tok) = c.peek().filter(|t| t.kind == TokenKind::Item && st.tok(t).starts_with("--")) {
        let flag = match st.tok(tok) {
            "--env" => DefFlag::Env,
            "--wrapped" => DefFlag::Wrapped,
            other => {
                return Err(cut(Diagnostic::message(format!("unknown flag `{other}` for `def`"), tok.span)
                    .with_help("`def` accepts `--env` and `--wrapped`")));
            }
        };
        flags.push(Spanned::new(flag, tok.span));
        c.next();
    }
    Ok(())
}

/// The `[...]`/`(...)` signature item, returning whether it ended with `:`.
fn signature_item<'a>(st: St<'_, 'a>, c: &mut Cursor<'_>) -> PResult<(Signature<'a>, bool)> {
    let tok = c.expect_item("signature")?;
    let text = st.tok(&tok);
    if !text.starts_with(['[', '(']) {
        return Err(cut(Diagnostic::expected("signature like `[param: type]`", tok.span)));
    }
    let (sig_span, has_colon) = match text.strip_suffix(':') {
        Some(_) => (Span::new(tok.span.start, tok.span.end - 1), true),
        None => (tok.span, false),
    };
    Ok((signature::parse_signature(st, sig_span)?, has_colon))
}

/// Attach `: in -> out, ...` items (everything between the signature and the body).
fn io_types<'a>(st: St<'_, 'a>, sig: &mut Signature<'a>, type_items: &[Token], has_colon: bool) -> PResult<()> {
    let (has_colon, type_items) = match type_items.split_first() {
        Some((first, rest)) if st.tok(first) == ":" => {
            if has_colon {
                return Err(cut(Diagnostic::expected("type", first.span)));
            }
            (true, rest)
        }
        _ => (has_colon, type_items),
    };
    match (has_colon, type_items.first(), type_items.last()) {
        (true, Some(first), Some(last)) => {
            let span = first.span.merge(last.span);
            sig.io_types = signature::parse_io_types(st, span)?;
            sig.io_span = Some(span);
            sig.span = sig.span.merge(span);
            Ok(())
        }
        (true, ..) => Err(cut(Diagnostic::expected("input/output types after `:`", sig.span.past()))),
        (false, Some(first), _) => Err(cut(Diagnostic::expected("`:` before the input/output types", first.span))),
        (false, ..) => Ok(()),
    }
}

fn extern_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let kw = c.expect_item("extern")?;
    let name = definition_name(st, c.expect_item("command name")?.span)?;
    check_definition_name(&name, "command")?;
    let (mut signature, has_colon) = signature_item(st, &mut c)?;
    let rest = c.rest();
    io_types(st, &mut signature, rest, has_colon)?;
    let span = kw.span.merge(rest.last().map_or(signature.span, |t| t.span));
    Ok(Expr::new(ExprKind::Extern(Extern { name, signature }), span))
}

#[derive(Clone, Copy)]
enum BindingKind {
    Let,
    Mut,
    Const,
}

/// `let`, `mut` and `const`: `KW name[: type] [= value...]`.
fn binding_stmt<'a>(st: St<'_, 'a>, c: Cursor<'_>, kind: BindingKind) -> PResult<Expr<'a>> {
    let items = c.all();
    let kw = items[0];
    let name_tok = match items.get(1) {
        Some(tok) if tok.kind == TokenKind::Item => tok,
        _ => return Err(cut(Diagnostic::expected("variable name", c.slice(1..items.len()).here()))),
    };
    let (name, typed) = variable_declaration(st, name_tok)?;
    let eq_idx = items.iter().position(|t| matches!(t.kind, TokenKind::Assign(_))).unwrap_or(items.len());
    let eq_tok = items.get(eq_idx).copied();
    if let Some(eq_tok) = eq_tok
        && eq_tok.kind != TokenKind::Assign(AssignOp::Assign)
    {
        return Err(cut(Diagnostic::expected("`=`", eq_tok.span)));
    }
    let ty = type_after_name(st, &items[2..eq_idx], typed, name_tok.span.past())?;
    let (value, end) = match eq_tok {
        Some(eq_tok) => {
            let rhs = c.slice(eq_idx + 1..items.len());
            let Some(rhs_span) = rhs.span() else {
                return Err(cut(Diagnostic::expected("value after `=`", eq_tok.span.past())));
            };
            (Some(block::parse_block(st, rhs, rhs_span)), rhs_span)
        }
        None => (None, items[items.len() - 1].span),
    };
    let binding = Binding { name, ty, eq: eq_tok.map(|t| t.span), value };
    let kind = match kind {
        BindingKind::Let => ExprKind::Let(binding),
        BindingKind::Mut => ExprKind::Mut(binding),
        BindingKind::Const => ExprKind::Const(binding),
    };
    Ok(Expr::new(kind, kw.span.merge(end)))
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
    Ok((Spanned::new(name, Span::new(start, start + name.len())), typed))
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

fn for_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let kw = c.expect_item("for")?;
    let var_tok = c.expect_item("loop variable")?;
    let (var, typed) = variable_declaration(st, &var_tok)?;
    let ty = match typed {
        true => Some(signature::parse_type(st, c.expect_item("type")?.span)?),
        false => None,
    };
    let in_tok = c.expect_item("`in`")?;
    if st.tok(&in_tok) != "in" {
        return Err(cut(Diagnostic::new(ErrorKind::ExpectedKeyword("in"), in_tok.span)));
    }
    let iterable = value::value(st, c.expect_item("value to iterate")?.span, Hint::Any)?;
    let body_tok = c.expect_item("block")?;
    let body = block_item(st, &body_tok, "block")?;
    c.expect_end()?;
    let span = kw.span.merge(body_tok.span);
    Ok(Expr::new(ExprKind::For(For { var, ty, in_keyword: in_tok.span, iterable: Box::new(iterable), body }), span))
}

fn alias_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let kw = c.expect_item("alias")?;
    let name = definition_name(st, c.expect_item("alias name")?.span)?;
    check_definition_name(&name, "alias")?;
    let eq_tok = match c.next() {
        Some(tok) if tok.kind == TokenKind::Assign(AssignOp::Assign) => *tok,
        _ => return Err(cut(Diagnostic::expected("`=`", c.here()))),
    };
    // Nushell hands everything after `=` to the expression parser as plain
    // words, so `alias ll = ls | length` is `ls` with the arguments `|` and `length`.
    let words: Vec<Token> = c.rest().iter().map(|t| Token { kind: TokenKind::Item, span: t.span }).collect();
    if words.is_empty() {
        return Err(cut(Diagnostic::expected("command after `=`", eq_tok.span.past())));
    }
    // `alias i = if`: a keyword is aliased as a plain call, not parsed as a statement.
    let cursor = Cursor::new(&words, c.end_span().start);
    let value = match is_parser_keyword(st.tok(&words[0])) {
        true => expr::parse_call(st, cursor)?,
        false => expr::parse_expression(st, cursor)?,
    };
    // Like nu ("can't create alias to expression"), only a command can be aliased.
    if !matches!(value.kind, ExprKind::Call(_) | ExprKind::ExternalCall(_) | ExprKind::DynamicCall(_)) {
        return Err(cut(Diagnostic::message("cannot create an alias to an expression", value.span)
            .with_help("an alias names a command and its arguments, such as `alias ll = ls -l`")));
    }
    let span = kw.span.merge(value.span);
    Ok(Expr::new(ExprKind::Alias(Alias { name, eq: eq_tok.span, value: Box::new(value) }), span))
}

fn module_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let kw = c.expect_item("module")?;
    let name_tok = c.expect_item("module name or path")?;
    let name = value::value(st, name_tok.span, Hint::String)?;
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
        _ => None,
    };
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
    let kw = c.expect_item("use")?;
    let module_tok = c.expect_item("module name or path")?;
    let module = match st.tok(&module_tok) {
        "null" => Expr::new(ExprKind::Nothing, module_tok.span),
        _ => value::value(st, module_tok.span, Hint::String)?,
    };
    let mut members = Vec::new();
    let mut end = module_tok.span;
    while let Some(tok) = c.next() {
        if members
            .last()
            .is_some_and(|m: &UseMember<'_>| matches!(m.kind, UseMemberKind::Glob | UseMemberKind::List(_)))
        {
            return Err(cut(Diagnostic::message(
                "a `*` or `[...]` member can only be at the end of an import pattern",
                tok.span,
            )));
        }
        let kind = match st.tok(tok) {
            "*" => UseMemberKind::Glob,
            text if text.starts_with('[') => UseMemberKind::List(use_member_list(st, tok.span)?),
            _ => UseMemberKind::Name(strings::string_lit(st, tok.span)?.value),
        };
        members.push(UseMember { span: tok.span, kind });
        end = tok.span;
    }
    Ok(Expr::new(ExprKind::Use(Use { module: Box::new(module), members }), kw.span.merge(end)))
}

/// The names in a `use module [a b c]` list.
fn use_member_list<'a>(st: St<'_, 'a>, span: Span) -> PResult<Vec<Spanned<Cow<'a, str>>>> {
    let ExprKind::List(items) = collections::list_or_table(st, span)?.kind else {
        return Err(cut(Diagnostic::expected("list of names", span)));
    };
    // nu takes any item's text as a name (`use std [1 2]` parses and fails later).
    items
        .into_iter()
        .map(|item| match item {
            ListItem::Item(Expr { span, kind: ExprKind::String(s) }) => Ok(Spanned::new(s.value, span)),
            ListItem::Item(other) => Ok(Spanned::new(Cow::Borrowed(st.text(other.span)), other.span)),
            other => Err(cut(Diagnostic::expected("name", other.span()))),
        })
        .collect()
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
        other => {
            return Err(cut(Diagnostic::message(format!("`export {other}` is not a valid export"), next.span)
                .with_help("expected `def`, `extern`, `alias`, `use`, `module` or `const`")));
        }
    }
    let item = keyword_or_call(st, c.slice(1..items.len()))?;
    let span = kw.span.merge(item.span);
    Ok(Expr::new(ExprKind::Export(Export { item: Box::new(item) }), span))
}

fn export_env_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let kw = c.expect_item("export-env")?;
    let body_tok = c.expect_item("block")?;
    let body = block_item(st, &body_tok, "block")?;
    c.expect_end()?;
    Ok(Expr::new(ExprKind::ExportEnv(ExportEnv { body }), kw.span.merge(body_tok.span)))
}

/// `if COND... BLOCK [else BLOCK|EXPR]`: the condition is every item before
/// the block, which is the item before `else` or the last item.
fn if_stmt<'a>(st: St<'_, 'a>, c: Cursor<'_>) -> PResult<Expr<'a>> {
    let items = c.all();
    let kw = items[0];
    let else_idx = items.iter().position(|t| t.kind == TokenKind::Item && st.tok(t) == "else");
    let block_idx = match else_idx {
        Some(k) if k >= 2 => k - 1,
        Some(k) => return Err(cut(Diagnostic::expected("condition and block before `else`", items[k].span))),
        None => items.len() - 1,
    };
    if block_idx < 2 {
        return Err(cut(Diagnostic::expected("condition", items.get(1).map_or(kw.span.past(), |t| t.span))));
    }
    let condition = expr::math_expression(st, c.slice(1..block_idx), false)?;
    let then_block = block_item(st, &items[block_idx], "block after the condition")?;
    let mut span = kw.span.merge(items[block_idx].span);
    let else_branch = match else_idx {
        None => None,
        Some(k) => {
            let else_tok = items[k];
            let rest = c.slice(k + 1..items.len());
            let body = match rest.all() {
                [] => return Err(cut(Diagnostic::expected("block or expression after `else`", else_tok.span.past()))),
                [only] if st.tok(only).starts_with('{') => {
                    Expr::new(ExprKind::Block(block_item(st, only, "block")?), only.span)
                }
                _ => expr::parse_expression(st, rest)?,
            };
            span = span.merge(body.span);
            Some(Else { keyword: else_tok.span, body: Box::new(body) })
        }
    };
    Ok(Expr::new(ExprKind::If(If { condition: Box::new(condition), then_block, else_branch }), span))
}

fn match_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let kw = c.expect_item("match")?;
    let value = value::value(st, c.expect_item("value to match on")?.span, Hint::Any)?;
    let block_tok = c.expect_item("match block")?;
    let (block_span, arms) = pattern::match_block(st, block_tok.span)?;
    c.expect_end()?;
    Ok(Expr::new(ExprKind::Match(Match { value: Box::new(value), block_span, arms }), kw.span.merge(block_tok.span)))
}

fn while_stmt<'a>(st: St<'_, 'a>, c: Cursor<'_>) -> PResult<Expr<'a>> {
    let items = c.all();
    let kw = items[0];
    let Some((block_tok, _)) = items.split_last().filter(|_| items.len() >= 3) else {
        return Err(cut(Diagnostic::expected("condition and block", c.slice(1..items.len()).end_span())));
    };
    let condition = expr::math_expression(st, c.slice(1..items.len() - 1), false)?;
    let body = block_item(st, block_tok, "block")?;
    Ok(Expr::new(ExprKind::While(While { condition: Box::new(condition), body }), kw.span.merge(block_tok.span)))
}

fn loop_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let kw = c.expect_item("loop")?;
    let body_tok = c.expect_item("block")?;
    let body = block_item(st, &body_tok, "block")?;
    c.expect_end()?;
    Ok(Expr::new(ExprKind::Loop(Loop { body }), kw.span.merge(body_tok.span)))
}

fn try_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let kw = c.expect_item("try")?;
    let body_tok = c.expect_item("block")?;
    let body = block_item(st, &body_tok, "block")?;
    let mut end = body_tok.span;
    let mut handlers = Vec::new();
    while !c.at_end() {
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
        let handler = value::value(st, handler_tok.span, Hint::Closure)?;
        end = handler_tok.span;
        handlers.push(Handler { kind, keyword: kw_tok.span, body: Box::new(handler) });
    }
    Ok(Expr::new(ExprKind::Try(Try { body, handlers }), kw.span.merge(end)))
}

fn return_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let kw = c.expect_item("return")?;
    let value = match c.at_end() {
        true => None,
        false => Some(Box::new(value::value(st, c.expect_item("value")?.span, Hint::Any)?)),
    };
    c.expect_end()?;
    let span = value.as_ref().map_or(kw.span, |v| kw.span.merge(v.span));
    Ok(Expr::new(ExprKind::Return(Return { value }), span))
}

fn simple_stmt<'a>(mut c: Cursor<'_>, kind: ExprKind<'a>) -> PResult<Expr<'a>> {
    let kw = c.expect_item("keyword")?;
    c.expect_end()?;
    Ok(Expr::new(kind, kw.span))
}

/// `where {closure}` or `where ROW-CONDITION...`.
fn where_stmt<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let kw = c.expect_item("where")?;
    let condition = match c.rest() {
        [] => return Err(cut(Diagnostic::expected("row condition or closure", kw.span.past()))),
        [only] if st.tok(only).starts_with('{') => value::closure(st, only.span)?,
        _ => expr::math_expression(st, c.remaining(), true)?,
    };
    let span = kw.span.merge(condition.span);
    Ok(Expr::new(ExprKind::Where(Where { condition: Box::new(condition) }), span))
}
