//! Blocks, pipelines and pipeline elements (nu-parser's `parse_pipelines.rs`).

use crate::ast::{
    AttributeBlock, Block, Comment, Expr, Expression, Pipeline, PipelineElement, PipelineRedirection, RedirectionTarget,
};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{ParseResult, cut, into_diagnostic};
use crate::lex::{RedirectionSource, TokenContents};
use crate::span::Span;

use super::WorkingSet;
use super::lite_parser::{
    AfterPipe, LiteCommand, after_pipe, last_non_comment_token, parse_lite_command, skip_to_statement_end,
    take_pipe_on_later_line,
};
use super::parse_calls::{keyword_signature_of_call, parse_attribute};
use super::parse_def::parse_def_predecl;
use super::parse_expressions::{ExpectedShape, Position, parse_builtin_commands, parse_expression, parse_value};
use super::parse_helpers::garbage_pipeline;
use super::tokens::Tokens;

/// The pipelines of a block covering `span` (nu's `parse_block`).
///
/// Errors are recorded in the working set and the offending statement becomes
/// an [`Expr::Garbage`] pipeline, so parsing continues with the next line.
pub fn parse_block<'a>(mut tokens: Tokens<'_, 'a>, span: Span) -> Block<'a> {
    let working_set = tokens.working_set;
    parse_def_predecl(working_set, tokens.all());
    // nu's lite parser: a block whose last token, skipping trailing comment
    // lines, is a `|` has a pipeline with no end (`ls |`, `ls |\n# c`,
    // `alias x = ls |`), whatever absorbed the pipe.
    if last_non_comment_token(tokens.all()) == Some(TokenContents::Pipe)
        && let Some(last) = tokens.all().iter().rev().find(|token| token.contents == TokenContents::Pipe)
    {
        working_set.error(
            Diagnostic::new(ErrorKind::UnexpectedEof("command after `|`"), last.span)
                .with_context("pipeline")
                .with_help("the pipeline has no end: add a command after the `|` or remove it"),
        );
    }
    let mut pipelines: Vec<Pipeline<'a>> = Vec::new();
    let mut pending: Vec<Comment> = Vec::new();
    let mut last = TokenContents::Eol;
    // Set when a pipeline ended with a `|` that a blank line closed: nu's
    // lexer then refuses a `;` before the next item.
    let mut dangling_pipe: Option<Span> = None;
    while let Some(token) = tokens.peek_token() {
        match token.contents {
            TokenContents::Eol => {
                if last == TokenContents::Eol {
                    pending.clear();
                }
                tokens.next_token();
                last = TokenContents::Eol;
            }
            TokenContents::Semicolon => {
                if let Some(pipe) = dangling_pipe {
                    working_set.error(Diagnostic::new(ErrorKind::ExtraTokens, token.span).with_help(format!(
                        "the pipeline is still open after the `|` at {pipe}; add a command or remove the `;`"
                    )));
                }
                if !matches!(last, TokenContents::Eol | TokenContents::Semicolon)
                    && let Some(pipeline) = pipelines.last_mut()
                    && pipeline.terminator.is_none()
                {
                    pipeline.terminator = Some(token.span);
                }
                tokens.next_token();
                last = TokenContents::Semicolon;
            }
            TokenContents::Comment => {
                working_set.add_comment(token.span);
                match pipelines.last_mut() {
                    Some(pipeline) if last != TokenContents::Eol => {
                        pipeline.trailing_comments.push(Comment { span: token.span })
                    }
                    _ => pending.push(Comment { span: token.span }),
                }
                tokens.next_token();
                last = TokenContents::Comment;
            }
            _ => {
                dangling_pipe = None;
                let start = tokens.position();
                let start_span = token.span;
                match parse_pipeline(&mut tokens, std::mem::take(&mut pending)) {
                    Ok((pipeline, dangling)) => {
                        dangling_pipe = dangling;
                        pipelines.extend(pipeline);
                    }
                    Err(error) => {
                        working_set.error(into_diagnostic(error));
                        tokens.reset_to(start);
                        let end = skip_to_statement_end(&mut tokens);
                        pipelines.push(garbage_pipeline(start_span.merge(end)));
                    }
                }
                last = TokenContents::Item;
            }
        }
    }
    Block { span, pipelines }
}

/// One pipeline (nu's `parse_pipeline`): commands separated by `|`. Also
/// returns the span of a trailing `|` that a blank line closed, if any. `None`
/// when there was no command at all (a lone `|` before a blank line).
fn parse_pipeline<'a>(
    tokens: &mut Tokens<'_, 'a>,
    leading_comments: Vec<Comment>,
) -> ParseResult<(Option<Pipeline<'a>>, Option<Span>)> {
    let working_set = tokens.working_set;
    // The lite parse first: collect the commands, then parse them, because a
    // command is parsed differently when it is one element of a longer pipeline.
    let mut lite_commands: Vec<(Option<Span>, LiteCommand)> = Vec::new();
    let mut trailing_comments = Vec::new();
    let mut pipe: Option<Span> = None;
    let mut dangling = None;
    'commands: loop {
        // A pipeline may start with `|` (`( | str join)`) and `a | | b` is
        // `a | b`: the empty commands are dropped.
        while let Some(token) = tokens.peek_token().filter(|token| token.contents == TokenContents::Pipe) {
            let span = token.span;
            tokens.next_token();
            pipe = Some(span);
            if let AfterPipe::Dangling = after_pipe(tokens, span, &mut trailing_comments)? {
                dangling = pipe.take();
                break 'commands;
            }
        }
        if pipe.is_none() && !lite_commands.is_empty() {
            // After a command: the pipeline goes on only through a `|` on a later line.
            if !take_pipe_on_later_line(tokens, &mut trailing_comments)? {
                break;
            }
            continue;
        }
        let lite_command = parse_lite_command(tokens, pipe.is_none())?;
        trailing_comments.extend(lite_command.comments.iter().copied());
        let pipe_after = lite_command.pipe_after;
        lite_commands.push((pipe.take(), lite_command));
        if let Some(span) = pipe_after {
            pipe = Some(span);
            if let AfterPipe::Dangling = after_pipe(tokens, span, &mut trailing_comments)? {
                dangling = pipe.take();
                break;
            }
        }
    }
    if lite_commands.is_empty() {
        // Only pipes (`|` and a blank line): nu drops the empty command.
        return Ok((None, dangling));
    }
    let single = lite_commands.len() == 1;
    let mut elements: Vec<PipelineElement<'a>> = Vec::with_capacity(lite_commands.len());
    for (pipe, lite_command) in &lite_commands {
        let (expr, redirection) = parse_pipeline_element(working_set, lite_command, !single)?;
        let start = pipe.map_or(expr.span.start, |pipe| pipe.start);
        let end = redirection.as_ref().map_or(expr.span.end, |redirection| redirection.span().end.max(expr.span.end));
        elements.push(PipelineElement { span: Span::new(start, end), pipe: *pipe, expr, redirection });
    }
    let span = elements[0].span.merge(elements.last().map_or(elements[0].span, |element| element.span));
    let terminator = match tokens.peek_token() {
        Some(token) if token.contents == TokenContents::Semicolon => {
            tokens.next_token();
            Some(token.span)
        }
        _ => None,
    };
    Ok((Some(Pipeline { span, elements, leading_comments, trailing_comments, terminator }), dangling))
}

/// One command of a pipeline, as collected by the lite parse (nu's
/// `parse_pipeline_element`). `in_pipeline` is set when it is one of several
/// elements; a lone command may be a statement (nu's `parse_builtin_commands`).
fn parse_pipeline_element<'a>(
    working_set: &WorkingSet<'a>,
    lite_command: &LiteCommand,
    in_pipeline: bool,
) -> ParseResult<(Expression<'a>, Option<PipelineRedirection<'a>>)> {
    let position = if in_pipeline { Position::Element } else { Position::Statement };
    let expr = match lite_command.attributes.as_slice() {
        [] => parse_expression(lite_command.tokens(working_set), position)?,
        attribute_lines => {
            let attributes = attribute_lines
                .iter()
                .map(|attribute_line| parse_attribute(working_set, attribute_line))
                .collect::<ParseResult<Vec<_>>>()?;
            let words: Vec<&str> =
                lite_command.parts.iter().take(2).map(|token| working_set.get_span_contents(token.span)).collect();
            let is_definition = matches!(words.as_slice(), ["def" | "extern", ..] | ["export", "def" | "extern"]);
            let item = match (is_definition, lite_command.parts.first()) {
                (true, Some(first)) if in_pipeline => {
                    return Err(cut(Diagnostic::new(
                        ErrorKind::KeywordInPipeline(working_set.get_span_contents(first.span).to_string()),
                        first.span,
                    )));
                }
                (true, _) => parse_builtin_commands(lite_command.tokens(working_set))?,
                (false, Some(first)) => {
                    return Err(cut(Diagnostic::message("attributes must be followed by a definition", first.span)
                        .with_help("only `def`, `extern`, `export def` and `export extern` take attributes")));
                }
                (false, None) => {
                    let last = attributes.last().map_or(Span::point(lite_command.end), |attribute| attribute.span);
                    return Err(cut(Diagnostic::message("attributes must be followed by a definition", last.past())
                        .with_help("put a `def` or `extern` on the line after the attributes")));
                }
            };
            let span = attributes[0].span.merge(item.span);
            Expression::new(Expr::AttributeBlock(AttributeBlock { attributes, item: Box::new(item) }), span)
        }
    };
    let redirection = parse_redirection(working_set, lite_command)?;
    if let Expr::ExportEnv(_) = expr.expr
        && redirection.is_some()
    {
        // nu never looks at a redirection on `export-env`.
        for (operator, target) in &lite_command.redirections {
            working_set.add_ignored(operator.span);
            if let Some(target) = target {
                working_set.add_ignored(target.span);
            }
        }
        return Ok((expr, None));
    }
    if redirection.is_some() && rejects_redirection(&expr) {
        let at = lite_command.redirections.first().map_or(expr.span, |(operator, _)| operator.span);
        return Err(cut(Diagnostic::message("this statement cannot be redirected", at)));
    }
    Ok((expr, redirection))
}

/// The statements nu refuses to redirect (nu's `redirecting_builtin_error`).
fn rejects_redirection(expr: &Expression<'_>) -> bool {
    match &expr.expr {
        Expr::Def(_)
        | Expr::Extern(_)
        | Expr::Let(_)
        | Expr::Mut(_)
        | Expr::Const(_)
        | Expr::For(_)
        | Expr::Alias(_)
        | Expr::Module(_)
        | Expr::Use(_)
        | Expr::Export(_)
        | Expr::ExportEnv(_)
        | Expr::AttributeBlock(_) => true,
        // `overlay <anything>` is refused by name before its arguments are looked at.
        Expr::Call(call) => {
            call.head.name.split(' ').next() == Some("overlay")
                || keyword_signature_of_call(call).is_some_and(|signature| !signature.redirectable)
        }
        _ => false,
    }
}

/// The redirections of a command (nu's `parse_redirection`): one stream, or
/// stdout and stderr separately.
fn parse_redirection<'a>(
    working_set: &WorkingSet<'a>,
    lite_command: &LiteCommand,
) -> ParseResult<Option<PipelineRedirection<'a>>> {
    let mut redirection: Option<PipelineRedirection<'a>> = None;
    for (operator, target) in &lite_command.redirections {
        let target = match target {
            Some(token) => RedirectionTarget::File {
                op: *operator,
                append: operator.item.is_append(),
                path: Box::new(parse_value(working_set, token.span, ExpectedShape::Any)?),
            },
            None => RedirectionTarget::Pipe { op: *operator },
        };
        redirection = Some(match (redirection.take(), operator.item.source()) {
            (None, source) => PipelineRedirection::Single { source, target },
            (
                Some(PipelineRedirection::Single { source: RedirectionSource::Stdout, target: stdout }),
                RedirectionSource::Stderr,
            ) => PipelineRedirection::Separate { out: stdout, err: target },
            (
                Some(PipelineRedirection::Single { source: RedirectionSource::Stderr, target: stderr }),
                RedirectionSource::Stdout,
            ) => PipelineRedirection::Separate { out: target, err: stderr },
            (Some(previous), _) => {
                return Err(cut(Diagnostic::message("multiple redirections of the same stream", operator.span)
                    .with_help(format!("the stream is already redirected at {}", previous.span()))));
            }
        });
    }
    Ok(redirection)
}
