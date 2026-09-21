//! The winnow stream type used by the lexer and the literal parsers.
//!
//! [`Input`] is a character-level stream over a slice of the source. It
//! carries the absolute byte offset of the slice so every span produced from
//! it is absolute, even when the slice is the interior of a nested `[...]`.
//! [`Diagnostic`] is the winnow error type, so an error carries the absolute
//! span at which it occurred.
//!
//! Token-level parsing does not use a winnow stream: the statement and
//! expression parsers walk items with a plain `Cursor` (`src/parser/cursor.rs`).

use winnow::error::{AddContext, ErrMode, FromExternalError, ModalResult, ParserError};
use winnow::stream::{LocatingSlice, Location, Stateful, Stream};

use crate::error::{Diagnostic, ErrorKind};
use crate::span::Span;

/// Absolute byte offset of the first byte of an [`Input`] slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Base(pub usize);

/// Character-level stream with absolute positions.
pub type Input<'a> = Stateful<LocatingSlice<&'a str>, Base>;

/// The result type used by every parser in this crate.
pub type PResult<T> = ModalResult<T, Diagnostic>;

/// Create a character stream over `text`, whose first byte lives at absolute offset `base`.
#[inline]
pub fn input(text: &str, base: usize) -> Input<'_> {
    Stateful { input: LocatingSlice::new(text), state: Base(base) }
}

/// Absolute byte offset of the next character.
#[inline]
pub fn pos(i: &Input<'_>) -> usize {
    i.state.0 + i.current_token_start()
}

/// The span from `start` to the current position.
#[inline]
pub fn span_from(i: &Input<'_>, start: usize) -> Span {
    Span::new(start, pos(i))
}

/// The absolute span of the remaining input.
#[inline]
pub fn rest_span(i: &Input<'_>) -> Span {
    let p = pos(i);
    Span::new(p, p + i.len())
}

/// A fatal (non-backtracking) error.
#[inline]
pub fn cut(d: Diagnostic) -> ErrMode<Diagnostic> {
    ErrMode::Cut(d)
}

/// A recoverable error: `alt` will try the next branch.
#[inline]
pub fn backtrack(d: Diagnostic) -> ErrMode<Diagnostic> {
    ErrMode::Backtrack(d)
}

impl<'a> ParserError<Input<'a>> for Diagnostic {
    type Inner = Diagnostic;

    fn from_input(i: &Input<'a>) -> Self {
        Diagnostic::new(ErrorKind::Expected("valid syntax"), Span::point(pos(i)))
    }

    fn or(self, other: Self) -> Self {
        // Prefer the error that got furthest.
        if other.span.start >= self.span.start { other } else { self }
    }

    fn into_inner(self) -> Result<Self::Inner, Self> {
        Ok(self)
    }
}

impl<'a> AddContext<Input<'a>, &'static str> for Diagnostic {
    fn add_context(mut self, _i: &Input<'a>, _start: &<Input<'a> as Stream>::Checkpoint, ctx: &'static str) -> Self {
        self.context.push(ctx);
        self
    }
}

impl<'a, E: std::fmt::Display> FromExternalError<Input<'a>, E> for Diagnostic {
    fn from_external_error(i: &Input<'a>, e: E) -> Self {
        Diagnostic::message(e.to_string(), Span::point(pos(i)))
    }
}

/// Convert a winnow error into a diagnostic (an incomplete-input error should never
/// happen since all input is complete).
pub fn into_diagnostic(e: ErrMode<Diagnostic>) -> Diagnostic {
    match e {
        ErrMode::Backtrack(d) | ErrMode::Cut(d) => d,
        ErrMode::Incomplete(_) => Diagnostic::new(ErrorKind::UnexpectedEof("more input"), Span::point(0)),
    }
}
