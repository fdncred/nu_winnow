//! `match` blocks and patterns.

use std::borrow::Cow;

use crate::ast::{MatchArm, Pattern, PatternKind};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut};
use crate::lexer::{LexOptions, Token, TokenKind};
use crate::span::{Span, Spanned};

use super::value::{self, Hint, is_identifier};
use super::{St, expr};

/// Parse the `{ pattern => body, ... }` item of a `match`.
pub fn match_block<'a>(st: St<'_, 'a>, span: Span) -> PResult<(Span, Vec<MatchArm<'a>>)> {
    let text = st.text(span);
    if !text.starts_with('{') {
        return Err(cut(Diagnostic::expected("match block", span)));
    }
    if text.len() < 2 || !text.ends_with('}') {
        return Err(cut(Diagnostic::new(
            ErrorKind::Unclosed { delimiter: "}", open: Span::new(span.start, span.start + 1) },
            span.past(),
        )));
    }
    let inner = Span::new(span.start + 1, span.end - 1);
    let tokens = st.lex_span(inner, LexOptions::MATCH).map_err(cut)?;
    st.comments_from(&tokens);
    let tokens: Vec<Token> =
        tokens.into_iter().filter(|t| !matches!(t.kind, TokenKind::Comment | TokenKind::Eol)).collect();
    let mut arms = Vec::new();
    let mut idx = 0;
    let last = tokens.len() - 1; // Eof
    while idx < last {
        let mut pattern = parse_pattern(st, &tokens[idx])?;
        idx += 1;
        // Or-patterns.
        if tokens[idx].kind == TokenKind::Pipe {
            let mut alternatives = vec![pattern];
            while tokens[idx].kind == TokenKind::Pipe {
                idx += 1;
                if idx >= last {
                    return Err(cut(Diagnostic::expected("pattern after `|`", tokens[idx].span)));
                }
                alternatives.push(parse_pattern(st, &tokens[idx])?);
                idx += 1;
            }
            let span = alternatives.first().unwrap().span.merge(alternatives.last().unwrap().span);
            pattern = Pattern { span, kind: PatternKind::Or(alternatives) };
        }
        // Guard.
        let mut guard = None;
        if tokens[idx].kind == TokenKind::Item && st.tok(&tokens[idx]) == "if" {
            let if_span = tokens[idx].span;
            idx += 1;
            let arrow = tokens[idx..last].iter().position(|t| t.kind == TokenKind::Item && st.tok(t) == "=>");
            let end = arrow.map_or(last, |p| idx + p);
            if end == idx {
                return Err(cut(Diagnostic::expected("expression after `if` in match guard", if_span.past())
                    .with_help("the `if` keyword must be followed by an expression")));
            }
            let guard_tokens = expr::with_eof(&tokens[idx..end]);
            guard = Some(Box::new(expr::math_expression(st, &guard_tokens, false)?));
            idx = end;
        }
        // Arrow.
        if idx >= last || tokens[idx].kind != TokenKind::Item || st.tok(&tokens[idx]) != "=>" {
            let at = tokens[idx.min(last)].span;
            return Err(cut(Diagnostic::expected("`=>`", at)));
        }
        let arrow = tokens[idx].span;
        idx += 1;
        // Body.
        if idx >= last {
            return Err(cut(Diagnostic::expected("match arm body", arrow.past())));
        }
        let body_tok = tokens[idx];
        if body_tok.kind != TokenKind::Item {
            return Err(cut(Diagnostic::expected("match arm body", body_tok.span)));
        }
        // `{ ... }` is a block unless it looks like a record (`{a: 1}`) or a
        // closure (`{|x| ...}`).
        let body = if st.tok(&body_tok).starts_with('{') {
            let cp = st.checkpoint();
            match value::value(st, body_tok.span, Hint::Block) {
                Ok(body) => body,
                Err(_) => {
                    st.rollback(cp);
                    value::value(st, body_tok.span, Hint::Any)?
                }
            }
        } else {
            expr::parse_expression(st, &expr::with_eof(&tokens[idx..idx + 1]))?
        };
        idx += 1;
        arms.push(MatchArm { span: pattern.span.merge(body.span), pattern, guard, arrow, body });
    }
    Ok((span, arms))
}

/// Parse one pattern item.
pub fn parse_pattern<'a>(st: St<'_, 'a>, tok: &Token) -> PResult<Pattern<'a>> {
    if tok.kind != TokenKind::Item {
        return Err(cut(Diagnostic::expected("pattern", tok.span)));
    }
    let text = st.tok(tok);
    let span = tok.span;
    let kind = match text.as_bytes()[0] {
        b'$' => PatternKind::Variable(variable_name(st, span)?),
        b'{' => PatternKind::Record(record_pattern(st, span)?),
        b'[' => PatternKind::List(list_pattern(st, span)?),
        b'_' if text == "_" => PatternKind::Wildcard,
        _ => PatternKind::Value(value::value(st, span, Hint::Any)?),
    };
    Ok(Pattern { span, kind })
}

fn variable_name<'a>(st: St<'_, 'a>, span: Span) -> PResult<&'a str> {
    let name = st.text(span).strip_prefix('$').unwrap_or_default();
    if !is_identifier(name) {
        return Err(cut(Diagnostic::expected("valid variable name", span)));
    }
    Ok(name)
}

fn list_pattern<'a>(st: St<'_, 'a>, span: Span) -> PResult<Vec<Pattern<'a>>> {
    let text = st.text(span);
    if text.len() < 2 || !text.ends_with(']') {
        return Err(cut(Diagnostic::new(
            ErrorKind::Unclosed { delimiter: "]", open: Span::new(span.start, span.start + 1) },
            span.past(),
        )));
    }
    let inner = Span::new(span.start + 1, span.end - 1);
    let tokens = st.lex_span(inner, LexOptions::PATTERN_LIST).map_err(cut)?;
    st.comments_from(&tokens);
    let mut out = Vec::new();
    for tok in tokens.iter().filter(|t| t.kind != TokenKind::Eof && t.kind != TokenKind::Comment) {
        let text = st.tok(tok);
        if let Some(rest) = text.strip_prefix("..").filter(|_| !text.starts_with("...")) {
            let kind = if rest.is_empty() {
                PatternKind::Rest(None)
            } else {
                let name = variable_name(st, Span::new(tok.span.start + 2, tok.span.end))?;
                PatternKind::Rest(Some(Spanned::new(name, Span::new(tok.span.start + 2, tok.span.end))))
            };
            out.push(Pattern { span: tok.span, kind });
        } else {
            out.push(parse_pattern(st, tok)?);
        }
    }
    Ok(out)
}

fn record_pattern<'a>(st: St<'_, 'a>, span: Span) -> PResult<Vec<(Spanned<Cow<'a, str>>, Pattern<'a>)>> {
    let text = st.text(span);
    if text.len() < 2 || !text.ends_with('}') {
        return Err(cut(Diagnostic::new(
            ErrorKind::Unclosed { delimiter: "}", open: Span::new(span.start, span.start + 1) },
            span.past(),
        )));
    }
    let inner = Span::new(span.start + 1, span.end - 1);
    let tokens = st.lex_span(inner, LexOptions::PATTERN_RECORD).map_err(cut)?;
    st.comments_from(&tokens);
    let items: Vec<&Token> = tokens.iter().filter(|t| t.kind == TokenKind::Item).collect();
    let mut out = Vec::new();
    let mut idx = 0;
    while idx < items.len() {
        let tok = items[idx];
        let text = st.tok(tok);
        if text.starts_with('$') {
            let name = variable_name(st, tok.span)?;
            let pattern = Pattern { span: tok.span, kind: PatternKind::Variable(name) };
            out.push((Spanned::new(Cow::Borrowed(name), tok.span), pattern));
            idx += 1;
            continue;
        }
        let field = value::string_lit(st, tok.span)?.value;
        idx += 1;
        match items.get(idx) {
            Some(colon) if st.tok(colon) == ":" => idx += 1,
            Some(other) => return Err(cut(Diagnostic::expected("`:` after field name in record pattern", other.span))),
            None => return Err(cut(Diagnostic::expected("`:` after field name in record pattern", tok.span.past()))),
        }
        let Some(pat_tok) = items.get(idx) else {
            return Err(cut(Diagnostic::expected("pattern for record field", tok.span.past())));
        };
        let pattern = parse_pattern(st, pat_tok)?;
        idx += 1;
        out.push((Spanned::new(field, tok.span), pattern));
    }
    Ok(out)
}
