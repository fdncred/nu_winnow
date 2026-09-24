//! Control flow: `if`, `match`, `while`, `loop`, `try`, `return`, `break`
//! and `continue`.
//!
//! nu-parser has no file for these: in nu they are ordinary commands whose
//! signatures take blocks and keywords, parsed by `parse_call`. Here each is a
//! node of its own, parsed from its fixed shape with a [`KeywordCall`], which
//! gives them nu's handling of flags.

use winnow::Parser;
use winnow::combinator::{alt, opt};

use crate::ast::{Else, Expr, Expression, Handler, HandlerKind, If, Loop, Match, Return, Try, While};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{ParseResult, cut};
use crate::lex::Token;

use super::WorkingSet;
use super::parse_expressions::{
    BraceShape, ExpectedShape, Position, brace_shape, parse_block_body, parse_closure_expression, parse_expression,
    parse_match_block_expression, parse_math_expression, parse_value,
};
use super::parse_keywords::{KeywordCall, parse_block_argument};
use super::tokens::{Tokens, expected, keyword, tokens_until};

/// A `{ ... }` where nu accepts a block or any expression (the `else` branch
/// and match arms): a closure or a record parses as that value, the rest is
/// a block.
fn parse_block_or_value<'a>(working_set: &WorkingSet<'a>, token: &Token) -> ParseResult<Expression<'a>> {
    match brace_shape(working_set, token.span)? {
        BraceShape::ClosureParams | BraceShape::Record => parse_value(working_set, token.span, ExpectedShape::Any),
        BraceShape::Empty | BraceShape::Spread | BraceShape::Other => {
            Ok(Expression::new(Expr::Block(parse_block_body(working_set, token.span)?), token.span))
        }
    }
}

/// `if condition... { block } [else { block } | else expression...]`: the
/// condition is every item before the block, which is the item before the
/// `else`, or the last item.
pub fn parse_if<'a>(mut tokens: Tokens<'_, 'a>) -> ParseResult<Expression<'a>> {
    let working_set = tokens.working_set;
    let mut call = KeywordCall::start(&mut tokens)?;
    call.flags(&mut tokens)?;
    if call.wants_help() && tokens.at_end() {
        return call.help_call();
    }
    let then_part = tokens_until("else").parse_next(&mut tokens)?;
    let else_keyword = opt(keyword("else")).parse_next(&mut tokens)?;
    let (condition, block) = match (then_part.all(), else_keyword) {
        ([], Some(else_keyword)) => {
            return Err(cut(Diagnostic::expected("condition and block before `else`", else_keyword.span)));
        }
        ([condition @ .., block], _) if !condition.is_empty() => (then_part.slice(0..condition.len()), block),
        _ if call.wants_help() => return call.help_call(),
        (items, _) => {
            let at = items.first().map_or(call.keyword.span.past(), |token| token.span);
            return Err(cut(Diagnostic::expected("condition", at)));
        }
    };
    let condition = parse_math_expression(condition)?;
    let then_block = parse_block_argument(working_set, block, "block after the condition")?;
    let mut span = call.keyword.span.merge(block.span);
    let else_branch = match else_keyword {
        None => None,
        Some(else_keyword) => {
            let body = match tokens.remaining() {
                [] => {
                    return Err(cut(Diagnostic::expected(
                        "block or expression after `else`",
                        else_keyword.span.past(),
                    )));
                }
                [only] if tokens.text(only).starts_with('{') => parse_block_or_value(working_set, only)?,
                _ => parse_expression(tokens.rest_stream(), Position::Element)?,
            };
            span = span.merge(body.span);
            Some(Else { keyword: else_keyword.span, body: Box::new(body) })
        }
    };
    let if_expression = If { condition: Box::new(condition), then_block, else_branch };
    call.finish(Expression::new(Expr::If(if_expression), span))
}

/// `match value { pattern => body, ... }`.
pub fn parse_match<'a>(mut tokens: Tokens<'_, 'a>) -> ParseResult<Expression<'a>> {
    let working_set = tokens.working_set;
    let mut call = KeywordCall::start(&mut tokens)?;
    let Some(value) = call.positional(&mut tokens, "value to match on")? else { return call.help_call() };
    let value = parse_value(working_set, value.span, ExpectedShape::Any)?;
    let Some(block) = call.positional(&mut tokens, "match block")? else { return call.help_call() };
    // nu decides what the `{ ... }` is before it knows it wants arms: closure
    // parameters or a `key:` make it a closure or a record, which it accepts;
    // a variable or a subexpression in that position is accepted as well.
    let (arms, value_block) = match tokens.text(&block).as_bytes().first() {
        Some(b'{') => match brace_shape(working_set, block.span)? {
            BraceShape::ClosureParams | BraceShape::Record => {
                (Vec::new(), Some(Box::new(parse_value(working_set, block.span, ExpectedShape::Any)?)))
            }
            _ => (parse_match_block_expression(working_set, block.span)?, None),
        },
        Some(b'$' | b'(') => (Vec::new(), Some(Box::new(parse_value(working_set, block.span, ExpectedShape::Any)?))),
        _ => return Err(cut(Diagnostic::expected("match block", block.span))),
    };
    call.end(&mut tokens)?;
    let match_expression = Match { value: Box::new(value), block_span: block.span, arms, value_block };
    let span = call.keyword.span.merge(block.span);
    call.finish(Expression::new(Expr::Match(match_expression), span))
}

/// `while condition... { block }`: the condition is every item before the
/// last, which is the block.
pub fn parse_while<'a>(mut tokens: Tokens<'_, 'a>) -> ParseResult<Expression<'a>> {
    let mut call = KeywordCall::start(&mut tokens)?;
    call.flags(&mut tokens)?;
    let Some((block, condition)) = tokens.remaining().split_last().filter(|(_, condition)| !condition.is_empty())
    else {
        if call.wants_help() {
            return call.help_call();
        }
        return Err(cut(Diagnostic::expected("condition and block", tokens.end_span())));
    };
    let start = tokens.position();
    let condition = parse_math_expression(tokens.slice(start..start + condition.len()))?;
    let body = parse_block_argument(tokens.working_set, block, "block")?;
    let span = call.keyword.span.merge(block.span);
    call.finish(Expression::new(Expr::While(While { condition: Box::new(condition), body }), span))
}

/// `loop { block }`.
pub fn parse_loop<'a>(mut tokens: Tokens<'_, 'a>) -> ParseResult<Expression<'a>> {
    let mut call = KeywordCall::start(&mut tokens)?;
    let Some(block) = call.positional(&mut tokens, "block")? else { return call.help_call() };
    let body = parse_block_argument(tokens.working_set, &block, "block")?;
    call.end(&mut tokens)?;
    let span = call.keyword.span.merge(block.span);
    call.finish(Expression::new(Expr::Loop(Loop { body }), span))
}

/// `try { block } [catch handler] [finally handler]`, the handlers in either
/// order, at most two.
pub fn parse_try<'a>(mut tokens: Tokens<'_, 'a>) -> ParseResult<Expression<'a>> {
    let working_set = tokens.working_set;
    let mut call = KeywordCall::start(&mut tokens)?;
    let Some(block) = call.positional(&mut tokens, "block")? else { return call.help_call() };
    let body = parse_block_argument(working_set, &block, "block")?;
    let mut span = call.keyword.span.merge(block.span);
    let mut handlers = Vec::new();
    loop {
        // `try {} --`: the marker may be the last item.
        call.flags(&mut tokens)?;
        if tokens.at_end() {
            break;
        }
        let handler_keyword =
            expected("`catch` or `finally`", alt((keyword("catch"), keyword("finally")))).parse_next(&mut tokens)?;
        if handlers.len() == 2 {
            return Err(cut(Diagnostic::new(ErrorKind::ExtraTokens, handler_keyword.span)
                .with_help("`try` takes at most two handlers (`catch` and `finally`)")));
        }
        let kind = match tokens.text(&handler_keyword) {
            "catch" => HandlerKind::Catch,
            _ => HandlerKind::Finally,
        };
        let handler = tokens.expect_item("closure")?;
        span = span.merge(handler.span);
        let body = Box::new(parse_try_handler(working_set, &handler)?);
        handlers.push(Handler { kind, keyword: handler_keyword.span, body });
    }
    call.finish(Expression::new(Expr::Try(Try { body, handlers }), span))
}

/// A `catch`/`finally` handler: a closure, or a variable or subexpression
/// that may hold one.
fn parse_try_handler<'a>(working_set: &WorkingSet<'a>, token: &Token) -> ParseResult<Expression<'a>> {
    match working_set.get_span_contents(token.span).as_bytes().first() {
        Some(b'{') if brace_shape(working_set, token.span)? == BraceShape::Record => {
            Err(cut(Diagnostic::expected("closure", token.span).with_help("found a record")))
        }
        Some(b'{') => parse_closure_expression(working_set, token.span),
        Some(b'$' | b'(') => parse_value(working_set, token.span, ExpectedShape::Any),
        _ => Err(cut(Diagnostic::expected("closure", token.span))),
    }
}

/// `return [value]`.
pub fn parse_return<'a>(mut tokens: Tokens<'_, 'a>) -> ParseResult<Expression<'a>> {
    let mut call = KeywordCall::start(&mut tokens)?;
    call.flags(&mut tokens)?;
    let value = match tokens.at_end() {
        true => None,
        false => {
            let value = tokens.expect_item("value")?;
            Some(Box::new(parse_value(tokens.working_set, value.span, ExpectedShape::Any)?))
        }
    };
    call.end(&mut tokens)?;
    let span = value.as_ref().map_or(call.keyword.span, |value| call.keyword.span.merge(value.span));
    call.finish(Expression::new(Expr::Return(Return { value }), span))
}

/// `break` or `continue`, which take no arguments.
pub fn parse_break_or_continue<'a>(mut tokens: Tokens<'_, 'a>, expr: Expr<'a>) -> ParseResult<Expression<'a>> {
    let mut call = KeywordCall::start(&mut tokens)?;
    call.end(&mut tokens)?;
    let span = call.keyword.span;
    call.finish(Expression::new(expr, span))
}
