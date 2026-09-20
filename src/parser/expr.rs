//! Expressions spanning several items: math expressions, assignments, calls,
//! external calls and environment shorthand.
//!
//! These parsers work on a token stream ([`Toks`]) holding the items of one
//! pipeline element, always terminated by an `Eof` token.

use std::borrow::Cow;

use winnow::stream::{Stateful, Stream, TokenSlice};

use crate::ast::{
    Arg, Assignment, BinaryOp, Call, CallHead, EnvAssignment, EnvShorthand, Expr, ExprKind, ExternalArg, ExternalCall,
    Flag, InterpPart, Interpolation, Operator, Quote, StringLit, UnaryNot,
};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, Tokens, cut};
use crate::lexer::{Token, TokenKind};
use crate::span::{Span, Spanned};

use super::value::{self, Hint};
use super::{St, block, literal, statement};

/// A token stream over the items of one command, carrying the parser state.
pub type Toks<'t, 's, 'a> = Tokens<'t, St<'s, 'a>>;

/// Wrap tokens in a stream.
pub fn toks<'t, 's, 'a>(st: St<'s, 'a>, tokens: &'t [Token]) -> Toks<'t, 's, 'a> {
    Stateful { input: TokenSlice::new(tokens), state: st }
}

/// Copy `tokens` and append an `Eof` token positioned after the last one.
pub fn with_eof(tokens: &[Token]) -> Vec<Token> {
    let mut v = Vec::with_capacity(tokens.len() + 1);
    v.extend(tokens.iter().filter(|t| t.kind != TokenKind::Eof).copied());
    let end = v.last().map_or(Span::point(0), |t| t.span.past());
    v.push(Token { kind: TokenKind::Eof, span: end });
    v
}

/// The items of `tokens` without the trailing `Eof`.
pub fn items(tokens: &[Token]) -> &[Token] {
    match tokens.last() {
        Some(t) if t.kind == TokenKind::Eof => &tokens[..tokens.len() - 1],
        _ => tokens,
    }
}

/// Where the stream ends (the `Eof` token's span).
pub fn end_span(tokens: &[Token]) -> Span {
    tokens.last().map_or(Span::point(0), |t| t.span)
}

/// The next token without consuming it.
pub fn peek_token<'t>(i: &Toks<'t, '_, '_>) -> Option<&'t Token> {
    i.input.peek_finish().first()
}

/// `true` if only `Eof` remains.
pub fn at_end(i: &Toks<'_, '_, '_>) -> bool {
    peek_token(i).is_none_or(|t| t.kind == TokenKind::Eof)
}

/// Consume the next token if it is an item, else fail with `expected <what>`.
pub fn expect_item(i: &mut Toks<'_, '_, '_>, what: &'static str) -> PResult<Token> {
    match peek_token(i) {
        Some(t) if t.kind == TokenKind::Item => {
            let t = *t;
            i.next_token();
            Ok(t)
        }
        Some(t) => Err(cut(Diagnostic::expected(what, t.span))),
        None => Err(cut(Diagnostic::expected(what, Span::point(0)))),
    }
}

/// Fail with `extra tokens` unless only `Eof` remains.
pub fn expect_end(i: &mut Toks<'_, '_, '_>) -> PResult<()> {
    match peek_token(i) {
        Some(t) if t.kind != TokenKind::Eof => Err(cut(Diagnostic::new(ErrorKind::ExtraTokens, t.span))),
        _ => Ok(()),
    }
}

/// Consume all remaining tokens up to `Eof` and return their combined span.
pub fn rest_span(i: &mut Toks<'_, '_, '_>) -> Option<Span> {
    let rest = items(i.input.peek_finish());
    let span = rest.first().map(|f| f.span.merge(rest.last().unwrap().span));
    i.input.next_slice(rest.len());
    span
}

/// Parse one pipeline element's items.
///
/// Handles, in this order: statement keywords, `NAME=value` environment
/// shorthand, assignments, math expressions (when the first item looks like a
/// value), and finally keyword expressions and calls.
pub fn parse_expression<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let items_ = items(tokens);
    let Some(first) = items_.first() else {
        return Err(cut(Diagnostic::expected("command", end_span(tokens))));
    };
    if first.kind == TokenKind::Item && statement::is_statement_keyword(st.tok(first)) {
        return statement::keyword_or_call(st, tokens);
    }
    let (vars, consumed) = env_shorthand_prefix(st, items_)?;
    if consumed == items_.len() {
        return Err(cut(Diagnostic::expected("command after environment shorthand", end_span(tokens))));
    }
    let rest: Vec<Token>;
    let tokens = if consumed > 0 {
        rest = with_eof(&items_[consumed..]);
        &rest[..]
    } else {
        tokens
    };
    let inner = parse_expression_inner(st, tokens)?;
    if vars.is_empty() {
        return Ok(inner);
    }
    let span = vars[0].span.merge(inner.span);
    Ok(Expr::new(ExprKind::EnvShorthand(EnvShorthand { vars, expr: Box::new(inner) }), span))
}

fn parse_expression_inner<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let items_ = items(tokens);
    if items_.iter().any(|t| matches!(t.kind, TokenKind::Assign(_))) {
        return assignment(st, tokens);
    }
    let first = items_[0];
    if first.kind != TokenKind::Item {
        return Err(cut(Diagnostic::expected("command", first.span)));
    }
    if value::looks_like_value(st, first.span) {
        return math_expression(st, tokens, false);
    }
    statement::keyword_or_call(st, tokens)
}

fn is_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Parse leading `NAME=value` items. Returns the assignments and how many
/// items were consumed.
fn env_shorthand_prefix<'a>(st: St<'_, 'a>, items_: &[Token]) -> PResult<(Vec<EnvAssignment<'a>>, usize)> {
    let mut vars = Vec::new();
    let mut consumed = 0;
    for tok in items_ {
        if tok.kind != TokenKind::Item {
            break;
        }
        let text = st.tok(tok);
        let Some(eq) = text.find('=') else { break };
        let name = &text[..eq];
        if !is_env_var_name(name) {
            break;
        }
        let value_span = Span::new(tok.span.start + eq + 1, tok.span.end);
        let value_text = &text[eq + 1..];
        let cp = st.checkpoint();
        let value = if value_text.starts_with('$') {
            value::value(st, value_span, Hint::Any)
        } else if value_text.is_empty() {
            Ok(Expr::new(ExprKind::String(StringLit { value: Cow::Borrowed(""), quote: Quote::Bare }), value_span))
        } else {
            value::string_lit(st, value_span).map(|lit| Expr::new(ExprKind::String(lit), value_span))
        };
        let Ok(value) = value else {
            st.rollback(cp);
            break;
        };
        vars.push(EnvAssignment {
            span: tok.span,
            name: Spanned::new(name, Span::new(tok.span.start, tok.span.start + eq)),
            value,
        });
        consumed += 1;
    }
    Ok((vars, consumed))
}

/// Parse `lhs op= rhs`, where `rhs` is everything to the end of the line.
fn assignment<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let items_ = items(tokens);
    let op_idx = items_.iter().position(|t| matches!(t.kind, TokenKind::Assign(_))).expect("checked by caller");
    let op_tok = items_[op_idx];
    let TokenKind::Assign(op) = op_tok.kind else { unreachable!() };
    let op = Spanned::new(op, op_tok.span);
    if op_idx == 0 {
        return Err(cut(Diagnostic::expected("left hand side of assignment", op_tok.span)));
    }
    let lhs_tokens = with_eof(&items_[..op_idx]);
    let lhs = parse_expression(st, &lhs_tokens)?;
    match &lhs.kind {
        ExprKind::Var(_) => {}
        ExprKind::FullCellPath(p) if matches!(p.head.kind, ExprKind::Var(_)) => {}
        _ => {
            return Err(cut(Diagnostic::message("assignment requires a variable", lhs.span)
                .with_help("only variables (`$x`) and their cell paths (`$x.a`, `$env.FOO`) can be assigned to")));
        }
    }
    let rhs_tokens = &tokens[op_idx + 1..];
    if items(rhs_tokens).is_empty() {
        return Err(cut(Diagnostic::expected("right hand side of assignment", op_tok.span.past())));
    }
    let rhs_span = items(rhs_tokens)[0].span.merge(items(rhs_tokens).last().unwrap().span);
    let rhs = block::parse_block_tokens(st, rhs_tokens, rhs_span);
    let span = lhs.span.merge(rhs_span);
    Ok(Expr::new(ExprKind::Assignment(Assignment { lhs: Box::new(lhs), op, rhs }), span))
}

/// Parse an operator item, with hints for common mistakes.
fn operator(st: St<'_, '_>, tok: &Token) -> PResult<Spanned<Operator>> {
    let text = st.tok(tok);
    if let Some(op) = Operator::from_spelling(text) {
        return Ok(Spanned::new(op, tok.span));
    }
    let help = match text {
        "^" | "pow" => Some("use `**` for exponentiation"),
        "is" | "===" => Some("use `==` for equality"),
        "contains" => Some("use `has` to test membership"),
        "%" => Some("use `mod` for the remainder"),
        "&" => Some("use `bit-and`"),
        "<<" => Some("use `bit-shl`"),
        ">>" => Some("use `bit-shr`"),
        "bits-and" => Some("did you mean `bit-and`?"),
        "bits-xor" => Some("did you mean `bit-xor`?"),
        "bits-or" => Some("did you mean `bit-or`?"),
        "bits-shl" => Some("did you mean `bit-shl`?"),
        "bits-shr" => Some("did you mean `bit-shr`?"),
        "!" => Some("use `not` for boolean negation"),
        _ => None,
    };
    let mut d = if help.is_some() {
        Diagnostic::new(ErrorKind::UnknownOperator(text.to_string()), tok.span)
    } else {
        Diagnostic::expected("operator", tok.span)
    };
    if let Some(h) = help {
        d = d.with_help(h);
    }
    Err(cut(d))
}

enum Stacked<'a> {
    Expr(Expr<'a>),
    Op(Spanned<Operator>),
}

/// Parse a math expression: operands separated by operators, with Nushell's
/// precedence (all left-associative except `**`), `not` prefixes, and `if` /
/// `match` allowed as operands.
///
/// With `row` set, bare strings on the left of an operator (or a lone operand)
/// become cell paths on the implicit `$it` — the `where` row condition.
pub fn math_expression<'a>(st: St<'_, 'a>, tokens: &[Token], row: bool) -> PResult<Expr<'a>> {
    let mut i = toks(st, tokens);
    let Some(first) = peek_token(&i).filter(|t| t.kind == TokenKind::Item) else {
        return Err(cut(Diagnostic::expected("expression", end_span(tokens))));
    };
    if matches!(st.tok(first), "if" | "match") {
        return statement::keyword_or_call(st, tokens);
    }
    let mut lhs = operand(&mut i)?;
    if at_end(&i) {
        if row {
            lhs = expand_row(st, lhs)?;
        }
        return Ok(lhs);
    }
    let mut stack: Vec<Stacked<'a>> = vec![Stacked::Expr(lhs)];
    let mut last_prec = u8::MAX;
    while !at_end(&i) {
        let op_tok = expect_item(&mut i, "operator")?;
        let op = operator(st, &op_tok)?;
        let op_prec = op.item.precedence();
        if at_end(&i) {
            return Err(cut(Diagnostic::expected("expression after operator", op_tok.span.past())
                .with_help("this math expression is incomplete")));
        }
        let next = peek_token(&i).expect("not at end");
        if next.kind == TokenKind::Item && matches!(st.tok(next), "if" | "match") {
            let rest = i.input.peek_finish();
            let rhs = statement::keyword_or_call(st, rest)?;
            let _ = rest_span(&mut i);
            stack.push(Stacked::Op(op));
            stack.push(Stacked::Expr(rhs));
            break;
        }
        let rhs = operand(&mut i)?;
        let left_assoc = !op.item.is_right_associative() && op_prec <= last_prec;
        while left_assoc && stack.len() > 1 {
            let Some(Stacked::Expr(rhs2)) = stack.pop() else { unreachable!() };
            let Some(Stacked::Op(op2)) = stack.pop() else { unreachable!() };
            last_prec = op2.item.precedence();
            if last_prec < op_prec {
                stack.push(Stacked::Op(op2));
                stack.push(Stacked::Expr(rhs2));
                break;
            }
            let Some(Stacked::Expr(lhs2)) = stack.pop() else { unreachable!() };
            stack.push(Stacked::Expr(binary(st, lhs2, op2, rhs2, row)?));
        }
        stack.push(Stacked::Op(op));
        stack.push(Stacked::Expr(rhs));
        last_prec = op_prec;
    }
    while stack.len() > 1 {
        let Some(Stacked::Expr(rhs)) = stack.pop() else { unreachable!() };
        let Some(Stacked::Op(op)) = stack.pop() else { unreachable!() };
        let Some(Stacked::Expr(lhs)) = stack.pop() else { unreachable!() };
        stack.push(Stacked::Expr(binary(st, lhs, op, rhs, row)?));
    }
    match stack.pop() {
        Some(Stacked::Expr(e)) => Ok(e),
        _ => unreachable!("expression stack always ends with one expression"),
    }
}

fn binary<'a>(st: St<'_, 'a>, lhs: Expr<'a>, op: Spanned<Operator>, rhs: Expr<'a>, row: bool) -> PResult<Expr<'a>> {
    let lhs = if row { expand_row(st, lhs)? } else { lhs };
    let span = lhs.span.merge(rhs.span);
    Ok(Expr::new(ExprKind::BinaryOp(BinaryOp { lhs: Box::new(lhs), op, rhs: Box::new(rhs) }), span))
}

/// In a row condition, a string operand `size` means `$it.size`.
fn expand_row<'a>(st: St<'_, 'a>, expr: Expr<'a>) -> PResult<Expr<'a>> {
    match expr.kind {
        ExprKind::String(_) => value::full_cell_path(st, expr.span, true),
        ExprKind::UnaryNot(n) => {
            let inner = expand_row(st, *n.expr)?;
            Ok(Expr::new(ExprKind::UnaryNot(UnaryNot { not_span: n.not_span, expr: Box::new(inner) }), expr.span))
        }
        kind => Ok(Expr { span: expr.span, kind }),
    }
}

/// `not* value`.
fn operand<'a>(i: &mut Toks<'_, '_, 'a>) -> PResult<Expr<'a>> {
    let st = i.state;
    let mut nots = Vec::new();
    while let Some(t) = peek_token(i)
        && t.kind == TokenKind::Item
        && st.tok(t) == "not"
    {
        nots.push(t.span);
        i.next_token();
    }
    let tok = expect_item(i, "expression")?;
    let mut expr = value::value(st, tok.span, Hint::Any)?;
    for not_span in nots.into_iter().rev() {
        let span = not_span.merge(expr.span);
        expr = Expr::new(ExprKind::UnaryNot(UnaryNot { not_span, expr: Box::new(expr) }), span);
    }
    Ok(expr)
}

/// The longest number of leading words that form a known multi-word command.
const MAX_COMMAND_WORDS: usize = 5;

/// Parse a call: a (possibly multi-word) command name followed by arguments.
pub fn parse_call<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let items_ = items(tokens);
    let Some(first) = items_.first() else {
        return Err(cut(Diagnostic::expected("command", end_span(tokens))));
    };
    if first.kind != TokenKind::Item {
        return Err(cut(Diagnostic::expected("command", first.span)));
    }
    if st.tok(first).starts_with('^') {
        return external_call(st, tokens);
    }
    let (head, consumed) = resolve_head(st, items_, "");
    let args = parse_args(st, &items_[consumed..])?;
    let span = first.span.merge(items_.last().unwrap().span);
    Ok(Expr::new(ExprKind::Call(Call { head, args }), span))
}

/// Resolve the longest known command name among the leading items, with an
/// optional prefix (`"attr "` for attributes). Always consumes at least one item.
pub fn resolve_head<'a>(st: St<'_, 'a>, items_: &[Token], prefix: &str) -> (CallHead<'a>, usize) {
    let first_word = st.tok(&items_[0]);
    let first_word = first_word.strip_prefix('@').filter(|_| !prefix.is_empty()).unwrap_or(first_word);
    let single = || (CallHead { name: Cow::Borrowed(first_word), span: items_[0].span }, 1);
    // Fast path: most heads are single words that start no multi-word command.
    let prefix_word = if prefix.is_empty() { first_word } else { prefix.trim_end() };
    if items_.len() < 2 || !st.is_command_prefix(prefix_word) {
        return single();
    }
    let mut words: Vec<&'a str> =
        items_.iter().take(MAX_COMMAND_WORDS).take_while(|t| t.kind == TokenKind::Item).map(|t| st.tok(t)).collect();
    words[0] = first_word;
    for n in (2..=words.len()).rev() {
        let mut name = String::with_capacity(prefix.len() + words[..n].iter().map(|w| w.len() + 1).sum::<usize>());
        name.push_str(prefix);
        for (k, w) in words[..n].iter().enumerate() {
            if k > 0 {
                name.push(' ');
            }
            name.push_str(w);
        }
        if st.is_known_command(&name) {
            let display = name[prefix.len()..].to_string();
            return (CallHead { name: Cow::Owned(display), span: items_[0].span.merge(items_[n - 1].span) }, n);
        }
    }
    single()
}

fn is_negative_number_like(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() > 1 && b[0] == b'-' && (b[1].is_ascii_digit() || (b[1] == b'.' && b.get(2).is_some_and(u8::is_ascii_digit)))
}

/// Parse call arguments: flags, positionals, spreads and `--`.
pub fn parse_args<'a>(st: St<'_, 'a>, items_: &[Token]) -> PResult<Vec<Arg<'a>>> {
    let mut args = Vec::with_capacity(items_.len());
    for tok in items(items_) {
        if tok.kind != TokenKind::Item {
            return Err(cut(Diagnostic::expected("argument", tok.span)));
        }
        let text = st.tok(tok);
        let span = tok.span;
        let arg = if text == "--" {
            Arg::EndOfOptions(span)
        } else if let Some(rest) = text.strip_prefix("--").filter(|r| !r.is_empty()) {
            let (name, value) = match rest.split_once('=') {
                Some((name, _)) => {
                    let value_start = span.start + 2 + name.len() + 1;
                    let value = if value_start >= span.end {
                        return Err(cut(Diagnostic::expected("value after `=`", span.past())));
                    } else {
                        value::value(st, Span::new(value_start, span.end), Hint::Any)?
                    };
                    (name, Some(Box::new(value)))
                }
                None => (rest, None),
            };
            Arg::Flag(Flag { span, name, long: true, value })
        } else if text.len() > 1 && text.starts_with('-') && !is_negative_number_like(text) && !text.starts_with("-..")
        {
            Arg::Flag(Flag { span, name: &text[1..], long: false, value: None })
        } else if value::is_spread(text, b"[$({") {
            let dots = Span::new(span.start, span.start + 3);
            let expr = value::value(st, Span::new(span.start + 3, span.end), Hint::Any)?;
            Arg::Spread { dots, expr }
        } else {
            Arg::Positional(value::value(st, span, Hint::Any)?)
        };
        args.push(arg);
    }
    Ok(args)
}

/// Parse `^cmd args...`.
fn external_call<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Expr<'a>> {
    let items_ = items(tokens);
    let first = items_[0];
    let caret = Span::new(first.span.start, first.span.start + 1);
    let head_span = Span::new(first.span.start + 1, first.span.end);
    if head_span.is_empty() {
        return Err(cut(Diagnostic::expected("command name after `^`", head_span)));
    }
    let head = external_head(st, head_span)?;
    let mut args = Vec::with_capacity(items_.len() - 1);
    for tok in &items_[1..] {
        if tok.kind != TokenKind::Item {
            return Err(cut(Diagnostic::expected("argument", tok.span)));
        }
        let text = st.tok(tok);
        if value::is_spread(text, b"[$(") {
            let dots = Span::new(tok.span.start, tok.span.start + 3);
            let expr = value::value(st, Span::new(tok.span.start + 3, tok.span.end), Hint::Any)?;
            args.push(ExternalArg::Spread { dots, expr });
        } else {
            args.push(ExternalArg::Regular(external_arg(st, tok.span)?));
        }
    }
    let span = first.span.merge(items_.last().unwrap().span);
    Ok(Expr::new(ExprKind::ExternalCall(ExternalCall { caret, head: Box::new(head), args }), span))
}

fn external_head<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    match st.text(span).as_bytes()[0] {
        b'$' | b'(' => value::value(st, span, Hint::Any),
        _ => external_string(st, span),
    }
}

/// An external argument: `$vars`, `(...)`, `[...]` and `{...}` are parsed,
/// everything else is an external string.
pub fn external_arg<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    match st.text(span).as_bytes()[0] {
        b'$' | b'(' | b'[' | b'{' => value::value(st, span, Hint::Any),
        _ => external_string(st, span),
    }
}

/// A word passed to an external command.
///
/// Following Nushell, the word is split into segments — bare text, quoted
/// strings (`'...'`, `"..."`, `` `...` ``, `$"..."`) and parenthesised
/// subexpressions — so that `--query='a (b)'` keeps its parentheses literal
/// while `--out=(pwd)/x` interpolates. All-literal words become one string;
/// otherwise the segments form a bare interpolation.
pub fn external_string<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let text = st.text(span);
    let bytes = text.as_bytes();
    if text.starts_with("r#") {
        return literal::raw_string(st, span);
    }
    if !bytes.iter().any(|b| matches!(b, b'"' | b'\'' | b'(' | b')' | b'`')) {
        return Ok(Expr::new(ExprKind::String(StringLit::bare(text)), span));
    }
    enum State {
        Bare,
        Quote { quote: u8, escaped: bool },
        Backtick,
        Paren { depth: usize },
    }
    let mut segments: Vec<(usize, usize)> = Vec::new();
    let mut from = 0;
    let mut state = State::Bare;
    let mut index = 0;
    while index < bytes.len() {
        let ch = bytes[index];
        match &mut state {
            State::Bare => match ch {
                b'"' | b'\'' => {
                    if index != from {
                        segments.push((from, index));
                    }
                    from = index;
                    state = State::Quote { quote: ch, escaped: false };
                }
                b'$' if matches!(bytes.get(index + 1), Some(b'"' | b'\'')) => {
                    if index != from {
                        segments.push((from, index));
                    }
                    from = index;
                    state = State::Quote { quote: bytes[index + 1], escaped: false };
                    index += 2;
                    continue;
                }
                b'`' => {
                    if index != from {
                        segments.push((from, index));
                    }
                    from = index;
                    state = State::Backtick;
                }
                b'(' => {
                    if index != from {
                        segments.push((from, index));
                    }
                    from = index;
                    state = State::Paren { depth: 1 };
                }
                _ => {}
            },
            State::Quote { quote, escaped } => {
                if ch == *quote && !*escaped {
                    segments.push((from, index + 1));
                    from = index + 1;
                    state = State::Bare;
                } else {
                    *escaped = ch == b'\\' && !*escaped && *quote == b'"';
                }
            }
            State::Backtick => {
                if ch == b'`' {
                    segments.push((from, index + 1));
                    from = index + 1;
                    state = State::Bare;
                }
            }
            State::Paren { depth } => {
                if ch == b')' {
                    if *depth == 1 {
                        segments.push((from, index + 1));
                        from = index + 1;
                        state = State::Bare;
                    } else {
                        *depth -= 1;
                    }
                } else if ch == b'(' {
                    *depth += 1;
                }
            }
        }
        index += 1;
    }
    if from < bytes.len() {
        segments.push((from, bytes.len()));
    }
    let mut parts: Vec<InterpPart<'a>> = Vec::with_capacity(segments.len());
    let mut all_text = true;
    for (start, end) in segments {
        let seg = Span::new(span.start + start, span.start + end);
        let expr = value::string(st, seg)?;
        match expr.kind {
            ExprKind::String(lit) => parts.push(InterpPart::Text { span: seg, value: lit.value }),
            ExprKind::Interpolation(inner) => {
                all_text &= inner.parts.iter().all(|p| matches!(p, InterpPart::Text { .. }));
                parts.extend(inner.parts);
            }
            _ => {
                all_text = false;
                parts.push(InterpPart::Expr(expr));
            }
        }
    }
    let quoted = matches!(bytes, [b'\'', .., b'\''] | [b'"', .., b'"'])
        || (bytes.len() >= 3 && bytes.starts_with(b"$\"") && bytes.ends_with(b"\""));
    let quote = match (quoted, bytes.first()) {
        (true, Some(b'\'')) => Quote::Single,
        (true, _) => Quote::Double,
        (false, _) => Quote::Bare,
    };
    if all_text {
        let value: Cow<'a, str> = match parts.as_slice() {
            [InterpPart::Text { value, .. }] => value.clone(),
            _ => Cow::Owned(
                parts
                    .iter()
                    .filter_map(|p| match p {
                        InterpPart::Text { value, .. } => Some(value.as_ref()),
                        InterpPart::Expr(_) => None,
                    })
                    .collect::<String>(),
            ),
        };
        return Ok(Expr::new(ExprKind::String(StringLit { value, quote }), span));
    }
    Ok(Expr::new(ExprKind::Interpolation(Interpolation { quote, parts }), span))
}
