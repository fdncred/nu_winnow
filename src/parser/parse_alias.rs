//! `alias` (nu-parser's `parse_alias.rs`).

use crate::ast::{Alias, Expr, Expression};
use crate::error::Diagnostic;
use crate::input::{ParseResult, cut};
use crate::lex::{AssignmentOperator, Token, TokenContents};

use super::parse_calls::parse_call_lenient;
use super::parse_def::check_definition_name;
use super::parse_expressions::is_math_expression_like;
use super::parse_keywords::{
    ALIASABLE_PARSER_KEYWORDS, KeywordBoundary, UNALIASABLE_PARSER_KEYWORDS, keyword_boundary_with, parse_help_call,
};
use super::parse_signatures::parse_definition_name;
use super::tokens::Tokens;

/// `alias name = target` (nu's `parse_alias`); `exported` for `export alias`,
/// which nu lets go
/// without a target (`export alias x =`) because its length check counts the
/// `export` word.
pub fn parse_alias<'a>(mut tokens: Tokens<'_, 'a>, exported: bool) -> ParseResult<Expression<'a>> {
    let working_set = tokens.working_set;
    let statement = tokens;
    let keyword = tokens.expect_item("alias")?;
    // nu parses the alias call first and returns it when it is a help call;
    // otherwise the `=` must sit right after the name, so `alias --help x =
    // ls` and `alias x --help extra` are "missing sign" and `--` is a name.
    if let KeywordBoundary::Help = keyword_boundary_with(&mut tokens, "alias", &[], false)? {
        tokens.next_token();
        return if tokens.at_end() {
            parse_help_call(statement)
        } else {
            Err(cut(Diagnostic::expected("`=`", tokens.here())))
        };
    }
    let name_token = tokens.expect_item("alias name")?;
    if working_set.get_span_contents(name_token.span).starts_with('-') {
        return Err(cut(Diagnostic::message("alias name not supported", name_token.span)
            .with_help("a bare alias name cannot start with `-`; quote it")));
    }
    let name = parse_definition_name(working_set, name_token.span)?;
    // `alias alias --help` is a help call before the name is checked.
    if let KeywordBoundary::Help = keyword_boundary_with(&mut tokens, "alias", &[], false)? {
        tokens.next_token();
        return if tokens.at_end() {
            parse_help_call(statement)
        } else {
            Err(cut(Diagnostic::expected("`=`", tokens.here())))
        };
    }
    check_definition_name(&name, "alias")?;
    let equals = match tokens.next_token() {
        Some(token) if token.contents == TokenContents::AssignmentOperator(AssignmentOperator::Assign) => *token,
        _ => return Err(cut(Diagnostic::expected("`=`", tokens.here()))),
    };
    // Nushell hands everything after `=` to the call parser as plain words,
    // so `alias ll = ls | length` is `ls` with the arguments `|` and `length`,
    // and `alias x = FOO=1 ls` calls the external command `FOO=1`.
    let words: Vec<Token> =
        tokens.remaining().iter().map(|token| Token { contents: TokenContents::Item, span: token.span }).collect();
    let Some(first) = words.first() else {
        if exported {
            let alias = Alias { name, eq: equals.span, value: None };
            return Ok(Expression::new(Expr::Alias(alias), keyword.span.merge(equals.span)));
        }
        return Err(cut(Diagnostic::expected("command after `=`", equals.span.past())));
    };
    let first_text = working_set.get_span_contents(first.span);
    if !matches!(first_text, "if" | "match") && is_math_expression_like(first_text) {
        return Err(cut(Diagnostic::message("cannot create an alias to an expression", first.span)
            .with_help("an alias names a command and its arguments, such as `alias ll = ls -l`")));
    }
    // Like nu, only the aliasable keywords (`if`, `match`, `try`, `overlay ...`)
    // may be aliased; `alias d = def` is an error.
    let target: String =
        words.iter().take(2).map(|token| working_set.get_span_contents(token.span)).collect::<Vec<_>>().join(" ");
    let single = first_text;
    if UNALIASABLE_PARSER_KEYWORDS.contains(&target.as_str()) || UNALIASABLE_PARSER_KEYWORDS.contains(&single) {
        return Err(cut(Diagnostic::message("cannot create an alias to a parser keyword", first.span)
            .with_help(format!("only {} can be aliased", ALIASABLE_PARSER_KEYWORDS.join(", ")))));
    }
    // nu forgives missing positionals and flag values in an alias target
    // (`alias x = overlay new`), but not unknown flags.
    let value = parse_call_lenient(Tokens::new(working_set, &words, tokens.end_span().start), true)?;
    let span = keyword.span.merge(value.span);
    Ok(Expression::new(Expr::Alias(Alias { name, eq: equals.span, value: Some(Box::new(value)) }), span))
}
