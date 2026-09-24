//! `match` blocks and patterns.

use std::borrow::Cow;

use crate::ast::{MatchArm, Pattern, PatternKind};
use crate::error::Diagnostic;
use crate::input::{PResult, cut};
use crate::lexer::{LexOptions, Token, TokenKind};
use crate::span::{Span, Spanned};

use super::cellpath::is_identifier;
use super::cursor::Cursor;
use super::expr::Position;
use super::statement::check_variable_name;
use super::value::{self, Hint, interior};
use super::{St, collections, expr};

/// Parse the `{ pattern => body, ... }` item of a `match`.
pub fn match_block<'a>(st: St<'_, 'a>, span: Span) -> PResult<(Span, Vec<MatchArm<'a>>)> {
    let inner = interior(st, span, "{", "}")?;
    let tokens = st.lex_span(inner, LexOptions::MATCH).map_err(cut)?;
    st.comments_from(&tokens);
    let tokens: Vec<Token> = tokens
        .into_iter()
        .filter(|t| !matches!(t.kind, TokenKind::Comment | TokenKind::Eol | TokenKind::Eof))
        .collect();
    let mut c = Cursor::new(&tokens, inner.end);
    let mut arms = Vec::new();
    while !c.at_end() {
        arms.push(match_arm(st, &mut c)?);
    }
    Ok((span, arms))
}

/// `pattern ( | pattern )* [if guard...] => body`.
fn match_arm<'a>(st: St<'_, 'a>, c: &mut Cursor<'_>) -> PResult<MatchArm<'a>> {
    let mut pattern = parse_pattern(st, &c.expect_item("pattern")?)?;
    if c.peek().is_some_and(|t| t.kind == TokenKind::Pipe) {
        let mut alternatives = vec![pattern];
        while c.peek().is_some_and(|t| t.kind == TokenKind::Pipe) {
            c.next();
            alternatives.push(parse_pattern(st, &c.expect_item("pattern after `|`")?)?);
        }
        let span = alternatives[0].span.merge(alternatives[alternatives.len() - 1].span);
        pattern = Pattern { span, kind: PatternKind::Or(alternatives) };
    }
    let guard = match c.peek() {
        Some(tok) if tok.kind == TokenKind::Item && st.tok(tok) == "if" => {
            c.next();
            let start = c.position();
            let arrow = c.rest().iter().position(|t| t.kind == TokenKind::Item && st.tok(t) == "=>");
            let end = arrow.map_or(c.all().len(), |p| start + p);
            if end == start {
                return Err(cut(Diagnostic::expected("expression after `if` in match guard", tok.span.past())
                    .with_help("the `if` keyword must be followed by an expression")));
            }
            let guard = expr::math_expression(st, c.slice(start..end), false)?;
            c.reset(end);
            Some(Box::new(guard))
        }
        _ => None,
    };
    let arrow = match c.next() {
        Some(tok) if tok.kind == TokenKind::Item && st.tok(tok) == "=>" => tok.span,
        Some(tok) => return Err(cut(Diagnostic::expected("`=>`", tok.span))),
        None => return Err(cut(Diagnostic::expected("`=>`", c.end_span()))),
    };
    // The body is one item: a block (or a record or closure when it looks like
    // one), otherwise an expression.
    let body_tok = c.expect_item("match arm body")?;
    let body = match st.tok(&body_tok).starts_with('{') {
        true => value::value(st, body_tok.span, Hint::MatchBody)?,
        false => expr::parse_expression(st, c.slice(c.position() - 1..c.position()), Position::Element)?,
    };
    Ok(MatchArm { span: pattern.span.merge(body.span), pattern, guard, arrow, body })
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

/// The name of a `$var` pattern, which binds a variable and so may not be a
/// reserved name.
fn variable_name<'a>(st: St<'_, 'a>, span: Span) -> PResult<&'a str> {
    let name = st.text(span).strip_prefix('$').unwrap_or_default();
    if !is_identifier(name) {
        return Err(cut(Diagnostic::expected("valid variable name", span)));
    }
    check_variable_name(name, span)?;
    Ok(name)
}

/// `[p1, p2, ..$rest]`. Like nu, the interior is lite-parsed: `|` separates
/// groups (`[1 | 2]` is `[1, 2]`), a redirection is dropped, and the items
/// after a `..`/`..$rest` in the same group are dropped too.
fn list_pattern<'a>(st: St<'_, 'a>, span: Span) -> PResult<Vec<Pattern<'a>>> {
    let inner = interior(st, span, "[", "]")?;
    let tokens = st.lex_span(inner, LexOptions::PATTERN_LIST).map_err(cut)?;
    st.comments_from(&tokens);
    let tokens: Vec<Token> =
        tokens.into_iter().filter(|t| !matches!(t.kind, TokenKind::Eof | TokenKind::Comment)).collect();
    if let Some(tok) = tokens.iter().find(|t| t.kind == TokenKind::Semicolon) {
        return Err(cut(Diagnostic::message("unexpected semicolon in list pattern", tok.span)
            .with_help("use commas or whitespace to separate list items")));
    }
    let mut out = Vec::new();
    for group in collections::lite_parts(st, &tokens)? {
        let mut idx = 0;
        while let Some(tok) = group.get(idx) {
            idx += 1;
            let text = st.tok(tok);
            let rest = match text.strip_prefix("..").filter(|_| !text.starts_with("...")) {
                Some("") => Some(PatternKind::Rest(None)),
                Some(name) if name.starts_with('$') => {
                    let name_span = Span::new(tok.span.start + 2, tok.span.end);
                    let name = variable_name(st, name_span)?;
                    Some(PatternKind::Rest(Some(Spanned::new(name, name_span))))
                }
                _ => None,
            };
            match rest {
                Some(kind) => {
                    out.push(Pattern { span: tok.span, kind });
                    // nu stops reading the group here.
                    for ignored in &group[idx..] {
                        st.ignore(ignored.span);
                    }
                    break;
                }
                None => out.push(parse_pattern(st, &Token { kind: TokenKind::Item, span: tok.span })?),
            }
        }
    }
    Ok(out)
}

/// `{key: pattern, $shorthand}`. Like nu, every token is a field name, kept
/// verbatim (`{"a": $x}` has the field `"a"`, quotes included), and must be
/// followed by `:` and a pattern.
fn record_pattern<'a>(st: St<'_, 'a>, span: Span) -> PResult<Vec<(Spanned<Cow<'a, str>>, Pattern<'a>)>> {
    let inner = interior(st, span, "{", "}")?;
    let tokens = st.lex_span(inner, LexOptions::PATTERN_RECORD).map_err(cut)?;
    st.comments_from(&tokens);
    let items: Vec<Token> = tokens
        .into_iter()
        .filter(|t| !matches!(t.kind, TokenKind::Eof | TokenKind::Comment))
        .map(|t| Token { kind: TokenKind::Item, span: t.span })
        .collect();
    let mut c = Cursor::new(&items, inner.end);
    let mut out = Vec::new();
    while let Some(tok) = c.next() {
        if st.tok(tok).starts_with('$') {
            // `{$name}` binds the field of the same name.
            let name = variable_name(st, tok.span)?;
            let pattern = Pattern { span: tok.span, kind: PatternKind::Variable(name) };
            out.push((Spanned::new(Cow::Borrowed(name), tok.span), pattern));
            continue;
        }
        let field = Cow::Borrowed(st.tok(tok));
        match c.next() {
            Some(colon) if st.tok(colon) == ":" => {}
            _ => {
                return Err(cut(Diagnostic::expected("record", span)
                    .with_help("record patterns look like `{key: pattern}`; a `:` must follow the field name")));
            }
        }
        let pattern = parse_pattern(st, &c.expect_item("pattern for record field")?)?;
        out.push((Spanned::new(field, tok.span), pattern));
    }
    Ok(out)
}
