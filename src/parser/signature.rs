//! Signatures (`[a: int, --flag(-f), ...rest]`), type annotations and
//! input/output type lists.

use std::borrow::Cow;

use crate::ast::{Comment, IoType, Param, ParamKind, Signature, TypeAnnotation, TypeField, TypeKind};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut};
use crate::lexer::{LexOptions, Token, TokenKind};
use crate::span::{Span, Spanned};

use super::St;
use super::value::{self, Hint, is_identifier};

/// Parse a `[...]` or `(...)` signature item.
pub fn parse_signature<'a>(st: St<'_, 'a>, span: Span) -> PResult<Signature<'a>> {
    let text = st.text(span);
    let (open, close) = match text.as_bytes().first() {
        Some(b'[') => ("[", "]"),
        Some(b'(') => ("(", ")"),
        _ => return Err(cut(Diagnostic::expected("signature", span))),
    };
    if text.len() < 2 || !text.ends_with(close) {
        return Err(cut(Diagnostic::new(
            ErrorKind::Unclosed { delimiter: close, open: Span::new(span.start, span.start + 1) },
            span.past(),
        )));
    }
    let _ = open;
    parse_signature_inner(st, Span::new(span.start + 1, span.end - 1), span)
}

/// Parse the parameters in `inner` (the text between the delimiters); `outer`
/// becomes the signature's span.
pub fn parse_signature_inner<'a>(st: St<'_, 'a>, inner: Span, outer: Span) -> PResult<Signature<'a>> {
    let tokens = st.lex_span(inner, LexOptions::SIGNATURE).map_err(cut)?;
    let params = parse_params(st, &tokens).map_err(|e| e.map(|d| d.with_context("signature")))?;
    Ok(Signature { span: outer, params, io_types: Vec::new(), io_span: None })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Arg,
    AfterComma,
    Type,
    AfterType,
    Default,
}

fn parse_params<'a>(st: St<'_, 'a>, tokens: &[Token]) -> PResult<Vec<Param<'a>>> {
    let mut params: Vec<Param<'a>> = Vec::new();
    let mut mode = Mode::Arg;
    let items: Vec<&Token> = tokens.iter().filter(|t| t.kind != TokenKind::Eof).collect();
    for (idx, tok) in items.iter().enumerate() {
        let last = idx + 1 == items.len();
        let text = st.tok(tok);
        match tok.kind {
            TokenKind::Comment => {
                st.comment(tok.span);
                if let Some(p) = params.last_mut() {
                    p.description = Some(Comment { span: tok.span });
                }
                continue;
            }
            TokenKind::Pipe | TokenKind::PipePipe => continue,
            TokenKind::Item | TokenKind::Assign(_) => {}
            _ => return Err(cut(Diagnostic::expected("parameter", tok.span))),
        }
        match text {
            ":" => match mode {
                Mode::Arg if last => return Err(cut(Diagnostic::expected("type", tok.span.past()))),
                Mode::Arg => mode = Mode::Type,
                Mode::AfterComma | Mode::AfterType => {
                    return Err(cut(Diagnostic::expected("parameter or flag", tok.span)));
                }
                Mode::Type | Mode::Default => return Err(cut(Diagnostic::expected("type", tok.span))),
            },
            "=" => match mode {
                Mode::Arg | Mode::AfterType if last => {
                    return Err(cut(Diagnostic::expected("default value", tok.span.past())));
                }
                Mode::Arg | Mode::AfterType => mode = Mode::Default,
                Mode::Type => return Err(cut(Diagnostic::expected("type", tok.span))),
                Mode::AfterComma => return Err(cut(Diagnostic::expected("parameter or flag", tok.span))),
                Mode::Default => return Err(cut(Diagnostic::expected("default value", tok.span))),
            },
            "," => match mode {
                Mode::Arg | Mode::AfterType => mode = Mode::AfterComma,
                Mode::AfterComma => return Err(cut(Diagnostic::expected("parameter or flag", tok.span))),
                Mode::Type => return Err(cut(Diagnostic::expected("type", tok.span))),
                Mode::Default => return Err(cut(Diagnostic::expected("default value", tok.span))),
            },
            _ => match mode {
                Mode::Arg | Mode::AfterComma | Mode::AfterType => {
                    if let Some(short) = text.strip_prefix("(-") {
                        // `--long (-s)`: a short alias for the preceding flag.
                        if mode == Mode::AfterComma {
                            return Err(cut(Diagnostic::expected("parameter or flag", tok.span)));
                        }
                        let short = short
                            .strip_suffix(')')
                            .ok_or_else(|| cut(Diagnostic::expected("short flag like `(-s)`", tok.span)))?;
                        let mut chars = short.chars();
                        let (Some(c), None) = (chars.next(), chars.next()) else {
                            return Err(cut(Diagnostic::expected("single-character short flag", tok.span)));
                        };
                        match params.last_mut() {
                            Some(Param { kind: ParamKind::Flag { short: slot @ None, .. }, span, .. }) => {
                                *slot = Some(Spanned::new(c, Span::new(tok.span.start + 2, tok.span.end - 1)));
                                *span = span.merge(tok.span);
                            }
                            Some(Param { kind: ParamKind::Flag { short: Some(_), .. }, .. }) => {
                                return Err(cut(Diagnostic::message("this flag already has a short form", tok.span)));
                            }
                            _ => {
                                return Err(cut(Diagnostic::message(
                                    "short flag alias without a preceding long flag",
                                    tok.span,
                                )));
                            }
                        }
                        continue;
                    }
                    params.push(new_param(st, tok)?);
                    mode = Mode::Arg;
                }
                Mode::Type => {
                    let (ty, completer) = parse_type_with_completer(st, tok.span)?;
                    let Some(p) = params.last_mut() else {
                        return Err(cut(Diagnostic::expected("parameter before type", tok.span)));
                    };
                    p.ty = Some(ty);
                    p.completer = completer;
                    p.span = p.span.merge(tok.span);
                    mode = Mode::AfterType;
                }
                Mode::Default => {
                    let default = value::value(st, tok.span, Hint::Any)?;
                    let Some(p) = params.last_mut() else {
                        return Err(cut(Diagnostic::expected("parameter before default value", tok.span)));
                    };
                    p.default = Some(default);
                    p.span = p.span.merge(tok.span);
                    mode = Mode::Arg;
                }
            },
        }
    }
    let end = items.last().map_or(Span::point(0), |t| t.span.past());
    match mode {
        Mode::Type => Err(cut(Diagnostic::expected("type", end))),
        Mode::Default => Err(cut(Diagnostic::expected("default value", end))),
        _ => Ok(params),
    }
}

fn new_param<'a>(st: St<'_, 'a>, tok: &Token) -> PResult<Param<'a>> {
    let text = st.tok(tok);
    let span = tok.span;
    let base = Param {
        span,
        kind: ParamKind::Positional { optional: false },
        name: Spanned::new("", span),
        ty: None,
        default: None,
        completer: None,
        description: None,
    };
    if let Some(rest) = text.strip_prefix("--").filter(|r| !r.is_empty()) {
        // `--long` or `--long(-s)`
        let (long, short) = match rest.split_once('(') {
            None => (rest, None),
            Some((long, short)) => {
                let short = short.strip_prefix('-').and_then(|s| s.strip_suffix(')'));
                let Some(short) = short else {
                    return Err(cut(Diagnostic::expected("short flag alternative like `--flag(-f)`", span)));
                };
                let mut chars = short.chars();
                let (Some(c), None) = (chars.next(), chars.next()) else {
                    return Err(cut(Diagnostic::expected("single-character short flag", span)));
                };
                let short_start = span.start + 2 + long.len() + 2;
                (long, Some(Spanned::new(c, Span::new(short_start, short_start + c.len_utf8()))))
            }
        };
        if !is_identifier(&long.replace('-', "_")) {
            return Err(cut(Diagnostic::expected("valid name for this long flag", span)));
        }
        let long_span = Span::new(span.start + 2, span.start + 2 + long.len());
        return Ok(Param {
            kind: ParamKind::Flag { long: Some(Spanned::new(long, long_span)), short },
            name: Spanned::new(long, long_span),
            ..base
        });
    }
    if let Some(short) = text.strip_prefix('-').filter(|r| !r.is_empty()) {
        let mut chars = short.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return Err(cut(Diagnostic::expected("single-character short flag", span)));
        };
        let short_span = Span::new(span.start + 1, span.end);
        return Ok(Param {
            kind: ParamKind::Flag { long: None, short: Some(Spanned::new(c, short_span)) },
            name: Spanned::new(short, short_span),
            ..base
        });
    }
    if let Some(name) = text.strip_suffix('?') {
        if !is_identifier(name) {
            return Err(cut(Diagnostic::expected("valid variable name for this optional parameter", span)));
        }
        return Ok(Param {
            kind: ParamKind::Positional { optional: true },
            name: Spanned::new(name, Span::new(span.start, span.end - 1)),
            ..base
        });
    }
    if let Some(name) = text.strip_prefix("...") {
        if !is_identifier(name) {
            return Err(cut(Diagnostic::expected("valid variable name for this rest parameter", span)));
        }
        return Ok(Param {
            kind: ParamKind::Rest,
            name: Spanned::new(name, Span::new(span.start + 3, span.end)),
            ..base
        });
    }
    if !is_identifier(text) {
        return Err(cut(Diagnostic::expected("valid variable name for this parameter", span)));
    }
    Ok(Param { name: Spanned::new(text, span), ..base })
}

/// Split `type@completer` at the first `@` outside angle brackets.
fn split_completer(text: &str) -> (usize, Option<usize>) {
    let mut depth = 0i32;
    for (i, b) in text.bytes().enumerate() {
        match b {
            b'<' => depth += 1,
            b'>' => depth -= 1,
            b'@' if depth == 0 => return (i, Some(i + 1)),
            _ => {}
        }
    }
    (text.len(), None)
}

/// Parse a type token that may carry a `@completer` suffix.
pub fn parse_type_with_completer<'a>(
    st: St<'_, 'a>,
    span: Span,
) -> PResult<(TypeAnnotation<'a>, Option<Spanned<&'a str>>)> {
    let text = st.text(span);
    let (type_len, completer_start) = split_completer(text);
    let completer = completer_start.map(|s| Spanned::new(&text[s..], Span::new(span.start + s, span.end)));
    if type_len == 0 {
        // `name@completer` without a type
        return Ok((TypeAnnotation { span: Span::point(span.start), kind: TypeKind::Any }, completer));
    }
    let ty = parse_type(st, Span::new(span.start, span.start + type_len))?;
    Ok((ty, completer))
}

/// Parse a type annotation such as `int`, `list<string>` or `record<a: int>`.
pub fn parse_type<'a>(st: St<'_, 'a>, span: Span) -> PResult<TypeAnnotation<'a>> {
    let text = st.text(span);
    let kind = match text {
        "any" => TypeKind::Any,
        "binary" => TypeKind::Binary,
        "bool" => TypeKind::Bool,
        "cell-path" => TypeKind::CellPath,
        "closure" => TypeKind::Closure,
        "datetime" => TypeKind::DateTime,
        "directory" => TypeKind::Directory,
        "duration" => TypeKind::Duration,
        "error" => TypeKind::Error,
        "external_arg" => TypeKind::ExternalArg,
        "float" => TypeKind::Float,
        "filesize" => TypeKind::Filesize,
        "glob" => TypeKind::Glob,
        "int" => TypeKind::Int,
        "nothing" => TypeKind::Nothing,
        "number" => TypeKind::Number,
        "path" => TypeKind::Path,
        "range" => TypeKind::Range,
        "string" => TypeKind::String,
        "block" => {
            return Err(cut(Diagnostic::message("blocks are not supported as first-class values", span)
                .with_help("use `closure` instead of `block`")));
        }
        _ if text.starts_with("list")
            || text.starts_with("record")
            || text.starts_with("table")
            || text.starts_with("oneof") =>
        {
            generic_type(st, span)?
        }
        _ => {
            let mut d = Diagnostic::new(ErrorKind::UnknownType(text.to_string()), span);
            if text.contains('@') {
                d = d.with_help("type specifications do not support custom completers here");
            }
            return Err(cut(d));
        }
    };
    Ok(TypeAnnotation { span, kind })
}

fn generic_type<'a>(st: St<'_, 'a>, span: Span) -> PResult<TypeKind<'a>> {
    let text = st.text(span);
    let (name, params) = match text.find('<') {
        None => (text, None),
        Some(lt) => {
            let Some(inner) = text[lt + 1..].strip_suffix('>') else {
                return Err(cut(Diagnostic::new(
                    ErrorKind::Unclosed { delimiter: ">", open: Span::new(span.start + lt, span.start + lt + 1) },
                    span.past(),
                )));
            };
            let inner_span = Span::new(span.start + lt + 1, span.start + lt + 1 + inner.len());
            (&text[..lt], Some(inner_span))
        }
    };
    Ok(match name {
        "list" => {
            let inner = match params {
                None => None,
                Some(p) => {
                    let types = comma_separated_types(st, p)?;
                    if types.len() > 1 {
                        return Err(cut(Diagnostic::message("expected a single type parameter", p)));
                    }
                    types.into_iter().next().map(Box::new)
                }
            };
            TypeKind::List(inner)
        }
        "oneof" => TypeKind::OneOf(match params {
            None => Vec::new(),
            Some(p) => comma_separated_types(st, p)?,
        }),
        "record" => TypeKind::Record(match params {
            None => Vec::new(),
            Some(p) => named_type_params(st, p)?,
        }),
        "table" => TypeKind::Table(match params {
            None => Vec::new(),
            Some(p) => named_type_params(st, p)?,
        }),
        _ => return Err(cut(Diagnostic::new(ErrorKind::UnknownType(text.to_string()), span))),
    })
}

fn comma_separated_types<'a>(st: St<'_, 'a>, span: Span) -> PResult<Vec<TypeAnnotation<'a>>> {
    let tokens = st.lex_span(span, LexOptions::IO_TYPES).map_err(cut)?;
    tokens.iter().filter(|t| t.kind == TokenKind::Item).map(|t| parse_type(st, t.span)).collect()
}

fn named_type_params<'a>(st: St<'_, 'a>, span: Span) -> PResult<Vec<TypeField<'a>>> {
    let tokens = st.lex_span(span, LexOptions::SIGNATURE).map_err(cut)?;
    let items: Vec<&Token> =
        tokens.iter().filter(|t| matches!(t.kind, TokenKind::Item | TokenKind::Assign(_))).collect();
    let mut fields = Vec::new();
    let mut idx = 0;
    while idx < items.len() {
        let name_tok = items[idx];
        let name_text = st.tok(name_tok);
        if name_text == "," || name_text == ":" {
            return Err(cut(Diagnostic::expected("field name", name_tok.span)));
        }
        let name = value::string_lit(st, name_tok.span)?.value;
        idx += 1;
        let ty = if idx < items.len() && st.tok(items[idx]) == ":" {
            idx += 1;
            let Some(ty_tok) = items.get(idx) else {
                return Err(cut(Diagnostic::expected("type", name_tok.span.past())));
            };
            idx += 1;
            parse_type(st, ty_tok.span)?
        } else {
            TypeAnnotation { span: name_tok.span.past(), kind: TypeKind::Any }
        };
        fields.push(TypeField { name: Spanned::new(name, name_tok.span), ty });
        // Fields may be separated by commas, whitespace or newlines.
        if idx < items.len() && st.tok(items[idx]) == "," {
            idx += 1;
        }
    }
    Ok(fields)
}

/// Parse `int -> string` or `[int -> string, nothing -> nothing]` covering `span`.
pub fn parse_io_types<'a>(st: St<'_, 'a>, span: Span) -> PResult<Vec<IoType<'a>>> {
    let text = st.text(span);
    let inner = if text.starts_with('[') && text.ends_with(']') && text.len() >= 2 {
        Span::new(span.start + 1, span.end - 1)
    } else {
        span
    };
    let tokens = st.lex_span(inner, LexOptions::IO_TYPES).map_err(cut)?;
    let items: Vec<&Token> = tokens.iter().filter(|t| t.kind == TokenKind::Item).collect();
    let mut out = Vec::new();
    let mut idx = 0;
    while idx < items.len() {
        let input = parse_type(st, items[idx].span)?;
        let Some(arrow) = items.get(idx + 1) else {
            return Err(cut(Diagnostic::expected("arrow (->)", items[idx].span.past())));
        };
        if st.tok(arrow) != "->" {
            return Err(cut(Diagnostic::expected("arrow (->)", arrow.span)));
        }
        let Some(out_tok) = items.get(idx + 2) else {
            return Err(cut(Diagnostic::expected("output type", arrow.span.past())));
        };
        let output = parse_type(st, out_tok.span)?;
        out.push(IoType { input, arrow: arrow.span, output });
        idx += 3;
    }
    Ok(out)
}

/// The quoted or bare name of a definition (`def "foo bar"`), without quotes.
pub fn definition_name<'a>(st: St<'_, 'a>, span: Span) -> PResult<Spanned<Cow<'a, str>>> {
    let lit = value::string_lit(st, span)?;
    Ok(Spanned::new(lit.value, span))
}
