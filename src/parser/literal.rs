//! Character-level literal parsers: numbers, units, datetimes, binary blobs,
//! string escapes and raw strings.
//!
//! Most of these take the text of one lexed item and either recognise it
//! completely or fail; the caller (`value.rs`) tries them in Nushell's order.

use std::borrow::Cow;

use winnow::ascii::digit1;
use winnow::combinator::{alt, opt, preceded};
use winnow::prelude::*;
use winnow::token::{one_of, take_while};

use crate::ast::{BinaryLit, Duration, DurationUnit, Expr, ExprKind, Filesize, FilesizeUnit, Quote, StringLit};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut};
use crate::lexer::{LexOptions, TokenKind, lex};
use crate::span::Span;

use super::St;

/// Remove `_` digit separators, borrowing when there are none.
fn strip_underscores(text: &str) -> Cow<'_, str> {
    if text.contains('_') { Cow::Owned(text.replace('_', "")) } else { Cow::Borrowed(text) }
}

/// Parse an integer literal: decimal, `0x`, `0o` or `0b`, with optional `_`.
///
/// Like Nushell, radix literals are parsed as `u64` and reinterpreted, so
/// `0xffffffffffffffff` is `-1`.
pub fn parse_int(text: &str) -> Option<i64> {
    let text = strip_underscores(text);
    if text.is_empty() {
        return None;
    }
    if let Some(hex) = text.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).ok().map(|n| n as i64)
    } else if let Some(oct) = text.strip_prefix("0o") {
        u64::from_str_radix(oct, 8).ok().map(|n| n as i64)
    } else if let Some(bin) = text.strip_prefix("0b") {
        u64::from_str_radix(bin, 2).ok().map(|n| n as i64)
    } else {
        text.parse::<i64>().ok()
    }
}

/// Parse a float literal. Accepts everything Rust's `f64::from_str` accepts
/// (including `inf`, `NaN`, `1e5`, `.5`), with optional `_` separators.
pub fn parse_float(text: &str) -> Option<f64> {
    let text = strip_underscores(text);
    if text.is_empty() {
        return None;
    }
    text.parse::<f64>().ok()
}

/// Parse an int or float.
pub fn number<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let text = st.text(span);
    if let Some(i) = parse_int(text) {
        Ok(Expr::new(ExprKind::Int(i), span))
    } else if let Some(f) = parse_float(text) {
        Ok(Expr::new(ExprKind::Float(f), span))
    } else {
        Err(cut(Diagnostic::expected("number", span)))
    }
}

/// `true` if `text` could start a unit literal: a digit, `.digit` or `-digit`.
fn unit_literal_start(text: &[u8]) -> bool {
    text.len() >= 2
        && (text[0].is_ascii_digit()
            || (text[0] == b'.' && text[1].is_ascii_digit())
            || (text[0] == b'-' && text[1].is_ascii_digit()))
}

const FILESIZE_UNITS: &[(&str, FilesizeUnit)] = &[
    ("KB", FilesizeUnit::KB),
    ("MB", FilesizeUnit::MB),
    ("GB", FilesizeUnit::GB),
    ("TB", FilesizeUnit::TB),
    ("PB", FilesizeUnit::PB),
    ("EB", FilesizeUnit::EB),
    ("KIB", FilesizeUnit::KiB),
    ("MIB", FilesizeUnit::MiB),
    ("GIB", FilesizeUnit::GiB),
    ("TIB", FilesizeUnit::TiB),
    ("PIB", FilesizeUnit::PiB),
    ("EIB", FilesizeUnit::EiB),
    ("B", FilesizeUnit::B),
];

const DURATION_UNITS: &[(&str, DurationUnit)] = &[
    ("ns", DurationUnit::Nanosecond),
    ("us", DurationUnit::Microsecond),
    ("\u{00B5}s", DurationUnit::Microsecond),
    ("\u{03BC}s", DurationUnit::Microsecond),
    ("ms", DurationUnit::Millisecond),
    ("sec", DurationUnit::Second),
    ("min", DurationUnit::Minute),
    ("hr", DurationUnit::Hour),
    ("day", DurationUnit::Day),
    ("wk", DurationUnit::Week),
];

/// Split `text` into `(number, unit)` using `units`, comparing with `normalize`
/// applied to the text. Returns `Err` when the text has a unit suffix but the
/// number before it is malformed.
fn split_unit<'u, U: Copy>(
    text: &str,
    units: &'u [(&'u str, U)],
    uppercase: bool,
) -> Option<Result<(f64, U), &'static str>> {
    if !unit_literal_start(text.as_bytes()) {
        return None;
    }
    let normalized: Cow<'_, str> = if uppercase { Cow::Owned(text.to_ascii_uppercase()) } else { Cow::Borrowed(text) };
    let (name, unit) = units.iter().find(|(name, _)| normalized.ends_with(name))?;
    let number = &text[..text.len() - name.len()];
    if number.ends_with('$') {
        return None;
    }
    let number = strip_underscores(number);
    match number.parse::<f64>() {
        Ok(v) => Some(Ok((v, *unit))),
        Err(_) => Some(Err("value must be a number")),
    }
}

/// Recognise a filesize literal such as `10kb` or `1.5MiB` (units are case-insensitive).
pub fn filesize(text: &str) -> Option<Result<Filesize, &'static str>> {
    // `0x1b` would otherwise look like `0x1` bytes.
    if text.starts_with("0x") {
        return None;
    }
    split_unit(text, FILESIZE_UNITS, true).map(|r| r.map(|(value, unit)| Filesize { value, unit }))
}

/// Recognise a duration literal such as `1sec` or `2.5hr` (units are case-sensitive).
pub fn duration(text: &str) -> Option<Result<Duration, &'static str>> {
    split_unit(text, DURATION_UNITS, false).map(|r| r.map(|(value, unit)| Duration { value, unit }))
}

fn fixed_digits<'i>(n: usize) -> impl Parser<&'i str, &'i str, winnow::error::ContextError> {
    take_while(n..=n, |c: char| c.is_ascii_digit())
}

/// `true` if `text` is a date/time literal Nushell would accept:
/// `YYYY-MM-DD`, optionally followed by `Thh:mm:ss[.frac]` and optionally a
/// `Z` or `±hh:mm` offset. Field ranges are checked loosely.
pub fn is_datetime(text: &str) -> bool {
    fn date(i: &mut &str) -> winnow::Result<()> {
        let year: &str = fixed_digits(4).parse_next(i)?;
        let _ = year;
        '-'.parse_next(i)?;
        let month = fixed_digits(2).parse_to::<u32>().verify(|m| (1..=12).contains(m)).parse_next(i)?;
        let _ = month;
        '-'.parse_next(i)?;
        let _day = fixed_digits(2).parse_to::<u32>().verify(|d| (1..=31).contains(d)).parse_next(i)?;
        Ok(())
    }
    fn time(i: &mut &str) -> winnow::Result<()> {
        one_of(['T', 't']).parse_next(i)?;
        let _h = fixed_digits(2).parse_to::<u32>().verify(|h| *h < 24).parse_next(i)?;
        ':'.parse_next(i)?;
        let _m = fixed_digits(2).parse_to::<u32>().verify(|m| *m < 60).parse_next(i)?;
        ':'.parse_next(i)?;
        let _s = fixed_digits(2).parse_to::<u32>().verify(|s| *s < 61).parse_next(i)?;
        opt(preceded('.', digit1)).parse_next(i)?;
        Ok(())
    }
    fn offset(i: &mut &str) -> winnow::Result<()> {
        alt((
            one_of(['Z', 'z']).void(),
            (
                one_of(['+', '-']),
                fixed_digits(2).parse_to::<u32>().verify(|h| *h < 24),
                ':',
                fixed_digits(2).parse_to::<u32>().verify(|m| *m < 60),
            )
                .void(),
        ))
        .parse_next(i)
    }
    let mut i = text;
    let ok = (date, opt((time, opt(offset)))).parse_next(&mut i).is_ok();
    ok && i.is_empty()
}

/// Parse `0x[...]`, `0o[...]` or `0b[...]`. Returns `None` if `text` does not
/// start like a binary literal at all.
pub fn binary<'a>(st: St<'_, 'a>, span: Span) -> Option<PResult<Expr<'a>>> {
    let text = st.text(span);
    let (radix, digits_per_byte, prefix) = if text.starts_with("0x[") {
        (16, 2, "0x[")
    } else if text.starts_with("0o[") {
        (8, 3, "0o[")
    } else if text.starts_with("0b[") {
        (2, 8, "0b[")
    } else {
        return None;
    };
    Some(binary_inner(st, span, text, radix, digits_per_byte, prefix))
}

fn binary_inner<'a>(
    st: St<'_, 'a>,
    span: Span,
    text: &str,
    radix: u32,
    digits_per_byte: usize,
    prefix: &str,
) -> PResult<Expr<'a>> {
    let Some(inner) = text.strip_prefix(prefix).and_then(|t| t.strip_suffix(']')) else {
        return Err(cut(Diagnostic::expected("binary literal", span)));
    };
    let inner_span = Span::new(span.start + prefix.len(), span.end - 1);
    let tokens = lex(inner, inner_span.start, LexOptions::BINARY).map_err(cut)?;
    let mut digits = String::new();
    for tok in &tokens {
        match tok.kind {
            TokenKind::Item => digits.push_str(st.tok(tok)),
            TokenKind::Eof | TokenKind::Comment | TokenKind::Semicolon | TokenKind::Eol => {}
            _ => return Err(cut(Diagnostic::expected("binary digits", tok.span))),
        }
    }
    let valid = |c: char| c.is_digit(radix);
    if let Some(bad) = digits.chars().find(|c| !valid(*c)) {
        return Err(cut(Diagnostic::new(
            ErrorKind::InvalidLiteral {
                kind: "binary",
                message: format!("`{bad}` is not a valid digit for radix {radix}"),
            },
            span,
        )));
    }
    let padding = (digits_per_byte - digits.len() % digits_per_byte) % digits_per_byte;
    let padded = format!("{}{}", "0".repeat(padding), digits);
    let mut bytes = Vec::with_capacity(padded.len() / digits_per_byte);
    for chunk in padded.as_bytes().chunks(digits_per_byte) {
        let s = std::str::from_utf8(chunk).expect("ascii digits");
        match u8::from_str_radix(s, radix) {
            Ok(b) => bytes.push(b),
            Err(_) => {
                return Err(cut(Diagnostic::new(
                    ErrorKind::InvalidLiteral { kind: "binary", message: format!("`{s}` does not fit in a byte") },
                    span,
                )));
            }
        }
    }
    Ok(Expr::new(ExprKind::Binary(BinaryLit { radix, bytes }), span))
}

/// Decode the escape sequences of a double-quoted string body.
///
/// `base` is the absolute offset of `text`, used for error spans.
pub fn unescape(text: &str, base: usize) -> Result<Cow<'_, str>, Diagnostic> {
    if !text.contains('\\') {
        return Ok(Cow::Borrowed(text));
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    while let Some((idx, c)) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some((_, esc)) = chars.next() else {
            return Err(Diagnostic::new(
                ErrorKind::InvalidLiteral { kind: "string", message: "incomplete escape sequence after `\\`".into() },
                Span::new(base + idx, base + text.len()),
            ));
        };
        let simple = match esc {
            '"' => '"',
            '\'' => '\'',
            '\\' => '\\',
            '/' => '/',
            '(' => '(',
            ')' => ')',
            '{' => '{',
            '}' => '}',
            '$' => '$',
            '^' => '^',
            '#' => '#',
            '|' => '|',
            '~' => '~',
            ' ' => ' ',
            'a' => '\u{07}',
            'b' => '\u{08}',
            'e' => '\u{1b}',
            'f' => '\u{0c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '0' => '\0',
            'x' => {
                let start = idx + 2;
                let hex = text.get(start..start + 2).filter(|h| h.chars().all(|c| c.is_ascii_hexdigit()));
                let Some(hex) = hex else {
                    return Err(Diagnostic::new(
                        ErrorKind::InvalidLiteral {
                            kind: "string",
                            message: "incomplete hex escape `\\xHH`, expected 2 hex digits".into(),
                        },
                        Span::new(base + idx, base + text.len().min(idx + 4)),
                    ));
                };
                let byte = u8::from_str_radix(hex, 16).expect("validated hex");
                if byte > 0x7f {
                    return Err(Diagnostic::new(
                        ErrorKind::InvalidLiteral {
                            kind: "string",
                            message: format!("hex escape `\\x{hex}` is not valid UTF-8; use `\\u{{{hex}}}`"),
                        },
                        Span::new(base + idx, base + idx + 4),
                    ));
                }
                chars.next();
                chars.next();
                byte as char
            }
            'u' => {
                let rest = &text[idx + 2..];
                let close = rest.strip_prefix('{').and_then(|r| r.find('}'));
                let Some(close) = close else {
                    return Err(Diagnostic::new(
                        ErrorKind::InvalidLiteral {
                            kind: "string",
                            message: "unicode escape must look like `\\u{XXXX}`".into(),
                        },
                        Span::new(base + idx, base + text.len().min(idx + 3)),
                    ));
                };
                let hex = &rest[1..1 + close];
                let ch =
                    u32::from_str_radix(hex, 16).ok().filter(|_| (1..=6).contains(&hex.len())).and_then(char::from_u32);
                let Some(ch) = ch else {
                    return Err(Diagnostic::new(
                        ErrorKind::InvalidLiteral {
                            kind: "string",
                            message: format!("invalid unicode escape `\\u{{{hex}}}`"),
                        },
                        Span::new(base + idx, base + idx + 3 + close),
                    ));
                };
                for _ in 0..close + 2 {
                    chars.next();
                }
                ch
            }
            other => {
                return Err(Diagnostic::new(
                    ErrorKind::InvalidLiteral {
                        kind: "string",
                        message: format!("unrecognized escape sequence `\\{other}`"),
                    },
                    Span::new(base + idx, base + idx + 1 + other.len_utf8()),
                ));
            }
        };
        out.push(simple);
    }
    Ok(Cow::Owned(out))
}

/// Parse a raw string `r#'...'#`. The lexer guarantees the delimiters balance.
pub fn raw_string<'a>(st: St<'_, 'a>, span: Span) -> PResult<Expr<'a>> {
    let text = st.text(span);
    let Some(after_r) = text.strip_prefix('r') else {
        return Err(cut(Diagnostic::expected("raw string", span)));
    };
    let hashes = after_r.bytes().take_while(|b| *b == b'#').count();
    if hashes == 0 || hashes > u8::MAX as usize {
        return Err(cut(Diagnostic::expected("`#` after `r` in raw string", span)));
    }
    let body_start = 1 + hashes;
    let body_end = text.len().checked_sub(hashes).filter(|e| *e > body_start + 1);
    let Some(body_end) = body_end else {
        return Err(cut(Diagnostic::new(ErrorKind::Unclosed { delimiter: "'", open: span }, span.past())));
    };
    if !text[body_end..].bytes().all(|b| b == b'#')
        || text.as_bytes()[body_start] != b'\''
        || text.as_bytes()[body_end - 1] != b'\''
    {
        return Err(cut(Diagnostic::new(ErrorKind::Unclosed { delimiter: "'", open: span }, span.past())));
    }
    let value = &text[body_start + 1..body_end - 1];
    Ok(Expr::new(ExprKind::String(StringLit { value: Cow::Borrowed(value), quote: Quote::Raw(hashes as u8) }), span))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ints() {
        assert_eq!(parse_int("42"), Some(42));
        assert_eq!(parse_int("-42"), Some(-42));
        assert_eq!(parse_int("+7"), Some(7));
        assert_eq!(parse_int("1_000_000"), Some(1_000_000));
        assert_eq!(parse_int("0xff"), Some(255));
        assert_eq!(parse_int("0o17"), Some(15));
        assert_eq!(parse_int("0b101"), Some(5));
        assert_eq!(parse_int("0xffffffffffffffff"), Some(-1));
        assert_eq!(parse_int("1.5"), None);
        assert_eq!(parse_int("abc"), None);
        assert_eq!(parse_int(""), None);
        assert_eq!(parse_int("99999999999999999999"), None);
    }

    #[test]
    fn floats() {
        assert_eq!(parse_float("1.5"), Some(1.5));
        assert_eq!(parse_float(".5"), Some(0.5));
        assert_eq!(parse_float("5."), Some(5.0));
        assert_eq!(parse_float("1e3"), Some(1000.0));
        assert_eq!(parse_float("1_0.5"), Some(10.5));
        assert_eq!(parse_float("inf"), Some(f64::INFINITY));
        assert!(parse_float("NaN").unwrap().is_nan());
        assert_eq!(parse_float("abc"), None);
    }

    #[test]
    fn units() {
        assert_eq!(filesize("1kb").unwrap().unwrap(), Filesize { value: 1.0, unit: FilesizeUnit::KB });
        assert_eq!(filesize("1.5MiB").unwrap().unwrap(), Filesize { value: 1.5, unit: FilesizeUnit::MiB });
        assert_eq!(filesize("10B").unwrap().unwrap(), Filesize { value: 10.0, unit: FilesizeUnit::B });
        assert_eq!(filesize("1b").unwrap().unwrap().unit, FilesizeUnit::B);
        assert!(filesize("0x1b").is_none());
        assert!(filesize("kb").is_none());
        assert!(filesize("1_000kb").unwrap().is_ok());
        assert_eq!(duration("5ns").unwrap().unwrap().unit, DurationUnit::Nanosecond);
        assert_eq!(duration("1\u{00B5}s").unwrap().unwrap().unit, DurationUnit::Microsecond);
        assert_eq!(duration("2.5hr").unwrap().unwrap(), Duration { value: 2.5, unit: DurationUnit::Hour });
        assert!(duration("5NS").is_none());
        assert!(duration("-3sec").unwrap().is_ok());
        assert!(duration("1..2sec").unwrap().is_err());
    }

    #[test]
    fn datetimes() {
        assert!(is_datetime("2024-01-02"));
        assert!(is_datetime("2024-01-02T03:04:05"));
        assert!(is_datetime("2024-01-02T03:04:05.123Z"));
        assert!(is_datetime("2024-01-02T03:04:05+05:30"));
        assert!(!is_datetime("2024-13-02"));
        assert!(!is_datetime("2024-01-02T25:00:00"));
        assert!(!is_datetime("2024-01-02x"));
        assert!(!is_datetime("2024-01"));
    }

    #[test]
    fn escapes() {
        assert_eq!(unescape("plain", 0).unwrap(), "plain");
        assert!(matches!(unescape("plain", 0).unwrap(), Cow::Borrowed(_)));
        assert_eq!(unescape(r#"a\nb\t\"\\\("#, 0).unwrap(), "a\nb\t\"\\(");
        assert_eq!(unescape(r"\u{1F600}\x41\e", 0).unwrap(), "😀A\u{1b}");
        let err = unescape(r"a\qb", 10).unwrap_err();
        assert_eq!(err.span, Span::new(11, 13));
        assert!(unescape(r"\x4", 0).is_err());
        assert!(unescape(r"\u{110000}", 0).is_err());
        assert!(unescape(r"abc\", 0).is_err());
    }
}
