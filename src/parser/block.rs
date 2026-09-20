//! Grouping tokens into pipelines and commands.
//!
//! This is the "lite" parse: it decides where statements start and end,
//! attaches comments, absorbs the rest of a line after an assignment operator,
//! collects redirections and attribute lines, and recovers from errors at
//! statement boundaries.

use winnow::stream::Stream;

use crate::ast::{Block, Comment, Expr, ExprKind, Pipeline, PipelineElement};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut, into_diagnostic};
use crate::lexer::{RedirectOp, Token, TokenKind};
use crate::span::{Span, Spanned};

use super::expr::{Toks, peek_token, toks};
use super::{St, statement};

/// One command as grouped by the lite parse, before its items are interpreted.
#[derive(Debug, Default)]
pub struct RawCommand {
    /// The items (and, after an assignment operator, everything to the end of
    /// the line), terminated by an `Eof` token.
    pub parts: Vec<Token>,
    /// Preceding `@attribute` lines, each terminated by an `Eof` token.
    pub attributes: Vec<Vec<Token>>,
    /// Redirections and their file targets (`None` for `e>|`).
    pub redirections: Vec<(Spanned<RedirectOp>, Option<Token>)>,
    /// The span of the `|` (or `e>|`) that ended the command, if any.
    pub pipe_after: Option<Span>,
    /// Comments found between the command's tokens.
    pub comments: Vec<Comment>,
}

/// Parse a lexed token stream (ending in `Eof`) into a block covering `span`.
///
/// Errors are recorded in the shared state and the offending statement becomes
/// a [`ExprKind::Garbage`] pipeline, so parsing continues with the next line.
pub fn parse_block_tokens<'a>(st: St<'_, 'a>, tokens: &[Token], span: Span) -> Block<'a> {
    predeclare(st, tokens);
    let mut i = toks(st, tokens);
    let mut pipelines: Vec<Pipeline<'a>> = Vec::new();
    let mut pending: Vec<Comment> = Vec::new();
    let mut last = TokenKind::Eol;
    while let Some(tok) = peek_token(&i) {
        match tok.kind {
            TokenKind::Eof => break,
            TokenKind::Eol => {
                if last == TokenKind::Eol {
                    pending.clear();
                }
                i.next_token();
                last = TokenKind::Eol;
            }
            TokenKind::Semicolon => {
                if last != TokenKind::Eol
                    && last != TokenKind::Semicolon
                    && let Some(p) = pipelines.last_mut()
                    && p.terminator.is_none()
                {
                    p.terminator = Some(tok.span);
                }
                i.next_token();
                last = TokenKind::Semicolon;
            }
            TokenKind::Comment => {
                st.comment(tok.span);
                if last == TokenKind::Eol {
                    pending.push(Comment { span: tok.span });
                } else if let Some(p) = pipelines.last_mut() {
                    p.trailing_comments.push(Comment { span: tok.span });
                } else {
                    pending.push(Comment { span: tok.span });
                }
                last = TokenKind::Comment;
                i.next_token();
            }
            _ => {
                let start = i.checkpoint();
                let start_span = tok.span;
                match pipeline(&mut i, std::mem::take(&mut pending)) {
                    Ok(p) => pipelines.push(p),
                    Err(e) => {
                        st.error(into_diagnostic(e));
                        i.reset(&start);
                        let end = skip_statement(&mut i);
                        pipelines.push(garbage_pipeline(start_span.merge(end)));
                    }
                }
                last = TokenKind::Item;
            }
        }
    }
    Block { span, pipelines }
}

/// Skip tokens up to (not including) the next `Eol`/`Semicolon`/`Eof`,
/// returning the span of the last token skipped.
fn skip_statement(i: &mut Toks<'_, '_, '_>) -> Span {
    let mut end = None;
    while let Some(t) = peek_token(i) {
        if matches!(t.kind, TokenKind::Eol | TokenKind::Semicolon | TokenKind::Eof) {
            break;
        }
        end = Some(t.span);
        i.next_token();
    }
    end.unwrap_or_else(|| peek_token(i).map_or(Span::point(0), |t| Span::point(t.span.start)))
}

fn garbage_pipeline<'a>(span: Span) -> Pipeline<'a> {
    Pipeline {
        span,
        elements: vec![PipelineElement {
            span,
            pipe: None,
            expr: Expr::new(ExprKind::Garbage, span),
            redirection: None,
        }],
        leading_comments: Vec::new(),
        trailing_comments: Vec::new(),
        terminator: None,
    }
}

/// Declare the names of `def`/`extern`/`alias` statements in this block before
/// parsing it, so calls to multi-word commands defined later resolve.
fn predeclare(st: St<'_, '_>, tokens: &[Token]) {
    let mut at_start = true;
    let mut idx = 0;
    while idx < tokens.len() {
        let tok = &tokens[idx];
        match tok.kind {
            TokenKind::Eol | TokenKind::Semicolon => at_start = true,
            TokenKind::Item if at_start => {
                at_start = false;
                let mut k = idx;
                let mut head = st.tok(tok);
                if head == "export" {
                    k += 1;
                    match tokens.get(k) {
                        Some(t) if t.kind == TokenKind::Item => head = st.tok(t),
                        _ => continue,
                    }
                }
                if matches!(head, "def" | "extern" | "alias") {
                    k += 1;
                    while let Some(t) = tokens.get(k) {
                        if t.kind != TokenKind::Item {
                            break;
                        }
                        let text = st.tok(t);
                        if text.starts_with("--") {
                            k += 1;
                            continue;
                        }
                        let name = text.trim_matches(|c| c == '"' || c == '\'' || c == '`');
                        if !name.is_empty() {
                            st.declare_command(name);
                        }
                        break;
                    }
                }
            }
            TokenKind::Comment | TokenKind::Eof => {}
            _ => at_start = false,
        }
        idx += 1;
    }
}

/// Parse one pipeline: commands separated by `|`.
fn pipeline<'a>(i: &mut Toks<'_, '_, 'a>, leading_comments: Vec<Comment>) -> PResult<Pipeline<'a>> {
    let st = i.state;
    let mut elements: Vec<PipelineElement<'a>> = Vec::new();
    let mut trailing_comments = Vec::new();
    let mut pipe: Option<Span> = None;
    // A pipeline may start with `|` (`( | str join)`): the empty first command is dropped.
    skip_pipe_continuation(i, &mut pipe, &mut trailing_comments);
    loop {
        if let Some(t) = peek_token(i)
            && matches!(t.kind, TokenKind::Eof | TokenKind::Semicolon | TokenKind::Eol)
            && let Some(pipe_span) = pipe
        {
            return Err(cut(
                Diagnostic::new(ErrorKind::UnexpectedEof("command after `|`"), pipe_span).with_context("pipeline")
            ));
        }
        let raw = raw_command(i)?;
        trailing_comments.extend(raw.comments.iter().copied());
        let (expr, redirection) = statement::parse_command(st, &raw)?;
        let start = pipe.map_or(expr.span.start, |p| p.start);
        let end = redirection.as_ref().map_or(expr.span.end, |r| r.span().end.max(expr.span.end));
        elements.push(PipelineElement { span: Span::new(start, end), pipe, expr, redirection });
        let Some(pipe_span) = raw.pipe_after else { break };
        pipe = Some(pipe_span);
        skip_pipe_continuation(i, &mut pipe, &mut trailing_comments);
    }
    let span = elements[0].span.merge(elements.last().unwrap().span);
    let terminator = match peek_token(i) {
        Some(t) if t.kind == TokenKind::Semicolon => {
            let s = t.span;
            i.next_token();
            Some(s)
        }
        _ => None,
    };
    Ok(Pipeline { span, elements, leading_comments, trailing_comments, terminator })
}

/// After a `|`: `a |\n  b`, `a | # comment\n  b` and `a | | b` all continue
/// the pipeline. Consumes newlines, comments and extra pipes.
fn skip_pipe_continuation(i: &mut Toks<'_, '_, '_>, pipe: &mut Option<Span>, comments: &mut Vec<Comment>) {
    let st = i.state;
    while let Some(t) = peek_token(i) {
        match t.kind {
            TokenKind::Eol if pipe.is_some() => {
                i.next_token();
            }
            TokenKind::Comment if pipe.is_some() => {
                st.comment(t.span);
                comments.push(Comment { span: t.span });
                i.next_token();
            }
            TokenKind::Pipe => {
                *pipe = Some(t.span);
                i.next_token();
            }
            _ => break,
        }
    }
}

fn is_attribute_start(st: St<'_, '_>, tok: &Token) -> bool {
    tok.kind == TokenKind::Item && st.tok(tok).starts_with('@')
}

/// Collect the tokens of one command.
fn raw_command<'a>(i: &mut Toks<'_, '_, 'a>) -> PResult<RawCommand> {
    let st = i.state;
    let mut raw = RawCommand::default();
    // Attribute lines: `@name args` up to the end of the line, repeated.
    while let Some(t) = peek_token(i).filter(|t| is_attribute_start(st, t)) {
        let _ = t;
        let mut attr = Vec::new();
        while let Some(t) = peek_token(i) {
            match t.kind {
                TokenKind::Item => {
                    attr.push(*t);
                    i.next_token();
                }
                TokenKind::Comment => {
                    st.comment(t.span);
                    raw.comments.push(Comment { span: t.span });
                    i.next_token();
                }
                TokenKind::Eol | TokenKind::Semicolon => {
                    i.next_token();
                    break;
                }
                TokenKind::Eof => break,
                _ => {
                    return Err(cut(Diagnostic::message(
                        "attributes cannot contain pipelines or redirections",
                        t.span,
                    )
                    .with_context("attribute")));
                }
            }
        }
        raw.attributes.push(super::expr::with_eof(&attr));
        // Blank lines and comment lines may separate attributes from the definition.
        while let Some(t) = peek_token(i) {
            match t.kind {
                TokenKind::Eol => {
                    i.next_token();
                }
                TokenKind::Comment => {
                    st.comment(t.span);
                    raw.comments.push(Comment { span: t.span });
                    i.next_token();
                }
                _ => break,
            }
        }
    }
    let mut absorbing = false;
    while let Some(t) = peek_token(i) {
        let t = *t;
        match t.kind {
            TokenKind::Item => {
                raw.parts.push(t);
                i.next_token();
            }
            TokenKind::Assign(_) => {
                raw.parts.push(t);
                absorbing = true;
                i.next_token();
            }
            TokenKind::Pipe if absorbing => {
                raw.parts.push(t);
                i.next_token();
            }
            TokenKind::Redirect(_) if absorbing => {
                raw.parts.push(t);
                i.next_token();
            }
            TokenKind::PipePipe if absorbing => {
                raw.parts.push(t);
                i.next_token();
            }
            TokenKind::Eol if absorbing => {
                // A pipe at the end of the line continues the assignment's pipeline.
                let continues = raw
                    .parts
                    .iter()
                    .rev()
                    .find(|p| p.kind != TokenKind::Comment)
                    .is_some_and(|p| p.kind == TokenKind::Pipe);
                if continues {
                    i.next_token();
                } else {
                    break;
                }
            }
            TokenKind::Comment => {
                st.comment(t.span);
                raw.comments.push(Comment { span: t.span });
                i.next_token();
            }
            TokenKind::Redirect(op) => {
                i.next_token();
                if raw.parts.is_empty() {
                    return Err(cut(Diagnostic::message("unexpected redirection: nothing to redirect", t.span)));
                }
                if op.is_pipe() {
                    raw.redirections.push((Spanned::new(op, t.span), None));
                    raw.pipe_after = Some(t.span);
                    break;
                }
                let Some(target) = peek_token(i).filter(|n| n.kind == TokenKind::Item).copied() else {
                    let at = peek_token(i).map_or(t.span.past(), |n| n.span);
                    return Err(cut(Diagnostic::expected("redirection target", at)));
                };
                i.next_token();
                raw.redirections.push((Spanned::new(op, t.span), Some(target)));
            }
            TokenKind::Pipe => {
                raw.pipe_after = Some(t.span);
                i.next_token();
                break;
            }
            TokenKind::PipePipe => {
                return Err(cut(Diagnostic::new(ErrorKind::ShellSyntax { found: "||", use_instead: "or" }, t.span)
                    .with_help("use `or` for boolean logic, or `try { } catch { }` to run a fallback command")));
            }
            TokenKind::Eol | TokenKind::Semicolon | TokenKind::Eof => break,
        }
    }
    if raw.parts.is_empty() && raw.attributes.is_empty() && raw.redirections.is_empty() {
        let at = peek_token(i).map_or(Span::point(0), |t| t.span);
        return Err(cut(Diagnostic::expected("command", at)));
    }
    let end = raw
        .parts
        .last()
        .map_or_else(|| peek_token(i).map_or(Span::point(0), |t| Span::point(t.span.start)), |t| t.span.past());
    raw.parts.push(Token { kind: TokenKind::Eof, span: end });
    Ok(raw)
}
