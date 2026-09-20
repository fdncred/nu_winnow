//! Parsing a single lexed item into an expression.
//!
//! [`value`] is the entry point. It dispatches on the first character of the
//! item (`$`, `(`, `{`, `[`, `r#`) and otherwise tries the literal parsers in
//! the same order as the reference implementation: binary, range, filesize,
//! duration, datetime, int, float, and finally a string (bare or quoted).
//! Nested constructs re-lex the interior of the item.

use std::borrow::Cow;

use crate::ast::{
    Block, CellPath, Closure, Expr, ExprKind, FullCellPath, InterpPart, Interpolation, ListItem, PathMember,
    PathMemberKind, Quote, Range, RangeInclusion, RecordItem, Signature, StringLit, Table, Var,
};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut};
use crate::lexer::{LexOptions, Token, TokenKind, interp_subexpr_step, lex_prefix, lex_prefix_at};
use crate::span::Span;

use super::{St, block, literal, signature};

/// What the surrounding grammar expects an item to be. This only matters for
/// `{ ... }` (block vs closure vs record) and for a few literal positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hint {
    /// Anything.
    Any,
    /// A `{ ... }` block (bodies of `if`, `for`, `def`, ...).
    Block,
    /// A closure (`{|x| ...}` or `{ ... }`).
    Closure,
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
    let Some(&first) = text.as_bytes().first() else {
        return Err(cut(Diagnostic::expected("value", span)));
    };
    match first {
        b'$' => return dollar(st, span),
        b'(' => return paren(st, span, hint),
        b'{' => return brace(st, span, hint),
        b'[' => {
            return match hint {
                Hint::Signature => signature::parse_signature(st, span).map(|s| signature_expr(s, span)),
                _ => full_cell_path(st, span, false),
            };
        }
        b'r' if text.as_bytes().get(1) == Some(&b'#') => return literal::raw_string(st, span),
        _ => {}
    }
    match hint {
        Hint::Number => literal::number(st, span),
        Hint::String => string(st, span),
        Hint::Block => Err(cut(Diagnostic::expected("block", span))),
        Hint::Closure => Err(cut(Diagnostic::expected("closure", span))),
        Hint::Signature => Err(cut(Diagnostic::expected("signature", span))),
        Hint::Any => any_value(st, span, text),
    }
}

/// Signatures are not expressions in this AST; callers that need one use
/// [`signature::parse_signature`] directly. When one is parsed through
/// [`value`] (only for error recovery paths) it becomes a garbage node.
fn signature_expr<'a>(_sig: Signature<'a>, span: Span) -> Expr<'a> {
    Expr::new(ExprKind::Garbage, span)
}

fn any_value<'a>(st: St<'_, 'a>, span: Span, text: &'a str) -> PResult<Expr<'a>> {
    match text {
        "null" => return Ok(Expr::new(ExprKind::Nothing, span)),
        "true" => return Ok(Expr::new(ExprKind::Bool(true), span)),
        "false" => return Ok(Expr::new(ExprKind::Bool(false), span)),
        _ => {}
    }
    if let Some(bin) = literal::binary(st, span) {
        return bin;
    }
    if is_range_candidate(text)
        && let Some(range) = range(st, span)
    {
        return Ok(range);
    }
    if let Some(fs) = literal::filesize(text) {
        return fs.map(|fs| Expr::new(ExprKind::Filesize(fs), span)).map_err(|msg| {
            cut(Diagnostic::new(ErrorKind::InvalidLiteral { kind: "filesize", message: msg.into() }, span))
        });
    }
    if let Some(d) = literal::duration(text) {
        return d.map(|d| Expr::new(ExprKind::Duration(d), span)).map_err(|msg| {
            cut(Diagnostic::new(ErrorKind::InvalidLiteral { kind: "duration", message: msg.into() }, span))
        });
    }
    if literal::is_datetime(text) {
        return Ok(Expr::new(ExprKind::DateTime(text), span));
    }
    if let Some(i) = literal::parse_int(text) {
        return Ok(Expr::new(ExprKind::Int(i), span));
    }
    if let Some(f) = literal::parse_float(text) {
        return Ok(Expr::new(ExprKind::Float(f), span));
    }
    string(st, span)
}

/// `true` if `text` is one of the things Nushell parses as the start of a math
/// expression rather than a command name (`is_math_expression_like`).
pub fn looks_like_value(st: St<'_, '_>, span: Span) -> bool {
    let text = st.text(span);
    match text {
        "" => return false,
        "true" | "false" | "null" | "not" | "if" | "match" => return true,
        _ => {}
    }
    let bytes = text.as_bytes();
    if bytes.starts_with(b"r#") || matches!(bytes[0], b'(' | b'{' | b'[' | b'$' | b'"' | b'\'' | b'-') {
        return true;
    }
    literal::parse_int(text).is_some()
        || literal::parse_float(text).is_some()
        || literal::filesize(text).is_some_and(|r| r.is_ok())
        || literal::duration(text).is_some_and(|r| r.is_ok())
        || literal::is_datetime(text)
        || text.starts_with("0x[")
        || text.starts_with("0o[")
        || text.starts_with("0b[")
        || (is_range_candidate(text) && {
            let cp = st.checkpoint();
            let ok = range(st, span).is_some();
            st.rollback(cp);
            ok
        })
}

// --- strings ----------------------------------------------------------------

/// `true` for a bare word that Nushell treats as an interpolation because it
/// contains `(`: `foo(1 + 1)bar`.
fn is_bare_interpolation(text: &str) -> bool {
    !text.is_empty() && !matches!(text.as_bytes()[0], b'\'' | b'"' | b'`') && text.contains('(')
}

/// Parse a string item: quoted (`"`, `'`, `` ` ``), raw, or a bare word.
pub fn string<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let text = st.text(span);
    if text.is_empty() {
        return Err(cut(Diagnostic::expected("string", span)));
    }
    if text.starts_with("r#") {
        return literal::raw_string(st, span);
    }
    if is_bare_interpolation(text) {
        return interpolation(st, span);
    }
    let lit = string_lit(st, span)?;
    Ok(Expr::new(ExprKind::String(lit), span))
}

/// Parse a string item into its literal without wrapping it in an expression.
/// Bare-word interpolation is *not* handled here.
pub fn string_lit<'a>(st: St<'_, 'a>, span: Span) -> PResult<StringLit<'a>> {
    let text = st.text(span);
    let bytes = text.as_bytes();
    match bytes.first() {
        Some(q @ (b'"' | b'\'')) => {
            let body = quoted_body(st, span, *q)?;
            let value =
                if *q == b'"' { literal::unescape(body, span.start + 1).map_err(cut)? } else { Cow::Borrowed(body) };
            let quote = if *q == b'"' { Quote::Double } else { Quote::Single };
            Ok(StringLit { value, quote })
        }
        Some(b'`') => {
            let body = quoted_body(st, span, b'`')?;
            Ok(StringLit { value: Cow::Borrowed(body), quote: Quote::Backtick })
        }
        _ => Ok(StringLit { value: Cow::Borrowed(text), quote: Quote::Bare }),
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
        return Ok(if bytes.len() >= 2 && bytes[bytes.len() - 1] == b'`' {
            &text[1..text.len() - 1]
        } else {
            &text[1..]
        });
    }
    let last = bytes.iter().rposition(|b| *b == quote).expect("starts with quote");
    if last == 0 {
        return Err(cut(Diagnostic::new(
            ErrorKind::Unclosed { delimiter: literal_quote(quote), open: Span::new(span.start, span.start + 1) },
            span.past(),
        )));
    }
    if last + 1 != bytes.len() {
        return Err(cut(Diagnostic::new(ErrorKind::ExtraTokens, Span::new(span.start + last + 1, span.end))
            .with_help("invalid characters after the closing quote; quote the whole string or remove them")));
    }
    Ok(&text[1..last])
}

fn literal_quote(q: u8) -> &'static str {
    match q {
        b'"' => "\"",
        b'\'' => "'",
        _ => "`",
    }
}

/// Parse `$"..."`, `$'...'` or a bare interpolation `foo(...)`.
pub fn interpolation<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let text = st.text(span);
    let (quote, body_span) = if text.starts_with("$\"") || text.starts_with("$'") {
        let q = text.as_bytes()[1];
        let inner = Span::new(span.start + 1, span.end);
        let body = quoted_body(st, inner, q)?;
        let body = Span::new(inner.start + 1, inner.start + 1 + body.len());
        (if q == b'"' { Quote::Double } else { Quote::Single }, body)
    } else {
        (Quote::Bare, span)
    };
    let parts = interpolation_parts(st, body_span, quote)?;
    Ok(Expr::new(ExprKind::Interpolation(Interpolation { quote, parts }), span))
}

fn interpolation_parts<'a>(st: St<'_, 'a>, body: Span, quote: Quote) -> PResult<Vec<InterpPart<'a>>> {
    let text = st.text(body);
    let bytes = text.as_bytes();
    let double = quote == Quote::Double;
    let mut parts = Vec::new();
    let mut idx = 0;
    let mut token_start = 0;
    let mut backslashes = 0usize;
    let mut stack: Vec<(u8, usize)> = Vec::new();
    let mut in_expr = false;

    let flush_text = |parts: &mut Vec<InterpPart<'a>>, start: usize, end: usize| -> PResult<()> {
        if start < end {
            let span = Span::new(body.start + start, body.start + end);
            let raw = &text[start..end];
            let value = if double { literal::unescape(raw, span.start).map_err(cut)? } else { Cow::Borrowed(raw) };
            parts.push(InterpPart::Text { span, value });
        }
        Ok(())
    };

    while idx < bytes.len() {
        let c = bytes[idx];
        if !in_expr {
            let preceding = backslashes;
            backslashes = if c == b'\\' { preceding + 1 } else { 0 };
            if c == b'(' && (!double || preceding.is_multiple_of(2)) {
                flush_text(&mut parts, token_start, idx)?;
                in_expr = true;
                token_start = idx;
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
            let span = Span::new(body.start + token_start, body.start + idx + 1);
            parts.push(InterpPart::Expr(paren(st, span, Hint::Any)?));
            in_expr = false;
            token_start = idx + 1;
        }
        idx += 1;
    }
    if in_expr {
        return Err(cut(Diagnostic::new(
            ErrorKind::Unclosed {
                delimiter: ")",
                open: Span::new(body.start + token_start, body.start + token_start + 1),
            },
            body.past(),
        )));
    }
    flush_text(&mut parts, token_start, bytes.len())?;
    Ok(parts)
}

// --- `$` expressions --------------------------------------------------------

fn dollar<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let text = st.text(span);
    if text.starts_with("$\"") || text.starts_with("$'") {
        return interpolation(st, span);
    }
    if text.starts_with("$.") {
        // `$.` alone is the empty cell path (the identity).
        let members_span = Span::new(span.start + 2, span.end);
        let tokens = st.lex_span(members_span, LexOptions::CELL_PATH).map_err(cut)?;
        let items: Vec<Token> = tokens.into_iter().filter(|t| t.kind == TokenKind::Item).collect();
        let members = cell_path_members(st, &items, false)?;
        return Ok(Expr::new(ExprKind::CellPath(CellPath { members }), span));
    }
    if is_range_candidate(text)
        && let Some(range) = range(st, span)
    {
        return Ok(range);
    }
    full_cell_path(st, span, false)
}

/// The characters that cannot appear in a variable or parameter name.
pub fn is_identifier(name: &str) -> bool {
    !name.is_empty() && !name.bytes().any(|b| b".[({+-*^%/=!<>&|".contains(&b))
}

/// Parse `$name`.
pub fn variable<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let text = st.text(span);
    let name = text.strip_prefix('$').unwrap_or(text);
    if !is_identifier(name) {
        return Err(cut(Diagnostic::expected("valid variable name", span)
            .with_help("variable names may not contain `.[({+-*^%/=!<>&|`")));
    }
    Ok(Expr::new(ExprKind::Var(Var { name }), span))
}

// --- cell paths -------------------------------------------------------------

/// Parse a head (`$var`, `(...)`, `[...]`, `{...}`) followed by `.member` accesses.
///
/// With `implicit` set, a bare head is taken as a column of the row variable
/// `$it` (used by `where` row conditions).
pub fn full_cell_path<'a>(st: St<'_, 'a>, span: Span, implicit: bool) -> PResult<Expr<'a>> {
    let tokens = st.lex_span(span, LexOptions::CELL_PATH).map_err(cut)?;
    let items: Vec<Token> = tokens.into_iter().filter(|t| t.kind == TokenKind::Item).collect();
    let Some(head_tok) = items.first() else {
        return Err(cut(Diagnostic::expected("value", span)));
    };
    let head_text = st.tok(head_tok);
    if head_text.starts_with('(') && !head_text.ends_with(')') {
        // `(pwd)/x`: not a subexpression head but a bare interpolation.
        return interpolation(st, span);
    }
    let (head, expect_dot, member_start) = match head_text.as_bytes()[0] {
        b'(' => (subexpression(st, head_tok.span)?, true, 1),
        b'[' => (list_or_table(st, head_tok.span)?, true, 1),
        b'{' => (record(st, head_tok.span)?, true, 1),
        b'$' => (variable(st, head_tok.span)?, true, 1),
        _ if implicit => (Expr::new(ExprKind::Var(Var { name: "it" }), Span::point(span.start)), false, 0),
        _ => return Err(cut(Diagnostic::expected("variable or subexpression", head_tok.span))),
    };
    let members = cell_path_members(st, &items[member_start..], expect_dot)?;
    if members.is_empty() && !implicit {
        return Ok(head);
    }
    Ok(Expr::new(ExprKind::FullCellPath(FullCellPath { head: Box::new(head), implicit_head: implicit, members }), span))
}

/// Parse the `.a.0?.b!` tail of a cell path from tokens lexed with
/// [`LexOptions::CELL_PATH`].
pub fn cell_path_members<'a>(st: St<'_, 'a>, tokens: &[Token], expect_dot: bool) -> PResult<Vec<PathMember<'a>>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Expect {
        Dot,
        DotOrSign,
        DotOrExclamation,
        DotOrQuestion,
        Member,
    }
    let mut expect = if expect_dot { Expect::Dot } else { Expect::Member };
    let mut members: Vec<PathMember<'a>> = Vec::new();
    for tok in tokens {
        let text = st.tok(tok);
        if expect == Expect::Member {
            let kind = match literal::parse_int(text) {
                Some(i) if i < 0 => {
                    return Err(cut(Diagnostic::new(
                        ErrorKind::InvalidLiteral {
                            kind: "cell path",
                            message: "negative index is not supported".into(),
                        },
                        tok.span,
                    )));
                }
                Some(i) => PathMemberKind::Int(i as usize),
                None => PathMemberKind::String(string_lit(st, tok.span)?.value),
            };
            members.push(PathMember { span: tok.span, kind, optional: false, insensitive: false });
            expect = Expect::DotOrSign;
            continue;
        }
        let c = if text.len() == 1 { text.as_bytes()[0] } else { b' ' };
        let (next, modify) = match (expect, c) {
            (_, b'.') => (Expect::Member, None),
            (Expect::DotOrSign, b'!') => (Expect::DotOrQuestion, Some(true)),
            (Expect::DotOrSign, b'?') => (Expect::DotOrExclamation, Some(false)),
            (Expect::DotOrExclamation, b'!') => (Expect::Dot, Some(true)),
            (Expect::DotOrQuestion, b'?') => (Expect::Dot, Some(false)),
            (Expect::DotOrSign, _) => return Err(cut(Diagnostic::expected("`.`, `?` or `!`", tok.span))),
            (Expect::DotOrExclamation, _) => return Err(cut(Diagnostic::expected("`.` or `!`", tok.span))),
            (Expect::DotOrQuestion, _) => return Err(cut(Diagnostic::expected("`.` or `?`", tok.span))),
            (Expect::Dot, _) => return Err(cut(Diagnostic::expected("`.`", tok.span))),
            (Expect::Member, _) => unreachable!(),
        };
        if let Some(insensitive) = modify
            && let Some(last) = members.last_mut()
        {
            if insensitive {
                last.insensitive = true;
            } else {
                last.optional = true;
            }
            last.span = last.span.merge(tok.span);
        }
        expect = next;
    }
    // Like Nushell, a trailing `.` (`$x.a.`) is accepted and ignored.
    Ok(members)
}

// --- ranges -----------------------------------------------------------------

fn is_range_candidate(text: &str) -> bool {
    text.contains("..") && !text.starts_with("...")
}

/// Try to parse `from..to`, `from..<to`, `from..next..to`, `..to`, `from..`.
///
/// Returns `None` (with any speculative state rolled back) when the item is
/// not a range, so the caller can try the next literal kind.
fn range<'a>(st: St<'_, 'a>, span: Span) -> Option<Expr<'a>> {
    let text = st.text(span);
    let positions: Vec<usize> = text
        .match_indices("..")
        .filter_map(|(pos, _)| {
            let before = &text[..pos];
            let depth = before
                .bytes()
                .filter(|b| *b == b'(')
                .count()
                .checked_sub(before.bytes().filter(|b| *b == b')').count())?;
            (depth == 0).then_some(pos)
        })
        .collect();
    let (next_pos, op_pos) = match positions.as_slice() {
        [op] => (None, *op),
        [next, op] => (Some(*next), *op),
        _ => return None,
    };
    let (inclusion, op_len) = if text[op_pos..].starts_with("..<") {
        (RangeInclusion::RightExclusive, 3)
    } else if text[op_pos..].starts_with("..=") {
        (RangeInclusion::Inclusive, 3)
    } else {
        (RangeInclusion::Inclusive, 2)
    };
    if let Some(p) = text.find("..<")
        && p != op_pos
    {
        return None;
    }
    let has_from = !text.starts_with("..");
    let has_to = text.len() > op_pos + op_len;
    if !has_from && !has_to {
        return None;
    }
    let cp = st.checkpoint();
    let bound = |start: usize, end: usize| -> Option<Expr<'a>> {
        if start >= end {
            return None;
        }
        value(st, Span::new(span.start + start, span.start + end), Hint::Number).ok()
    };
    let from_end = next_pos.unwrap_or(op_pos);
    let from = if has_from {
        match bound(0, from_end) {
            Some(e) => Some(Box::new(e)),
            None => {
                st.rollback(cp);
                return None;
            }
        }
    } else {
        None
    };
    let next = match next_pos {
        Some(np) => match bound(np + 2, op_pos) {
            Some(e) => Some(Box::new(e)),
            None => {
                st.rollback(cp);
                return None;
            }
        },
        None => None,
    };
    let to = if has_to {
        match bound(op_pos + op_len, text.len()) {
            Some(e) => Some(Box::new(e)),
            None => {
                st.rollback(cp);
                return None;
            }
        }
    } else {
        None
    };
    Some(Expr::new(
        ExprKind::Range(Range {
            from,
            next,
            to,
            inclusion,
            op_span: Span::new(span.start + op_pos, span.start + op_pos + op_len),
            next_op_span: next_pos.map(|np| Span::new(span.start + np, span.start + np + 2)),
        }),
        span,
    ))
}

// --- brackets ---------------------------------------------------------------

fn paren<'a>(st: St<'_, 'a>, span: Span, hint: Hint) -> PResult<Expr<'a>> {
    let text = st.text(span);
    if is_range_candidate(text)
        && let Some(range) = range(st, span)
    {
        return Ok(range);
    }
    if hint == Hint::Signature {
        return signature::parse_signature(st, span).map(|s| signature_expr(s, span));
    }
    full_cell_path(st, span, false)
}

/// The interior of a delimited item, checking the closing delimiter.
fn interior(st: St<'_, '_>, span: Span, open: &'static str, close: &'static str) -> PResult<Span> {
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

/// Parse `( ... )` as a subexpression.
pub fn subexpression<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let inner = interior(st, span, "(", ")")?;
    let tokens = st.lex_span(inner, LexOptions::SUBEXPRESSION).map_err(cut)?;
    st.push_scope();
    let block = block::parse_block_tokens(st, &tokens, inner);
    st.pop_scope();
    Ok(Expr::new(ExprKind::Subexpression(block), span))
}

fn brace<'a>(st: St<'_, 'a>, span: Span, hint: Hint) -> PResult<Expr<'a>> {
    let text = st.text(span);
    if !text.ends_with('}') {
        // `{a: 1}.a`
        return full_cell_path(st, span, false);
    }
    let inner = Span::new(span.start + 1, span.end - 1);
    let probe = lex_prefix(st.text(inner), inner.start, LexOptions::BRACE_PROBE, 2).unwrap_or_default();
    let probe: Vec<&Token> = probe.iter().filter(|t| t.kind != TokenKind::Eof).collect();
    let by_hint = |st: St<'_, 'a>| -> PResult<Expr<'a>> {
        match hint {
            Hint::Closure | Hint::Any => closure(st, span),
            Hint::Block => block_expr(st, span),
            _ => Err(cut(Diagnostic::expected("value", span).with_help("found a block or closure"))),
        }
    };
    match probe.as_slice() {
        [] => match hint {
            Hint::Closure => closure(st, span),
            Hint::Block => block_expr(st, span),
            _ => record(st, span),
        },
        [first, ..] if matches!(first.kind, TokenKind::Pipe | TokenKind::PipePipe) => {
            if hint == Hint::Block {
                return Err(cut(
                    Diagnostic::expected("block", span).with_help("found a closure; blocks cannot have parameters")
                ));
            }
            closure(st, span)
        }
        [_, second, ..] if st.tok(second) == ":" => record(st, span),
        [first, ..] => match hint {
            Hint::Closure => closure(st, span),
            Hint::Block => block_expr(st, span),
            _ if is_spread(st.tok(first), b"{$(") => record(st, span),
            _ => by_hint(st),
        },
    }
}

/// `true` for `...x` where `x` starts with one of `heads`.
pub fn is_spread(text: &str, heads: &[u8]) -> bool {
    text.len() > 3 && text.starts_with("...") && heads.contains(&text.as_bytes()[3])
}

/// Parse `{ ... }` as a block (no parameters allowed).
pub fn block_body<'a>(st: St<'_, 'a>, span: Span) -> PResult<Block<'a>> {
    let inner = interior(st, span, "{", "}")?;
    let tokens = st.lex_span(inner, LexOptions::BLOCK).map_err(cut)?;
    if let Some(first) = tokens.first()
        && matches!(first.kind, TokenKind::Pipe | TokenKind::PipePipe)
    {
        return Err(cut(Diagnostic::expected("block", first.span)
            .with_help("found closure parameters; blocks cannot have parameters")));
    }
    st.push_scope();
    let block = block::parse_block_tokens(st, &tokens, inner);
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
    let (params, body_start) = match tokens.first().map(|t| t.kind) {
        Some(TokenKind::Pipe) => {
            let open = tokens[0];
            let Some(close_idx) = tokens.iter().skip(1).position(|t| t.kind == TokenKind::Pipe).map(|p| p + 1) else {
                return Err(cut(Diagnostic::new(
                    ErrorKind::Unclosed { delimiter: "|", open: open.span },
                    inner.past(),
                )
                .with_context("closure parameters")));
            };
            let close = tokens[close_idx];
            let sig_span = open.span.merge(close.span);
            let sig = signature::parse_signature_inner(st, Span::new(open.span.end, close.span.start), sig_span)?;
            (Some(sig), close_idx + 1)
        }
        Some(TokenKind::PipePipe) => (Some(Signature { span: tokens[0].span, ..Signature::default() }), 1),
        _ => (None, 0),
    };
    let body_tokens = &tokens[body_start..];
    let body_span = Span::new(body_tokens.first().map_or(inner.end, |t| t.span.start), inner.end);
    st.push_scope();
    let body = block::parse_block_tokens(st, body_tokens, body_span);
    st.pop_scope();
    Ok(Expr::new(ExprKind::Closure(Closure { params, body }), span))
}

/// Parse `[ ... ]` as a list or a table.
pub fn list_or_table<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let inner = interior(st, span, "[", "]")?;
    let tokens = st.lex_span(inner, LexOptions::LIST).map_err(cut)?;
    st.comments_from(&tokens);
    let items: Vec<Token> = tokens
        .into_iter()
        .filter(|t| !matches!(t.kind, TokenKind::Comment | TokenKind::Eol | TokenKind::Eof))
        .collect();
    if let [first, second, rest @ ..] = items.as_slice()
        && first.kind == TokenKind::Item
        && st.tok(first).starts_with('[')
        && second.kind == TokenKind::Semicolon
        && !rest.is_empty()
    {
        let columns = list_row(st, first.span)?;
        let mut rows = Vec::with_capacity(rest.len());
        for tok in rest {
            if tok.kind != TokenKind::Item || !st.tok(tok).starts_with('[') {
                return Err(cut(Diagnostic::expected("table row", tok.span).with_help("all table rows must be lists")));
            }
            rows.push(list_row(st, tok.span)?);
        }
        return Ok(Expr::new(ExprKind::Table(Table { columns: Box::new(columns), rows }), span));
    }
    let mut out = Vec::with_capacity(items.len());
    for tok in &items {
        // Nushell tolerates `|` and `;` between list items.
        if matches!(tok.kind, TokenKind::Semicolon | TokenKind::Pipe | TokenKind::PipePipe) {
            continue;
        }
        out.push(list_item(st, tok)?);
    }
    Ok(Expr::new(ExprKind::List(out), span))
}

fn list_item<'a>(st: St<'_, 'a>, tok: &Token) -> PResult<ListItem<'a>> {
    match tok.kind {
        TokenKind::Item => {}
        // `[Assignment, =, Assign]`: an operator on its own is just a word here.
        TokenKind::Assign(_) | TokenKind::Redirect(_) => {
            return Ok(ListItem::Item(Expr::new(ExprKind::String(StringLit::bare(st.tok(tok))), tok.span)));
        }
        _ => return Err(cut(Diagnostic::expected("list item", tok.span))),
    }
    let text = st.tok(tok);
    if is_spread(text, b"[$(") {
        let dots = Span::new(tok.span.start, tok.span.start + 3);
        let expr = value(st, Span::new(tok.span.start + 3, tok.span.end), Hint::Any)?;
        return Ok(ListItem::Spread { dots, expr });
    }
    Ok(ListItem::Item(value(st, tok.span, Hint::Any)?))
}

/// A table header or row: a list without spreads.
fn list_row<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let inner = interior(st, span, "[", "]")?;
    let tokens = st.lex_span(inner, LexOptions::LIST).map_err(cut)?;
    st.comments_from(&tokens);
    let mut out = Vec::new();
    for tok in tokens
        .iter()
        .filter(|t| !matches!(t.kind, TokenKind::Eof | TokenKind::Comment | TokenKind::Semicolon | TokenKind::Pipe))
    {
        match list_item(st, tok)? {
            item @ ListItem::Item(_) => out.push(item),
            ListItem::Spread { dots, .. } => {
                return Err(cut(Diagnostic::message("cannot spread in a table row", dots)));
            }
        }
    }
    Ok(Expr::new(ExprKind::List(out), span))
}

/// Parse `{ key: value, ...$spread }`.
pub fn record<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let inner = interior(st, span, "{", "}")?;
    let text = st.text(inner);
    let mut off = 0;
    let mut items = Vec::new();
    let next = |off: &mut usize, opts: LexOptions| -> PResult<Option<Token>> {
        loop {
            let (toks, consumed) = lex_prefix_at(&text[*off..], inner.start + *off, opts, 1).map_err(cut)?;
            *off += consumed;
            match toks.first() {
                None => return Ok(None),
                Some(t) if t.kind == TokenKind::Comment => st.comment(t.span),
                Some(t) if matches!(t.kind, TokenKind::Assign(_) | TokenKind::Redirect(_)) => {
                    return Ok(Some(Token { kind: TokenKind::Item, span: t.span }));
                }
                Some(t) => return Ok(Some(*t)),
            }
        }
    };
    while let Some(key_tok) = next(&mut off, LexOptions::RECORD_KEY)? {
        let key_text = st.tok(&key_tok);
        if key_tok.kind != TokenKind::Item {
            return Err(cut(Diagnostic::expected("record key", key_tok.span)));
        }
        if is_spread(key_text, b"{$(") {
            let dots = Span::new(key_tok.span.start, key_tok.span.start + 3);
            let expr = value(st, Span::new(key_tok.span.start + 3, key_tok.span.end), Hint::Any)?;
            items.push(RecordItem::Spread { dots, expr });
            continue;
        }
        let key = value(st, key_tok.span, Hint::String)?;
        let Some(colon) = next(&mut off, LexOptions::RECORD_KEY)? else {
            return Err(cut(Diagnostic::expected("`:` after record key", key_tok.span.past())
                .with_help("record fields look like `key: value`")));
        };
        if st.tok(&colon) != ":" {
            return Err(cut(Diagnostic::expected("`:` after record key", colon.span).with_help(
                "record fields look like `key: value`; a missing colon often makes this parse as a block or closure",
            )));
        }
        let Some(value_tok) = next(&mut off, LexOptions::RECORD_VALUE)? else {
            return Err(cut(Diagnostic::expected("record value", colon.span.past())));
        };
        if value_tok.kind != TokenKind::Item {
            return Err(cut(Diagnostic::expected("record value", value_tok.span)));
        }
        let value = value(st, value_tok.span, Hint::Any)?;
        items.push(RecordItem::Pair { key, colon: colon.span, value });
    }
    Ok(Expr::new(ExprKind::Record(items), span))
}
