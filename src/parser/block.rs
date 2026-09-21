//! Grouping tokens into pipelines and commands.
//!
//! This is the "lite" parse: it decides where statements start and end,
//! attaches comments, absorbs the rest of a line after an assignment operator,
//! collects redirections and attribute lines, and recovers from errors at
//! statement boundaries. It never looks inside an item.

use crate::ast::{Block, Comment, Expr, ExprKind, Pipeline, PipelineElement};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut, into_diagnostic};
use crate::lexer::{RedirectOp, Token, TokenKind};
use crate::span::{Span, Spanned};

use super::cursor::Cursor;
use super::{St, statement};

/// One command as grouped by the lite parse, before its items are interpreted.
#[derive(Debug, Default)]
pub struct RawCommand {
    /// The items and, after an assignment operator, everything to the end of the line.
    pub parts: Vec<Token>,
    /// Byte offset just past the last part.
    pub end: usize,
    /// Preceding `@attribute` lines.
    pub attributes: Vec<Vec<Token>>,
    /// Redirections and their file targets (`None` for `e>|`).
    pub redirections: Vec<(Spanned<RedirectOp>, Option<Token>)>,
    /// The span of the `|` (or `e>|`) that ended the command, if any.
    pub pipe_after: Option<Span>,
    /// Comments found between the command's tokens.
    pub comments: Vec<Comment>,
}

impl RawCommand {
    /// A cursor over the parts.
    pub fn cursor(&self) -> Cursor<'_> {
        Cursor::new(&self.parts, self.end)
    }
}

/// Parse tokens into a block covering `span`.
///
/// Errors are recorded in the shared state and the offending statement becomes
/// a [`ExprKind::Garbage`] pipeline, so parsing continues with the next line.
pub fn parse_block<'a>(st: St<'_, 'a>, mut c: Cursor<'_>, span: Span) -> Block<'a> {
    predeclare(st, c.all());
    let mut pipelines: Vec<Pipeline<'a>> = Vec::new();
    let mut pending: Vec<Comment> = Vec::new();
    let mut last = TokenKind::Eol;
    while let Some(tok) = c.peek() {
        match tok.kind {
            TokenKind::Eol => {
                if last == TokenKind::Eol {
                    pending.clear();
                }
                c.next();
                last = TokenKind::Eol;
            }
            TokenKind::Semicolon => {
                if !matches!(last, TokenKind::Eol | TokenKind::Semicolon)
                    && let Some(p) = pipelines.last_mut()
                    && p.terminator.is_none()
                {
                    p.terminator = Some(tok.span);
                }
                c.next();
                last = TokenKind::Semicolon;
            }
            TokenKind::Comment => {
                st.comment(tok.span);
                match pipelines.last_mut() {
                    Some(p) if last != TokenKind::Eol => p.trailing_comments.push(Comment { span: tok.span }),
                    _ => pending.push(Comment { span: tok.span }),
                }
                c.next();
                last = TokenKind::Comment;
            }
            _ => {
                let start = c.position();
                let start_span = tok.span;
                match pipeline(st, &mut c, std::mem::take(&mut pending)) {
                    Ok(p) => pipelines.push(p),
                    Err(e) => {
                        st.error(into_diagnostic(e));
                        c.reset(start);
                        let end = skip_statement(&mut c);
                        pipelines.push(garbage_pipeline(start_span.merge(end)));
                    }
                }
                last = TokenKind::Item;
            }
        }
    }
    Block { span, pipelines }
}

/// Skip tokens up to (not including) the next `Eol`/`;`, returning the span
/// of the last token skipped.
fn skip_statement(c: &mut Cursor<'_>) -> Span {
    let mut end = None;
    while let Some(tok) = c.peek() {
        if matches!(tok.kind, TokenKind::Eol | TokenKind::Semicolon) {
            break;
        }
        end = Some(tok.span);
        c.next();
    }
    end.unwrap_or_else(|| Span::point(c.here().start))
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
    let mut at_line_start = true;
    let mut idx = 0;
    while let Some(tok) = tokens.get(idx) {
        idx += 1;
        match tok.kind {
            TokenKind::Eol | TokenKind::Semicolon => at_line_start = true,
            TokenKind::Comment => {}
            TokenKind::Item if at_line_start => {
                at_line_start = false;
                let mut words = tokens[idx..].iter().take_while(|t| t.kind == TokenKind::Item).map(|t| st.tok(t));
                let head = match st.tok(tok) {
                    "export" => words.next().unwrap_or(""),
                    head => head,
                };
                if matches!(head, "def" | "extern" | "alias")
                    && let Some(name) = words.find(|w| !w.starts_with("--"))
                {
                    let name = name.trim_matches(['"', '\'', '`']);
                    if !name.is_empty() {
                        st.declare_command(name);
                    }
                }
            }
            _ => at_line_start = false,
        }
    }
}

/// If a `|` follows the current position after nothing but newlines and
/// comments, return its index: `a\n# c\n| b` continues the pipeline.
fn pipe_ahead(c: &Cursor<'_>) -> Option<usize> {
    let rest = c.rest();
    let idx = rest.iter().position(|t| !matches!(t.kind, TokenKind::Eol | TokenKind::Comment))?;
    (rest[idx].kind == TokenKind::Pipe).then_some(c.position() + idx)
}

/// Consume newlines and comments up to a pipe found by [`pipe_ahead`],
/// recording the comments, and return the pipe (consumed).
fn take_pipe_ahead(st: St<'_, '_>, c: &mut Cursor<'_>, comments: &mut Vec<Comment>) -> Option<Span> {
    let idx = pipe_ahead(c)?;
    while c.position() < idx {
        let tok = c.next()?;
        if tok.kind == TokenKind::Comment {
            st.comment(tok.span);
            comments.push(Comment { span: tok.span });
        }
    }
    c.next().map(|pipe| pipe.span)
}

/// After a `|`: `a |\n  b`, `a | # comment\n  b` and `a | | b` all continue
/// the pipeline. Consumes newlines, comments and extra pipes; returns
/// whether a newline was crossed.
fn skip_pipe_continuation(
    st: St<'_, '_>,
    c: &mut Cursor<'_>,
    pipe: &mut Option<Span>,
    comments: &mut Vec<Comment>,
) -> bool {
    let mut newline = false;
    while let Some(tok) = c.peek() {
        match tok.kind {
            TokenKind::Eol if pipe.is_some() => newline = true,
            TokenKind::Comment if pipe.is_some() => {
                st.comment(tok.span);
                comments.push(Comment { span: tok.span });
            }
            TokenKind::Pipe => *pipe = Some(tok.span),
            _ => break,
        }
        c.next();
    }
    newline
}

/// Parse one pipeline: commands separated by `|`.
fn pipeline<'a>(st: St<'_, 'a>, c: &mut Cursor<'_>, leading_comments: Vec<Comment>) -> PResult<Pipeline<'a>> {
    let mut elements: Vec<PipelineElement<'a>> = Vec::new();
    let mut trailing_comments = Vec::new();
    let mut pipe: Option<Span> = None;
    // A pipeline may start with `|` (`( | str join)`): the empty first command is dropped.
    let mut newline_after_pipe = skip_pipe_continuation(st, c, &mut pipe, &mut trailing_comments);
    loop {
        if let Some(pipe_span) = pipe
            && c.peek().is_none_or(|t| matches!(t.kind, TokenKind::Semicolon | TokenKind::Eol))
        {
            // Like nu's lite parser, a `|` followed by a newline and then the end
            // of the block is tolerated (`ls |\n`); one that is the last token
            // (`ls |`, `(ls |\n)`, `let x = 1 |`) is not.
            if c.peek().is_none() && newline_after_pipe && !elements.is_empty() {
                break;
            }
            return Err(cut(
                Diagnostic::new(ErrorKind::UnexpectedEof("command after `|`"), pipe_span).with_context("pipeline")
            ));
        }
        let raw = raw_command(st, c)?;
        trailing_comments.extend(raw.comments.iter().copied());
        let (expr, redirection) = statement::parse_command(st, &raw)?;
        let start = pipe.map_or(expr.span.start, |p| p.start);
        let end = redirection.as_ref().map_or(expr.span.end, |r| r.span().end.max(expr.span.end));
        elements.push(PipelineElement { span: Span::new(start, end), pipe, expr, redirection });
        let Some(next_pipe) = raw.pipe_after.or_else(|| take_pipe_ahead(st, c, &mut trailing_comments)) else { break };
        pipe = Some(next_pipe);
        newline_after_pipe = skip_pipe_continuation(st, c, &mut pipe, &mut trailing_comments);
    }
    // Like nu ("statement used in pipeline"): a declaration or a `for` loop
    // is a statement of its own, never an element of a longer pipeline, and
    // `source`, `hide`, `overlay` and `plugin use` cannot follow a pipe.
    if elements.len() > 1
        && let Some(statement) = elements
            .iter()
            .enumerate()
            .find(|(i, e)| is_statement_only(&e.expr) || (*i > 0 && is_statement_call(st, &e.expr)))
    {
        return Err(cut(Diagnostic::message("statement used in pipeline", statement.1.expr.span).with_help(
            "declarations, `for`, `source`, `hide`, `overlay` and `plugin use` are statements, not pipeline elements",
        )));
    }
    let span = elements[0].span.merge(elements.last().map_or(elements[0].span, |e| e.span));
    let terminator = match c.peek() {
        Some(tok) if tok.kind == TokenKind::Semicolon => {
            c.next();
            Some(tok.span)
        }
        _ => None,
    };
    Ok(Pipeline { span, elements, leading_comments, trailing_comments, terminator })
}

fn is_statement_only(expr: &Expr<'_>) -> bool {
    matches!(
        expr.kind,
        ExprKind::Def(_)
            | ExprKind::Extern(_)
            | ExprKind::Alias(_)
            | ExprKind::Module(_)
            | ExprKind::Use(_)
            | ExprKind::Export(_)
            | ExprKind::ExportEnv(_)
            | ExprKind::AttributeBlock(_)
            | ExprKind::For(_)
    )
}

/// Calls nu refuses after a `|` (`HeadKind::Builtin` in its `parse_expression`).
fn is_statement_call(st: St<'_, '_>, expr: &Expr<'_>) -> bool {
    let ExprKind::Call(call) = &expr.kind else { return false };
    let head = st.text(call.head.span);
    let first_word = head.split_whitespace().next().unwrap_or("");
    match first_word {
        "source" | "hide" | "plugin" if head.starts_with("plugin use") || first_word != "plugin" => true,
        "overlay" => {
            !head.starts_with("overlay list") && !(call.args.first().is_some_and(|a| st.text(a.span()) == "list"))
        }
        _ => false,
    }
}

/// Collect the tokens of one command.
fn raw_command(st: St<'_, '_>, c: &mut Cursor<'_>) -> PResult<RawCommand> {
    let mut raw = RawCommand::default();
    attribute_lines(st, c, &mut raw)?;
    // After `=` everything to the end of the line belongs to the command.
    let mut absorbing = false;
    while let Some(&tok) = c.peek() {
        match tok.kind {
            TokenKind::Item => raw.parts.push(tok),
            TokenKind::Assign(_) => {
                raw.parts.push(tok);
                absorbing = true;
            }
            TokenKind::Pipe | TokenKind::PipePipe | TokenKind::Redirect(_) if absorbing => raw.parts.push(tok),
            TokenKind::Eol if absorbing => {
                // `$x = a |\n b` and `$x = a\n | b` continue the assignment's pipeline.
                let ends_with_pipe = raw.parts.last().is_some_and(|p| p.kind == TokenKind::Pipe);
                match (ends_with_pipe, pipe_ahead(c)) {
                    (true, _) => {}
                    (false, Some(idx)) => {
                        while c.position() < idx {
                            if let Some(t) = c.next()
                                && t.kind == TokenKind::Comment
                            {
                                st.comment(t.span);
                                raw.comments.push(Comment { span: t.span });
                            }
                        }
                        continue;
                    }
                    (false, None) => break,
                }
            }
            TokenKind::Comment => {
                st.comment(tok.span);
                raw.comments.push(Comment { span: tok.span });
            }
            TokenKind::Redirect(op) => {
                c.next();
                if raw.parts.is_empty() {
                    return Err(cut(Diagnostic::message("unexpected redirection: nothing to redirect", tok.span)));
                }
                if op.is_pipe() {
                    raw.redirections.push((Spanned::new(op, tok.span), None));
                    raw.pipe_after = Some(tok.span);
                    break;
                }
                let target = match c.peek() {
                    Some(target) if target.kind == TokenKind::Item => *target,
                    _ => return Err(cut(Diagnostic::expected("redirection target", c.here()))),
                };
                raw.redirections.push((Spanned::new(op, tok.span), Some(target)));
            }
            TokenKind::Pipe => {
                raw.pipe_after = Some(tok.span);
                c.next();
                break;
            }
            TokenKind::PipePipe => {
                return Err(cut(Diagnostic::new(ErrorKind::ShellSyntax { found: "||", use_instead: "or" }, tok.span)
                    .with_help("use `or` for boolean logic, or `try { } catch { }` to run a fallback command")));
            }
            TokenKind::Eol | TokenKind::Semicolon | TokenKind::Eof => break,
        }
        c.next();
    }
    if raw.parts.is_empty() && raw.attributes.is_empty() && raw.redirections.is_empty() {
        return Err(cut(Diagnostic::expected("command", c.here())));
    }
    raw.end = raw.parts.last().map_or(c.here().start, |t| t.span.end);
    Ok(raw)
}

/// Leading `@name args` lines, each up to the end of its line. Like nu, the
/// definition must follow on the very next line: a blank or comment line in
/// between is an error.
fn attribute_lines(st: St<'_, '_>, c: &mut Cursor<'_>, raw: &mut RawCommand) -> PResult<()> {
    while c.peek().is_some_and(|t| t.kind == TokenKind::Item && st.tok(t).starts_with('@')) {
        let mut attr = Vec::new();
        while let Some(&tok) = c.peek() {
            match tok.kind {
                TokenKind::Item => attr.push(tok),
                TokenKind::Comment => {
                    st.comment(tok.span);
                    raw.comments.push(Comment { span: tok.span });
                }
                TokenKind::Eol | TokenKind::Semicolon => {
                    c.next();
                    break;
                }
                _ => {
                    return Err(cut(Diagnostic::message(
                        "attributes cannot contain pipelines or redirections",
                        tok.span,
                    )
                    .with_context("attribute")));
                }
            }
            c.next();
        }
        raw.attributes.push(attr);
        if let Some(tok) = c.peek()
            && matches!(tok.kind, TokenKind::Eol | TokenKind::Comment)
        {
            return Err(cut(Diagnostic::message("attributes must be followed by a definition", tok.span)
                .with_help("put the `def`, `extern` or `export` on the line right after the attributes")));
        }
    }
    Ok(())
}
