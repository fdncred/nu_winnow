//! A compact Nushell formatter built on `nu-winnow-parser`, in the spirit of
//! [`nufmt`](https://github.com/nushell/nufmt).
//!
//! The formatter walks the AST and re-emits the program with normalised
//! whitespace: one space between words, spaces around operators and pipes,
//! consistent indentation inside blocks, closures and multi-line collections,
//! and every comment kept in place. Atoms (numbers, strings, variables, cell
//! paths, flags) are copied verbatim from the source using their spans, so
//! quoting and escapes survive untouched.
//!
//! Layout decisions follow the source: a list, record, closure or pipeline
//! that was written on one line stays on one line; one that spanned several
//! lines is laid out one item per line. This keeps the formatter idempotent
//! and predictable.

use std::fmt::Write as _;

use nu_winnow_parser::ast::*;
use nu_winnow_parser::{ParseConfig, ParseError, Span, parse_with};

/// Formatting options.
#[derive(Clone, Debug)]
pub struct Options {
    /// Spaces per indentation level.
    pub indent: usize,
    /// Known extra command names (multi-word commands from modules).
    pub config: ParseConfig,
}

impl Default for Options {
    fn default() -> Self {
        Self { indent: 4, config: ParseConfig::new() }
    }
}

/// A change the formatter made beyond whitespace, reported to the user.
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    /// Byte offset in the source of the text the note is about.
    pub offset: usize,
    /// What was done.
    pub message: String,
}

/// Format a Nushell source text.
#[allow(dead_code)] // the binary reports notes; `tests/nufmt.rs` uses this one
pub fn format(src: &str, options: &Options) -> Result<String, ParseError> {
    format_with_notes(src, options).map(|(out, _)| out)
}

/// Format a Nushell source text and also return the [`Note`]s about
/// rewrites that go beyond whitespace (see [`Formatter::compact_comparison`]).
pub fn format_with_notes(src: &str, options: &Options) -> Result<(String, Vec<Note>), ParseError> {
    let ast = parse_with(src, &options.config)?;
    let mut f = Formatter {
        src,
        out: String::new(),
        indent: 0,
        options,
        comments: ast.comments.clone(),
        next_comment: 0,
        row_condition: false,
        notes: Vec::new(),
    };
    f.block_body(&ast.block, 0, src.len());
    f.flush_comments(src.len());
    let mut out = std::mem::take(&mut f.out);
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok((out, f.notes))
}

/// The comparison operators a bare word in a row condition may have been
/// written around without spaces, longest first so `>=` wins over `>`.
const COMPARISONS: [&str; 8] = ["==", "!=", "<=", ">=", "=~", "!~", "<", ">"];

/// Split `size>1kb` into `("size", ">", "1kb")`.
///
/// The left side must look like a column name (letters, digits, `_`, `-`,
/// `.`, `?`) and the right side must be non-empty and not start another
/// operator, so `a>>b` and `a==` are left alone.
fn split_compact_comparison(word: &str) -> Option<(&str, &str, &str)> {
    let at = word.find(['<', '>', '=', '!'])?;
    let (lhs, rest) = word.split_at(at);
    let op = COMPARISONS.iter().find(|op| rest.starts_with(*op))?;
    let rhs = &rest[op.len()..];
    let column_char = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '?');
    let ok = !lhs.is_empty()
        && lhs.chars().all(column_char)
        && !rhs.is_empty()
        && !rhs.starts_with(['<', '>', '=', '!', '~']);
    ok.then_some((lhs, op, rhs))
}

struct Formatter<'a> {
    src: &'a str,
    out: String,
    indent: usize,
    options: &'a Options,
    comments: Vec<Comment>,
    next_comment: usize,
    /// `true` while formatting the condition of a `where`.
    row_condition: bool,
    notes: Vec<Note>,
}

impl<'a> Formatter<'a> {
    // --- output helpers -------------------------------------------------------

    fn text(&self, span: Span) -> &'a str {
        span.slice(self.src)
    }

    fn newline(&mut self) {
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        self.out.push('\n');
    }

    fn indent_str(&mut self) {
        for _ in 0..self.indent * self.options.indent {
            self.out.push(' ');
        }
    }

    fn at_line_start(&self) -> bool {
        self.out.is_empty() || self.out.ends_with('\n')
    }

    /// Append a word, separated from the previous one by a space unless the
    /// previous character opens a bracket.
    fn word(&mut self, s: &str) {
        if self.at_line_start() {
            self.indent_str();
        } else if !self.out.ends_with([' ', '(', '[', '{']) {
            self.out.push(' ');
        }
        self.out.push_str(s);
    }

    /// Append without a separating space.
    fn glue(&mut self, s: &str) {
        if self.at_line_start() {
            self.indent_str();
        }
        self.out.push_str(s);
    }

    /// `true` if the source between `a` and `b` contains a newline.
    fn multiline_between(&self, a: usize, b: usize) -> bool {
        self.src[a.min(b)..b.max(a)].contains('\n')
    }

    fn spans_lines(&self, span: Span) -> bool {
        self.text(span).contains('\n')
    }

    /// `true` if a block or closure covering `outer` can be written on one line:
    /// it was on one line in the source and contains no comment.
    fn can_be_compact(&self, outer: Span) -> bool {
        !self.spans_lines(outer)
            && !self.comments.iter().any(|c| outer.start <= c.span.start && c.span.end <= outer.end)
    }

    /// `true` if at least one blank line separates two source positions.
    fn blank_line_between(&self, a: usize, b: usize) -> bool {
        let gap = &self.src[a.min(b)..b.max(a)];
        gap.split('\n').skip(1).any(|line| line.trim().is_empty()) && gap.matches('\n').count() >= 2
    }

    // --- comments -----------------------------------------------------------------

    /// Write, on their own lines, all comments that start before `pos` and have
    /// not been written yet.
    fn flush_comments(&mut self, pos: usize) {
        while self.next_comment < self.comments.len() && self.comments[self.next_comment].span.start < pos {
            let c = self.comments[self.next_comment];
            self.next_comment += 1;
            self.own_line_comment(c);
        }
    }

    fn own_line_comment(&mut self, c: Comment) {
        if !self.at_line_start() {
            self.newline();
        }
        self.indent_str();
        self.out.push_str(c.text(self.src).trim_end());
        self.newline();
    }

    /// Write the comments that sit on the same source line right after `pos`,
    /// appended to the current output line.
    fn trailing_comments(&mut self, pos: usize) {
        let line_end = self.src[pos..].find('\n').map_or(self.src.len(), |i| pos + i);
        while self.next_comment < self.comments.len() {
            let c = self.comments[self.next_comment];
            if c.span.start < pos || c.span.start > line_end {
                break;
            }
            self.next_comment += 1;
            while self.out.ends_with(' ') {
                self.out.pop();
            }
            if !self.at_line_start() {
                self.out.push_str("  ");
            }
            self.out.push_str(c.text(self.src).trim_end());
        }
    }

    // --- blocks and pipelines ----------------------------------------------------

    /// Emit the pipelines of a block whose source occupies `start..end`.
    fn block_body(&mut self, block: &Block<'a>, start: usize, end: usize) {
        let mut prev_end = start;
        for (i, pipeline) in block.pipelines.iter().enumerate() {
            let first_comment =
                self.comments.get(self.next_comment).map(|c| c.span.start).filter(|s| *s < pipeline.span.start);
            let next_start = first_comment.unwrap_or(pipeline.span.start);
            if i > 0 && self.blank_line_between(prev_end, next_start) && !self.out.ends_with("\n\n") {
                self.newline();
            }
            self.flush_comments(pipeline.span.start);
            self.pipeline(pipeline);
            let pipeline_end = pipeline.terminator.map_or(pipeline.span.end, |t| t.end);
            self.trailing_comments(pipeline_end);
            self.newline();
            prev_end = pipeline_end;
        }
        self.flush_comments(end);
    }

    /// `true` if any `|` of the pipeline starts a line in the source.
    fn pipeline_is_multiline(&self, pipeline: &Pipeline<'a>) -> bool {
        pipeline
            .elements
            .iter()
            .skip(1)
            .any(|e| e.pipe.is_some_and(|p| self.src[..p.start].trim_end_matches([' ', '\t']).ends_with('\n')))
    }

    fn pipeline(&mut self, pipeline: &Pipeline<'a>) {
        let multiline = self.pipeline_is_multiline(pipeline);
        for (i, element) in pipeline.elements.iter().enumerate() {
            if i > 0 {
                let pipe_text = element.pipe.map_or("|", |p| self.text(p));
                if multiline {
                    self.trailing_comments(pipeline.elements[i - 1].span.end);
                    self.newline();
                    self.indent += 1;
                    self.flush_comments(element.span.start);
                    self.word(pipe_text);
                    self.expr(&element.expr);
                    if let Some(r) = &element.redirection {
                        self.redirection(r);
                    }
                    self.indent -= 1;
                    continue;
                }
                self.word(pipe_text);
            }
            self.expr(&element.expr);
            if let Some(r) = &element.redirection {
                self.redirection(r);
            }
        }
        if pipeline.terminator.is_some() {
            self.glue(";");
        }
    }

    fn redirection(&mut self, r: &Redirection<'a>) {
        // A `e>|` target is the pipe of the next element and is written there.
        let target = |f: &mut Self, t: &RedirectTarget<'a>| {
            if let RedirectTarget::File { op, path, .. } = t {
                f.word(f.text(op.span));
                f.expr(path);
            }
        };
        match r {
            Redirection::Single { target: t, .. } => target(self, t),
            Redirection::Separate { out, err } => {
                target(self, out);
                target(self, err);
            }
        }
    }

    /// `{ ... }` for a block. Single-line if it was single-line in the source
    /// and holds at most one pipeline.
    fn braced_block(&mut self, block: &Block<'a>, outer: Span) {
        if block.pipelines.is_empty() && !self.spans_lines(outer) {
            self.word("{ }");
            return;
        }
        let compact = self.can_be_compact(outer) && block.pipelines.len() <= 1;
        self.word("{");
        if compact {
            self.out.push(' ');
            self.pipeline(&block.pipelines[0]);
            self.word("}");
        } else {
            self.newline();
            self.indent += 1;
            self.block_body(block, block.span.start, block.span.end);
            self.indent -= 1;
            self.glue("}");
        }
    }

    fn closure(&mut self, c: &Closure<'a>, outer: Span) {
        let compact = self.can_be_compact(outer) && c.body.pipelines.len() <= 1;
        self.word("{");
        if let Some(sig) = &c.params {
            let params = self.signature_inline(sig);
            self.glue(&format!("|{params}|"));
        }
        if c.body.pipelines.is_empty() {
            self.glue(" }");
            return;
        }
        if compact {
            self.out.push(' ');
            self.pipeline(&c.body.pipelines[0]);
            self.word("}");
        } else {
            self.newline();
            self.indent += 1;
            self.block_body(&c.body, c.body.span.start, c.body.span.end);
            self.indent -= 1;
            self.glue("}");
        }
    }

    fn subexpression(&mut self, block: &Block<'a>, outer: Span) {
        let multiline = self.spans_lines(outer)
            && (block.pipelines.len() > 1 || block.pipelines.iter().any(|p| self.pipeline_is_multiline(p)));
        self.word("(");
        if multiline {
            self.newline();
            self.indent += 1;
            self.block_body(block, block.span.start, block.span.end);
            self.indent -= 1;
            self.glue(")");
        } else {
            for (i, p) in block.pipelines.iter().enumerate() {
                if i > 0 {
                    self.glue("; ");
                }
                self.glued(|f| f.pipeline(p));
            }
            self.flush_comments(block.span.end);
            self.glue(")");
        }
    }

    // --- signatures ------------------------------------------------------------------

    fn param(&self, p: &Param<'a>) -> String {
        let mut s = String::new();
        match &p.kind {
            ParamKind::Positional { optional } => {
                s.push_str(p.name.item);
                if *optional {
                    s.push('?');
                }
            }
            ParamKind::Rest => {
                s.push_str("...");
                s.push_str(p.name.item);
            }
            ParamKind::Flag { long, short } => {
                if let Some(l) = long {
                    s.push_str("--");
                    s.push_str(l.item);
                    if let Some(sh) = short {
                        let _ = write!(s, "(-{})", sh.item);
                    }
                } else if let Some(sh) = short {
                    let _ = write!(s, "-{}", sh.item);
                }
            }
        }
        if let Some(ty) = &p.ty {
            let _ = write!(s, ": {}", self.text(ty.span));
        }
        if let Some(c) = p.completer {
            let _ = write!(s, "@{}", c.item);
        }
        if let Some(d) = &p.default {
            let _ = write!(s, " = {}", self.text(d.span));
        }
        s
    }

    fn signature_inline(&self, sig: &Signature<'a>) -> String {
        sig.params.iter().map(|p| self.param(p)).collect::<Vec<_>>().join(", ")
    }

    /// `[params]` plus `: in -> out` types.
    fn signature(&mut self, sig: &Signature<'a>) {
        let multiline = self.spans_lines(sig.span) && !sig.params.is_empty();
        if multiline {
            self.word("[");
            self.newline();
            self.indent += 1;
            for p in &sig.params {
                let text = self.param(p);
                self.flush_comments(p.span.start);
                self.indent_str();
                self.out.push_str(&text);
                if let Some(d) = &p.description {
                    self.out.push_str("  ");
                    self.out.push_str(d.text(self.src).trim_end());
                    while self.next_comment < self.comments.len()
                        && self.comments[self.next_comment].span.start <= d.span.start
                    {
                        self.next_comment += 1;
                    }
                }
                self.newline();
            }
            self.indent -= 1;
            self.glue("]");
        } else {
            let inline = self.signature_inline(sig);
            self.word(&format!("[{inline}]"));
        }
        if let Some(io) = sig.io_span {
            let types: Vec<String> = sig
                .io_types
                .iter()
                .map(|t| format!("{} -> {}", self.text(t.input.span), self.text(t.output.span)))
                .collect();
            if types.len() == 1 && !self.text(io).starts_with('[') {
                self.glue(&format!(": {}", types[0]));
            } else {
                self.glue(&format!(": [{}]", types.join(", ")));
            }
        }
        // Comments inside the signature were written as parameter descriptions.
        while self.next_comment < self.comments.len() && self.comments[self.next_comment].span.start < sig.span.end {
            self.next_comment += 1;
        }
    }

    // --- expressions ------------------------------------------------------------------

    fn args(&mut self, args: &[Arg<'a>]) {
        for arg in args {
            match arg {
                Arg::Positional(e) => self.expr(e),
                Arg::Flag(f) => {
                    let dashes = if f.long { "--" } else { "-" };
                    match &f.value {
                        Some(v) => {
                            self.word(&format!("{dashes}{}=", f.name));
                            self.glued(|f| f.expr(v));
                        }
                        None => self.word(&format!("{dashes}{}", f.name)),
                    }
                }
                Arg::Spread { expr, .. } => {
                    self.word("...");
                    self.glued(|f| f.expr(expr));
                }
                Arg::EndOfOptions(_) => self.word("--"),
            }
        }
    }

    /// Run `f` and remove the space it would put before its first word.
    fn glued(&mut self, f: impl FnOnce(&mut Self)) {
        let saved = self.out.len();
        f(self);
        if self.out[saved..].starts_with(' ') {
            self.out.remove(saved);
        }
    }

    fn items_multiline(&self, spans: &[Span], outer: Span) -> bool {
        self.spans_lines(outer)
            && (spans.len() > 1 || spans.windows(2).any(|w| self.multiline_between(w[0].end, w[1].start)))
    }

    fn uses_commas(&self, spans: &[Span]) -> bool {
        spans.windows(2).any(|w| self.src[w[0].end..w[1].start].contains(','))
    }

    fn list(&mut self, items: &[ListItem<'a>], outer: Span) {
        if items.is_empty() {
            self.word("[");
            self.flush_comments(outer.end);
            self.glue("]");
            return;
        }
        let spans: Vec<Span> = items.iter().map(ListItem::span).collect();
        let multiline = self.items_multiline(&spans, outer);
        let sep = if self.uses_commas(&spans) { ", " } else { " " };
        self.word("[");
        if multiline {
            self.newline();
            self.indent += 1;
            for item in items {
                self.flush_comments(item.span().start);
                self.list_item(item);
                self.trailing_comments(item.span().end);
                self.newline();
            }
            self.flush_comments(outer.end);
            self.indent -= 1;
            self.glue("]");
        } else {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    self.glue(sep);
                }
                self.glued(|f| f.list_item(item));
            }
            self.flush_comments(outer.end);
            self.glue("]");
        }
    }

    fn list_item(&mut self, item: &ListItem<'a>) {
        match item {
            ListItem::Item(e) => self.expr(e),
            ListItem::Spread { expr, .. } => {
                self.word("...");
                self.glued(|f| f.expr(expr));
            }
        }
    }

    fn record(&mut self, items: &[RecordItem<'a>], outer: Span) {
        if items.is_empty() {
            self.word("{");
            self.flush_comments(outer.end);
            self.glue("}");
            return;
        }
        let spans: Vec<Span> = items.iter().map(RecordItem::span).collect();
        let multiline = self.items_multiline(&spans, outer);
        self.word("{");
        if multiline {
            self.newline();
            self.indent += 1;
            for item in items {
                self.flush_comments(item.span().start);
                self.record_item(item);
                self.trailing_comments(item.span().end);
                self.newline();
            }
            self.flush_comments(outer.end);
            self.indent -= 1;
            self.glue("}");
        } else {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    self.glue(", ");
                }
                self.glued(|f| f.record_item(item));
            }
            self.flush_comments(outer.end);
            self.glue("}");
        }
    }

    fn record_item(&mut self, item: &RecordItem<'a>) {
        match item {
            RecordItem::Pair { key, value, .. } => {
                self.expr(key);
                self.glue(":");
                self.expr(value);
            }
            RecordItem::Spread { expr, .. } => {
                self.word("...");
                self.glued(|f| f.expr(expr));
            }
        }
    }

    fn table(&mut self, t: &Table<'a>, outer: Span) {
        let multiline = self.spans_lines(outer);
        self.word("[");
        self.glued(|f| f.expr(&t.columns));
        self.glue(";");
        if multiline {
            self.newline();
            self.indent += 1;
            for row in &t.rows {
                self.flush_comments(row.span.start);
                self.expr(row);
                self.trailing_comments(row.span.end);
                self.newline();
            }
            self.flush_comments(outer.end);
            self.indent -= 1;
            self.glue("]");
        } else {
            let row_spans: Vec<Span> = t.rows.iter().map(|r| r.span).collect();
            let sep = if self.uses_commas(&row_spans) { ", " } else { " " };
            for (i, row) in t.rows.iter().enumerate() {
                if i > 0 {
                    self.glue(sep);
                    self.glued(|f| f.expr(row));
                } else {
                    self.expr(row);
                }
            }
            self.glue("]");
        }
    }

    fn match_block(&mut self, m: &Match<'a>) {
        self.word("match");
        self.expr(&m.value);
        self.word("{");
        self.newline();
        self.indent += 1;
        for arm in &m.arms {
            self.flush_comments(arm.span.start);
            self.indent_str();
            self.out.push_str(self.text(arm.pattern.span));
            if let Some(g) = &arm.guard {
                self.word("if");
                self.expr(g);
            }
            self.word("=>");
            self.expr(&arm.body);
            self.trailing_comments(arm.span.end);
            self.newline();
        }
        self.flush_comments(m.block_span.end);
        self.indent -= 1;
        self.glue("}");
    }

    fn expr(&mut self, e: &Expr<'a>) {
        let span = e.span;
        match &e.kind {
            ExprKind::Bool(_)
            | ExprKind::Nothing
            | ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::String(_)
            | ExprKind::Binary(_)
            | ExprKind::Duration(_)
            | ExprKind::Filesize(_)
            | ExprKind::DateTime(_)
            | ExprKind::Var(_)
            | ExprKind::CellPath(_)
            | ExprKind::Interpolation(_)
            | ExprKind::Range(_)
            | ExprKind::Garbage => self.word(self.text(span)),
            ExprKind::FullCellPath(p) => {
                if p.implicit_head && p.members.len() == 1 && self.compact_comparison(span) {
                    return;
                }
                self.expr(&p.head);
                let tail = Span::new(p.head.span.end, span.end);
                self.glue(self.text(tail));
            }
            ExprKind::List(items) => self.list(items, span),
            ExprKind::Table(t) => self.table(t, span),
            ExprKind::Record(items) => self.record(items, span),
            ExprKind::Closure(c) => self.closure(c, span),
            ExprKind::Block(b) => self.braced_block(b, span),
            ExprKind::Subexpression(b) => self.subexpression(b, span),
            ExprKind::BinaryOp(b) => {
                let boolean = matches!(b.op.item, Operator::Boolean(_));
                self.row_condition_operand(&b.lhs, boolean);
                self.word(self.text(b.op.span));
                self.row_condition_operand(&b.rhs, boolean);
            }
            ExprKind::UnaryNot(n) => {
                self.word("not");
                self.expr(&n.expr);
            }
            ExprKind::Assignment(a) => {
                self.expr(&a.lhs);
                self.word(a.op.item.as_str());
                self.inline_block(&a.rhs);
            }
            ExprKind::Call(c) => {
                self.word(self.text(c.head.span));
                self.args(&c.args);
            }
            ExprKind::ExternalCall(c) => {
                self.word("^");
                self.glued(|f| f.expr(&c.head));
                for arg in &c.args {
                    match arg {
                        ExternalArg::Regular(e) => self.expr(e),
                        ExternalArg::Spread { expr, .. } => {
                            self.word("...");
                            self.glued(|f| f.expr(expr));
                        }
                    }
                }
            }
            ExprKind::EnvShorthand(e) => {
                for v in &e.vars {
                    self.word(self.text(v.span));
                }
                self.expr(&e.expr);
            }
            ExprKind::AttributeBlock(a) => {
                for attr in &a.attributes {
                    self.word(&format!("@{}", attr.name.item));
                    self.args(&attr.args);
                    self.trailing_comments(attr.span.end);
                    self.newline();
                }
                self.flush_comments(a.item.span.start);
                self.expr(&a.item);
            }
            ExprKind::Let(b) | ExprKind::Mut(b) | ExprKind::Const(b) => {
                self.word(e.kind.keyword().unwrap_or("let"));
                match &b.ty {
                    Some(ty) => self.word(&format!("{}: {}", b.name.item, self.text(ty.span))),
                    None => self.word(b.name.item),
                }
                if let Some(value) = &b.value {
                    self.word("=");
                    self.inline_block(value);
                }
            }
            ExprKind::Def(d) => {
                self.word("def");
                for f in &d.flags {
                    self.word(self.text(f.span));
                }
                self.word(self.text(d.name.span));
                self.signature(&d.signature);
                self.braced_block(&d.body, Span::new(d.signature.span.end, span.end));
            }
            ExprKind::Extern(x) => {
                self.word("extern");
                self.word(self.text(x.name.span));
                self.signature(&x.signature);
            }
            ExprKind::Alias(a) => {
                self.word("alias");
                self.word(self.text(a.name.span));
                self.word("=");
                self.expr(&a.value);
            }
            ExprKind::Use(u) => {
                self.word("use");
                self.expr(&u.module);
                for m in &u.members {
                    self.word(self.text(m.span));
                }
            }
            ExprKind::Module(m) => {
                self.word("module");
                self.expr(&m.name);
                if let Some(b) = &m.body {
                    self.braced_block(b, Span::new(m.name.span.end, span.end));
                }
            }
            ExprKind::Export(x) => {
                self.word("export");
                self.expr(&x.item);
            }
            ExprKind::ExportEnv(x) => {
                self.word("export-env");
                self.braced_block(&x.body, Span::new(span.start + "export-env".len(), span.end));
            }
            ExprKind::If(i) => {
                self.word("if");
                self.expr(&i.condition);
                let then_end = i.else_branch.as_ref().map_or(span.end, |e| e.keyword.start);
                self.braced_block(&i.then_block, Span::new(i.condition.span.end, then_end));
                if let Some(e) = &i.else_branch {
                    self.word("else");
                    self.expr(&e.body);
                }
            }
            ExprKind::Match(m) => self.match_block(m),
            ExprKind::For(f) => {
                self.word("for");
                match &f.ty {
                    Some(ty) => self.word(&format!("{}: {}", f.var.item, self.text(ty.span))),
                    None => self.word(f.var.item),
                }
                self.word("in");
                self.expr(&f.iterable);
                self.braced_block(&f.body, Span::new(f.iterable.span.end, span.end));
            }
            ExprKind::While(w) => {
                self.word("while");
                self.expr(&w.condition);
                self.braced_block(&w.body, Span::new(w.condition.span.end, span.end));
            }
            ExprKind::Loop(l) => {
                self.word("loop");
                self.braced_block(&l.body, Span::new(span.start + "loop".len(), span.end));
            }
            ExprKind::Break => self.word("break"),
            ExprKind::Continue => self.word("continue"),
            ExprKind::Return(r) => {
                self.word("return");
                if let Some(v) = &r.value {
                    self.expr(v);
                }
            }
            ExprKind::Try(t) => {
                self.word("try");
                let body_end = t.handlers.first().map_or(span.end, |h| h.keyword.start);
                self.braced_block(&t.body, Span::new(span.start + "try".len(), body_end));
                for h in &t.handlers {
                    self.word(self.text(h.keyword));
                    self.expr(&h.body);
                }
            }
            ExprKind::Where(w) => {
                self.word("where");
                let saved = std::mem::replace(&mut self.row_condition, true);
                self.expr(&w.condition);
                self.row_condition = saved;
            }
            _ => self.word(self.text(span)),
        }
    }

    /// An operand of a binary operator. In a row condition, a bare word that
    /// is the operand of `and`/`or`/`xor` (`where a > 1 and size>1kb`) is a
    /// string in Nushell, which the boolean operator always rejects, so it
    /// gets the same treatment as a bare column name.
    fn row_condition_operand(&mut self, e: &Expr<'a>, boolean: bool) {
        let bare_string = matches!(&e.kind, ExprKind::String(s) if s.quote == Quote::Bare);
        if self.row_condition && boolean && bare_string && self.compact_comparison(e.span) {
            return;
        }
        self.expr(e);
    }

    /// Write a bare word of a row condition that was written without spaces
    /// around a comparison (`size>1kb`) as the comparison it was meant to be
    /// (`size > 1kb`), and record a [`Note`]. Returns `false`, writing nothing,
    /// when the word is not of that shape.
    ///
    /// Nushell lexes `size>1kb` as one word and `where` then looks up a column
    /// literally named `size>1kb`; that is never what was meant, and nu itself
    /// answers "did you mean 'size'?".
    fn compact_comparison(&mut self, span: Span) -> bool {
        let word = self.text(span);
        let Some((lhs, op, rhs)) = split_compact_comparison(word) else { return false };
        self.word(lhs);
        self.word(op);
        self.word(rhs);
        self.notes.push(Note {
            offset: span.start,
            message: format!("`{word}` written as the comparison `{lhs} {op} {rhs}`"),
        });
        true
    }

    /// The right-hand side of `let`/assignment: pipelines written inline.
    fn inline_block(&mut self, block: &Block<'a>) {
        for (i, p) in block.pipelines.iter().enumerate() {
            if i > 0 {
                self.glue(";");
            }
            self.pipeline(p);
        }
    }
}
