//! Expressions spanning several items: math expressions, assignments, calls,
//! external calls and environment shorthand.
//!
//! These parsers take a [`Cursor`] over the items of one pipeline element.

use std::borrow::Cow;

use crate::ast::{
    Arg, Assignment, BinaryOp, Call, CallHead, DynamicCall, EnvAssignment, EnvShorthand, Expr, ExprKind, ExternalArg,
    ExternalCall, Flag, InterpPart, Interpolation, Operator, Quote, StringLit, UnaryNot,
};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut};
use crate::lexer::{Token, TokenKind};
use crate::span::{Span, Spanned};

use super::cursor::Cursor;
use super::value::{self, Hint, is_spread, looks_like_value};
use super::{St, block, cellpath, literal, statement, strings};

/// Parse one pipeline element's items.
///
/// Handles, in this order: statement keywords, `NAME=value` environment
/// shorthand, assignments, math expressions (when the first item looks like a
/// value), and finally keyword expressions and calls.
pub fn parse_expression<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let Some(first) = c.peek() else {
        return Err(cut(Diagnostic::expected("command", c.end_span())));
    };
    if first.kind == TokenKind::Item && statement::is_statement_keyword(st.tok(first)) {
        return statement::keyword_or_call(st, c);
    }
    let vars = env_shorthand_prefix(st, &mut c)?;
    let Some(first) = c.peek() else {
        return Err(cut(Diagnostic::expected("command after environment shorthand", c.end_span())));
    };
    let inner = if c.rest().iter().any(|t| matches!(t.kind, TokenKind::Assign(_))) {
        assignment(st, c.remaining())?
    } else if first.kind != TokenKind::Item {
        return Err(cut(Diagnostic::expected("command", first.span)));
    } else if looks_like_value(st.tok(first)) {
        math_expression(st, c.remaining(), false)?
    } else {
        statement::keyword_or_call(st, c.remaining())?
    };
    match vars.first() {
        None => Ok(inner),
        Some(v) => {
            let span = v.span.merge(inner.span);
            Ok(Expr::new(ExprKind::EnvShorthand(EnvShorthand { vars, expr: Box::new(inner) }), span))
        }
    }
}

fn is_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Consume leading `NAME=value` items.
fn env_shorthand_prefix<'a>(st: St<'_, 'a>, c: &mut Cursor<'_>) -> PResult<Vec<EnvAssignment<'a>>> {
    let mut vars = Vec::new();
    while let Some(tok) = c.peek().filter(|t| t.kind == TokenKind::Item) {
        let text = st.tok(tok);
        let Some(eq) = text.find('=').filter(|eq| is_env_var_name(&text[..*eq])) else { break };
        let value_span = Span::new(tok.span.start + eq + 1, tok.span.end);
        let value = match &text[eq + 1..] {
            "" => Expr::new(ExprKind::String(StringLit::bare("")), value_span),
            v if v.starts_with('$') => value::value(st, value_span, Hint::Any)?,
            _ => Expr::new(ExprKind::String(strings::string_lit(st, value_span)?), value_span),
        };
        vars.push(EnvAssignment {
            span: tok.span,
            name: Spanned::new(&text[..eq], Span::new(tok.span.start, tok.span.start + eq)),
            value,
        });
        c.next();
    }
    Ok(vars)
}

/// Parse `lhs op= rhs`, where `rhs` is everything to the end of the line.
fn assignment<'a>(st: St<'_, 'a>, c: Cursor<'_>) -> PResult<Expr<'a>> {
    let items = c.all();
    let op_idx = items.iter().position(|t| matches!(t.kind, TokenKind::Assign(_))).expect("checked by caller");
    let op_tok = items[op_idx];
    let TokenKind::Assign(op) = op_tok.kind else { unreachable!("position found an assignment token") };
    if op_idx == 0 {
        return Err(cut(Diagnostic::expected("left hand side of assignment", op_tok.span)));
    }
    let lhs = parse_expression(st, c.slice(0..op_idx))?;
    // nu accepts a subexpression head too (`(1) = 2`) and fails at run time.
    match &lhs.kind {
        ExprKind::Var(_) | ExprKind::Subexpression(_) => {}
        ExprKind::FullCellPath(p) if matches!(p.head.kind, ExprKind::Var(_) | ExprKind::Subexpression(_)) => {}
        _ => {
            return Err(cut(Diagnostic::message("assignment requires a variable", lhs.span)
                .with_help("only variables (`$x`) and their cell paths (`$x.a`, `$env.FOO`) can be assigned to")));
        }
    }
    let rhs = c.slice(op_idx + 1..items.len());
    let Some(rhs_span) = rhs.span() else {
        return Err(cut(Diagnostic::expected("right hand side of assignment", op_tok.span.past())));
    };
    let rhs = block::parse_block(st, rhs, rhs_span);
    let span = lhs.span.merge(rhs_span);
    Ok(Expr::new(ExprKind::Assignment(Assignment { lhs: Box::new(lhs), op: Spanned::new(op, op_tok.span), rhs }), span))
}

/// Parse an operator item, with hints for common mistakes.
fn operator(st: St<'_, '_>, tok: &Token) -> PResult<Spanned<Operator>> {
    let text = st.tok(tok);
    if let Some(op) = Operator::from_spelling(text) {
        return Ok(Spanned::new(op, tok.span));
    }
    let help = match text {
        "^" | "pow" => "use `**` for exponentiation",
        "is" | "===" => "use `==` for equality",
        "contains" => "use `has` to test membership",
        "%" => "use `mod` for the remainder",
        "&" => "use `bit-and`",
        "<<" => "use `bit-shl`",
        ">>" => "use `bit-shr`",
        "bits-and" => "did you mean `bit-and`?",
        "bits-xor" => "did you mean `bit-xor`?",
        "bits-or" => "did you mean `bit-or`?",
        "bits-shl" => "did you mean `bit-shl`?",
        "bits-shr" => "did you mean `bit-shr`?",
        "!" => "use `not` for boolean negation",
        _ => return Err(cut(Diagnostic::expected("operator", tok.span))),
    };
    Err(cut(Diagnostic::new(ErrorKind::UnknownOperator(text.to_string()), tok.span).with_help(help)))
}

/// Parse a math expression: operands separated by operators, with Nushell's
/// precedence (all left-associative except `**`), `not` prefixes, and `if` /
/// `match` allowed as operands.
///
/// With `row` set, bare strings on the left of an operator (or a lone operand)
/// become cell paths on the implicit `$it`: the `where` row condition.
pub fn math_expression<'a>(st: St<'_, 'a>, mut c: Cursor<'_>, row: bool) -> PResult<Expr<'a>> {
    let Some(first) = c.peek().filter(|t| t.kind == TokenKind::Item) else {
        return Err(cut(Diagnostic::expected("expression", c.here())));
    };
    if matches!(st.tok(first), "if" | "match") {
        return statement::keyword_or_call(st, c);
    }
    let lhs = operand(st, &mut c)?;
    if c.at_end() {
        return if row { expand_row(st, lhs) } else { Ok(lhs) };
    }
    // The same fold as `nu-parser`: the stack grows while precedence rises and
    // collapses when it falls or stays level (except for the right-associative `**`).
    let mut exprs: Vec<Expr<'a>> = vec![lhs];
    let mut ops: Vec<Spanned<Operator>> = Vec::new();
    let mut last_prec = u8::MAX;
    while !c.at_end() {
        let op_tok = c.expect_item("operator")?;
        let op = operator(st, &op_tok)?;
        if c.at_end() {
            return Err(cut(Diagnostic::expected("expression after operator", op_tok.span.past())
                .with_help("this math expression is incomplete")));
        }
        let keyword_operand =
            c.peek().is_some_and(|t| t.kind == TokenKind::Item && matches!(st.tok(t), "if" | "match"));
        if keyword_operand {
            // `1 + if $x { 2 } else { 3 }`: the keyword takes the rest of the items.
            let rhs = statement::keyword_or_call(st, c.remaining())?;
            c.rest_span();
            ops.push(op);
            exprs.push(rhs);
            break;
        }
        let rhs = operand(st, &mut c)?;
        let prec = op.item.precedence();
        if !op.item.is_right_associative() && prec <= last_prec {
            while ops.last().is_some_and(|prev| prev.item.precedence() >= prec) {
                fold_top(st, &mut exprs, &mut ops, row)?;
            }
        }
        last_prec = prec;
        ops.push(op);
        exprs.push(rhs);
    }
    while !ops.is_empty() {
        fold_top(st, &mut exprs, &mut ops, row)?;
    }
    exprs.pop().ok_or_else(|| cut(Diagnostic::expected("expression", c.end_span())))
}

/// Combine the two topmost operands with the topmost operator.
fn fold_top<'a>(st: St<'_, 'a>, exprs: &mut Vec<Expr<'a>>, ops: &mut Vec<Spanned<Operator>>, row: bool) -> PResult<()> {
    let (Some(rhs), Some(op), Some(lhs)) = (exprs.pop(), ops.pop(), exprs.pop()) else {
        unreachable!("operators and operands are pushed in pairs")
    };
    let lhs = if row { expand_row(st, lhs)? } else { lhs };
    let span = lhs.span.merge(rhs.span);
    exprs.push(Expr::new(ExprKind::BinaryOp(BinaryOp { lhs: Box::new(lhs), op, rhs: Box::new(rhs) }), span));
    Ok(())
}

/// In a row condition, a string operand `size` means `$it.size`.
fn expand_row<'a>(st: St<'_, 'a>, expr: Expr<'a>) -> PResult<Expr<'a>> {
    match expr.kind {
        ExprKind::String(_) => cellpath::full_cell_path(st, expr.span, true),
        ExprKind::UnaryNot(n) => {
            let inner = expand_row(st, *n.expr)?;
            Ok(Expr::new(ExprKind::UnaryNot(UnaryNot { not_span: n.not_span, expr: Box::new(inner) }), expr.span))
        }
        kind => Ok(Expr { span: expr.span, kind }),
    }
}

/// `not* value`.
fn operand<'a>(st: St<'_, 'a>, c: &mut Cursor<'_>) -> PResult<Expr<'a>> {
    let mut nots = Vec::new();
    while let Some(tok) = c.peek().filter(|t| t.kind == TokenKind::Item && st.tok(t) == "not") {
        nots.push(tok.span);
        c.next();
    }
    let tok = c.expect_item("expression")?;
    let mut expr = value::value(st, tok.span, Hint::Any)?;
    for not_span in nots.into_iter().rev() {
        let span = not_span.merge(expr.span);
        expr = Expr::new(ExprKind::UnaryNot(UnaryNot { not_span, expr: Box::new(expr) }), span);
    }
    Ok(expr)
}

/// The most words that can form a known multi-word command.
const MAX_COMMAND_WORDS: usize = 5;

/// Parse a call: a (possibly multi-word) command name followed by arguments.
pub fn parse_call<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let first = c.expect_item("command")?;
    match st.tok(&first).as_bytes()[0] {
        b'^' => return external_call(st, first, c),
        b'%' => return percent_call(st, first, c),
        _ => {}
    }
    let head = resolve_head(st, first, &mut c, "");
    let args = parse_args(st, c)?;
    let span = first.span.merge(args.last().map_or(head.span, Arg::span));
    Ok(Expr::new(ExprKind::Call(Call { head, args, sigil: None }), span))
}

const PERCENT_HELP: &str =
    "write the built-in command's name bare (`%ls`), or `%$var` / `%(expr)` to name it at run time";

/// `%cmd args`, `% cmd args`, `%$var args` and `%(expr) args`: a call that
/// must resolve to a built-in command, never a custom command or alias.
/// `first` is the item starting with `%`, already consumed.
fn percent_call<'a>(st: St<'_, 'a>, first: Token, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let sigil = Span::new(first.span.start, first.span.start + 1);
    // The head is the rest of the item, or the next item after a bare `%`.
    let head_tok = match first.span.len() {
        1 => match c.next() {
            Some(tok) if tok.kind == TokenKind::Item => *tok,
            _ => {
                return Err(cut(
                    Diagnostic::message("percent sigil requires a built-in command", sigil).with_help(PERCENT_HELP)
                ));
            }
        },
        _ => Token { kind: TokenKind::Item, span: Span::new(sigil.end, first.span.end) },
    };
    let head_text = st.tok(&head_tok);
    match head_text.as_bytes()[0] {
        b'$' | b'(' => {
            let head = value::value(st, head_tok.span, Hint::Any)?;
            let args = parse_args(st, c)?;
            let span = sigil.merge(args.last().map_or(head.span, Arg::span));
            Ok(Expr::new(ExprKind::DynamicCall(DynamicCall { sigil, head: Box::new(head), args }), span))
        }
        b'"' | b'\'' | b'`' | b'[' | b'{' | b'^' | b'%' => {
            Err(cut(
                Diagnostic::message("percent sigil requires a built-in command", head_tok.span).with_help(PERCENT_HELP)
            ))
        }
        _ => {
            let head = resolve_head(st, head_tok, &mut c, "");
            if st.is_builtin_command(&head.name) == Some(false) {
                return Err(cut(Diagnostic::message("percent sigil requires a built-in command", head.span)
                    .with_help(format!("`{}` is not a built-in command; {PERCENT_HELP}", head.name))));
            }
            let args = parse_args(st, c)?;
            let span = sigil.merge(args.last().map_or(head.span, Arg::span));
            Ok(Expr::new(ExprKind::Call(Call { head, args, sigil: Some(sigil) }), span))
        }
    }
}

/// Resolve the longest known command name starting at `first` (already
/// consumed), consuming any further words that belong to the name. `prefix`
/// is `"attr "` for attributes, whose first word carries a leading `@`.
pub fn resolve_head<'a>(st: St<'_, 'a>, first: Token, c: &mut Cursor<'_>, prefix: &str) -> CallHead<'a> {
    let first_word = st.tok(&first);
    let first_word = if prefix.is_empty() { first_word } else { first_word.strip_prefix('@').unwrap_or(first_word) };
    let single = CallHead { name: Cow::Borrowed(first_word), span: first.span };
    // Fast path: most heads are single words that start no multi-word command.
    let prefix_word = if prefix.is_empty() { first_word } else { prefix.trim_end() };
    if !st.is_command_prefix(prefix_word) {
        return single;
    }
    let more: Vec<&Token> =
        c.rest().iter().take(MAX_COMMAND_WORDS - 1).take_while(|t| t.kind == TokenKind::Item).collect();
    for n in (1..=more.len()).rev() {
        let mut name = String::from(prefix);
        name.push_str(first_word);
        for tok in &more[..n] {
            name.push(' ');
            name.push_str(st.tok(tok));
        }
        if st.is_known_command(&name) {
            let display = name.split_off(prefix.len());
            for _ in 0..n {
                c.next();
            }
            return CallHead { name: Cow::Owned(display), span: first.span.merge(more[n - 1].span) };
        }
    }
    single
}

fn is_negative_number_like(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() > 1 && b[0] == b'-' && (b[1].is_ascii_digit() || (b[1] == b'.' && b.get(2).is_some_and(u8::is_ascii_digit)))
}

/// Parse call arguments: flags, positionals, spreads and `--`.
pub fn parse_args<'a>(st: St<'_, 'a>, mut c: Cursor<'_>) -> PResult<Vec<Arg<'a>>> {
    let mut args = Vec::with_capacity(c.rest().len());
    while !c.at_end() {
        let tok = c.expect_item("argument")?;
        let text = st.tok(&tok);
        let span = tok.span;
        let arg = match text {
            "--" => Arg::EndOfOptions(span),
            _ if text.starts_with("--") && text.len() > 2 => {
                let rest = &text[2..];
                let (name, value) = match rest.split_once('=') {
                    Some((_, "")) => return Err(cut(Diagnostic::expected("value after `=`", span.past()))),
                    Some((name, _)) => {
                        let value_span = Span::new(span.start + 3 + name.len(), span.end);
                        (name, Some(Box::new(value::value(st, value_span, Hint::Any)?)))
                    }
                    None => (rest, None),
                };
                Arg::Flag(Flag { span, name, long: true, value })
            }
            _ if text.starts_with('-')
                && text.len() > 1
                && !is_negative_number_like(text)
                && !text.starts_with("-..") =>
            {
                Arg::Flag(Flag { span, name: &text[1..], long: false, value: None })
            }
            _ if is_spread(text, b"[$({") => {
                let dots = Span::new(span.start, span.start + 3);
                Arg::Spread { dots, expr: value::value(st, Span::new(span.start + 3, span.end), Hint::Any)? }
            }
            _ => Arg::Positional(value::value(st, span, Hint::Any)?),
        };
        args.push(arg);
    }
    Ok(args)
}

/// Parse `^cmd args...`; `first` is the `^cmd` item, already consumed.
fn external_call<'a>(st: St<'_, 'a>, first: Token, mut c: Cursor<'_>) -> PResult<Expr<'a>> {
    let caret = Span::new(first.span.start, first.span.start + 1);
    let head_span = Span::new(first.span.start + 1, first.span.end);
    if head_span.is_empty() {
        return Err(cut(Diagnostic::expected("command name after `^`", head_span)));
    }
    let head = match st.text(head_span).as_bytes()[0] {
        b'$' | b'(' => value::value(st, head_span, Hint::Any)?,
        _ => external_string(st, head_span)?,
    };
    let mut args = Vec::with_capacity(c.rest().len());
    let mut end = first.span;
    while !c.at_end() {
        let tok = c.expect_item("argument")?;
        end = tok.span;
        let text = st.tok(&tok);
        args.push(if is_spread(text, b"[$(") {
            let dots = Span::new(tok.span.start, tok.span.start + 3);
            ExternalArg::Spread {
                dots,
                expr: value::value(st, Span::new(tok.span.start + 3, tok.span.end), Hint::Any)?,
            }
        } else {
            ExternalArg::Regular(external_arg(st, tok.span)?)
        });
    }
    let span = first.span.merge(end);
    Ok(Expr::new(ExprKind::ExternalCall(ExternalCall { caret, head: Box::new(head), args }), span))
}

/// An external argument: `$vars`, `(...)`, `[...]` and `{...}` are parsed,
/// everything else is an external string.
pub fn external_arg<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    match st.text(span).as_bytes()[0] {
        b'$' | b'(' | b'[' | b'{' => value::value(st, span, Hint::Any),
        _ => external_string(st, span),
    }
}

/// The segments of a word passed to an external command.
enum Segment {
    Bare,
    Quote { quote: u8, escaped: bool },
    Backtick,
    Paren { depth: usize },
}

/// A word passed to an external command.
///
/// Following Nushell, the word is split into segments (bare text, quoted
/// strings, backtick strings and parenthesised subexpressions) so that
/// `--query='a (b)'` keeps its parentheses literal while `--out=(pwd)/x`
/// interpolates. All-literal words become one string; otherwise the segments
/// form a bare interpolation.
pub fn external_string<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let text = st.text(span);
    let bytes = text.as_bytes();
    if text.starts_with("r#") {
        return literal::raw_string(st, span);
    }
    if !bytes.iter().any(|b| matches!(b, b'"' | b'\'' | b'(' | b')' | b'`')) {
        return Ok(Expr::new(ExprKind::String(StringLit::bare(text)), span));
    }
    let mut segments: Vec<(usize, usize)> = Vec::new();
    let mut from = 0;
    let mut state = Segment::Bare;
    let mut index = 0;
    while index < bytes.len() {
        let ch = bytes[index];
        match &mut state {
            Segment::Bare => {
                let opener = match ch {
                    b'"' | b'\'' => Some(Segment::Quote { quote: ch, escaped: false }),
                    b'$' if matches!(bytes.get(index + 1), Some(b'"' | b'\'')) => {
                        Some(Segment::Quote { quote: bytes[index + 1], escaped: false })
                    }
                    b'`' => Some(Segment::Backtick),
                    b'(' => Some(Segment::Paren { depth: 1 }),
                    _ => None,
                };
                if let Some(next) = opener {
                    if index != from {
                        segments.push((from, index));
                    }
                    from = index;
                    if ch == b'$' {
                        index += 1;
                    }
                    state = next;
                }
            }
            Segment::Quote { quote, escaped } => {
                if ch == *quote && !*escaped {
                    segments.push((from, index + 1));
                    from = index + 1;
                    state = Segment::Bare;
                } else {
                    *escaped = ch == b'\\' && !*escaped && *quote == b'"';
                }
            }
            Segment::Backtick => {
                if ch == b'`' {
                    segments.push((from, index + 1));
                    from = index + 1;
                    state = Segment::Bare;
                }
            }
            Segment::Paren { depth } => match ch {
                b')' if *depth == 1 => {
                    segments.push((from, index + 1));
                    from = index + 1;
                    state = Segment::Bare;
                }
                b')' => *depth -= 1,
                b'(' => *depth += 1,
                _ => {}
            },
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
        match strings::string(st, seg)?.kind {
            ExprKind::String(lit) => parts.push(InterpPart::Text { span: seg, value: lit.value }),
            ExprKind::Interpolation(inner) => {
                all_text &= inner.parts.iter().all(|p| matches!(p, InterpPart::Text { .. }));
                parts.extend(inner.parts);
            }
            kind => {
                all_text = false;
                parts.push(InterpPart::Expr(Expr::new(kind, seg)));
            }
        }
    }
    let quote = match bytes {
        [b'\'', .., b'\''] => Quote::Single,
        [b'"', .., b'"'] => Quote::Double,
        [b'$', b'"', .., b'"'] if bytes.len() >= 3 => Quote::Double,
        _ => Quote::Bare,
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
                    .collect(),
            ),
        };
        return Ok(Expr::new(ExprKind::String(StringLit { value, quote }), span));
    }
    Ok(Expr::new(ExprKind::Interpolation(Interpolation { quote, parts }), span))
}
