//! The lexer.
//!
//! Nushell is lexed into *items*: maximal runs of non-whitespace text in which
//! brackets (`()`, `[]`, `{}`) and quotes are balanced. `[1 2 3]` is a single
//! item, and so is `$x.a.b` or `foo(bar)`. Nested constructs are lexed again
//! from the interior of the item when they are parsed. This mirrors the
//! reference implementation in `nu-parser`, which is what gives Nushell its
//! whitespace-sensitive semantics (`1+1` is a bare word, `1 + 1` is math).
//!
//! Besides items the lexer produces pipes, redirection operators, semicolons,
//! end-of-line markers, comments and assignment operators. All spans are
//! absolute byte offsets into the original source.

use winnow::combinator::{dispatch, empty, fail, peek, preceded};
use winnow::prelude::*;
use winnow::stream::{Location, Stream};
use winnow::token::{any, take_till, take_while};

use crate::error::{Diagnostic, ErrorKind};
use crate::input::{Input, PResult, cut, input, pos, span_from};
use crate::span::Span;

/// Which stream a redirection applies to and where it goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum RedirectOp {
    /// `o>` / `out>`
    Out,
    /// `o>>` / `out>>`
    OutAppend,
    /// `e>` / `err>`
    Err,
    /// `e>>` / `err>>`
    ErrAppend,
    /// `o+e>` / `out+err>` / `e+o>` / `err+out>`
    OutErr,
    /// `o+e>>` and friends
    OutErrAppend,
    /// `e>|` / `err>|`
    ErrPipe,
    /// `o+e>|` and friends
    OutErrPipe,
}

impl RedirectOp {
    /// `true` for `>>` (append) variants.
    pub fn is_append(self) -> bool {
        matches!(self, RedirectOp::OutAppend | RedirectOp::ErrAppend | RedirectOp::OutErrAppend)
    }

    /// `true` for `>|` variants, which redirect into the next pipeline element.
    pub fn is_pipe(self) -> bool {
        matches!(self, RedirectOp::ErrPipe | RedirectOp::OutErrPipe)
    }

    /// Which stream(s) are redirected.
    pub fn source(self) -> RedirectSource {
        match self {
            RedirectOp::Out | RedirectOp::OutAppend => RedirectSource::Stdout,
            RedirectOp::Err | RedirectOp::ErrAppend | RedirectOp::ErrPipe => RedirectSource::Stderr,
            RedirectOp::OutErr | RedirectOp::OutErrAppend | RedirectOp::OutErrPipe => RedirectSource::StdoutAndStderr,
        }
    }
}

/// The stream a redirection reads from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum RedirectSource {
    /// `o>`
    Stdout,
    /// `e>`
    Stderr,
    /// `o+e>`
    StdoutAndStderr,
}

/// Assignment operators. These are lexed specially because they make the rest
/// of the line (pipes included) belong to the assignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AssignOp {
    /// `=`
    Assign,
    /// `+=`
    AddAssign,
    /// `-=`
    SubAssign,
    /// `*=`
    MulAssign,
    /// `/=`
    DivAssign,
    /// `++=`
    ConcatAssign,
}

impl AssignOp {
    /// The source spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            AssignOp::Assign => "=",
            AssignOp::AddAssign => "+=",
            AssignOp::SubAssign => "-=",
            AssignOp::MulAssign => "*=",
            AssignOp::DivAssign => "/=",
            AssignOp::ConcatAssign => "++=",
        }
    }

    /// Parse the spelling of an assignment operator.
    pub fn from_spelling(s: &str) -> Option<Self> {
        Some(match s {
            "=" => AssignOp::Assign,
            "+=" => AssignOp::AddAssign,
            "-=" => AssignOp::SubAssign,
            "*=" => AssignOp::MulAssign,
            "/=" => AssignOp::DivAssign,
            "++=" => AssignOp::ConcatAssign,
            _ => return None,
        })
    }
}

/// The kind of a lexed token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    /// A bracket-balanced run of text: a word, literal, list, block, ...
    Item,
    /// `# ...` to the end of the line (without the newline).
    Comment,
    /// `|`
    Pipe,
    /// `||`
    PipePipe,
    /// `;`
    Semicolon,
    /// A newline.
    Eol,
    /// An assignment operator standing alone.
    Assign(AssignOp),
    /// A redirection operator standing alone.
    Redirect(RedirectOp),
    /// End of the token stream (always the last token).
    Eof,
}

/// A token with its absolute span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    /// What it is.
    pub kind: TokenKind,
    /// Where it is.
    pub span: Span,
}

impl Token {
    /// The token's text.
    #[inline]
    pub fn text<'a>(&self, source: &'a str) -> &'a str {
        self.span.slice(source)
    }

    /// `true` for an [`TokenKind::Item`].
    #[inline]
    pub fn is_item(&self) -> bool {
        self.kind == TokenKind::Item
    }
}

/// Options controlling how a piece of text is lexed.
///
/// Nested constructs lex their interior with different delimiters: lists treat
/// `,` and newlines as whitespace, records additionally split on `:`, cell
/// paths split on `.`, and so on.
#[derive(Clone, Copy, Debug, Default)]
pub struct LexOptions {
    /// Extra bytes treated as whitespace (in addition to space, tab, `\r`).
    /// Including `\n` here suppresses [`TokenKind::Eol`] tokens.
    pub extra_whitespace: &'static [u8],
    /// Bytes that are emitted as single-character items when they start a token
    /// and that terminate the item otherwise (e.g. `:` in records).
    pub special: &'static [u8],
    /// Drop comments instead of emitting [`TokenKind::Comment`].
    pub skip_comments: bool,
    /// Treat `<`/`>` as nesting brackets (used for type annotations such as `list<int>`).
    pub signature: bool,
}

impl LexOptions {
    /// The options used for blocks and the top level of a file.
    pub const BLOCK: LexOptions =
        LexOptions { extra_whitespace: &[], special: &[], skip_comments: false, signature: false };
    /// Options for subexpressions `( ... )`: newlines are whitespace, so a
    /// parenthesised pipeline may span several lines.
    pub const SUBEXPRESSION: LexOptions =
        LexOptions { extra_whitespace: b"\n\r", special: &[], skip_comments: false, signature: false };
    /// Options for list interiors: commas and newlines are whitespace.
    pub const LIST: LexOptions =
        LexOptions { extra_whitespace: b"\n\r,", special: &[], skip_comments: false, signature: false };
    /// Options for record interiors: like lists, and `:` is special.
    pub const RECORD_KEY: LexOptions =
        LexOptions { extra_whitespace: b"\n\r,", special: b":", skip_comments: false, signature: false };
    /// Options for record values: like lists, but nothing is special.
    pub const RECORD_VALUE: LexOptions =
        LexOptions { extra_whitespace: b"\n\r,", special: &[], skip_comments: false, signature: false };
    /// Options for signatures `[a: int, --flag(-f)]`.
    pub const SIGNATURE: LexOptions =
        LexOptions { extra_whitespace: b"\n\r", special: b":=,", skip_comments: false, signature: true };
    /// Options for input/output type lists `[int -> string, nothing -> nothing]`.
    pub const IO_TYPES: LexOptions =
        LexOptions { extra_whitespace: b"\n\r,", special: &[], skip_comments: true, signature: true };
    /// Options for cell paths: `.`, `?` and `!` are special.
    pub const CELL_PATH: LexOptions =
        LexOptions { extra_whitespace: b"\n\r", special: b".?!", skip_comments: true, signature: false };
    /// Options for match blocks: commas, newlines and (see below) pipes separate arms.
    pub const MATCH: LexOptions =
        LexOptions { extra_whitespace: b" \r\n,", special: &[], skip_comments: true, signature: false };
    /// Options for the first two tokens of a `{...}` body, used to decide what it is.
    pub const BRACE_PROBE: LexOptions =
        LexOptions { extra_whitespace: b"\r\n\t", special: b":", skip_comments: true, signature: false };
    /// Options for binary literals `0x[ff 00]`.
    pub const BINARY: LexOptions =
        LexOptions { extra_whitespace: b",\r\n", special: &[], skip_comments: true, signature: false };
    /// Options for match list/record patterns.
    pub const PATTERN_LIST: LexOptions =
        LexOptions { extra_whitespace: b"\n\r,", special: &[], skip_comments: true, signature: false };
    /// Options for record patterns.
    pub const PATTERN_RECORD: LexOptions =
        LexOptions { extra_whitespace: b"\n\r,", special: b":", skip_comments: true, signature: false };
}

/// Lex `text`, whose first byte is at absolute offset `base`.
///
/// The returned vector always ends with a [`TokenKind::Eof`] token whose span is
/// the empty span at the end of `text`.
pub fn lex(text: &str, base: usize, opts: LexOptions) -> Result<Vec<Token>, Diagnostic> {
    lex_prefix(text, base, opts, usize::MAX)
}

/// Like [`lex`], but stop after `max_tokens` tokens (not counting `Eof`).
///
/// This is used to look at the first couple of tokens of a `{ ... }` body
/// without lexing all of it.
pub fn lex_prefix(text: &str, base: usize, opts: LexOptions, max_tokens: usize) -> Result<Vec<Token>, Diagnostic> {
    let (mut out, end) = lex_prefix_at(text, base, opts, max_tokens)?;
    out.push(Token { kind: TokenKind::Eof, span: Span::point(base + end) });
    Ok(out)
}

/// Lex at most `max_tokens` tokens from the start of `text` and return them
/// (without an `Eof` token) together with the byte offset in `text` just past
/// the last token consumed. Used to lex a record entry by entry with different
/// options for keys and values.
pub fn lex_prefix_at(
    text: &str,
    base: usize,
    opts: LexOptions,
    max_tokens: usize,
) -> Result<(Vec<Token>, usize), Diagnostic> {
    let mut i = input(text, base);
    let mut out: Vec<Token> = Vec::with_capacity((text.len() / 6).max(4));
    let mut count = 0;
    loop {
        skip_whitespace(&mut i, opts);
        if i.is_empty() || count >= max_tokens {
            break;
        }
        match token(&mut i, opts) {
            Ok(Some(tok)) => {
                push_token(&mut out, tok);
                count += 1;
            }
            Ok(None) => {}
            Err(e) => return Err(crate::input::into_diagnostic(e)),
        }
    }
    Ok((out, i.current_token_start()))
}

/// Push a token, applying the "a `|` after a newline continues the pipeline" rule:
/// `foo\n| bar` lexes as `foo | bar`, and comment lines in between are kept
/// without the newlines that would otherwise break the pipeline.
fn push_token(out: &mut Vec<Token>, tok: Token) {
    if tok.kind == TokenKind::Pipe
        && let Some(prev) = out.last_mut()
        && prev.kind == TokenKind::Eol
    {
        *prev = tok;
        // Remove `Eol` tokens that separate comment lines preceding this pipe.
        let mut idx = out.len() - 1;
        while idx >= 2 && out[idx - 1].kind == TokenKind::Comment && out[idx - 2].kind == TokenKind::Eol {
            out.remove(idx - 2);
            idx -= 2;
        }
        return;
    }
    out.push(tok);
}

fn skip_whitespace(i: &mut Input<'_>, opts: LexOptions) {
    let _ = take_while::<_, _, Diagnostic>(0.., |c: char| {
        c == ' ' || c == '\t' || c == '\r' || (c.is_ascii() && opts.extra_whitespace.contains(&(c as u8)))
    })
    .parse_next(i);
}

/// One token. Returns `None` for input that produces no token (a skipped comment).
fn token(i: &mut Input<'_>, opts: LexOptions) -> PResult<Option<Token>> {
    let start = pos(i);
    dispatch! {peek(any);
        '\n' => any.map(|_| Some(TokenKind::Eol)),
        '#' => comment_body.map(move |_| if opts.skip_comments { None } else { Some(TokenKind::Comment) }),
        '|' => preceded('|', winnow::combinator::opt('|')).map(|second| Some(if second.is_some() { TokenKind::PipePipe } else { TokenKind::Pipe })),
        ';' => any.map(|_| Some(TokenKind::Semicolon)),
        _ => move |i: &mut Input<'_>| item(i, opts).map(Some),
    }
    .parse_next(i)
    .map(|kind| kind.map(|kind| Token { kind, span: span_from(i, start) }))
}

fn comment_body(i: &mut Input<'_>) -> PResult<()> {
    ('#', take_till(0.., ['\n', '\r'])).void().parse_next(i)
}

/// The opening bracket kinds tracked while scanning an item.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bracket {
    Paren,
    Square,
    Curly,
    Angle,
}

impl Bracket {
    fn closer(self) -> &'static str {
        match self {
            Bracket::Paren => ")",
            Bracket::Square => "]",
            Bracket::Curly => "}",
            Bracket::Angle => ">",
        }
    }
}

fn quote_str(q: u8) -> &'static str {
    match q {
        b'"' => "\"",
        b'\'' => "'",
        b'`' => "`",
        b')' => ")",
        _ => "?",
    }
}

/// Advance the delimiter matching inside a `(...)` subexpression of an
/// interpolated string. Returns `true` when `byte` is a backslash inside a
/// nested double-quoted string, in which case the caller must skip the next byte.
pub(crate) fn interp_subexpr_step(stack: &mut Vec<(u8, usize)>, byte: u8, at: usize) -> bool {
    match stack.last() {
        Some(&(expected, _)) if expected != b')' => {
            if expected == b'"' && byte == b'\\' {
                return true;
            }
            if byte == expected {
                stack.pop();
            }
        }
        _ => match byte {
            b'\'' | b'"' | b'`' => stack.push((byte, at)),
            b'(' => stack.push((b')', at)),
            b')' => {
                stack.pop();
            }
            _ => {}
        },
    }
    false
}

fn is_redirection_prefix(text: &[u8]) -> bool {
    matches!(text, b"o>" | b"out>" | b"e>" | b"err>" | b"o+e>" | b"e+o>" | b"out+err>" | b"err+out>")
}

/// Scan one item starting at the current position.
///
/// This is the heart of the lexer and a direct port of the reference
/// algorithm: it consumes text until an item terminator is found at bracket
/// depth zero, tracking quotes, brackets, raw strings, comments inside
/// brackets and interpolated-string subexpressions.
fn item(i: &mut Input<'_>, opts: LexOptions) -> PResult<TokenKind> {
    let text = *i.input.as_ref();
    let bytes = text.as_bytes();
    let base = i.state.0 + i.current_token_start();
    let abs = |off: usize| base + off;

    let mut quote: Option<(u8, usize)> = None;
    let mut quote_is_interp = false;
    let mut interp_level: Vec<(u8, usize)> = Vec::new();
    let mut in_comment = false;
    let mut brackets: Vec<(Bracket, usize)> = Vec::new();
    let mut prev: Option<u8> = None;
    let mut off = 0usize;

    let is_terminator = |brackets: &[(Bracket, usize)], c: u8| {
        brackets.is_empty()
            && (matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'|' | b';')
                || opts.extra_whitespace.contains(&c)
                || opts.special.contains(&c))
    };

    while off < bytes.len() {
        let c = bytes[off];
        if let Some((q, _)) = quote {
            if !interp_level.is_empty() {
                if interp_subexpr_step(&mut interp_level, c, abs(off)) && off + 1 < bytes.len() {
                    off += 2;
                    prev = Some(c);
                    continue;
                }
                off += 1;
                prev = Some(c);
                continue;
            }
            if c == b'\\' && q == b'"' {
                if off + 1 < bytes.len() {
                    off += 2;
                    prev = Some(c);
                    continue;
                }
                let (_, open) = quote.expect("in quote");
                return Err(cut(Diagnostic::new(
                    ErrorKind::Unclosed { delimiter: quote_str(q), open: Span::new(open, open + 1) },
                    Span::point(abs(off + 1)),
                )));
            }
            if c == q {
                quote = None;
            } else if quote_is_interp && c == b'(' {
                interp_level.push((b')', abs(off)));
            }
        } else if c == b'#' && !in_comment {
            in_comment = prev.map(|p| p.is_ascii_whitespace()).unwrap_or(true);
        } else if c == b'\n' || c == b'\r' {
            in_comment = false;
            if is_terminator(&brackets, c) {
                break;
            }
        } else if in_comment {
            if is_terminator(&brackets, c) {
                break;
            }
        } else if brackets.is_empty() && opts.special.contains(&c) && off == 0 {
            off += 1;
            break;
        } else if c == b'\'' || c == b'"' || c == b'`' {
            quote = Some((c, abs(off)));
            quote_is_interp = c != b'`' && prev == Some(b'$');
        } else if c == b'[' {
            brackets.push((Bracket::Square, abs(off)));
        } else if c == b'<' && opts.signature {
            brackets.push((Bracket::Angle, abs(off)));
        } else if c == b'>' && opts.signature {
            if matches!(brackets.last(), Some((Bracket::Angle, _))) {
                brackets.pop();
            }
        } else if c == b']' {
            if matches!(brackets.last(), Some((Bracket::Square, _))) {
                brackets.pop();
            } else if let Some(&(open, open_at)) = brackets.last() {
                return Err(unbalanced("]", open, open_at, abs(off)));
            }
        } else if c == b'{' {
            brackets.push((Bracket::Curly, abs(off)));
        } else if c == b'}' {
            if matches!(brackets.last(), Some((Bracket::Curly, _))) {
                brackets.pop();
            } else {
                return Err(match brackets.last() {
                    Some(&(open, open_at)) => unbalanced("}", open, open_at, abs(off)),
                    None => cut(Diagnostic::new(
                        ErrorKind::Unbalanced { found: "}", expected: "{" },
                        Span::new(abs(off), abs(off + 1)),
                    )),
                });
            }
        } else if c == b'(' {
            brackets.push((Bracket::Paren, abs(off)));
        } else if c == b')' {
            if matches!(brackets.last(), Some((Bracket::Paren, _))) {
                brackets.pop();
            } else {
                return Err(match brackets.last() {
                    Some(&(open, open_at)) => unbalanced(")", open, open_at, abs(off)),
                    None => cut(Diagnostic::new(
                        ErrorKind::Unbalanced { found: ")", expected: "(" },
                        Span::new(abs(off), abs(off + 1)),
                    )),
                });
            }
        } else if c == b'r' && bytes.get(off + 1) == Some(&b'#') {
            off = raw_string_end(bytes, off, abs)?;
            prev = Some(b'#');
            continue;
        } else if c == b'|' && is_redirection_prefix(&bytes[..off]) {
            off += 1;
            break;
        } else if is_terminator(&brackets, c) {
            break;
        }
        off += 1;
        prev = Some(c);
    }

    if let Some(&(closer, open_at)) = interp_level.first() {
        return Err(cut(Diagnostic::new(
            ErrorKind::Unclosed { delimiter: quote_str(closer), open: Span::new(open_at, open_at + 1) },
            Span::point(abs(off)),
        )));
    }
    if let Some((q, open_at)) = quote {
        return Err(cut(Diagnostic::new(
            ErrorKind::Unclosed { delimiter: quote_str(q), open: Span::new(open_at, open_at + 1) },
            Span::point(abs(off)),
        )));
    }
    if let Some(&(open, open_at)) = brackets.last() {
        return Err(cut(Diagnostic::new(
            ErrorKind::Unclosed { delimiter: open.closer(), open: Span::new(open_at, open_at + 1) },
            Span::point(abs(off)),
        )));
    }
    if off == 0 {
        return Err(cut(Diagnostic::new(ErrorKind::UnexpectedEof("command"), Span::point(base))));
    }

    let item_text = &bytes[..off];
    let kind = classify(item_text).map_err(|(kind, help)| {
        let mut d = Diagnostic::new(kind, Span::new(base, abs(off)));
        if let Some(h) = help {
            d = d.with_help(h);
        }
        cut(d)
    })?;
    i.next_slice(off);
    Ok(kind)
}

fn unbalanced(found: &'static str, open: Bracket, open_at: usize, at: usize) -> winnow::error::ErrMode<Diagnostic> {
    cut(Diagnostic::new(ErrorKind::Unbalanced { found, expected: open_kind(open) }, Span::new(at, at + 1))
        .with_help(format!("the innermost open delimiter is `{}` at byte {open_at}", open_kind(open))))
}

fn open_kind(b: Bracket) -> &'static str {
    match b {
        Bracket::Paren => "(",
        Bracket::Square => "[",
        Bracket::Curly => "{",
        Bracket::Angle => "<",
    }
}

/// Scan a raw string `r#'...'#` starting at the `r`; returns the offset just past it.
fn raw_string_end(bytes: &[u8], start: usize, abs: impl Fn(usize) -> usize) -> PResult<usize> {
    let mut hashes = 0;
    while bytes.get(start + 1 + hashes) == Some(&b'#') {
        hashes += 1;
    }
    let quote_at = start + 1 + hashes;
    if bytes.get(quote_at) != Some(&b'\'') {
        return Err(cut(Diagnostic::expected("`'` after `r#`", Span::point(abs(quote_at)))));
    }
    let mut off = quote_at + 1;
    while off < bytes.len() {
        if bytes[off] == b'\'' && bytes[off + 1..].starts_with(&b"#".repeat(hashes)) {
            return Ok(off + 1 + hashes);
        }
        off += 1;
    }
    Err(cut(Diagnostic::new(
        ErrorKind::Unclosed { delimiter: "'", open: Span::new(abs(start), abs(quote_at + 1)) },
        Span::point(abs(bytes.len())),
    )))
}

/// Classify a lexed item as an assignment operator, a redirection, or a plain item.
fn classify(text: &[u8]) -> Result<TokenKind, (ErrorKind, Option<&'static str>)> {
    Ok(match text {
        b"=" => TokenKind::Assign(AssignOp::Assign),
        b"+=" => TokenKind::Assign(AssignOp::AddAssign),
        b"-=" => TokenKind::Assign(AssignOp::SubAssign),
        b"*=" => TokenKind::Assign(AssignOp::MulAssign),
        b"/=" => TokenKind::Assign(AssignOp::DivAssign),
        b"++=" => TokenKind::Assign(AssignOp::ConcatAssign),
        b"out>" | b"o>" => TokenKind::Redirect(RedirectOp::Out),
        b"out>>" | b"o>>" => TokenKind::Redirect(RedirectOp::OutAppend),
        b"err>" | b"e>" => TokenKind::Redirect(RedirectOp::Err),
        b"err>>" | b"e>>" => TokenKind::Redirect(RedirectOp::ErrAppend),
        b"err>|" | b"e>|" => TokenKind::Redirect(RedirectOp::ErrPipe),
        b"out+err>" | b"err+out>" | b"o+e>" | b"e+o>" => TokenKind::Redirect(RedirectOp::OutErr),
        b"out+err>>" | b"err+out>>" | b"o+e>>" | b"e+o>>" => TokenKind::Redirect(RedirectOp::OutErrAppend),
        b"out+err>|" | b"err+out>|" | b"o+e>|" | b"e+o>|" => TokenKind::Redirect(RedirectOp::OutErrPipe),
        b"out>|" | b"o>|" => {
            return Err((
                ErrorKind::ShellSyntax { found: "o>|", use_instead: "|" },
                Some("redirecting stdout to a pipe is the same as normal piping"),
            ));
        }
        b"&&" => {
            return Err((
                ErrorKind::ShellSyntax { found: "&&", use_instead: ";" },
                Some("use `;` to run commands in sequence, or `and` for boolean logic"),
            ));
        }
        b"2>" => return Err((ErrorKind::ShellSyntax { found: "2>", use_instead: "e>" }, None)),
        b"2>&1" => return Err((ErrorKind::ShellSyntax { found: "2>&1", use_instead: "o+e>" }, None)),
        _ => TokenKind::Item,
    })
}

/// A tiny helper for tests and debugging: lex and return `(kind, text)` pairs.
#[cfg(test)]
pub(crate) fn lex_debug(text: &str, opts: LexOptions) -> Vec<(TokenKind, &str)> {
    lex(text, 0, opts).unwrap().into_iter().map(|t| (t.kind, t.text(text))).collect()
}

// `empty` and `fail` are re-exported for parsers in other modules that build on
// the same dispatch style; keep the imports used.
#[allow(dead_code)]
fn _unused(i: &mut Input<'_>) -> PResult<()> {
    empty.parse_next(i)?;
    fail.parse_next(i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use TokenKind::*;

    #[test]
    fn simple_pipeline() {
        let toks = lex_debug("ls -l | where size > 1kb\n", LexOptions::BLOCK);
        let kinds: Vec<_> = toks.iter().map(|(k, t)| (*k, *t)).collect();
        assert_eq!(
            kinds,
            vec![
                (Item, "ls"),
                (Item, "-l"),
                (Pipe, "|"),
                (Item, "where"),
                (Item, "size"),
                (Item, ">"),
                (Item, "1kb"),
                (Eol, "\n"),
                (Eof, ""),
            ]
        );
    }

    #[test]
    fn brackets_are_one_item() {
        let toks = lex_debug("echo [1 2 {a: (3 | 4)}] {|x| $x}", LexOptions::BLOCK);
        assert_eq!(toks[1], (Item, "[1 2 {a: (3 | 4)}]"));
        assert_eq!(toks[2], (Item, "{|x| $x}"));
    }

    #[test]
    fn strings_and_interpolation() {
        let toks = lex_debug(r#"print "a | b" 'c;d' `e f` $"x (1 + ")") y" foo"bar""#, LexOptions::BLOCK);
        let texts: Vec<_> = toks.iter().map(|t| t.1).collect();
        assert_eq!(texts, vec!["print", "\"a | b\"", "'c;d'", "`e f`", "$\"x (1 + \")\") y\"", "foo\"bar\"", ""]);
    }

    #[test]
    fn raw_strings() {
        let toks = lex_debug("echo r#'a ' # b'# r##'c'#'##", LexOptions::BLOCK);
        assert_eq!(toks[1], (Item, "r#'a ' # b'#"));
        assert_eq!(toks[2], (Item, "r##'c'#'##"));
    }

    #[test]
    fn comments_and_pipe_continuation() {
        let src = "ls\n# c\n| length # trailing\n";
        let toks = lex_debug(src, LexOptions::BLOCK);
        let kinds: Vec<_> = toks.iter().map(|t| t.0).collect();
        assert_eq!(kinds, vec![Item, Comment, Pipe, Item, Comment, Eol, Eof]);
    }

    #[test]
    fn redirections_and_assignment() {
        let toks = lex_debug("cmd o> f e>| x; $y += 1", LexOptions::BLOCK);
        let kinds: Vec<_> = toks.iter().map(|t| t.0).collect();
        assert_eq!(
            kinds,
            vec![
                Item,
                Redirect(RedirectOp::Out),
                Item,
                Redirect(RedirectOp::ErrPipe),
                Item,
                Semicolon,
                Item,
                Assign(AssignOp::AddAssign),
                Item,
                Eof
            ]
        );
    }

    #[test]
    fn special_tokens_split() {
        let toks = lex_debug("a:1, b: 2", LexOptions::RECORD_KEY);
        let texts: Vec<_> = toks.iter().map(|t| t.1).collect();
        assert_eq!(texts, vec!["a", ":", "1", "b", ":", "2", ""]);
        let toks = lex_debug("$x.a?.0", LexOptions::CELL_PATH);
        let texts: Vec<_> = toks.iter().map(|t| t.1).collect();
        assert_eq!(texts, vec!["$x", ".", "a", "?", ".", "0", ""]);
    }

    #[test]
    fn signature_angle_brackets() {
        let toks = lex_debug("x: list<record<a: int>>, y", LexOptions::SIGNATURE);
        let texts: Vec<_> = toks.iter().map(|t| t.1).collect();
        assert_eq!(texts, vec!["x", ":", "list<record<a: int>>", ",", "y", ""]);
    }

    #[test]
    fn comments_inside_brackets_are_part_of_item() {
        let toks = lex_debug("[\n  1 # one ]\n  2\n]", LexOptions::BLOCK);
        assert_eq!(toks[0].0, Item);
        assert_eq!(toks.len(), 2);
    }

    #[test]
    fn unclosed_errors() {
        let err = lex("echo [1 2", 0, LexOptions::BLOCK).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::Unclosed { delimiter: "]", .. }));
        let err = lex("echo 'abc", 0, LexOptions::BLOCK).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::Unclosed { delimiter: "'", .. }));
        let err = lex("echo )", 0, LexOptions::BLOCK).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::Unbalanced { found: ")", .. }));
        let err = lex("a && b", 0, LexOptions::BLOCK).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::ShellSyntax { found: "&&", .. }));
    }

    #[test]
    fn absolute_offsets_with_base() {
        let toks = lex("a b", 10, LexOptions::BLOCK).unwrap();
        assert_eq!(toks[1].span, Span::new(12, 13));
        assert_eq!(toks[2].span, Span::point(13));
    }

    #[test]
    fn prefix_lexing_stops_early() {
        let toks = lex_prefix("a: 1, b: 2", 0, LexOptions::BRACE_PROBE, 2).unwrap();
        assert_eq!(toks.len(), 3);
        assert_eq!(toks[1].text("a: 1, b: 2"), ":");
    }
}
