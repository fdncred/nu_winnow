//! Grouping tokens into pipelines and commands.
//!
//! This is the "lite" parse: it decides where statements start and end,
//! attaches comments, absorbs the rest of a line after an assignment operator,
//! collects redirections and attribute lines, and recovers from errors at
//! statement boundaries. It never looks inside an item.
//!
//! The rules for newlines around `|` are nu's exactly: a `|` continues the
//! previous line when only an end of line, or comment lines each on their own
//! line, stand between them (`a\n# c\n| b`); after a `|` the pipeline goes on
//! across one end of line and comment lines (`a |\n# c\n b`). A blank line on
//! either side closes the pipeline: `a\n\n| b` and `a |\n\n b` are two
//! pipelines each (the trailing `|` of the first is dropped silently, as nu
//! does), and a `|` that nothing but comments follow at the end of a block is
//! an error.

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
    /// The span of an `e>|` (or `o+e>|`) that ended the command, if any.
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
    // nu's lite parser: a block whose last token, skipping trailing comment
    // lines, is a `|` has a pipeline with no end (`ls |`, `ls |\n# c`,
    // `alias x = ls |`), whatever absorbed the pipe.
    if last_non_comment_token(c.all()) == Some(TokenKind::Pipe)
        && let Some(last) = c.all().iter().rev().find(|t| t.kind == TokenKind::Pipe)
    {
        st.error(
            Diagnostic::new(ErrorKind::UnexpectedEof("command after `|`"), last.span)
                .with_context("pipeline")
                .with_help("the pipeline has no end: add a command after the `|` or remove it"),
        );
    }
    let mut pipelines: Vec<Pipeline<'a>> = Vec::new();
    let mut pending: Vec<Comment> = Vec::new();
    let mut last = TokenKind::Eol;
    // Set when a pipeline ended with a `|` that a blank line closed: nu's
    // lexer then refuses a `;` before the next item.
    let mut dangling_pipe: Option<Span> = None;
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
                if let Some(pipe) = dangling_pipe {
                    st.error(Diagnostic::new(ErrorKind::ExtraTokens, tok.span).with_help(format!(
                        "the pipeline is still open after the `|` at {pipe}; add a command or remove the `;`"
                    )));
                }
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
                dangling_pipe = None;
                let start = c.position();
                let start_span = tok.span;
                match pipeline(st, &mut c, std::mem::take(&mut pending)) {
                    Ok((p, dangling)) => {
                        dangling_pipe = dangling;
                        pipelines.extend(p);
                    }
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
/// parsing it, so calls to multi-word commands defined later resolve. Like nu,
/// a `def` or `extern` name declared twice in one block is an error.
fn predeclare(st: St<'_, '_>, tokens: &[Token]) {
    let mut declared: Vec<&str> = Vec::new();
    let mut at_line_start = true;
    let mut idx = 0;
    while let Some(tok) = tokens.get(idx) {
        idx += 1;
        match tok.kind {
            TokenKind::Eol | TokenKind::Semicolon => at_line_start = true,
            TokenKind::Comment => {}
            TokenKind::Item if at_line_start => {
                at_line_start = false;
                let mut words =
                    tokens[idx..].iter().take_while(|t| t.kind == TokenKind::Item).map(|t| (st.tok(t), t.span));
                let head = match st.tok(tok) {
                    "export" => words.next().map_or("", |w| w.0),
                    head => head,
                };
                if !matches!(head, "def" | "extern" | "alias") {
                    continue;
                }
                let Some((name, name_span)) = words.find(|w| !w.0.starts_with('-')) else { continue };
                let name = name.trim_matches(['"', '\'', '`']);
                if name.is_empty() {
                    continue;
                }
                st.declare_command(name);
                // nu predeclares a definition only when a signature item follows the name.
                let has_signature = head != "alias" && words.any(|w| w.0.starts_with(['[', '(']));
                if !has_signature {
                    continue;
                }
                if declared.contains(&name) {
                    st.error(
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

/// The last token that is not part of a trailing `([Comment]+ [Eol])*`
/// sequence: nu's `last_non_comment_token`, used to tell `ls |\n` (fine)
/// from `ls |` and `ls |\n# c` (a pipeline with no end).
fn last_non_comment_token(tokens: &[Token]) -> Option<TokenKind> {
    let mut expect = TokenKind::Comment;
    for tok in tokens.iter().rev() {
        match (tok.kind, expect) {
            (TokenKind::Comment, TokenKind::Comment | TokenKind::Eol) => expect = TokenKind::Eol,
            (TokenKind::Eol, TokenKind::Eol) => expect = TokenKind::Comment,
            (kind, _) => return Some(kind),
        }
    }
    None
}

/// If a `|` on a later line continues the pipeline, return its index: exactly
/// one end of line, then any number of comment lines (`Eol (Comment Eol)*`),
/// then the pipe. A blank line in between closes the pipeline instead.
fn pipe_ahead(c: &Cursor<'_>) -> Option<usize> {
    let rest = c.rest();
    let mut idx = 0;
    if rest.first()?.kind != TokenKind::Eol {
        return None;
    }
    idx += 1;
    loop {
        match rest.get(idx)?.kind {
            TokenKind::Pipe => return Some(c.position() + idx),
            TokenKind::Comment if rest.get(idx + 1)?.kind == TokenKind::Eol => idx += 2,
            _ => return None,
        }
    }
}

/// Consume newlines and comments up to a pipe found by [`pipe_ahead`],
/// recording the comments.
fn take_pipe_ahead(st: St<'_, '_>, c: &mut Cursor<'_>, comments: &mut Vec<Comment>) -> bool {
    let Some(idx) = pipe_ahead(c) else { return false };
    while c.position() < idx {
        if let Some(tok) = c.next()
            && tok.kind == TokenKind::Comment
        {
            st.comment(tok.span);
            comments.push(Comment { span: tok.span });
        }
    }
    true
}

/// What follows a `|` (just consumed) after its continuation lines.
enum AfterPipe {
    /// A command follows.
    Command,
    /// A blank line or the end of the block: the pipeline ends here and the
    /// `|` is dropped.
    Dangling,
}

/// After a `|`: consume comments on the same line, then one end of line and
/// any comment lines (`Eol (Comment Eol)*`), the way nu's lite parser keeps a
/// pipeline open across them.
fn after_pipe(st: St<'_, '_>, c: &mut Cursor<'_>, pipe: Span, comments: &mut Vec<Comment>) -> PResult<AfterPipe> {
    let mut comment = |st: St<'_, '_>, tok: &Token| {
        st.comment(tok.span);
        comments.push(Comment { span: tok.span });
    };
    while let Some(tok) = c.peek().filter(|t| t.kind == TokenKind::Comment) {
        comment(st, tok);
        c.next();
    }
    if c.peek().is_some_and(|t| t.kind == TokenKind::Eol) {
        c.next();
        while let Some(tok) = c.peek().filter(|t| t.kind == TokenKind::Comment)
            && c.rest().get(1).is_some_and(|t| t.kind == TokenKind::Eol)
        {
            comment(st, tok);
            c.next();
            c.next();
        }
    }
    // A `|` that only comments follow at the end of the block is reported
    // once, by `parse_block`, whichever command absorbed it.
    match c.peek().map(|t| t.kind) {
        None | Some(TokenKind::Eol) => Ok(AfterPipe::Dangling),
        Some(TokenKind::Semicolon) => Err(cut(
            Diagnostic::new(ErrorKind::UnexpectedEof("command after `|`"), pipe).with_context("pipeline")
        )),
        Some(_) => Ok(AfterPipe::Command),
    }
}

/// Parse one pipeline: commands separated by `|`. Also returns the span of a
/// trailing `|` that a blank line closed, if any. `None` when there was no
/// command at all (a lone `|` before a blank line).
fn pipeline<'a>(
    st: St<'_, 'a>,
    c: &mut Cursor<'_>,
    leading_comments: Vec<Comment>,
) -> PResult<(Option<Pipeline<'a>>, Option<Span>)> {
    // The lite parse first: collect the commands, then parse them, because a
    // command is parsed differently when it is one element of a longer pipeline.
    let mut raws: Vec<(Option<Span>, RawCommand)> = Vec::new();
    let mut trailing_comments = Vec::new();
    let mut pipe: Option<Span> = None;
    let mut dangling = None;
    'commands: loop {
        // A pipeline may start with `|` (`( | str join)`) and `a | | b` is
        // `a | b`: the empty commands are dropped.
        while let Some(tok) = c.peek().filter(|t| t.kind == TokenKind::Pipe) {
            let span = tok.span;
            c.next();
            pipe = Some(span);
            if let AfterPipe::Dangling = after_pipe(st, c, span, &mut trailing_comments)? {
                dangling = pipe.take();
                break 'commands;
            }
        }
        if pipe.is_none() && !raws.is_empty() {
            // After a command: the pipeline goes on only through a `|` on a later line.
            if !take_pipe_ahead(st, c, &mut trailing_comments) {
                break;
            }
            continue;
        }
        let raw = raw_command(st, c, pipe.is_none())?;
        trailing_comments.extend(raw.comments.iter().copied());
        let pipe_after = raw.pipe_after;
        raws.push((pipe.take(), raw));
        if let Some(span) = pipe_after {
            pipe = Some(span);
            if let AfterPipe::Dangling = after_pipe(st, c, span, &mut trailing_comments)? {
                dangling = pipe.take();
                break;
            }
        }
    }
    if raws.is_empty() {
        // Only pipes (`|` and a blank line): nu drops the empty command.
        return Ok((None, dangling));
    }
    let single = raws.len() == 1;
    let mut elements: Vec<PipelineElement<'a>> = Vec::with_capacity(raws.len());
    for (pipe, raw) in &raws {
        let (expr, redirection) = statement::parse_command(st, raw, !single)?;
        let start = pipe.map_or(expr.span.start, |p| p.start);
        let end = redirection.as_ref().map_or(expr.span.end, |r| r.span().end.max(expr.span.end));
        elements.push(PipelineElement { span: Span::new(start, end), pipe: *pipe, expr, redirection });
    }
    let span = elements[0].span.merge(elements.last().map_or(elements[0].span, |e| e.span));
    let terminator = match c.peek() {
        Some(tok) if tok.kind == TokenKind::Semicolon => {
            c.next();
            Some(tok.span)
        }
        _ => None,
    };
    Ok((Some(Pipeline { span, elements, leading_comments, trailing_comments, terminator }), dangling))
}

/// Collect the tokens of one command. `first` is set for the first command of
/// a pipeline, the only place attribute lines can precede it.
fn raw_command(st: St<'_, '_>, c: &mut Cursor<'_>, first: bool) -> PResult<RawCommand> {
    let mut raw = RawCommand::default();
    if first {
        attribute_lines(st, c, &mut raw)?;
    }
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
                if ends_with_pipe {
                    c.next();
                    continue;
                }
                if take_pipe_ahead(st, c, &mut raw.comments) {
                    continue;
                }
                break;
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
            TokenKind::Pipe => break,
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

/// Leading `@name args` lines, each up to the end of its line or a `;`. Like
/// nu, everything on the line is an argument of the attribute, pipes and
/// redirections included, and the definition must follow on the very next
/// line: a blank or comment line in between is an error.
fn attribute_lines(st: St<'_, '_>, c: &mut Cursor<'_>, raw: &mut RawCommand) -> PResult<()> {
    while c.peek().is_some_and(|t| t.kind == TokenKind::Item && st.tok(t).starts_with('@')) {
        let mut attr = Vec::new();
        while let Some(&tok) = c.peek() {
            match tok.kind {
                TokenKind::Comment => {
                    st.comment(tok.span);
                    raw.comments.push(Comment { span: tok.span });
                }
                TokenKind::Eol | TokenKind::Semicolon => {
                    c.next();
                    break;
                }
                TokenKind::Eof => break,
                _ => attr.push(Token { kind: TokenKind::Item, span: tok.span }),
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
