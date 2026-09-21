//! A read position in a slice of tokens.
//!
//! Statement and expression parsers walk the items of one command with a
//! [`Cursor`]. Besides the slice and the position it remembers where the
//! slice *ends* in the source, so that an error such as "expected block" can
//! point just past the last token when the input ran out.

use std::ops::Range;

use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut};
use crate::lexer::{Token, TokenKind};
use crate::span::Span;

/// A cursor over tokens. Cheap to copy; copies are independent positions.
#[derive(Clone, Copy, Debug)]
pub struct Cursor<'t> {
    tokens: &'t [Token],
    pos: usize,
    /// Byte offset just past the last token, for errors at the end.
    end: usize,
}

impl<'t> Cursor<'t> {
    /// A cursor over `tokens`, which end at byte offset `end` in the source.
    pub fn new(tokens: &'t [Token], end: usize) -> Self {
        Self { tokens, pos: 0, end }
    }

    /// A cursor over the output of the lexer, whose last token is `Eof`.
    pub fn from_lexed(tokens: &'t [Token]) -> Self {
        match tokens.split_last() {
            Some((eof, rest)) if eof.kind == TokenKind::Eof => Self::new(rest, eof.span.start),
            _ => Self::new(tokens, tokens.last().map_or(0, |t| t.span.end)),
        }
    }

    /// The next token, without consuming it.
    pub fn peek(&self) -> Option<&'t Token> {
        self.tokens.get(self.pos)
    }

    /// Consume and return the next token.
    pub fn next(&mut self) -> Option<&'t Token> {
        let tok = self.tokens.get(self.pos)?;
        self.pos += 1;
        Some(tok)
    }

    /// `true` when every token has been consumed.
    pub fn at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    /// The tokens not yet consumed.
    pub fn rest(&self) -> &'t [Token] {
        &self.tokens[self.pos.min(self.tokens.len())..]
    }

    /// All tokens, consumed or not.
    pub fn all(&self) -> &'t [Token] {
        self.tokens
    }

    /// The current position (an index into [`Cursor::all`]).
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Move back (or forward) to a position obtained from [`Cursor::position`].
    pub fn reset(&mut self, pos: usize) {
        self.pos = pos;
    }

    /// The span of the next token, or the empty span at the end.
    pub fn here(&self) -> Span {
        self.peek().map_or(Span::point(self.end), |t| t.span)
    }

    /// The empty span at the end of the tokens.
    pub fn end_span(&self) -> Span {
        Span::point(self.end)
    }

    /// The span from the first to the last token, if any.
    pub fn span(&self) -> Option<Span> {
        Some(self.tokens.first()?.span.merge(self.tokens.last()?.span))
    }

    /// Consume the next token if it is an item; otherwise fail with `expected <what>`.
    pub fn expect_item(&mut self, what: &'static str) -> PResult<Token> {
        match self.peek() {
            Some(tok) if tok.kind == TokenKind::Item => {
                self.pos += 1;
                Ok(*tok)
            }
            _ => Err(cut(Diagnostic::expected(what, self.here()))),
        }
    }

    /// Fail with "extra tokens" unless everything has been consumed.
    pub fn expect_end(&self) -> PResult<()> {
        match self.peek() {
            Some(tok) => Err(cut(Diagnostic::new(ErrorKind::ExtraTokens, tok.span))),
            None => Ok(()),
        }
    }

    /// Consume everything that is left and return the span it covers.
    pub fn rest_span(&mut self) -> Option<Span> {
        let rest = self.rest();
        let span = Some(rest.first()?.span.merge(rest.last()?.span));
        self.pos = self.tokens.len();
        span
    }

    /// A cursor over the tokens in `range` (indices into [`Cursor::all`]),
    /// ending where the token after the range starts.
    pub fn slice(&self, range: Range<usize>) -> Cursor<'t> {
        let end = self.tokens.get(range.end).map_or(self.end, |t| t.span.start);
        Cursor { tokens: &self.tokens[range], pos: 0, end }
    }

    /// A cursor over what is left, as an independent slice.
    pub fn remaining(&self) -> Cursor<'t> {
        self.slice(self.pos.min(self.tokens.len())..self.tokens.len())
    }
}
