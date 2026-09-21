//! Lists, tables and records.

use crate::ast::{Expr, ExprKind, InterpPart, ListItem, Quote, RecordItem, StringLit, Table};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut};
use crate::lexer::{LexOptions, Token, TokenKind, lex_prefix_at};
use crate::span::Span;

use super::St;
use super::value::{self, Hint, interior, is_spread};

/// Parse `[ ... ]` as a list or, when it is `[[cols]; [row] ...]`, a table.
pub fn list_or_table<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let inner = interior(st, span, "[", "]")?;
    let tokens = st.lex_span(inner, LexOptions::LIST).map_err(cut)?;
    st.comments_from(&tokens);
    let items: Vec<Token> = tokens
        .into_iter()
        .filter(|t| !matches!(t.kind, TokenKind::Comment | TokenKind::Eol | TokenKind::Eof))
        .collect();
    if let [first, second, rows @ ..] = items.as_slice()
        && first.kind == TokenKind::Item
        && st.tok(first).starts_with('[')
        && second.kind == TokenKind::Semicolon
        && !rows.is_empty()
    {
        return table(st, span, first, rows);
    }
    // Nushell tolerates `|` and `;` between list items, but not a `|` at the end.
    if let Some(last) = items.last()
        && matches!(last.kind, TokenKind::Pipe | TokenKind::PipePipe)
    {
        return Err(cut(Diagnostic::new(ErrorKind::UnexpectedEof("list item after `|`"), last.span)));
    }
    let mut out = Vec::with_capacity(items.len());
    for tok in &items {
        if !matches!(tok.kind, TokenKind::Semicolon | TokenKind::Pipe | TokenKind::PipePipe) {
            out.push(list_item(st, tok)?);
        }
    }
    Ok(Expr::new(ExprKind::List(out), span))
}

fn table<'a>(st: St<'_, 'a>, span: Span, columns: &Token, rows: &[Token]) -> PResult<Expr<'a>> {
    let columns = list_row(st, columns.span)?;
    let rows = rows
        .iter()
        .map(|tok| match tok.kind {
            TokenKind::Item if st.tok(tok).starts_with('[') => list_row(st, tok.span),
            _ => Err(cut(Diagnostic::expected("table row", tok.span).with_help("all table rows must be lists"))),
        })
        .collect::<PResult<Vec<_>>>()?;
    Ok(Expr::new(ExprKind::Table(Table { columns: Box::new(columns), rows }), span))
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
        let expr = value::value(st, Span::new(tok.span.start + 3, tok.span.end), Hint::Any)?;
        return Ok(ListItem::Spread { dots, expr });
    }
    Ok(ListItem::Item(value::value(st, tok.span, Hint::Any)?))
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
///
/// Entries are lexed one at a time: the key with `:` as a special character
/// (so `a:1` splits), the value without (so `http://x` stays whole).
pub fn record<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let inner = interior(st, span, "{", "}")?;
    let text = st.text(inner);
    let mut offset = 0;
    let mut next = |opts: LexOptions| -> PResult<Option<Token>> {
        loop {
            let (tokens, consumed) = lex_prefix_at(&text[offset..], inner.start + offset, opts, 1).map_err(cut)?;
            offset += consumed;
            match tokens.first() {
                None => return Ok(None),
                Some(tok) if tok.kind == TokenKind::Comment => st.comment(tok.span),
                Some(tok) if matches!(tok.kind, TokenKind::Assign(_) | TokenKind::Redirect(_)) => {
                    return Ok(Some(Token { kind: TokenKind::Item, span: tok.span }));
                }
                Some(tok) => return Ok(Some(*tok)),
            }
        }
    };
    let mut items = Vec::new();
    while let Some(key_tok) = next(LexOptions::RECORD_KEY)? {
        if key_tok.kind != TokenKind::Item {
            return Err(cut(Diagnostic::expected("record key", key_tok.span)));
        }
        let key_text = st.tok(&key_tok);
        if is_spread(key_text, b"{$(") {
            let dots = Span::new(key_tok.span.start, key_tok.span.start + 3);
            let expr = value::value(st, Span::new(key_tok.span.start + 3, key_tok.span.end), Hint::Any)?;
            items.push(RecordItem::Spread { dots, expr });
            continue;
        }
        if matches!(key_text, "true" | "false" | "null") {
            return Err(cut(Diagnostic::expected("string", key_tok.span)
                .with_help(format!("`{key_text}` is a value; quote it to use it as a record key"))));
        }
        let key = value::value(st, key_tok.span, Hint::String)?;
        check_bare_colon(st, &key, "key")?;
        let colon = match next(LexOptions::RECORD_KEY)? {
            Some(colon) if st.tok(&colon) == ":" => colon,
            Some(other) => {
                return Err(cut(Diagnostic::expected("`:` after record key", other.span).with_help(
                    "record fields look like `key: value`; a missing colon often makes this parse as a block or closure",
                )));
            }
            None => {
                return Err(cut(Diagnostic::expected("`:` after record key", key_tok.span.past())
                    .with_help("record fields look like `key: value`")));
            }
        };
        let value = match next(LexOptions::RECORD_VALUE)? {
            Some(tok) if tok.kind == TokenKind::Item => value::value(st, tok.span, Hint::Any)?,
            Some(tok) => return Err(cut(Diagnostic::expected("record value", tok.span))),
            None => return Err(cut(Diagnostic::expected("record value", colon.span.past()))),
        };
        check_bare_colon(st, &value, "value")?;
        items.push(RecordItem::Pair { key, colon: colon.span, value });
    }
    Ok(Expr::new(ExprKind::Record(items), span))
}

/// Like Nushell, refuse a bare word containing `:` as a record key or value
/// (`{a: x:y}`, `{a: http://x}`): it is almost always a missing quote or
/// separator, and the lexer would otherwise split it unpredictably.
fn check_bare_colon(st: St<'_, '_>, expr: &Expr<'_>, position: &'static str) -> PResult<()> {
    let bare_spans: Vec<Span> = match &expr.kind {
        ExprKind::String(s) if s.quote == Quote::Bare => vec![expr.span],
        ExprKind::Interpolation(i) if i.quote == Quote::Bare => i
            .parts
            .iter()
            .filter_map(|p| match p {
                InterpPart::Text { span, .. } => Some(*span),
                InterpPart::Expr(_) => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    for span in bare_spans {
        if let Some(at) = st.text(span).find(':') {
            let colon = Span::new(span.start + at, span.start + at + 1);
            return Err(cut(Diagnostic::message(format!("colon in bare word specifying record {position}"), colon)
                .with_help(format!("quote the {position} if the `:` is part of it"))));
        }
    }
    Ok(())
}
