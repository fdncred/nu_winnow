//! Parsing a single lexed item into an expression.
//!
//! [`value`] is the entry point. It dispatches on the first character of the
//! item (`$`, `(`, `{`, `[`, `r#`) and otherwise tries the literal parsers in
//! the same order as the reference implementation: binary, range, filesize,
//! duration, datetime, int, float, and finally a string (bare or quoted).
//!
//! Strings live in `strings.rs`, `$` expressions, cell paths and ranges in
//! `cellpath.rs`, lists, tables and records in `collections.rs`. This file
//! keeps the dispatch and the `{ ... }` / `( ... )` forms.

use crate::ast::{Block, Closure, Expr, ExprKind, Signature, TypeKind};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut};
use crate::lexer::{LexOptions, TokenKind, lex_prefix};
use crate::span::Span;

use super::cursor::Cursor;
use super::{St, block, cellpath, collections, expr, literal, signature, strings};

/// What the surrounding grammar expects an item to be: nu's `SyntaxShape`
/// for the argument position. It decides what `{ ... }` is (closure, record
/// or block), which literals a bare word may be, and what `[` may start.
/// Statement bodies (`if`, `def`, ...) never go through [`value`]: the
/// statement parsers call [`block_body`] directly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hint<'t, 'a> {
    /// Anything.
    Any,
    /// A closure (`{|x| ...}` or `{ ... }`).
    Closure,
    /// A `match` arm body: a block, unless it is written as a closure or a record.
    MatchBody,
    /// A number (range bounds).
    Number,
    /// A string (record keys, module names, `record<...>` field names):
    /// `true`, `false` and `null` are refused and `[...]` is a bare word.
    String,
    /// A signature `[...]` / `(...)`.
    Signature,
    /// The declared type of a parameter whose default value this is
    /// (`[x: int = 1]`): nu parses the default with that shape.
    Typed(&'t TypeKind<'a>),
}

/// Parse one item.
pub fn value<'a>(st: St<'_, 'a>, span: Span, hint: Hint<'_, 'a>) -> PResult<Expr<'a>> {
    let text = st.text(span);
    if let Hint::Typed(kind) = hint {
        return typed_value(st, span, kind);
    }
    match text.as_bytes() {
        [] => Err(cut(Diagnostic::expected("value", span))),
        [b'$', ..] => cellpath::dollar(st, span),
        [b'(', ..] => cellpath::paren(st, span, hint),
        [b'{', ..] => brace(st, span, hint),
        [b'[', ..] if hint == Hint::Signature => Ok(signature_placeholder(span)),
        // `parse_string` on `[a b]`: the text is a bare word.
        [b'[', ..] if hint == Hint::String => strings::string(st, span),
        [b'[', ..] if hint == Hint::Number => Err(cut(Diagnostic::expected("number", span))),
        [b'[', ..] => cellpath::full_cell_path(st, span, false),
        [b'r', b'#', ..] => literal::raw_string(st, span),
        _ => match hint {
            Hint::Number => literal::number(st, span),
            Hint::String => string_shape(st, span),
            Hint::MatchBody => Err(cut(Diagnostic::expected("block", span))),
            Hint::Closure => Err(cut(Diagnostic::expected("closure", span))),
            Hint::Signature => Err(cut(Diagnostic::expected("signature", span))),
            Hint::Any | Hint::Typed(_) => any_value(st, span, text),
        },
    }
}

/// nu's `SyntaxShape::String`: the keywords are refused, everything else is a string.
fn string_shape<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    match st.text(span) {
        kw @ ("true" | "false" | "null") => Err(cut(Diagnostic::expected("string", span)
            .with_help(format!("`{kw}` is a value; quote it to use it as a string")))),
        _ => strings::string(st, span),
    }
}

/// Signatures are not expressions in this AST; statement parsers call
/// [`signature::parse_signature`] directly. Reaching one through [`value`]
/// only happens on error-recovery paths, where it becomes a garbage node.
fn signature_placeholder<'a>(span: Span) -> Expr<'a> {
    Expr::new(ExprKind::Garbage, span)
}

fn any_value<'a>(st: St<'_, 'a>, span: Span, text: &'a str) -> PResult<Expr<'a>> {
    match text {
        "null" => return Ok(Expr::new(ExprKind::Nothing, span)),
        "true" => return Ok(Expr::new(ExprKind::Bool(true), span)),
        "false" => return Ok(Expr::new(ExprKind::Bool(false), span)),
        _ => {}
    }
    if let Some(binary) = literal::binary(st, span) {
        return binary;
    }
    if cellpath::is_range_syntax(text) {
        return cellpath::range(st, span);
    }
    if let Some(filesize) = literal::filesize(text) {
        let filesize = filesize.map_err(|msg| invalid_literal("filesize", msg, span))?;
        return Ok(Expr::new(ExprKind::Filesize(filesize), span));
    }
    if let Some(duration) = literal::duration(text) {
        let duration = duration.map_err(|msg| invalid_literal("duration", msg, span))?;
        return Ok(Expr::new(ExprKind::Duration(duration), span));
    }
    if literal::is_datetime(text) {
        return Ok(Expr::new(ExprKind::DateTime(text), span));
    }
    if let Some(int) = literal::parse_int(text) {
        return Ok(Expr::new(ExprKind::Int(int), span));
    }
    // A radix prefix commits the word to being an int, as in Nushell: `0b2`
    // and `0x[13]=` are errors, not bare words.
    if let Some(radix) = literal::radix_prefix(text) {
        return Err(invalid_literal("int", &format!("invalid digits for radix {radix}"), span));
    }
    if let Some(float) = literal::parse_float(text) {
        return Ok(Expr::new(ExprKind::Float(float), span));
    }
    strings::string(st, span)
}

/// nu's `parse_value` with a declared shape: the value a parameter default
/// must be. `$`, `(` and `{` items are what they always are (a closure is
/// refused for a `block`), `[` is allowed for the list-like and string-like
/// shapes only, and a bare word must be a literal of the shape.
fn typed_value<'a>(st: St<'_, 'a>, span: Span, kind: &TypeKind<'a>) -> PResult<Expr<'a>> {
    let text = st.text(span);
    let name = type_name(kind);
    let expected = || cut(Diagnostic::expected(name, span));
    match text.as_bytes() {
        [] => return Err(cut(Diagnostic::expected("value", span))),
        [b'$', ..] => return cellpath::dollar(st, span),
        [b'(', ..] => return cellpath::paren(st, span, Hint::Any),
        [b'{', ..] => {
            let hint = match kind {
                TypeKind::Closure => Hint::Closure,
                TypeKind::Any => Hint::Any,
                _ => Hint::String,
            };
            return brace(st, span, hint).map_err(|e| {
                e.map(|d| match d.kind {
                    ErrorKind::Expected(_) => Diagnostic::expected(name, span).with_help("found a block or closure"),
                    _ => d,
                })
            });
        }
        [b'[', ..] => {
            return match kind {
                TypeKind::Any | TypeKind::Table(_) | TypeKind::ExternalArg => cellpath::full_cell_path(st, span, false),
                TypeKind::List(elem) => collections::list_or_table_typed(st, span, elem.as_deref().map(|t| &t.kind)),
                TypeKind::String | TypeKind::Path | TypeKind::Glob => strings::string(st, span),
                TypeKind::OneOf(types) => one_of(st, span, types),
                _ => Err(expected()),
            };
        }
        [b'r', b'#', ..] => return literal::raw_string(st, span),
        _ => {}
    }
    let lit = |kind: ExprKind<'a>| Ok(Expr::new(kind, span));
    match kind {
        TypeKind::Any => any_value(st, span, text),
        TypeKind::Number => literal::number(st, span),
        TypeKind::Float => literal::parse_float(text).map_or_else(|| Err(expected()), |f| lit(ExprKind::Float(f))),
        TypeKind::Int => match literal::parse_int(text) {
            Some(i) => lit(ExprKind::Int(i)),
            None if literal::radix_prefix(text).is_some() => any_value(st, span, text),
            None => Err(expected()),
        },
        TypeKind::Duration => match literal::duration(text) {
            Some(Ok(d)) => lit(ExprKind::Duration(d)),
            Some(Err(msg)) => Err(invalid_literal("duration", msg, span)),
            None => Err(expected()),
        },
        TypeKind::Filesize => match literal::filesize(text) {
            Some(Ok(f)) => lit(ExprKind::Filesize(f)),
            Some(Err(msg)) => Err(invalid_literal("filesize", msg, span)),
            None => Err(expected()),
        },
        TypeKind::DateTime if literal::is_datetime(text) => lit(ExprKind::DateTime(text)),
        TypeKind::Range if cellpath::is_range_syntax(text) => cellpath::range(st, span),
        TypeKind::Bool if text == "true" => lit(ExprKind::Bool(true)),
        TypeKind::Bool if text == "false" => lit(ExprKind::Bool(false)),
        TypeKind::Nothing if text == "null" => lit(ExprKind::Nothing),
        TypeKind::String | TypeKind::Path | TypeKind::Directory | TypeKind::Glob => string_shape(st, span),
        TypeKind::Binary => literal::binary(st, span).unwrap_or_else(|| Err(expected())),
        TypeKind::CellPath => cellpath::cell_path_literal(st, span),
        TypeKind::ExternalArg => expr::external_string(st, span),
        TypeKind::OneOf(types) => one_of(st, span, types),
        _ => Err(expected()),
    }
}

/// `oneof<a, b>`: the first shape the value parses as.
fn one_of<'a>(st: St<'_, 'a>, span: Span, types: &[crate::ast::TypeAnnotation<'a>]) -> PResult<Expr<'a>> {
    let mut first_error = None;
    for ty in types {
        match typed_value(st, span, &ty.kind) {
            Ok(v) => return Ok(v),
            Err(e) => first_error.get_or_insert(e),
        };
    }
    Err(first_error.unwrap_or_else(|| cut(Diagnostic::expected("value", span))))
}

/// The word nu uses for a shape in "expected ..." errors.
fn type_name(kind: &TypeKind<'_>) -> &'static str {
    match kind {
        TypeKind::Any => "any",
        TypeKind::Binary => "binary",
        TypeKind::Bool => "bool",
        TypeKind::CellPath => "cell-path",
        TypeKind::Closure => "closure",
        TypeKind::DateTime => "datetime",
        TypeKind::Directory => "directory",
        TypeKind::Duration => "duration",
        TypeKind::Error => "error",
        TypeKind::ExternalArg => "external argument",
        TypeKind::Float => "float",
        TypeKind::Filesize => "filesize with valid units",
        TypeKind::Glob => "glob pattern",
        TypeKind::Int => "int",
        TypeKind::Nothing => "nothing",
        TypeKind::Number => "number",
        TypeKind::Path => "path",
        TypeKind::Range => "range",
        TypeKind::String => "string",
        TypeKind::List(_) => "list",
        TypeKind::Record(_) => "record",
        TypeKind::Table(_) => "table",
        TypeKind::OneOf(_) => "one of the accepted shapes",
    }
}

fn invalid_literal(kind: &'static str, message: &str, span: Span) -> winnow::error::ErrMode<Diagnostic> {
    cut(Diagnostic::new(ErrorKind::InvalidLiteral { kind, message: message.into() }, span))
}

/// `true` if `text` is one of the things Nushell parses as the start of a math
/// expression rather than a command name (`is_math_expression_like`).
pub fn looks_like_value(text: &str) -> bool {
    match text.as_bytes() {
        [] => false,
        b"true" | b"false" | b"null" | b"not" | b"if" | b"match" => true,
        [b'r', b'#', ..] | [b'(' | b'{' | b'[' | b'$' | b'"' | b'\'' | b'-', ..] => true,
        _ => {
            literal::parse_int(text).is_some()
                || literal::parse_float(text).is_some()
                || literal::filesize(text).is_some_and(|r| r.is_ok())
                || literal::duration(text).is_some_and(|r| r.is_ok())
                || literal::is_datetime(text)
                || literal::looks_like_binary(text)
                || cellpath::is_range_syntax(text)
        }
    }
}

/// `true` for `...x` where `x` starts with one of `heads`.
pub fn is_spread(text: &str, heads: &[u8]) -> bool {
    text.len() > 3 && text.starts_with("...") && heads.contains(&text.as_bytes()[3])
}

/// The interior of a delimited item, checking the closing delimiter.
pub fn interior(st: St<'_, '_>, span: Span, open: &'static str, close: &'static str) -> PResult<Span> {
    let text = st.text(span);
    if !text.starts_with(open) {
        return Err(cut(Diagnostic::expected(open, Span::new(span.start, span.start + 1))));
    }
    if text.len() < 2 || !text.ends_with(close) {
        return Err(cut(Diagnostic::new(
            ErrorKind::Unclosed { delimiter: close, open: Span::new(span.start, span.start + 1) },
            span.past(),
        )));
    }
    Ok(Span::new(span.start + 1, span.end - 1))
}

/// Parse `( ... )` as a subexpression. Newlines inside are whitespace.
pub fn subexpression<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let inner = interior(st, span, "(", ")")?;
    let tokens = st.lex_span(inner, LexOptions::SUBEXPRESSION).map_err(cut)?;
    st.push_scope();
    let block = block::parse_block(st, Cursor::from_lexed(&tokens), inner);
    st.pop_scope();
    Ok(Expr::new(ExprKind::Subexpression(block), span))
}

/// What the first two tokens of a `{ ... }` body say about it (nu's
/// `parse_brace_expr` looks at the same two tokens).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BraceShape {
    /// `{}` or only whitespace and comments.
    Empty,
    /// Starts with `|` or `||`: closure parameters.
    ClosureParams,
    /// `key:`: a record.
    Record,
    /// `...spread`: a record in value position, a block or closure elsewhere.
    Spread,
    /// Anything else: code.
    Other,
}

fn probe_brace(st: St<'_, '_>, inner: Span) -> BraceShape {
    let probe = lex_prefix(st.text(inner), inner.start, LexOptions::BRACE_PROBE, 2).unwrap_or_default();
    match probe.as_slice() {
        [first, ..] if matches!(first.kind, TokenKind::Pipe | TokenKind::PipePipe) => BraceShape::ClosureParams,
        [_, second, ..] if st.tok(second) == ":" => BraceShape::Record,
        [first, ..] if first.kind == TokenKind::Item && is_spread(st.tok(first), b"{$(") => BraceShape::Spread,
        [first, ..] if first.kind != TokenKind::Eof => BraceShape::Other,
        _ => BraceShape::Empty,
    }
}

/// The shape of a `{ ... }` item (checking that it closes).
pub fn brace_shape(st: St<'_, '_>, span: Span) -> PResult<BraceShape> {
    let inner = interior(st, span, "{", "}")?;
    Ok(probe_brace(st, inner))
}

/// `{ ... }`: a record, a closure or a block, decided as `nu-parser` does
/// from the first two tokens of the body and the surrounding hint.
fn brace<'a>(st: St<'_, 'a>, span: Span, hint: Hint<'_, 'a>) -> PResult<Expr<'a>> {
    let text = st.text(span);
    if !text.ends_with('}') {
        // `{a: 1}.a`
        return cellpath::full_cell_path(st, span, false);
    }
    let inner = Span::new(span.start + 1, span.end - 1);
    match (probe_brace(st, inner), hint) {
        (BraceShape::Empty, Hint::Closure) => closure(st, span),
        (BraceShape::Empty, Hint::MatchBody) => block_expr(st, span),
        (BraceShape::Empty, _) => collections::record(st, span),
        (BraceShape::ClosureParams, _) => closure(st, span),
        (BraceShape::Record, _) => collections::record(st, span),
        (BraceShape::Spread, Hint::Closure) => closure(st, span),
        (BraceShape::Spread, Hint::MatchBody) => block_expr(st, span),
        (BraceShape::Spread, _) => collections::record(st, span),
        (BraceShape::Other, Hint::MatchBody) => block_expr(st, span),
        (BraceShape::Other, Hint::Closure | Hint::Any) => closure(st, span),
        (BraceShape::Other, Hint::Number | Hint::String | Hint::Signature | Hint::Typed(_)) => {
            Err(cut(Diagnostic::expected("value", span).with_help("found a block or closure")))
        }
    }
}

/// Parse `{ ... }` as a block: no parameters, and not a record.
pub fn block_body<'a>(st: St<'_, 'a>, span: Span) -> PResult<Block<'a>> {
    let inner = interior(st, span, "{", "}")?;
    match probe_brace(st, inner) {
        BraceShape::ClosureParams => {
            return Err(cut(Diagnostic::expected("block", span)
                .with_help("found closure parameters; blocks cannot have parameters")));
        }
        BraceShape::Record => {
            return Err(cut(Diagnostic::expected("block", span).with_help("found a record")));
        }
        BraceShape::Empty | BraceShape::Spread | BraceShape::Other => {}
    }
    block_unchecked(st, span)
}

/// Parse `{ ... }` as a block without looking at its shape (the body of a
/// `def`, which nu parses as a closure whatever it starts with).
pub fn block_unchecked<'a>(st: St<'_, 'a>, span: Span) -> PResult<Block<'a>> {
    let inner = interior(st, span, "{", "}")?;
    let tokens = st.lex_span(inner, LexOptions::BLOCK).map_err(cut)?;
    st.push_scope();
    let block = block::parse_block(st, Cursor::from_lexed(&tokens), inner);
    st.pop_scope();
    Ok(block)
}

fn block_expr<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    Ok(Expr::new(ExprKind::Block(block_body(st, span)?), span))
}

/// Parse `{|params| body}` or `{ body }` as a closure.
pub fn closure<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    Ok(Expr::new(ExprKind::Closure(closure_parts(st, span)?), span))
}

/// The parameters and body of a `{|params| body}` or `{ body }` item.
pub fn closure_parts<'a>(st: St<'_, 'a>, span: Span) -> PResult<Closure<'a>> {
    let inner = interior(st, span, "{", "}")?;
    let tokens = st.lex_span(inner, LexOptions::BLOCK).map_err(cut)?;
    // The parameter list may start on a later line: `{\n  |x| ... }`.
    let first = tokens.iter().position(|t| t.kind != TokenKind::Eol).unwrap_or(tokens.len());
    let (params, body_start) = match tokens.get(first).map(|t| t.kind) {
        Some(TokenKind::Pipe) => {
            let open = tokens[first];
            let close_idx =
                tokens.iter().skip(first + 1).position(|t| t.kind == TokenKind::Pipe).map(|p| p + first + 1);
            let Some(close_idx) = close_idx else {
                return Err(cut(Diagnostic::new(
                    ErrorKind::Unclosed { delimiter: "|", open: open.span },
                    inner.past(),
                )
                .with_context("closure parameters")));
            };
            let close = tokens[close_idx];
            let sig = signature::parse_signature_inner(
                st,
                Span::new(open.span.end, close.span.start),
                open.span.merge(close.span),
                false,
            )?;
            (Some(sig), close_idx + 1)
        }
        Some(TokenKind::PipePipe) => (Some(Signature { span: tokens[first].span, ..Signature::default() }), first + 1),
        _ => (None, 0),
    };
    let body_tokens = &tokens[body_start..];
    let body_span = Span::new(body_tokens.first().map_or(inner.end, |t| t.span.start), inner.end);
    st.push_scope();
    let body = block::parse_block(st, Cursor::from_lexed(body_tokens), body_span);
    st.pop_scope();
    Ok(Closure { params, body })
}
