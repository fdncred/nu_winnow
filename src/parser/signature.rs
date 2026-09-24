//! Signatures (`[a: int, --flag(-f), ...rest]`), type annotations and
//! input/output type lists.

use std::borrow::Cow;

use crate::ast::{Comment, IoType, Param, ParamKind, Signature, TypeAnnotation, TypeField, TypeKind};
use crate::error::{Diagnostic, ErrorKind};
use crate::input::{PResult, cut};
use crate::lexer::{LexOptions, Token, TokenKind};
use crate::span::{Span, Spanned};

use super::cellpath::is_identifier;
use super::statement::check_variable_name;
use super::value::{self, Hint};
use super::{St, strings};

/// Parse a `[...]` or `(...)` signature item. `external` is set for an
/// `extern`, whose parameters declare no variables: nu then checks no
/// reserved names and never parses default values.
pub fn parse_signature<'a>(st: St<'_, 'a>, span: Span, external: bool) -> PResult<Signature<'a>> {
    let text = st.text(span);
    let close = match text.as_bytes().first() {
        Some(b'[') => "]",
        Some(b'(') => ")",
        _ => return Err(cut(Diagnostic::expected("signature", span))),
    };
    if text.len() < 2 || !text.ends_with(close) {
        return Err(cut(Diagnostic::new(
            ErrorKind::Unclosed { delimiter: close, open: Span::new(span.start, span.start + 1) },
            span.past(),
        )));
    }
    parse_signature_inner(st, Span::new(span.start + 1, span.end - 1), span, external)
}

/// Parse the parameters in `inner` (the text between the delimiters); `outer`
/// becomes the signature's span.
pub fn parse_signature_inner<'a>(st: St<'_, 'a>, inner: Span, outer: Span, external: bool) -> PResult<Signature<'a>> {
    let tokens = st.lex_span(inner, LexOptions::SIGNATURE).map_err(cut)?;
    let params = parse_params(st, &tokens, external).map_err(|e| e.map(|d| d.with_context("signature")))?;
    check_params(&params, outer)?;
    Ok(Signature { span: outer, params, io_types: Vec::new(), io_span: None })
}

/// nu's checks over the finished list: a required parameter after an
/// optional one and more than one rest parameter are errors.
fn check_params(params: &[Param<'_>], span: Span) -> PResult<()> {
    let mut optional_seen = false;
    let mut rest_seen = false;
    for p in params {
        match p.kind {
            ParamKind::Positional { optional } if optional || p.default.is_some() => optional_seen = true,
            ParamKind::Positional { .. } if optional_seen => {
                return Err(cut(Diagnostic::message(
                    format!("required positional parameter `{}` after an optional parameter", p.name.item),
                    p.span,
                )
                .with_help("move the required parameter before the optional ones")));
            }
            ParamKind::Rest if rest_seen => {
                return Err(cut(Diagnostic::message("multiple rest params", span)
                    .with_help("a signature can have only one `...rest` parameter")));
            }
            ParamKind::Rest => rest_seen = true,
            _ => {}
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Arg,
    AfterComma,
    Type,
    AfterType,
    Default,
}

fn parse_params<'a>(st: St<'_, 'a>, tokens: &[Token], external: bool) -> PResult<Vec<Param<'a>>> {
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
                    p.description.push(Comment { span: tok.span });
                }
                continue;
            }
            // nu skips every token that is not an item: pipes, `;`, redirections.
            TokenKind::Pipe | TokenKind::PipePipe | TokenKind::Semicolon | TokenKind::Redirect(_) => continue,
            TokenKind::Item | TokenKind::Assign(_) => {}
            TokenKind::Eol | TokenKind::Eof => continue,
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
                    params.push(new_param(st, tok, external)?);
                    mode = Mode::Arg;
                }
                Mode::Type => {
                    // `[: int]`: nu silently drops a type with no parameter before it.
                    if params.is_empty() {
                        mode = Mode::AfterType;
                        continue;
                    }
                    let (ty, completer) = parse_type_with_completer(st, tok.span)?;
                    let Some(p) = params.last_mut() else { unreachable!("checked above") };
                    if let ParamKind::Flag { .. } = p.kind
                        && ty.kind == TypeKind::Bool
                    {
                        return Err(cut(Diagnostic::message(
                            "type annotations are not allowed for boolean switches",
                            tok.span,
                        )
                        .with_help("remove the `: bool` type annotation")));
                    }
                    p.ty = Some(ty);
                    p.completer = completer;
                    p.span = p.span.merge(tok.span);
                    mode = Mode::AfterType;
                }
                Mode::Default => {
                    // `[= 1]`: nu silently drops a default with no parameter before it.
                    if params.is_empty() {
                        mode = Mode::Arg;
                        continue;
                    }
                    if external {
                        // nu never parses the default values of an `extern` signature.
                        st.ignore(tok.span);
                        mode = Mode::Arg;
                        continue;
                    }
                    let Some(p) = params.last_mut() else { unreachable!("checked above") };
                    if let ParamKind::Rest = p.kind {
                        return Err(cut(Diagnostic::message("rest parameter was given a default value", tok.span)
                            .with_help("a `...rest` parameter can't have a default value")));
                    }
                    // The default is parsed with the declared shape (`[x: int = abc]` is an error).
                    let default = match &p.ty {
                        Some(ty) => value::value(st, tok.span, Hint::Typed(&ty.kind))?,
                        None => value::value(st, tok.span, Hint::Any)?,
                    };
                    p.default = Some(default);
                    p.span = p.span.merge(tok.span);
                    mode = Mode::Arg;
                }
            },
        }
    }
    // Like nu, a `:` or `=` that is not the last token (a comment may follow
    // it) leaves the list as it is: `[x: # c\n]` is a parameter without a type.
    Ok(params)
}

fn new_param<'a>(st: St<'_, 'a>, tok: &Token, external: bool) -> PResult<Param<'a>> {
    let text = st.tok(tok);
    let span = tok.span;
    let base = Param {
        span,
        kind: ParamKind::Positional { optional: false },
        name: Spanned::new("", span),
        ty: None,
        default: None,
        completer: None,
        description: Vec::new(),
    };
    // A parameter declares a variable, whose name may not be a reserved one
    // (`in`, `nu`, `env`, `ans`); an extern's parameters declare nothing.
    let declare = |name: &str, span: Span| if external { Ok(()) } else { check_variable_name(name, span) };
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
        let variable = long.replace('-', "_");
        if !is_identifier(&variable) {
            return Err(cut(Diagnostic::expected("valid name for this long flag", span)));
        }
        let long_span = Span::new(span.start + 2, span.start + 2 + long.len());
        declare(&variable, long_span)?;
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
        // `-.` and `--`: the letter must be an identifier byte.
        if !is_identifier(short) {
            return Err(cut(Diagnostic::expected("valid variable name for this short flag", span)));
        }
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
        let name_span = Span::new(span.start, span.end - 1);
        declare(name, name_span)?;
        return Ok(Param { kind: ParamKind::Positional { optional: true }, name: Spanned::new(name, name_span), ..base });
    }
    if let Some(name) = text.strip_prefix("...") {
        if !is_identifier(name) {
            return Err(cut(Diagnostic::expected("valid variable name for this rest parameter", span)));
        }
        let name_span = Span::new(span.start + 3, span.end);
        declare(name, name_span)?;
        return Ok(Param { kind: ParamKind::Rest, name: Spanned::new(name, name_span), ..base });
    }
    if !is_identifier(text) {
        return Err(cut(Diagnostic::expected("valid variable name for this parameter", span)));
    }
    declare(text, span)?;
    Ok(Param { name: Spanned::new(text, span), ..base })
}

/// Parse a type token that may carry a `@completer` suffix. Like nu, the
/// split is at the first `@` wherever it is (`record<a@b: int>` is then an
/// unclosed `record<`), and an empty type before the `@` is unknown.
pub fn parse_type_with_completer<'a>(
    st: St<'_, 'a>,
    span: Span,
) -> PResult<(TypeAnnotation<'a>, Option<Spanned<&'a str>>)> {
    let text = st.text(span);
    let (type_text, completer) = match text.find('@') {
        Some(at) => (&text[..at], Some(Spanned::new(&text[at + 1..], Span::new(span.start + at + 1, span.end)))),
        None => (text, None),
    };
    let ty = parse_type(st, Span::new(span.start, span.start + type_text.len()))?;
    if let Some(completer) = completer {
        check_completer(st, completer)?;
    }
    Ok((ty, completer))
}

/// A completer is the name of a command (bare or quoted) or a list of
/// values; a subexpression or a record cannot be one. Whether the command
/// exists is the consumer's business (it may come from a `use`d module).
fn check_completer(st: St<'_, '_>, completer: Spanned<&str>) -> PResult<()> {
    let text = completer.item;
    let not_a_name = || {
        cut(Diagnostic::message(
            "the parameter completer must be a string (the name of a command) or a list",
            completer.span,
        ))
    };
    match text.as_bytes().first() {
        None => Err(cut(Diagnostic::expected("completer after `@`", completer.span))),
        Some(b'[') => value::value(st, completer.span, Hint::Any).map(|_| ()),
        Some(b'$') => Ok(()),
        Some(b'(' | b'{') => Err(not_a_name()),
        Some(_) => match value::value(st, completer.span, Hint::String)?.kind {
            ExprKind::String(_) => Ok(()),
            _ => Err(not_a_name()),
        },
    }
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

/// The fields of `record<a: int, b>`. Like nu: every token must be an item
/// (a `;` or `|` is not a field name); stray commas are skipped; a name
/// followed by `:` needs a type token (which may be anything, even `,`:
/// `record<a:, b: int>` is an unknown type); a name without `:` has type `any`.
fn named_type_params<'a>(st: St<'_, 'a>, span: Span) -> PResult<Vec<TypeField<'a>>> {
    let tokens = st.lex_span(span, LexOptions::SIGNATURE).map_err(cut)?;
    let items: Vec<&Token> = tokens.iter().filter(|t| t.kind != TokenKind::Eof).collect();
    let mut fields = Vec::new();
    let mut idx = 0;
    while idx < items.len() {
        let name_tok = items[idx];
        if name_tok.kind != TokenKind::Item {
            return Err(cut(Diagnostic::message("annotation key not string", name_tok.span)
                .with_help("a field name must be a string")));
        }
        if st.tok(name_tok) == "," {
            idx += 1;
            continue;
        }
        let key = value::value(st, name_tok.span, Hint::String)?;
        let ExprKind::String(name) = key.kind else {
            return Err(cut(Diagnostic::message("annotation key not string", name_tok.span)
                .with_help("a field name must be a string")));
        };
        idx += 1;
        let ty = match items.get(idx).map(|t| st.tok(t)) {
            Some(":") => {
                idx += 1;
                let Some(ty_tok) = items.get(idx) else {
                    return Err(cut(Diagnostic::expected("type after colon", items[idx - 1].span)));
                };
                idx += 1;
                parse_type(st, ty_tok.span)?
            }
            Some(",") => {
                idx += 1;
                TypeAnnotation { span: name_tok.span.past(), kind: TypeKind::Any }
            }
            _ => TypeAnnotation { span: name_tok.span.past(), kind: TypeKind::Any },
        };
        fields.push(TypeField { name: Spanned::new(name.value, name_tok.span), ty });
    }
    Ok(fields)
}

use crate::ast::ExprKind;

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
    let lit = strings::string_lit(st, span)?;
    Ok(Spanned::new(lit.value, span))
}
