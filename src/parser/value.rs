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

use crate::ast::{Block, Closure, Expr, ExprKind, Signature};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut};
use crate::lexer::{LexOptions, TokenKind, lex_prefix};
use crate::span::Span;

use super::cursor::Cursor;
use super::{St, block, cellpath, collections, literal, signature, strings};

/// What the surrounding grammar expects an item to be. This only matters for
/// `{ ... }` (closure vs record vs block) and for a few literal positions.
/// Statement bodies (`if`, `def`, ...) never go through [`value`]: the
/// statement parsers call [`block_body`] directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hint {
    /// Anything.
    Any,
    /// A closure (`{|x| ...}` or `{ ... }`).
    Closure,
    /// A `match` arm body: a block, unless it is written as a closure or a record.
    MatchBody,
    /// A number (range bounds).
    Number,
    /// A string (record keys, command names).
    String,
    /// A signature `[...]` / `(...)`.
    Signature,
}

/// Parse one item.
pub fn value<'a>(st: St<'_, 'a>, span: Span, hint: Hint) -> PResult<Expr<'a>> {
    let text = st.text(span);
    match text.as_bytes() {
        [] => Err(cut(Diagnostic::expected("value", span))),
        [b'$', ..] => cellpath::dollar(st, span),
        [b'(', ..] => cellpath::paren(st, span, hint),
        [b'{', ..] => brace(st, span, hint),
        [b'[', ..] if hint == Hint::Signature => Ok(signature_placeholder(span)),
        [b'[', ..] => cellpath::full_cell_path(st, span, false),
        [b'r', b'#', ..] => literal::raw_string(st, span),
        _ => match hint {
            Hint::Number => literal::number(st, span),
            Hint::String => strings::string(st, span),
            Hint::MatchBody => Err(cut(Diagnostic::expected("block", span))),
            Hint::Closure => Err(cut(Diagnostic::expected("closure", span))),
            Hint::Signature => Err(cut(Diagnostic::expected("signature", span))),
            Hint::Any => any_value(st, span, text),
        },
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
    // is an error, not a bare word.
    if let Some(radix) = literal::radix_prefix(text) {
        return Err(invalid_literal("int", &format!("invalid digits for radix {radix}"), span));
    }
    if let Some(float) = literal::parse_float(text) {
        return Ok(Expr::new(ExprKind::Float(float), span));
    }
    strings::string(st, span)
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
                || text.starts_with("0x[")
                || text.starts_with("0o[")
                || text.starts_with("0b[")
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

/// What the first two tokens of a `{ ... }` body say about it.
enum BraceShape {
    Empty,
    ClosureParams,
    Record,
    Other,
}

fn probe_brace(st: St<'_, '_>, inner: Span) -> BraceShape {
    let probe = lex_prefix(st.text(inner), inner.start, LexOptions::BRACE_PROBE, 2).unwrap_or_default();
    match probe.as_slice() {
        [first, ..] if matches!(first.kind, TokenKind::Pipe | TokenKind::PipePipe) => BraceShape::ClosureParams,
        [_, second, ..] if st.tok(second) == ":" => BraceShape::Record,
        [first, ..] if first.kind == TokenKind::Item && is_spread(st.tok(first), b"{$(") => BraceShape::Record,
        [first, ..] if first.kind != TokenKind::Eof => BraceShape::Other,
        _ => BraceShape::Empty,
    }
}

/// `{ ... }`: a record, a closure or a block, decided as `nu-parser` does
/// from the first two tokens of the body and the surrounding hint.
fn brace<'a>(st: St<'_, 'a>, span: Span, hint: Hint) -> PResult<Expr<'a>> {
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
        (BraceShape::Other, Hint::MatchBody) => block_expr(st, span),
        (BraceShape::Other, Hint::Closure | Hint::Any) => closure(st, span),
        (BraceShape::Other, Hint::Number | Hint::String | Hint::Signature) => {
            Err(cut(Diagnostic::expected("value", span).with_help("found a block or closure")))
        }
    }
}

/// Parse `{ ... }` as a block (no parameters allowed).
pub fn block_body<'a>(st: St<'_, 'a>, span: Span) -> PResult<Block<'a>> {
    let inner = interior(st, span, "{", "}")?;
    let tokens = st.lex_span(inner, LexOptions::BLOCK).map_err(cut)?;
    if let Some(first) = tokens.iter().find(|t| t.kind != TokenKind::Eol)
        && matches!(first.kind, TokenKind::Pipe | TokenKind::PipePipe)
    {
        return Err(cut(Diagnostic::expected("block", first.span)
            .with_help("found closure parameters; blocks cannot have parameters")));
    }
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
    Ok(Expr::new(ExprKind::Closure(Closure { params, body }), span))
}
