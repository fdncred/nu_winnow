//! Keyword statements and expressions: `def`, `let`, `if`, `match`, ...
//!
//! Each parser takes the items of one pipeline element (terminated by `Eof`)
//! and produces the corresponding [`ExprKind`] variant.

use winnow::stream::Stream;

use crate::ast::{
    Alias, Attribute, AttributeBlock, Binding, Def, DefFlag, Else, Export, ExportEnv, Expr, ExprKind, Extern, For,
    Handler, HandlerKind, If, Loop, Match, Module, RedirectTarget, Redirection, Return, Try, Use, UseMember,
    UseMemberKind, Where, While,
};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut};
use crate::lexer::{AssignOp, RedirectSource, Token, TokenKind};
use crate::span::{Span, Spanned};

use super::block::RawCommand;
use super::expr::{self, at_end, expect_end, expect_item, items, peek_token, toks, with_eof};
use super::signature::{self, definition_name};
use super::value::{self, Hint, is_identifier};
use super::{St, block, pattern};

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

/// Reject a `def`/`extern`/`alias` name that is a parser keyword.
fn check_definition_name(name: &Spanned<std::borrow::Cow<'_, str>>, what: &str) -> PResult<()> {
    if is_parser_keyword(&name.item) {
        return Err(cut(Diagnostic::message(
            format!("cannot use parser keyword `{}` as {what} name", name.item),
            name.span,
        )
        .with_help("choose a different name; this word is parsed specially by Nushell")));
    }
    Ok(())
}

/// Parse a keyword construct or, failing that, a command call.
pub fn keyword_or_call<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let items_ = items(tokens);
    let Some(first) = items_.first() else {
        return Err(cut(Diagnostic::expected("command", expr::end_span(tokens))));
    };
    if first.kind != TokenKind::Item {
        return Err(cut(Diagnostic::expected("command", first.span)));
    }
    let head = st.tok(first);
    // `if`, `loop`, ... are ordinary commands in Nushell and may be shadowed by
    // a user definition, unlike the statement keywords.
    let head = if !is_statement_keyword(head) && st.is_declared_command(head) { "" } else { head };
    let ctx: &'static str;
    let result = match head {
        "def" => {
            ctx = "def";
            def_stmt(st, tokens)
        }
        "extern" => {
            ctx = "extern";
            extern_stmt(st, tokens)
        }
        "let" => {
            ctx = "let";
            binding_stmt(st, tokens, BindingKind::Let)
        }
        "mut" => {
            ctx = "mut";
            binding_stmt(st, tokens, BindingKind::Mut)
        }
        "const" => {
            ctx = "const";
            binding_stmt(st, tokens, BindingKind::Const)
        }
        "for" => {
            ctx = "for";
            for_stmt(st, tokens)
        }
        "alias" => {
            ctx = "alias";
            alias_stmt(st, tokens)
        }
        "module" => {
            ctx = "module";
            module_stmt(st, tokens)
        }
        "use" => {
            ctx = "use";
            use_stmt(st, tokens)
        }
        "export" => {
            ctx = "export";
            export_stmt(st, tokens)
        }
        "export-env" => {
            ctx = "export-env";
            export_env_stmt(st, tokens)
        }
        "if" => {
            ctx = "if";
            if_stmt(st, tokens)
        }
        "match" => {
            ctx = "match";
            match_stmt(st, tokens)
        }
        "while" => {
            ctx = "while";
            while_stmt(st, tokens)
        }
        "loop" => {
            ctx = "loop";
            loop_stmt(st, tokens)
        }
        "try" => {
            ctx = "try";
            try_stmt(st, tokens)
        }
        "return" => {
            ctx = "return";
            return_stmt(st, tokens)
        }
        "break" => {
            ctx = "break";
            simple_stmt(st, tokens, ExprKind::Break)
        }
        "continue" => {
            ctx = "continue";
            simple_stmt(st, tokens, ExprKind::Continue)
        }
        "where" => {
            ctx = "where";
            where_stmt(st, tokens)
        }
        _ => {
            ctx = "command call";
            expr::parse_call(st, tokens)
        }
    };
    result.map_err(|e| e.map(|d| d.with_context(ctx)))
}

/// Parse one command (a pipeline element) as collected by the block parser.
pub fn parse_command<'a>(st: St<'_, 'a>, raw: &RawCommand) -> PResult<(Expr<'a>, Option<Redirection<'a>>)> {
    let expr = if raw.attributes.is_empty() {
        expr::parse_expression(st, &raw.parts)?
    } else {
        let attributes = raw.attributes.iter().map(|a| attribute(st, a)).collect::<PResult<Vec<_>>>()?;
        let head = items(&raw.parts).first().map(|t| st.tok(t));
        let item = match head {
            Some("def" | "extern" | "export") => keyword_or_call(st, &raw.parts)?,
            Some(_) => {
                return Err(cut(Diagnostic::expected(
                    "`def`, `extern` or `export` after attributes",
                    items(&raw.parts)[0].span,
                )));
            }
            None => {
                let last = attributes.last().expect("non-empty").span;
                return Err(cut(Diagnostic::expected("a definition after the attributes", last.past())
                    .with_help("attributes must be followed by a `def` or `extern`")));
            }
        };
        let span = attributes[0].span.merge(item.span);
        Expr::new(ExprKind::AttributeBlock(AttributeBlock { attributes, item: Box::new(item) }), span)
    };
    let redirection = build_redirection(st, raw)?;
    if redirection.is_some() && is_redirect_forbidden(&expr) {
        return Err(cut(Diagnostic::message(
            "this statement cannot be redirected",
            raw.redirections.first().map(|(op, _)| op.span).unwrap_or(expr.span),
        )));
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
        let source = op.item.source();
        out = Some(match (out.take(), source) {
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
    let items_ = items(tokens);
    let first = items_[0];
    let (head, consumed) = expr::resolve_head(st, items_, "attr ");
    let name_span = Span::new(first.span.start + 1, head.span.end);
    let args = expr::parse_args(st, &items_[consumed..])?;
    let span = first.span.merge(items_.last().unwrap().span);
    Ok(Attribute { span, name: Spanned::new(head.name, name_span), args })
}

/// The `{ ... }` item that must end a statement, or an error.
fn block_item<'a>(st: St<'_, 'a>, tok: &Token, what: &'static str) -> PResult<crate::ast::Block<'a>> {
    if tok.kind != TokenKind::Item || !st.tok(tok).starts_with('{') {
        return Err(cut(Diagnostic::expected(what, tok.span)));
    }
    value::block_body(st, tok.span)
}

fn def_stmt<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let mut i = toks(st, tokens);
    let kw = expect_item(&mut i, "def")?;
    let mut flags = Vec::new();
    while let Some(t) = peek_token(&i).filter(|t| t.kind == TokenKind::Item && st.tok(t).starts_with("--")) {
        let flag = match st.tok(t) {
            "--env" => DefFlag::Env,
            "--wrapped" => DefFlag::Wrapped,
            other => {
                return Err(cut(Diagnostic::message(format!("unknown flag `{other}` for `def`"), t.span)
                    .with_help("`def` accepts `--env` and `--wrapped`")));
            }
        };
        flags.push(Spanned::new(flag, t.span));
        i.next_token();
    }
    let name_tok = expect_item(&mut i, "command name")?;
    let name = definition_name(st, name_tok.span)?;
    check_definition_name(&name, "command")?;
    let (mut signature, has_colon) = signature_item(&mut i)?;
    let rest = items(i.input.peek_finish());
    let Some((body_tok, type_items)) = rest.split_last() else {
        return Err(cut(Diagnostic::expected("block", expr::end_span(tokens))));
    };
    io_types(st, &mut signature, type_items, has_colon)?;
    let body = block_item(st, body_tok, "block")?;
    let span = kw.span.merge(body_tok.span);
    Ok(Expr::new(ExprKind::Def(Def { keyword: kw.span, flags, name, signature, body }), span))
}

/// The `[...]`/`(...)` signature item, returning whether it ended with `:`.
fn signature_item<'a>(i: &mut expr::Toks<'_, '_, 'a>) -> PResult<(crate::ast::Signature<'a>, bool)> {
    let st = i.state;
    let sig_tok = expect_item(i, "signature")?;
    let text = st.tok(&sig_tok);
    if !text.starts_with('[') && !text.starts_with('(') {
        return Err(cut(Diagnostic::expected("signature like `[param: type]`", sig_tok.span)));
    }
    let (sig_span, has_colon) = match text.strip_suffix(':') {
        Some(_) => (Span::new(sig_tok.span.start, sig_tok.span.end - 1), true),
        None => (sig_tok.span, false),
    };
    Ok((signature::parse_signature(st, sig_span)?, has_colon))
}

/// Attach `: in -> out, ...` items (everything between the signature and the body).
fn io_types<'a>(
    st: St<'_, 'a>,
    sig: &mut crate::ast::Signature<'a>,
    type_items: &[Token],
    has_colon: bool,
) -> PResult<()> {
    let mut type_items = type_items;
    let mut has_colon = has_colon;
    if let Some(first) = type_items.first()
        && st.tok(first) == ":"
    {
        if has_colon {
            return Err(cut(Diagnostic::expected("type", first.span)));
        }
        has_colon = true;
        type_items = &type_items[1..];
    }
    match (has_colon, type_items.first()) {
        (true, Some(first)) => {
            let span = first.span.merge(type_items.last().unwrap().span);
            sig.io_types = signature::parse_io_types(st, span)?;
            sig.io_span = Some(span);
            sig.span = sig.span.merge(span);
            Ok(())
        }
        (true, None) => Err(cut(Diagnostic::expected("input/output types after `:`", sig.span.past()))),
        (false, Some(first)) => Err(cut(Diagnostic::expected("`:` before the input/output types", first.span))),
        (false, None) => Ok(()),
    }
}

fn extern_stmt<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let mut i = toks(st, tokens);
    let kw = expect_item(&mut i, "extern")?;
    let name_tok = expect_item(&mut i, "command name")?;
    let name = definition_name(st, name_tok.span)?;
    check_definition_name(&name, "command")?;
    let (mut signature, has_colon) = signature_item(&mut i)?;
    let rest = items(i.input.peek_finish());
    io_types(st, &mut signature, rest, has_colon)?;
    let span = kw.span.merge(rest.last().map_or(signature.span, |t| t.span));
    Ok(Expr::new(ExprKind::Extern(Extern { keyword: kw.span, name, signature }), span))
}

#[derive(Clone, Copy)]
enum BindingKind {
    Let,
    Mut,
    Const,
}

fn binding_stmt<'a>(st: St<'_, 'a>, tokens: &[Token], kind: BindingKind) -> PResult<Expr<'a>> {
    let items_ = items(tokens);
    let kw = items_[0];
    let Some(name_tok) = items_.get(1) else {
        return Err(cut(Diagnostic::expected("variable name", kw.span.past())));
    };
    if name_tok.kind != TokenKind::Item {
        return Err(cut(Diagnostic::expected("variable name", name_tok.span)));
    }
    let name_text = st.tok(name_tok);
    let name_start = name_tok.span.start + usize::from(name_text.starts_with('$'));
    let name_text = name_text.strip_prefix('$').unwrap_or(name_text);
    let (name, typed) = match name_text.strip_suffix(':') {
        Some(n) => (n, true),
        None => (name_text, false),
    };
    if name.contains([' ', '"', '\'', '`']) || !is_identifier(name) {
        return Err(cut(Diagnostic::expected("valid variable name", name_tok.span)
            .with_help("variable names may not contain spaces, quotes or `.[({+-*^%/=!<>&|`")));
    }
    let name = Spanned::new(name, Span::new(name_start, name_start + name.len()));
    let eq_idx = items_.iter().position(|t| matches!(t.kind, TokenKind::Assign(_))).unwrap_or(items_.len());
    let eq_tok = items_.get(eq_idx).copied();
    if let Some(eq_tok) = eq_tok
        && eq_tok.kind != TokenKind::Assign(AssignOp::Assign)
    {
        return Err(cut(Diagnostic::expected("`=`", eq_tok.span)));
    }
    let mut type_items = &items_[2..eq_idx];
    let mut typed = typed;
    if let Some(first) = type_items.first()
        && st.tok(first) == ":"
    {
        typed = true;
        type_items = &type_items[1..];
    }
    let ty = match (typed, type_items.first()) {
        (true, Some(first)) => Some(signature::parse_type(st, first.span.merge(type_items.last().unwrap().span))?),
        (true, None) => return Err(cut(Diagnostic::expected("type after `:`", name_tok.span.past()))),
        (false, Some(first)) => return Err(cut(Diagnostic::new(ErrorKind::ExtraTokens, first.span))),
        (false, None) => None,
    };
    let (value, end) = match eq_tok {
        Some(eq_tok) => {
            let rhs = &tokens[eq_idx + 1..];
            let rhs_items = items(rhs);
            if rhs_items.is_empty() {
                return Err(cut(Diagnostic::expected("value after `=`", eq_tok.span.past())));
            }
            let rhs_span = rhs_items[0].span.merge(rhs_items.last().unwrap().span);
            (Some(block::parse_block_tokens(st, rhs, rhs_span)), rhs_span)
        }
        None => (None, items_.last().unwrap().span),
    };
    let binding = Binding { keyword: kw.span, name, ty, eq: eq_tok.map(|t| t.span), value };
    let span = kw.span.merge(end);
    let kind = match kind {
        BindingKind::Let => ExprKind::Let(binding),
        BindingKind::Mut => ExprKind::Mut(binding),
        BindingKind::Const => ExprKind::Const(binding),
    };
    Ok(Expr::new(kind, span))
}

fn for_stmt<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let mut i = toks(st, tokens);
    let kw = expect_item(&mut i, "for")?;
    let var_tok = expect_item(&mut i, "loop variable")?;
    let var_text = st.tok(&var_tok);
    let name_start = var_tok.span.start + usize::from(var_text.starts_with('$'));
    let var_text = var_text.strip_prefix('$').unwrap_or(var_text);
    let (name, typed) = match var_text.strip_suffix(':') {
        Some(n) => (n, true),
        None => (var_text, false),
    };
    if !is_identifier(name) {
        return Err(cut(Diagnostic::expected("valid variable name", var_tok.span)));
    }
    let var = Spanned::new(name, Span::new(name_start, name_start + name.len()));
    let ty = if typed {
        let ty_tok = expect_item(&mut i, "type")?;
        Some(signature::parse_type(st, ty_tok.span)?)
    } else {
        None
    };
    let in_tok = expect_item(&mut i, "`in`")?;
    if st.tok(&in_tok) != "in" {
        return Err(cut(Diagnostic::new(ErrorKind::ExpectedKeyword("in"), in_tok.span)));
    }
    let iter_tok = expect_item(&mut i, "value to iterate")?;
    let iterable = value::value(st, iter_tok.span, Hint::Any)?;
    let body_tok = expect_item(&mut i, "block")?;
    let body = block_item(st, &body_tok, "block")?;
    expect_end(&mut i)?;
    let span = kw.span.merge(body_tok.span);
    Ok(Expr::new(
        ExprKind::For(For { keyword: kw.span, var, ty, in_keyword: in_tok.span, iterable: Box::new(iterable), body }),
        span,
    ))
}

fn alias_stmt<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let items_ = items(tokens);
    let kw = items_[0];
    let Some(name_tok) = items_.get(1).filter(|t| t.kind == TokenKind::Item) else {
        return Err(cut(Diagnostic::expected("alias name", kw.span.past())));
    };
    let name = definition_name(st, name_tok.span)?;
    check_definition_name(&name, "alias")?;
    let Some(eq_tok) = items_.get(2) else {
        return Err(cut(Diagnostic::expected("`=`", name_tok.span.past())));
    };
    if eq_tok.kind != TokenKind::Assign(AssignOp::Assign) {
        return Err(cut(Diagnostic::expected("`=`", eq_tok.span)));
    }
    // Nushell hands everything after `=` to the expression parser as plain
    // words, so `alias ll = ls | length` is `ls` with the arguments `|` and `length`.
    let value_tokens: Vec<Token> = tokens[3..]
        .iter()
        .map(|t| if t.kind == TokenKind::Eof { *t } else { Token { kind: TokenKind::Item, span: t.span } })
        .collect();
    if items(&value_tokens).is_empty() {
        return Err(cut(Diagnostic::expected("command after `=`", eq_tok.span.past())));
    }
    let value = expr::parse_expression(st, &value_tokens)?;
    let span = kw.span.merge(value.span);
    Ok(Expr::new(ExprKind::Alias(Alias { keyword: kw.span, name, eq: eq_tok.span, value: Box::new(value) }), span))
}

fn module_stmt<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let mut i = toks(st, tokens);
    let kw = expect_item(&mut i, "module")?;
    let name_tok = expect_item(&mut i, "module name or path")?;
    let name = value::value(st, name_tok.span, Hint::String)?;
    let body = match peek_token(&i) {
        Some(t) if t.kind == TokenKind::Item && st.tok(t).starts_with('{') => {
            let t = *t;
            i.next_token();
            st.push_scope();
            let body = value::block_body(st, t.span);
            st.pop_scope();
            Some(body?)
        }
        _ => None,
    };
    expect_end(&mut i)?;
    let end = items(tokens).last().unwrap().span;
    Ok(Expr::new(ExprKind::Module(Module { keyword: kw.span, name: Box::new(name), body }), kw.span.merge(end)))
}

fn use_stmt<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let mut i = toks(st, tokens);
    let kw = expect_item(&mut i, "use")?;
    let module_tok = expect_item(&mut i, "module name or path")?;
    let module = if st.tok(&module_tok) == "null" {
        Expr::new(ExprKind::Nothing, module_tok.span)
    } else {
        value::value(st, module_tok.span, Hint::String)?
    };
    let mut members = Vec::new();
    while !at_end(&i) {
        let tok = expect_item(&mut i, "module member")?;
        if let Some(prev) = members.last()
            && matches!(prev, UseMember { kind: UseMemberKind::Glob | UseMemberKind::List(_), .. })
        {
            return Err(cut(Diagnostic::message(
                "a `*` or `[...]` member can only be at the end of an import pattern",
                tok.span,
            )));
        }
        let text = st.tok(&tok);
        let kind = if text == "*" {
            UseMemberKind::Glob
        } else if text.starts_with('[') {
            let list = value::list_or_table(st, tok.span)?;
            let ExprKind::List(list_items) = list.kind else {
                return Err(cut(Diagnostic::expected("list of names", tok.span)));
            };
            let mut names = Vec::with_capacity(list_items.len());
            for item in list_items {
                match item {
                    crate::ast::ListItem::Item(Expr { span, kind: ExprKind::String(s) }) => {
                        names.push(Spanned::new(s.value, span));
                    }
                    other => {
                        return Err(cut(Diagnostic::expected("name", other.span())));
                    }
                }
            }
            UseMemberKind::List(names)
        } else {
            UseMemberKind::Name(value::string_lit(st, tok.span)?.value)
        };
        members.push(UseMember { span: tok.span, kind });
    }
    let end = items(tokens).last().unwrap().span;
    Ok(Expr::new(ExprKind::Use(Use { keyword: kw.span, module: Box::new(module), members }), kw.span.merge(end)))
}

fn export_stmt<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let items_ = items(tokens);
    let kw = items_[0];
    let Some(next) = items_.get(1).filter(|t| t.kind == TokenKind::Item) else {
        return Err(cut(Diagnostic::expected(
            "`def`, `extern`, `alias`, `use`, `module` or `const` after `export`",
            kw.span.past(),
        )));
    };
    match st.tok(next) {
        "def" | "extern" | "alias" | "use" | "module" | "const" => {}
        other => {
            return Err(cut(Diagnostic::message(format!("`export {other}` is not a valid export"), next.span)
                .with_help("expected `def`, `extern`, `alias`, `use`, `module` or `const`")));
        }
    }
    let item = keyword_or_call(st, &tokens[1..])?;
    let span = kw.span.merge(item.span);
    Ok(Expr::new(ExprKind::Export(Export { keyword: kw.span, item: Box::new(item) }), span))
}

fn export_env_stmt<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let mut i = toks(st, tokens);
    let kw = expect_item(&mut i, "export-env")?;
    let body_tok = expect_item(&mut i, "block")?;
    let body = block_item(st, &body_tok, "block")?;
    expect_end(&mut i)?;
    Ok(Expr::new(ExprKind::ExportEnv(ExportEnv { keyword: kw.span, body }), kw.span.merge(body_tok.span)))
}

fn if_stmt<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let items_ = items(tokens);
    let kw = items_[0];
    let else_idx = items_.iter().position(|t| t.kind == TokenKind::Item && st.tok(t) == "else");
    let block_idx = match else_idx {
        Some(k) if k >= 2 => k - 1,
        Some(k) => return Err(cut(Diagnostic::expected("condition and block before `else`", items_[k].span))),
        None => items_.len() - 1,
    };
    if block_idx < 2 {
        let at = items_.get(1).map_or(kw.span.past(), |t| t.span);
        return Err(cut(Diagnostic::expected("condition", at)));
    }
    let cond_tokens = with_eof(&items_[1..block_idx]);
    let condition = expr::math_expression(st, &cond_tokens, false)?;
    let then_block = block_item(st, &items_[block_idx], "block after the condition")?;
    let mut span = kw.span.merge(items_[block_idx].span);
    let else_branch = match else_idx {
        None => None,
        Some(k) => {
            let else_tok = items_[k];
            let rest = &tokens[k + 1..];
            let rest_items = items(rest);
            if rest_items.is_empty() {
                return Err(cut(Diagnostic::expected("block or expression after `else`", else_tok.span.past())));
            }
            let body = if rest_items.len() == 1 && st.tok(&rest_items[0]).starts_with('{') {
                Expr::new(ExprKind::Block(block_item(st, &rest_items[0], "block")?), rest_items[0].span)
            } else {
                expr::parse_expression(st, rest)?
            };
            span = span.merge(body.span);
            Some(Else { keyword: else_tok.span, body: Box::new(body) })
        }
    };
    Ok(Expr::new(ExprKind::If(If { keyword: kw.span, condition: Box::new(condition), then_block, else_branch }), span))
}

fn match_stmt<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let mut i = toks(st, tokens);
    let kw = expect_item(&mut i, "match")?;
    let value_tok = expect_item(&mut i, "value to match on")?;
    let value = value::value(st, value_tok.span, Hint::Any)?;
    let block_tok = expect_item(&mut i, "match block")?;
    let (block_span, arms) = pattern::match_block(st, block_tok.span)?;
    expect_end(&mut i)?;
    Ok(Expr::new(
        ExprKind::Match(Match { keyword: kw.span, value: Box::new(value), block_span, arms }),
        kw.span.merge(block_tok.span),
    ))
}

fn while_stmt<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let items_ = items(tokens);
    let kw = items_[0];
    if items_.len() < 3 {
        let at = items_.get(1).map_or(kw.span.past(), |t| t.span.past());
        return Err(cut(Diagnostic::expected("condition and block", at)));
    }
    let block_tok = items_[items_.len() - 1];
    let cond_tokens = with_eof(&items_[1..items_.len() - 1]);
    let condition = expr::math_expression(st, &cond_tokens, false)?;
    let body = block_item(st, &block_tok, "block")?;
    Ok(Expr::new(
        ExprKind::While(While { keyword: kw.span, condition: Box::new(condition), body }),
        kw.span.merge(block_tok.span),
    ))
}

fn loop_stmt<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let mut i = toks(st, tokens);
    let kw = expect_item(&mut i, "loop")?;
    let body_tok = expect_item(&mut i, "block")?;
    let body = block_item(st, &body_tok, "block")?;
    expect_end(&mut i)?;
    Ok(Expr::new(ExprKind::Loop(Loop { keyword: kw.span, body }), kw.span.merge(body_tok.span)))
}

fn try_stmt<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let mut i = toks(st, tokens);
    let kw = expect_item(&mut i, "try")?;
    let body_tok = expect_item(&mut i, "block")?;
    let body = block_item(st, &body_tok, "block")?;
    let mut end = body_tok.span;
    let mut handlers = Vec::new();
    while !at_end(&i) {
        let kw_tok = expect_item(&mut i, "`catch` or `finally`")?;
        let kind = match st.tok(&kw_tok) {
            "catch" => HandlerKind::Catch,
            "finally" => HandlerKind::Finally,
            _ => return Err(cut(Diagnostic::expected("`catch` or `finally`", kw_tok.span))),
        };
        if handlers.len() == 2 {
            return Err(cut(Diagnostic::new(ErrorKind::ExtraTokens, kw_tok.span)
                .with_help("`try` takes at most two handlers (`catch` and `finally`)")));
        }
        let handler_tok = expect_item(&mut i, "closure")?;
        let handler = value::value(st, handler_tok.span, Hint::Closure)?;
        end = handler_tok.span;
        handlers.push(Handler { kind, keyword: kw_tok.span, body: Box::new(handler) });
    }
    Ok(Expr::new(ExprKind::Try(Try { keyword: kw.span, body, handlers }), kw.span.merge(end)))
}

fn return_stmt<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let mut i = toks(st, tokens);
    let kw = expect_item(&mut i, "return")?;
    let value = if at_end(&i) {
        None
    } else {
        let tok = expect_item(&mut i, "value")?;
        Some(Box::new(value::value(st, tok.span, Hint::Any)?))
    };
    expect_end(&mut i)?;
    let span = value.as_ref().map_or(kw.span, |v| kw.span.merge(v.span));
    Ok(Expr::new(ExprKind::Return(Return { keyword: kw.span, value }), span))
}

fn simple_stmt<'a>(st: St<'_, 'a>, tokens: &[Token], kind: ExprKind<'a>) -> PResult<Expr<'a>> {
    let mut i = toks(st, tokens);
    let kw = expect_item(&mut i, "keyword")?;
    expect_end(&mut i)?;
    Ok(Expr::new(kind, kw.span))
}

fn where_stmt<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let items_ = items(tokens);
    let kw = items_[0];
    let rest = &tokens[1..];
    let rest_items = items(rest);
    if rest_items.is_empty() {
        return Err(cut(Diagnostic::expected("row condition or closure", kw.span.past())));
    }
    let condition = if rest_items.len() == 1 && st.tok(&rest_items[0]).starts_with('{') {
        value::closure(st, rest_items[0].span)?
    } else {
        expr::math_expression(st, rest, true)?
    };
    let span = kw.span.merge(condition.span);
    Ok(Expr::new(ExprKind::Where(Where { keyword: kw.span, condition: Box::new(condition) }), span))
}
