//! `$` expressions, cell paths, parenthesised heads and ranges.

use crate::ast::{CellPath, Expr, ExprKind, FullCellPath, PathMember, PathMemberKind, Range, RangeInclusion, Var};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut};
use crate::lexer::{LexOptions, Token, TokenKind};
use crate::span::Span;

use super::value::{self, Hint};
use super::{St, collections, literal, strings};

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

/// An item starting with `$`: interpolation, cell-path literal, range, or a
/// variable with an optional cell path.
pub fn dollar<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let text = st.text(span);
    match text.as_bytes() {
        [b'$', b'"' | b'\'', ..] => strings::interpolation(st, span),
        [b'$', b'.', ..] => {
            // `$.` alone is the empty cell path (the identity).
            let members_span = Span::new(span.start + 2, span.end);
            let tokens = st.lex_span(members_span, LexOptions::CELL_PATH).map_err(cut)?;
            let items: Vec<Token> = tokens.into_iter().filter(|t| t.kind == TokenKind::Item).collect();
            let members = cell_path_members(st, &items, false)?;
            Ok(Expr::new(ExprKind::CellPath(CellPath { members }), span))
        }
        _ if is_range_syntax(text) => range(st, span),
        _ => full_cell_path(st, span, false),
    }
}

/// An item starting with `(`: a range, a signature, or a subexpression with
/// an optional cell path.
pub fn paren<'a>(st: St<'_, 'a>, span: Span, hint: Hint) -> PResult<Expr<'a>> {
    match hint {
        _ if is_range_syntax(st.text(span)) => range(st, span),
        Hint::Signature => Ok(Expr::new(ExprKind::Garbage, span)),
        _ => full_cell_path(st, span, false),
    }
}

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
    let (head, member_tokens) = match head_text.as_bytes() {
        // `(pwd)/x`: not a subexpression head but a bare interpolation.
        [b'(', ..] if !head_text.ends_with(')') => return strings::interpolation(st, span),
        [b'(', ..] => (value::subexpression(st, head_tok.span)?, &items[1..]),
        [b'[', ..] => (collections::list_or_table(st, head_tok.span)?, &items[1..]),
        [b'{', ..] => (collections::record(st, head_tok.span)?, &items[1..]),
        [b'$', ..] => (variable(st, head_tok.span)?, &items[1..]),
        _ if implicit => (Expr::new(ExprKind::Var(Var { name: "it" }), Span::point(span.start)), &items[..]),
        _ => return Err(cut(Diagnostic::expected("variable or subexpression", head_tok.span))),
    };
    let members = cell_path_members(st, member_tokens, !implicit)?;
    if members.is_empty() && !implicit {
        return Ok(head);
    }
    Ok(Expr::new(ExprKind::FullCellPath(FullCellPath { head: Box::new(head), implicit_head: implicit, members }), span))
}

/// What the cell-path state machine expects next.
#[derive(Clone, Copy, PartialEq)]
enum Expect {
    Dot,
    DotOrSign,
    DotOrExclamation,
    DotOrQuestion,
    Member,
}

/// Parse the `.a.0?.b!` tail of a cell path from tokens lexed with
/// [`LexOptions::CELL_PATH`]. A trailing `.` is accepted, as in Nushell.
pub fn cell_path_members<'a>(st: St<'_, 'a>, tokens: &[Token], expect_dot: bool) -> PResult<Vec<PathMember<'a>>> {
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
                None => PathMemberKind::String(strings::string_lit(st, tok.span)?.value),
            };
            members.push(PathMember { span: tok.span, kind, optional: false, insensitive: false });
            expect = Expect::DotOrSign;
            continue;
        }
        let (next, insensitive) = match (expect, text) {
            (_, ".") => (Expect::Member, None),
            (Expect::DotOrSign, "!") => (Expect::DotOrQuestion, Some(true)),
            (Expect::DotOrSign, "?") => (Expect::DotOrExclamation, Some(false)),
            (Expect::DotOrExclamation, "!") => (Expect::Dot, Some(true)),
            (Expect::DotOrQuestion, "?") => (Expect::Dot, Some(false)),
            (Expect::DotOrSign, _) => return Err(cut(Diagnostic::expected("`.`, `?` or `!`", tok.span))),
            (Expect::DotOrExclamation, _) => return Err(cut(Diagnostic::expected("`.` or `!`", tok.span))),
            (Expect::DotOrQuestion, _) => return Err(cut(Diagnostic::expected("`.` or `?`", tok.span))),
            (Expect::Dot | Expect::Member, _) => return Err(cut(Diagnostic::expected("`.`", tok.span))),
        };
        if let (Some(insensitive), Some(last)) = (insensitive, members.last_mut()) {
            if insensitive {
                last.insensitive = true;
            } else {
                last.optional = true;
            }
            last.span = last.span.merge(tok.span);
        }
        expect = next;
    }
    Ok(members)
}

/// Positions of the range operators (`..`) at parenthesis depth zero:
/// `(next, op)` for `a..b..c`, `(None, op)` for `a..b`.
fn range_operators(text: &str) -> Option<(Option<usize>, usize)> {
    let mut depth = 0i32;
    let mut positions = Vec::with_capacity(2);
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'.' if depth == 0 && bytes.get(i + 1) == Some(&b'.') => {
                positions.push(i);
                i += 2;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    match positions.as_slice() {
        [op] => Some((None, *op)),
        [next, op] => Some((Some(*next), *op)),
        _ => None,
    }
}

/// A range bound must be something `parse_value(Number)` accepts: a number,
/// a `$` expression or a parenthesised subexpression.
fn is_range_bound(text: &str) -> bool {
    literal::parse_int(text).is_some()
        || literal::parse_float(text).is_some()
        || text.starts_with('$')
        || (text.starts_with('(') && text.ends_with(')') && text.len() >= 2)
}

/// `true` if `text` has the shape of a range: `from..to`, `from..<to`,
/// `from..=to`, `from..next..to`, `..to`, `from..`, with every present bound
/// number-like. Decided without parsing, so callers can fall through to the
/// next literal kind (`cd ..`, `a..b`) when it is not.
pub fn is_range_syntax(text: &str) -> bool {
    if !text.contains("..") || text.starts_with("...") {
        return false;
    }
    let Some((next_pos, op_pos)) = range_operators(text) else { return false };
    let op_len = if text[op_pos..].starts_with("..<") || text[op_pos..].starts_with("..=") { 3 } else { 2 };
    if text.find("..<").is_some_and(|p| p != op_pos) {
        return false;
    }
    let from = &text[..next_pos.unwrap_or(op_pos)];
    let next = next_pos.map(|np| &text[np + 2..op_pos]);
    let to = &text[op_pos + op_len..];
    if from.is_empty() && to.is_empty() {
        return false;
    }
    (from.is_empty() || is_range_bound(from))
        && next.is_none_or(|n| !n.is_empty() && is_range_bound(n))
        && (to.is_empty() || is_range_bound(to))
}

/// Parse a range item; the caller has checked [`is_range_syntax`].
pub fn range<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let text = st.text(span);
    let Some((next_pos, op_pos)) = range_operators(text) else {
        return Err(cut(Diagnostic::expected("range", span)));
    };
    let (inclusion, op_len) = match &text[op_pos..] {
        t if t.starts_with("..<") => (RangeInclusion::RightExclusive, 3),
        t if t.starts_with("..=") => (RangeInclusion::Inclusive, 3),
        _ => (RangeInclusion::Inclusive, 2),
    };
    let bound = |start: usize, end: usize| -> PResult<Option<Box<Expr<'a>>>> {
        if start >= end {
            return Ok(None);
        }
        Ok(Some(Box::new(value::value(st, Span::new(span.start + start, span.start + end), Hint::Number)?)))
    };
    let from = bound(0, next_pos.unwrap_or(op_pos))?;
    let next = match next_pos {
        Some(np) => bound(np + 2, op_pos)?,
        None => None,
    };
    let to = bound(op_pos + op_len, text.len())?;
    Ok(Expr::new(
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
