//! `def`, `extern`, `for`, attribute blocks and the predeclaration of
//! commands (nu-parser's `parse_def.rs`).

use std::borrow::Cow;

use winnow::Parser;
use winnow::combinator::repeat;

use crate::ast::{Block, Def, DefFlag, Expr, Expression, Extern, For, Signature};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{ParseResult, cut};
use crate::lex::{Token, TokenContents};
use crate::span::{Span, Spanned};

use super::WorkingSet;
use super::parse_expressions::{
    BraceShape, ExpectedShape, brace_shape, parse_block_body_unchecked, parse_closure_parts, parse_value,
};
use super::parse_helpers::is_help_flag;
use super::parse_keywords::{KeywordCall, is_parser_keyword, parse_block_argument, seen_end_of_options};
use super::parse_literals::{parse_filesize, parse_float, parse_int};
use super::parse_shape_specs::parse_type;
use super::parse_signatures::{parse_definition_name, parse_full_signature, parse_var_with_opt_type};
use super::tokens::{Tokens, item, tokens_until};

/// Declare the names of the `def`/`extern`/`alias` statements of a block
/// before parsing it (nu's `parse_def_predecl`, which nu's `parse_block` calls
/// for every statement first), so calls to multi-word commands defined later
/// resolve. Like nu, a `def` or `extern` name declared twice in one block is
/// an error.
pub fn parse_def_predecl(working_set: &WorkingSet<'_>, tokens: &[Token]) {
    let mut declared: Vec<&str> = Vec::new();
    let mut at_line_start = true;
    let mut index = 0;
    while let Some(token) = tokens.get(index) {
        index += 1;
        match token.contents {
            TokenContents::Eol | TokenContents::Semicolon => at_line_start = true,
            TokenContents::Comment => {}
            TokenContents::Item if at_line_start => {
                at_line_start = false;
                let mut words = tokens[index..]
                    .iter()
                    .take_while(|token| token.contents == TokenContents::Item)
                    .map(|token| (working_set.get_span_contents(token.span), token.span));
                let head = match working_set.get_span_contents(token.span) {
                    "export" => words.next().map_or("", |(word, _)| word),
                    head => head,
                };
                if !matches!(head, "def" | "extern" | "alias") {
                    continue;
                }
                let Some((name, name_span)) = words.find(|(word, _)| !word.starts_with('-')) else { continue };
                let name = name.trim_matches(['"', '\'', '`']);
                if name.is_empty() {
                    continue;
                }
                working_set.add_predecl(name);
                // nu predeclares a definition only when a signature item follows the name.
                let has_signature = head != "alias" && words.any(|(word, _)| word.starts_with(['[', '(']));
                if !has_signature {
                    continue;
                }
                if declared.contains(&name) {
                    working_set.error(
                        Diagnostic::message("duplicate command definition within a block", name_span)
                            .with_help(format!("`{name}` is already defined in this block")),
                    );
                }
                declared.push(name);
            }
            _ => at_line_start = false,
        }
    }
}

/// Reject a `def`/`extern`/`alias` name that is a parser keyword, or that
/// nu refuses because it could never be called: one containing `#`, `^` or
/// `%`, or one that reads as a number or a filesize (`def 1kb`).
pub fn check_definition_name(name: &Spanned<Cow<'_, str>>, what: &str) -> ParseResult<()> {
    if is_parser_keyword(&name.item) {
        return Err(cut(Diagnostic::message(
            format!("cannot use parser keyword `{}` as {what} name", name.item),
            name.span,
        )
        .with_help("choose a different name; this word is parsed specially by Nushell")));
    }
    let text: &str = &name.item;
    if text.contains(['#', '^', '%'])
        || parse_int(text).is_some()
        || parse_float(text).is_some()
        || parse_filesize(text).is_some_and(|filesize| filesize.is_ok())
    {
        return Err(cut(Diagnostic::message(format!("{what} name not supported"), name.span)
            .with_help("a name may not contain `#`, `^` or `%`, or read as a number or filesize")));
    }
    Ok(())
}

/// The name item of a `def`/`extern`: a string literal. Like nu, a name
/// containing `[` or `(` (even quoted) is "no space between name and
/// parameters", and a `$` item is not a string.
fn parse_def_name<'a>(working_set: &WorkingSet<'a>, token: &Token) -> ParseResult<Spanned<Cow<'a, str>>> {
    let text = working_set.get_span_contents(token.span);
    if let Some(at) = text.find(['[', '(']) {
        let span = Span::point(token.span.start + at);
        return Err(cut(Diagnostic::message("no space between name and parameters", span)
            .with_help("consider adding a space between the command's name and its parameters")));
    }
    if text.starts_with('$') {
        return Err(cut(Diagnostic::expected("string", token.span).with_help("the name of a definition is a string")));
    }
    parse_definition_name(working_set, token.span)
}

/// `def [--env] [--wrapped] name signature [: input/output types] { body }`;
/// nu also accepts the flags after the name.
pub fn parse_def<'a>(mut tokens: Tokens<'_, 'a>) -> ParseResult<Expression<'a>> {
    let working_set = tokens.working_set;
    let mut call = KeywordCall::start(&mut tokens)?;
    let mut flags = parse_def_flags(&mut tokens)?;
    let Some(name) = call.positional(&mut tokens, "command name")? else { return call.help_call() };
    let name = parse_def_name(working_set, &name)?;
    check_definition_name(&name, "command")?;
    flags.extend(parse_def_flags(&mut tokens)?);
    call.flags(&mut tokens)?;
    // The signature positional takes every remaining item but the last, which
    // is the body's: `def foo [] {} --help` drops the `{}` (two-item form of
    // `parse_full_signature`) and `def foo [] {} {} --help` has no colon.
    let (signature_items, body) = match tokens.remaining() {
        [] if call.wants_help() => return call.help_call(),
        [] => return Err(cut(Diagnostic::expected("signature", tokens.end_span()))),
        [only] => (std::slice::from_ref(only), None),
        [signature_items @ .., body] => (signature_items, Some(body)),
    };
    let signature = parse_full_signature(working_set, signature_items, false)?;
    let (body_params, body_block) = match body {
        Some(token) if is_help_flag(working_set, token) && !seen_end_of_options(&tokens) => return call.help_call(),
        Some(token) => parse_def_body(working_set, token, "definition body closure { ... }")?,
        None if call.wants_help() => return call.help_call(),
        None => return Err(cut(Diagnostic::expected("block", signature_items[0].span.past()))),
    };
    if flags.iter().any(|flag| flag.item == DefFlag::Wrapped) {
        check_wrapped_signature(&signature, name.span)?;
    }
    let span = call.keyword.span.merge(body.map_or(signature.span, |token| token.span));
    let def = Def { flags, name, signature, body_params, body: body_block };
    call.finish(Expression::new(Expr::Def(def), span))
}

/// The body of a `def`: nu parses it as a closure without looking at its
/// shape first (`def f [] {a: 1}` calls `a:`), and drops its parameters.
fn parse_def_body<'a>(
    working_set: &WorkingSet<'a>,
    token: &Token,
    what: &'static str,
) -> ParseResult<(Option<Signature<'a>>, Block<'a>)> {
    if token.contents != TokenContents::Item || !working_set.get_span_contents(token.span).starts_with('{') {
        return Err(cut(Diagnostic::expected(what, token.span)));
    }
    match brace_shape(working_set, token.span)? {
        BraceShape::ClosureParams => {
            let closure = parse_closure_parts(working_set, token.span)?;
            Ok((closure.params, closure.body))
        }
        _ => Ok((None, parse_block_body_unchecked(working_set, token.span)?)),
    }
}

/// `def --wrapped` needs a rest parameter that is untyped or a `string`.
fn check_wrapped_signature(signature: &Signature<'_>, name_span: Span) -> ParseResult<()> {
    let rest = signature.params.iter().find(|p| matches!(p.kind, crate::ast::ParameterKind::Rest));
    let Some(rest) = rest else {
        return Err(cut(Diagnostic::message("missing required positional argument", name_span).with_help(
            "def --wrapped must have a ...rest-like positional argument; add `...rest: string` to the signature",
        )));
    };
    match &rest.ty {
        None => Ok(()),
        Some(ty) if ty.shape == crate::ast::SyntaxShape::String => Ok(()),
        Some(ty) => Err(cut(Diagnostic::message("type mismatch", ty.span).with_help(format!(
            "the ...rest-like positional argument of `def --wrapped` supports only strings; change the type of ...{} to `string`",
            rest.name.item
        )))),
    }
}

/// The `--env` and `--wrapped` flags of a `def`, up to a `--help`, a `--` or
/// an item that is not a long flag.
fn parse_def_flags(tokens: &mut Tokens<'_, '_>) -> ParseResult<Vec<Spanned<DefFlag>>> {
    repeat(0.., def_flag).parse_next(tokens)
}

fn def_flag(tokens: &mut Tokens<'_, '_>) -> ParseResult<Spanned<DefFlag>> {
    let working_set = tokens.working_set;
    let flag = item
        .verify(|token| {
            let text = working_set.get_span_contents(token.span);
            text.starts_with("--") && !matches!(text, "--help" | "--")
        })
        .parse_next(tokens)?;
    match tokens.text(&flag) {
        "--env" => Ok(Spanned::new(DefFlag::Env, flag.span)),
        "--wrapped" => Ok(Spanned::new(DefFlag::Wrapped, flag.span)),
        other => Err(cut(Diagnostic::message(format!("the `def` command doesn't have flag `{other}`"), flag.span)
            .with_help("`def` accepts `--env` and `--wrapped`"))),
    }
}

/// `extern name signature [: input/output types]`.
pub fn parse_extern<'a>(mut tokens: Tokens<'_, 'a>) -> ParseResult<Expression<'a>> {
    let working_set = tokens.working_set;
    let mut call = KeywordCall::start(&mut tokens)?;
    let Some(name) = call.positional(&mut tokens, "command name")? else { return call.help_call() };
    let name = parse_def_name(working_set, &name)?;
    check_definition_name(&name, "command")?;
    call.flags(&mut tokens)?;
    let rest = tokens.remaining();
    let Some(last) = rest.last() else {
        if call.wants_help() {
            return call.help_call();
        }
        return Err(cut(Diagnostic::expected("signature", tokens.end_span())));
    };
    // The signature argument takes every remaining item, so a body after it
    // (the old `extern-wrapped`) is dropped by nu without a look.
    let signature = parse_full_signature(working_set, rest, true)?;
    let span = call.keyword.span.merge(last.span);
    call.finish(Expression::new(Expr::Extern(Extern { name, signature }), span))
}

/// `for variable[: type] in iterable { block }`.
pub fn parse_for<'a>(mut tokens: Tokens<'_, 'a>) -> ParseResult<Expression<'a>> {
    let working_set = tokens.working_set;
    let mut call = KeywordCall::start(&mut tokens)?;
    let Some(variable) = call.positional(&mut tokens, "loop variable")? else { return call.help_call() };
    let (var, typed) = parse_var_with_opt_type(working_set, &variable)?;
    // The variable positional runs up to the `in` keyword, so a type after
    // `x:` is every item before it (`record<a: int, b: string>` is four items)
    // and `for x: int --help in [] {}` has the unknown type `int --help`.
    let ty = match typed {
        true => {
            let Some(type_span) = tokens_until("in").parse_next(&mut tokens)?.span() else {
                return Err(cut(Diagnostic::expected("type", variable.span.past())));
            };
            Some(parse_type(working_set, type_span)?)
        }
        false => None,
    };
    let Some(in_keyword) = call.positional(&mut tokens, "`in`")? else { return call.help_call() };
    if tokens.text(&in_keyword) != "in" {
        return Err(cut(Diagnostic::new(ErrorKind::ExpectedKeyword("in"), in_keyword.span)));
    }
    // nu reserves the last item for the block before it parses the keyword's
    // argument, so `for x in []` and `for x --help in []` lack the argument of
    // `in` (KeywordMissingArgument), help or not.
    if tokens.remaining().len() < 2 {
        return Err(cut(Diagnostic::message("missing argument to `in`", in_keyword.span)
            .with_help("`for` needs a value to iterate and a block: `for x in [1 2] { }`")));
    }
    let iterable = tokens.expect_item("value to iterate")?;
    let iterable = Box::new(parse_value(working_set, iterable.span, ExpectedShape::Any)?);
    let Some(block) = call.positional(&mut tokens, "block")? else { return call.help_call() };
    let body = parse_block_argument(working_set, &block, "block")?;
    call.end(&mut tokens)?;
    let span = call.keyword.span.merge(block.span);
    call.finish(Expression::new(Expr::For(For { var, ty, in_keyword: in_keyword.span, iterable, body }), span))
}
