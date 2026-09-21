//! String items: quoted, bare, raw, and interpolated.

use std::borrow::Cow;

use crate::ast::{Expr, ExprKind, InterpPart, Interpolation, Quote, StringLit};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut};
use crate::lexer::interp_subexpr_step;
use crate::span::Span;

use super::value::Hint;
use super::{St, cellpath, literal};

/// `true` for a bare word that Nushell treats as an interpolation because it
/// contains `(`: `foo(1 + 1)bar`.
fn is_bare_interpolation(text: &str) -> bool {
    !text.starts_with(['\'', '"', '`']) && text.contains('(')
}

/// Parse a string item: quoted (`"`, `'`, `` ` ``), raw, bare, or a bare interpolation.
pub fn string<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let text = st.text(span);
    match text {
        "" => Err(cut(Diagnostic::expected("string", span))),
        _ if text.starts_with("r#") => literal::raw_string(st, span),
        _ if is_bare_interpolation(text) => interpolation(st, span),
        _ => Ok(Expr::new(ExprKind::String(string_lit(st, span)?), span)),
    }
}

/// Parse a string item into its literal. Bare-word interpolation is *not*
/// handled here.
pub fn string_lit<'a>(st: St<'_, 'a>, span: Span) -> PResult<StringLit<'a>> {
    let text = st.text(span);
    match text.as_bytes().first() {
        Some(b'"') => {
            let body = quoted_body(st, span, b'"')?;
            Ok(StringLit { value: literal::unescape(body, span.start + 1).map_err(cut)?, quote: Quote::Double })
        }
        Some(b'\'') => Ok(StringLit { value: Cow::Borrowed(quoted_body(st, span, b'\'')?), quote: Quote::Single }),
        Some(b'`') => Ok(StringLit { value: Cow::Borrowed(quoted_body(st, span, b'`')?), quote: Quote::Backtick }),
        _ => Ok(StringLit::bare(text)),
    }
}

/// The text between the quotes of a quoted item.
///
/// Like Nushell, the *last* quote character in the item must be its final
/// byte (`"a"b` is an error) but quotes in between are kept as text, so
/// `"a"b"c"` is the string `a"b"c`. Backticks are not checked, only trimmed.
fn quoted_body<'a>(st: St<'_, 'a>, span: Span, quote: u8) -> PResult<&'a str> {
    let text = st.text(span);
    let bytes = text.as_bytes();
    if quote == b'`' {
        let closed = bytes.len() >= 2 && bytes[bytes.len() - 1] == b'`';
        return Ok(if closed { &text[1..text.len() - 1] } else { &text[1..] });
    }
    match bytes.iter().rposition(|b| *b == quote) {
        Some(0) | None => Err(cut(Diagnostic::new(
            ErrorKind::Unclosed { delimiter: quote_str(quote), open: Span::new(span.start, span.start + 1) },
            span.past(),
        ))),
        Some(last) if last + 1 != bytes.len() => {
            Err(cut(Diagnostic::new(ErrorKind::ExtraTokens, Span::new(span.start + last + 1, span.end))
                .with_help("invalid characters after the closing quote; quote the whole string or remove them")))
        }
        Some(last) => Ok(&text[1..last]),
    }
}

fn quote_str(q: u8) -> &'static str {
    match q {
        b'"' => "\"",
        b'\'' => "'",
        _ => "`",
    }
}

/// Parse `$"..."`, `$'...'` or a bare interpolation `foo(...)`.
pub fn interpolation<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let text = st.text(span);
    let (quote, body) = match text.as_bytes() {
        [b'$', q @ (b'"' | b'\''), ..] => {
            let body = quoted_body(st, Span::new(span.start + 1, span.end), *q)?;
            let body = Span::new(span.start + 2, span.start + 2 + body.len());
            (if *q == b'"' { Quote::Double } else { Quote::Single }, body)
        }
        _ => (Quote::Bare, span),
    };
    let parts = interpolation_parts(st, body, quote)?;
    Ok(Expr::new(ExprKind::Interpolation(Interpolation { quote, parts }), span))
}

/// Split the body of an interpolated string into literal text and `( ... )`
/// subexpressions, using the same delimiter rules as the lexer.
fn interpolation_parts<'a>(st: St<'_, 'a>, body: Span, quote: Quote) -> PResult<Vec<InterpPart<'a>>> {
    let text = st.text(body);
    let bytes = text.as_bytes();
    let double = quote == Quote::Double;
    let mut parts = Vec::new();
    let mut idx = 0;
    let mut part_start = 0;
    let mut backslashes = 0usize;
    let mut stack: Vec<(u8, usize)> = Vec::new();
    let mut in_expr = false;

    let text_part = |start: usize, end: usize| -> PResult<Option<InterpPart<'a>>> {
        if start >= end {
            return Ok(None);
        }
        let span = Span::new(body.start + start, body.start + end);
        let raw = &text[start..end];
        let value = if double { literal::unescape(raw, span.start).map_err(cut)? } else { Cow::Borrowed(raw) };
        Ok(Some(InterpPart::Text { span, value }))
    };

    while idx < bytes.len() {
        let c = bytes[idx];
        if !in_expr {
            let preceding = backslashes;
            backslashes = if c == b'\\' { preceding + 1 } else { 0 };
            // In double quotes `\(` is a literal parenthesis.
            if c == b'(' && (!double || preceding.is_multiple_of(2)) {
                parts.extend(text_part(part_start, idx)?);
                in_expr = true;
                part_start = idx;
                stack.push((b')', idx));
            }
            idx += 1;
            continue;
        }
        if interp_subexpr_step(&mut stack, c, idx) && idx + 1 < bytes.len() {
            idx += 2;
            continue;
        }
        if c == b')' && stack.is_empty() {
            let span = Span::new(body.start + part_start, body.start + idx + 1);
            parts.push(InterpPart::Expr(cellpath::paren(st, span, Hint::Any)?));
            in_expr = false;
            part_start = idx + 1;
        }
        idx += 1;
    }
    if in_expr {
        return Err(cut(Diagnostic::new(
            ErrorKind::Unclosed {
                delimiter: ")",
                open: Span::new(body.start + part_start, body.start + part_start + 1),
            },
            body.past(),
        )));
    }
    parts.extend(text_part(part_start, bytes.len())?);
    Ok(parts)
}
