//! Lists, tables and records.

use crate::ast::{Expr, ExprKind, InterpPart, ListItem, Quote, RecordItem, Table, TypeKind};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut};
use crate::lexer::{LexOptions, RedirectSource, Token, TokenKind, lex_prefix_at};
use crate::span::Span;

use super::St;
use super::value::{self, Hint, interior, is_spread};

/// nu's lite parse of the tokens inside `[...]`: the items of each
/// `|`-separated command. A redirection and its target are dropped from the
/// items as nu drops them (`[a o> b]` is `[a]`) and recorded as ignored text;
/// a redirection with nothing before it, a missing target, a second
/// redirection of the same stream, `||` and a `|` at the end are errors.
/// After an assignment operator everything is an item (`[a = b | c]` has
/// five). `;` is left to the caller, which has already refused it.
pub fn lite_parts(st: St<'_, '_>, tokens: &[Token]) -> PResult<Vec<Vec<Token>>> {
    #[derive(Clone, Copy)]
    enum Redirected {
        None,
        Single(RedirectSource),
        Separate,
    }
    if let Some(last) = tokens.last().filter(|t| t.kind == TokenKind::Pipe) {
        return Err(cut(Diagnostic::new(ErrorKind::UnexpectedEof("list item after `|`"), last.span)));
    }
    let mut groups: Vec<Vec<Token>> = Vec::new();
    let mut parts: Vec<Token> = Vec::new();
    let mut redirected = Redirected::None;
    let mut assignment = false;
    let mut idx = 0;
    while let Some(tok) = tokens.get(idx) {
        idx += 1;
        match tok.kind {
            TokenKind::Assign(_) => {
                assignment = true;
                parts.push(*tok);
            }
            _ if assignment => parts.push(*tok),
            TokenKind::Item => parts.push(*tok),
            TokenKind::PipePipe => {
                return Err(cut(Diagnostic::new(ErrorKind::ShellSyntax { found: "||", use_instead: "or" }, tok.span)));
            }
            TokenKind::Pipe => {
                groups.push(std::mem::take(&mut parts));
                redirected = Redirected::None;
            }
            TokenKind::Redirect(op) => {
                if parts.is_empty() {
                    return Err(cut(Diagnostic::message("unexpected redirection: nothing to redirect", tok.span)));
                }
                redirected = match (redirected, op.source()) {
                    (Redirected::None, source) => Redirected::Single(source),
                    (Redirected::Single(RedirectSource::Stdout), RedirectSource::Stderr)
                    | (Redirected::Single(RedirectSource::Stderr), RedirectSource::Stdout) => Redirected::Separate,
                    _ => {
                        return Err(cut(Diagnostic::message("multiple redirections of the same stream", tok.span)));
                    }
                };
                st.ignore(tok.span);
                if op.is_pipe() {
                    groups.push(std::mem::take(&mut parts));
                    redirected = Redirected::None;
                    continue;
                }
                match tokens.get(idx) {
                    Some(target) if target.kind == TokenKind::Item => {
                        st.ignore(target.span);
                        idx += 1;
                    }
                    _ => return Err(cut(Diagnostic::expected("redirection target", tok.span.past()))),
                }
            }
            TokenKind::Comment | TokenKind::Eol | TokenKind::Semicolon | TokenKind::Eof => {}
        }
    }
    groups.push(parts);
    Ok(groups)
}

/// The tokens of a `[...]` interior, comments recorded and dropped.
fn bracket_tokens(st: St<'_, '_>, span: Span) -> PResult<Vec<Token>> {
    let inner = interior(st, span, "[", "]")?;
    let tokens = st.lex_span(inner, LexOptions::LIST).map_err(cut)?;
    st.comments_from(&tokens);
    Ok(tokens.into_iter().filter(|t| !matches!(t.kind, TokenKind::Comment | TokenKind::Eol | TokenKind::Eof)).collect())
}

/// Like nu, a `;` between list items is an error (only `[[cols]; [row]]` has one).
fn refuse_semicolon(items: &[Token], what: &'static str) -> PResult<()> {
    match items.iter().find(|t| t.kind == TokenKind::Semicolon) {
        Some(tok) => Err(cut(Diagnostic::message(format!("unexpected semicolon in {what}"), tok.span)
            .with_help("use commas or whitespace to separate list items"))),
        None => Ok(()),
    }
}

/// Parse `[ ... ]` as a list or, when it is `[[cols]; [row] ...]`, a table.
pub fn list_or_table<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    list_or_table_typed(st, span, None)
}

/// [`list_or_table`] with the items parsed as `elem` (`list<int>` defaults).
pub fn list_or_table_typed<'a>(st: St<'_, 'a>, span: Span, elem: Option<&TypeKind<'a>>) -> PResult<Expr<'a>> {
    let items = bracket_tokens(st, span)?;
    if let [first, second, rows @ ..] = items.as_slice()
        && first.kind == TokenKind::Item
        && st.tok(first).starts_with('[')
        && second.kind == TokenKind::Semicolon
    {
        return table(st, span, first, second, rows);
    }
    refuse_semicolon(&items, "list")?;
    let mut out = Vec::with_capacity(items.len());
    for group in lite_parts(st, &items)? {
        for tok in &group {
            out.push(list_item(st, tok, elem)?);
        }
    }
    Ok(Expr::new(ExprKind::List(out), span))
}

/// `[[cols]; [row] ...]`: every row must be a list with as many items as
/// there are columns, and every column name must be a string.
fn table<'a>(st: St<'_, 'a>, span: Span, columns: &Token, semicolon: &Token, rows: &[Token]) -> PResult<Expr<'a>> {
    let columns = list_row(st, columns.span)?;
    if rows.is_empty() {
        return Err(cut(Diagnostic::expected("table row", semicolon.span.past())));
    }
    let ExprKind::List(column_items) = &columns.kind else { unreachable!("list_row returns a list") };
    let width = column_items.len();
    let rows = rows
        .iter()
        .map(|tok| {
            if tok.kind != TokenKind::Item || !st.tok(tok).starts_with('[') {
                return Err(cut(Diagnostic::message("table item not list", tok.span)
                    .with_help("all table items must be lists")));
            }
            let row = list_row(st, tok.span)?;
            let ExprKind::List(items) = &row.kind else { unreachable!("list_row returns a list") };
            match items.len().cmp(&width) {
                std::cmp::Ordering::Less => Err(cut(Diagnostic::message("missing columns", tok.span)
                    .with_help(format!("expected {width} columns, found {}", items.len())))),
                std::cmp::Ordering::Greater => {
                    let extra = items[width].span().merge(items[items.len() - 1].span());
                    Err(cut(Diagnostic::message("extra columns", extra)
                        .with_help(format!("expected {width} columns, found {}", items.len()))))
                }
                std::cmp::Ordering::Equal => Ok(row),
            }
        })
        .collect::<PResult<Vec<_>>>()?;
    for column in column_items {
        let ListItem::Item(expr) = column else { unreachable!("list_row refuses spreads") };
        let stringy = matches!(
            expr.kind,
            ExprKind::String(_)
                | ExprKind::Interpolation(_)
                | ExprKind::Var(_)
                | ExprKind::FullCellPath(_)
                | ExprKind::Subexpression(_)
        );
        if !stringy {
            return Err(cut(Diagnostic::message("table column name not string", expr.span)
                .with_help("table column names should be able to be converted into strings")));
        }
    }
    Ok(Expr::new(ExprKind::Table(Table { columns: Box::new(columns), rows }), span))
}

fn list_item<'a>(st: St<'_, 'a>, tok: &Token, elem: Option<&TypeKind<'a>>) -> PResult<ListItem<'a>> {
    let text = st.tok(tok);
    if tok.kind == TokenKind::Item && is_spread(text, b"[$(") {
        let dots = Span::new(tok.span.start, tok.span.start + 3);
        let expr = value::value(st, Span::new(tok.span.start + 3, tok.span.end), Hint::Any)?;
        return Ok(ListItem::Spread { dots, expr });
    }
    // `[Assignment, =, Assign]`: an operator on its own is just a word here,
    // and so is anything after it (`[a = b | c]`).
    if tok.kind != TokenKind::Item {
        return Ok(ListItem::Item(Expr::new(ExprKind::String(crate::ast::StringLit::bare(text)), tok.span)));
    }
    let hint = match elem {
        Some(kind) => Hint::Typed(kind),
        None => Hint::Any,
    };
    Ok(ListItem::Item(value::value(st, tok.span, hint)?))
}

/// A table header or row: a list without spreads.
fn list_row<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let items = bracket_tokens(st, span)?;
    refuse_semicolon(&items, "list")?;
    let mut out = Vec::new();
    for group in lite_parts(st, &items)? {
        for tok in &group {
            match list_item(st, tok, None)? {
                item @ ListItem::Item(_) => out.push(item),
                ListItem::Spread { dots, .. } => {
                    return Err(cut(Diagnostic::message("cannot spread in a table row", dots)));
                }
            }
        }
    }
    Ok(Expr::new(ExprKind::List(out), span))
}

/// Parse `{ key: value, ...$spread }`.
///
/// Entries are lexed one at a time: the key with `:` as a special character
/// (so `a:1` splits), the value without (so `http://x` stays whole). Like nu,
/// a key or a value must be an item: `{a: =}` and `{a: o>}` are errors.
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
                Some(tok) => return Ok(Some(*tok)),
            }
        }
    };
    let mut items = Vec::new();
    while let Some(key_tok) = next(LexOptions::RECORD_KEY)? {
        if key_tok.kind != TokenKind::Item {
            return Err(cut(Diagnostic::message("unexpected token in record", key_tok.span)
                .with_help("expected a record key here; fields look like `key: value`")));
        }
        let key_text = st.tok(&key_tok);
        if is_spread(key_text, b"{$(") {
            let dots = Span::new(key_tok.span.start, key_tok.span.start + 3);
            let expr = value::value(st, Span::new(key_tok.span.start + 3, key_tok.span.end), Hint::Any)?;
            items.push(RecordItem::Spread { dots, expr });
            continue;
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
            Some(tok) => {
                return Err(cut(Diagnostic::message("unexpected token in record value", tok.span)
                    .with_help("after `key:`, provide a value (string, number, record, list, ...)")));
            }
            None => return Err(cut(Diagnostic::expected("value for record field", colon.span.past()))),
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
