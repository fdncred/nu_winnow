//! A flat, source-ordered view of the AST.
//!
//! [`flatten`] walks the tree and emits `(span, shape)` pairs in source order,
//! in the spirit of `nu-parser`'s `flatten_block`. This is the representation
//! `nufmt` and syntax highlighters consume: every byte of significant source
//! text is covered by exactly one shape, and gaps between shapes are
//! whitespace, comments or punctuation belonging to the enclosing construct.
//!
//! Comments are emitted as [`FlatShape::Comment`].

use crate::ast::*;
use crate::span::Span;

/// The syntactic role of a piece of source text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[non_exhaustive]
pub enum FlatShape {
    /// `and`, `or`, `xor`, `not`.
    Boolean,
    /// A binary literal.
    Binary,
    /// The braces of a block.
    Block,
    /// `true` / `false`.
    Bool,
    /// The braces and pipes of a closure.
    Closure,
    /// A comment.
    Comment,
    /// A datetime literal.
    DateTime,
    /// The name of a definition (`def name`).
    Definition,
    /// An external command name.
    External,
    /// An external command argument.
    ExternalArg,
    /// A filesize literal.
    Filesize,
    /// A duration literal.
    Duration,
    /// `--flag` / `-f`.
    Flag,
    /// A float literal.
    Float,
    /// Text that failed to parse.
    Garbage,
    /// An integer literal.
    Int,
    /// An internal command name.
    InternalCall,
    /// A statement keyword (`let`, `def`, `if`, ...).
    Keyword,
    /// The brackets and commas of a list.
    List,
    /// A match pattern.
    MatchPattern,
    /// `null`.
    Nothing,
    /// A binary or assignment operator.
    Operator,
    /// `|`.
    Pipe,
    /// A range operator.
    Range,
    /// The braces of a record and the `:` of its entries.
    Record,
    /// A redirection operator.
    Redirection,
    /// A signature and its parameters.
    Signature,
    /// A string literal (any quoting).
    String,
    /// The literal parts of an interpolated string.
    StringInterpolation,
    /// The brackets of a table.
    Table,
    /// A type annotation.
    Type,
    /// A variable reference.
    Variable,
    /// A variable declaration (`let x`, closure parameters).
    VarDecl,
    /// A cell-path member.
    CellPath,
    /// `@attribute`.
    Attribute,
    /// Text nu-parser accepts and discards (see [`Ast::ignored`]).
    Ignored,
}

/// Flatten an AST into source-ordered `(span, shape)` pairs.
pub fn flatten(ast: &Ast<'_>) -> Vec<(Span, FlatShape)> {
    let mut f = Flattener { src: ast.source, out: Vec::new() };
    f.visit_block(&ast.block);
    // Comments and ignored text win over the shapes of the constructs they
    // sit in (the gaps of a list or record), so cut them out of every other shape.
    let mut cuts: Vec<(Span, FlatShape)> = ast.comments.iter().map(|c| (c.span, FlatShape::Comment)).collect();
    cuts.extend(ast.ignored.iter().map(|s| (*s, FlatShape::Ignored)));
    cuts.sort_by_key(|(s, _)| (s.start, s.end));
    let mut out = Vec::with_capacity(f.out.len() + cuts.len());
    for (span, shape) in f.out {
        let mut start = span.start;
        for (c, _) in cuts.iter().filter(|(c, _)| c.start < span.end && span.start < c.end) {
            if c.start > start {
                out.push((Span::new(start, c.start), shape));
            }
            start = start.max(c.end);
        }
        if start < span.end {
            out.push((Span::new(start, span.end), shape));
        }
    }
    out.extend(cuts);
    out.sort_by_key(|(s, _)| (s.start, s.end));
    out.dedup();
    out
}

struct Flattener<'s> {
    src: &'s str,
    out: Vec<(Span, FlatShape)>,
}

impl Flattener<'_> {
    fn push(&mut self, span: Span, shape: FlatShape) {
        if !span.is_empty() {
            self.out.push((span, shape));
        }
    }

    /// The declaration of a variable: the name plus a leading `$` and a
    /// trailing `:` when they are written (`let $x: int`, `for x: int in`).
    fn var_decl(&mut self, name: Span) {
        let start = if self.src[..name.start].ends_with('$') { name.start - 1 } else { name.start };
        let end = if self.src[name.end..].starts_with(':') { name.end + 1 } else { name.end };
        self.push(Span::new(start, end), FlatShape::VarDecl);
    }

    /// Emit `shape` for the parts of `outer` not covered by `inner` spans
    /// (the delimiters and punctuation of a collection).
    fn gaps(&mut self, outer: Span, inner: impl Iterator<Item = Span>, shape: FlatShape) {
        let mut cursor = outer.start;
        for s in inner {
            if s.start > cursor {
                self.push(Span::new(cursor, s.start), shape);
            }
            cursor = cursor.max(s.end);
        }
        if outer.end > cursor {
            self.push(Span::new(cursor, outer.end), shape);
        }
    }

    fn args(&mut self, args: &[Arg<'_>]) {
        for arg in args {
            match arg {
                Arg::Positional(e) => self.visit_expr(e),
                Arg::Flag(f) => match &f.value {
                    Some(v) => {
                        self.push(Span::new(f.span.start, v.span.start), FlatShape::Flag);
                        self.visit_expr(v);
                    }
                    None => self.push(f.span, FlatShape::Flag),
                },
                Arg::Spread { dots, expr } => {
                    self.push(*dots, FlatShape::Operator);
                    self.visit_expr(expr);
                }
                Arg::EndOfOptions(s) => self.push(*s, FlatShape::Flag),
            }
        }
    }

    fn members(&mut self, members: &[PathMember<'_>]) {
        for m in members {
            self.push(m.span, FlatShape::CellPath);
        }
    }

    fn block_braces(&mut self, expr_span: Span, block: &Block<'_>, shape: FlatShape) {
        self.push(Span::new(expr_span.start, block.span.start), shape);
        self.visit_block(block);
        self.push(Span::new(block.span.end, expr_span.end), shape);
    }
}

impl<'a> Visitor<'a> for Flattener<'_> {
    fn visit_element(&mut self, element: &PipelineElement<'a>) {
        // After `e>|` the "pipe" is the redirection operator, already emitted.
        if let Some(p) = element.pipe
            && &self.src[p.range()] == "|"
        {
            self.push(p, FlatShape::Pipe);
        }
        walk_element(self, element);
    }

    fn visit_redirection(&mut self, redirection: &Redirection<'a>) {
        let mut target = |t: &RedirectTarget<'a>| {
            self.push(t.op_span(), FlatShape::Redirection);
            if let RedirectTarget::File { path, .. } = t {
                self.visit_expr(path);
            }
        };
        match redirection {
            Redirection::Single { target: t, .. } => target(t),
            Redirection::Separate { out, err } => {
                target(out);
                target(err);
            }
        }
    }

    fn visit_signature(&mut self, sig: &Signature<'a>) {
        for p in &sig.params {
            match &p.kind {
                ParamKind::Flag { .. } => self.push(p.name.span, FlatShape::Flag),
                _ => self.push(p.name.span, FlatShape::VarDecl),
            }
            if let Some(ty) = &p.ty {
                self.push(ty.span, FlatShape::Type);
            }
            if let Some(c) = p.completer {
                self.push(c.span, FlatShape::InternalCall);
            }
            if let Some(d) = &p.default {
                self.visit_expr(d);
            }
        }
        for io in &sig.io_types {
            self.push(io.input.span, FlatShape::Type);
            self.push(io.arrow, FlatShape::Operator);
            self.push(io.output.span, FlatShape::Type);
        }
        let covered: Vec<Span> = self
            .out
            .iter()
            .filter(|(s, _)| sig.span.start <= s.start && s.end <= sig.span.end)
            .map(|(s, _)| *s)
            .collect();
        let mut covered = covered;
        covered.sort_by_key(|s| s.start);
        self.gaps(sig.span, covered.into_iter(), FlatShape::Signature);
    }

    fn visit_pattern(&mut self, pattern: &Pattern<'a>) {
        match &pattern.kind {
            PatternKind::Value(e) => self.visit_expr(e),
            _ => self.push(pattern.span, FlatShape::MatchPattern),
        }
    }

    fn visit_expr(&mut self, e: &Expr<'a>) {
        let span = e.span;
        // Keyword statements (`let`, `if`, ...) start with their keyword.
        let kw = e.keyword_span().unwrap_or(Span::point(span.start));
        if !kw.is_empty() {
            self.push(kw, FlatShape::Keyword);
        }
        match &e.kind {
            ExprKind::Bool(_) => self.push(span, FlatShape::Bool),
            ExprKind::Nothing => self.push(span, FlatShape::Nothing),
            ExprKind::Int(_) => self.push(span, FlatShape::Int),
            ExprKind::Float(_) => self.push(span, FlatShape::Float),
            ExprKind::String(_) => self.push(span, FlatShape::String),
            ExprKind::Interpolation(i) => {
                let mut inner = Vec::new();
                for part in &i.parts {
                    match part {
                        InterpPart::Text { span, .. } => {
                            self.push(*span, FlatShape::StringInterpolation);
                            inner.push(*span);
                        }
                        InterpPart::Expr(e) => {
                            self.visit_expr(e);
                            inner.push(e.span);
                        }
                    }
                }
                self.gaps(span, inner.into_iter(), FlatShape::StringInterpolation);
            }
            ExprKind::Binary(_) => self.push(span, FlatShape::Binary),
            ExprKind::Duration(_) => self.push(span, FlatShape::Duration),
            ExprKind::Filesize(_) => self.push(span, FlatShape::Filesize),
            ExprKind::DateTime(_) => self.push(span, FlatShape::DateTime),
            ExprKind::Range(r) => {
                if let Some(f) = &r.from {
                    self.visit_expr(f);
                }
                if let Some(s) = r.next_op_span {
                    self.push(s, FlatShape::Range);
                }
                if let Some(n) = &r.next {
                    self.visit_expr(n);
                }
                self.push(r.op_span, FlatShape::Range);
                if let Some(t) = &r.to {
                    self.visit_expr(t);
                }
            }
            ExprKind::Var(_) => self.push(span, FlatShape::Variable),
            ExprKind::CellPath(c) => {
                self.push(Span::new(span.start, span.start + 2), FlatShape::CellPath);
                self.members(&c.members);
            }
            ExprKind::FullCellPath(p) => {
                self.visit_expr(&p.head);
                self.members(&p.members);
            }
            ExprKind::List(items) => {
                let spans: Vec<Span> = items.iter().map(ListItem::span).collect();
                for item in items {
                    match item {
                        ListItem::Item(e) => self.visit_expr(e),
                        ListItem::Spread { dots, expr } => {
                            self.push(*dots, FlatShape::Operator);
                            self.visit_expr(expr);
                        }
                    }
                }
                self.gaps(span, spans.into_iter(), FlatShape::List);
            }
            ExprKind::Table(t) => {
                self.visit_expr(&t.columns);
                for row in &t.rows {
                    self.visit_expr(row);
                }
                let spans = std::iter::once(t.columns.span).chain(t.rows.iter().map(|r| r.span));
                self.gaps(span, spans, FlatShape::Table);
            }
            ExprKind::Record(items) => {
                let mut spans = Vec::new();
                for item in items {
                    match item {
                        RecordItem::Pair { key, colon, value } => {
                            self.visit_expr(key);
                            self.push(*colon, FlatShape::Record);
                            self.visit_expr(value);
                            spans.push(key.span);
                            spans.push(*colon);
                            spans.push(value.span);
                        }
                        RecordItem::Spread { dots, expr } => {
                            self.push(*dots, FlatShape::Operator);
                            self.visit_expr(expr);
                            spans.push(*dots);
                            spans.push(expr.span);
                        }
                    }
                }
                self.gaps(span, spans.into_iter(), FlatShape::Record);
            }
            ExprKind::Closure(c) => {
                let body_start = c.params.as_ref().map_or(c.body.span.start, |p| p.span.start);
                self.push(Span::new(span.start, body_start), FlatShape::Closure);
                if let Some(sig) = &c.params {
                    self.visit_signature(sig);
                }
                self.visit_block(&c.body);
                self.push(Span::new(c.body.span.end, span.end), FlatShape::Closure);
            }
            ExprKind::Block(b) => self.block_braces(span, b, FlatShape::Block),
            ExprKind::Subexpression(b) => self.block_braces(span, b, FlatShape::Block),
            ExprKind::BinaryOp(b) => {
                self.visit_expr(&b.lhs);
                let shape =
                    if matches!(b.op.item, Operator::Boolean(_)) { FlatShape::Boolean } else { FlatShape::Operator };
                self.push(b.op.span, shape);
                self.visit_expr(&b.rhs);
            }
            ExprKind::UnaryNot(n) => {
                self.push(n.not_span, FlatShape::Boolean);
                self.visit_expr(&n.expr);
            }
            ExprKind::Assignment(a) => {
                self.visit_expr(&a.lhs);
                self.push(a.op.span, FlatShape::Operator);
                self.visit_block(&a.rhs);
            }
            ExprKind::Call(c) => {
                match c.sigil {
                    Some(sigil) if sigil.end == c.head.span.start => {
                        self.push(sigil.merge(c.head.span), FlatShape::InternalCall);
                    }
                    Some(sigil) => {
                        self.push(sigil, FlatShape::InternalCall);
                        self.push(c.head.span, FlatShape::InternalCall);
                    }
                    None => self.push(c.head.span, FlatShape::InternalCall),
                }
                self.args(&c.args);
            }
            ExprKind::DynamicCall(d) => {
                self.push(d.sigil, FlatShape::InternalCall);
                self.visit_expr(&d.head);
                self.args(&d.args);
            }
            ExprKind::ExternalCall(c) => {
                self.push(c.caret, FlatShape::External);
                match &c.head.kind {
                    ExprKind::String(_) => self.push(c.head.span, FlatShape::External),
                    _ => self.visit_expr(&c.head),
                }
                for arg in &c.args {
                    match arg {
                        ExternalArg::Regular(e) => match &e.kind {
                            ExprKind::String(_) => self.push(e.span, FlatShape::ExternalArg),
                            _ => self.visit_expr(e),
                        },
                        ExternalArg::Spread { dots, expr } => {
                            self.push(*dots, FlatShape::Operator);
                            self.visit_expr(expr);
                        }
                    }
                }
            }
            ExprKind::EnvShorthand(e) => {
                for v in &e.vars {
                    self.push(Span::new(v.span.start, v.value.span.start), FlatShape::VarDecl);
                    self.visit_expr(&v.value);
                }
                self.visit_expr(&e.expr);
            }
            ExprKind::AttributeBlock(a) => {
                for attr in &a.attributes {
                    self.push(Span::new(attr.span.start, attr.name.span.end), FlatShape::Attribute);
                    self.args(&attr.args);
                }
                self.visit_expr(&a.item);
            }
            ExprKind::Let(b) | ExprKind::Mut(b) | ExprKind::Const(b) => {
                self.var_decl(b.name.span);
                if let Some(ty) = &b.ty {
                    self.push(ty.span, FlatShape::Type);
                }
                if let Some(eq) = b.eq {
                    self.push(eq, FlatShape::Operator);
                }
                if let Some(value) = &b.value {
                    self.visit_block(value);
                }
            }
            ExprKind::Def(d) => {
                for f in &d.flags {
                    self.push(f.span, FlatShape::Flag);
                }
                self.push(d.name.span, FlatShape::Definition);
                self.visit_signature(&d.signature);
                match &d.body_params {
                    // `def f [] {|x| }`: like a closure, the braces around the parameters.
                    Some(p) => {
                        self.push(Span::new(d.signature.span.end, p.span.start), FlatShape::Block);
                        self.visit_signature(p);
                        self.visit_block(&d.body);
                        self.push(Span::new(d.body.span.end, span.end), FlatShape::Block);
                    }
                    None => self.block_braces(Span::new(d.signature.span.end, span.end), &d.body, FlatShape::Block),
                }
            }
            ExprKind::Extern(x) => {
                self.push(x.name.span, FlatShape::Definition);
                self.visit_signature(&x.signature);
            }
            ExprKind::Alias(a) => {
                self.push(a.name.span, FlatShape::Definition);
                self.push(a.eq, FlatShape::Operator);
                if let Some(value) = &a.value {
                    self.visit_expr(value);
                }
            }
            ExprKind::Use(u) => {
                self.visit_expr(&u.module);
                for m in &u.members {
                    match &m.kind {
                        UseMemberKind::List(names) => {
                            for n in names {
                                self.push(n.span, FlatShape::String);
                            }
                            self.gaps(m.span, names.iter().map(|n| n.span), FlatShape::List);
                        }
                        UseMemberKind::Ignored(e) => {
                            self.visit_expr(e);
                            self.gaps(m.span, std::iter::once(e.span), FlatShape::Ignored);
                        }
                        _ => self.push(m.span, FlatShape::String),
                    }
                }
            }
            ExprKind::Module(m) => {
                self.visit_expr(&m.name);
                if let Some(b) = &m.body {
                    self.block_braces(Span::new(m.name.span.end, span.end), b, FlatShape::Block);
                }
            }
            ExprKind::Export(x) => {
                self.visit_expr(&x.item);
            }
            ExprKind::ExportEnv(x) => {
                self.block_braces(Span::new(kw.end, span.end), &x.body, FlatShape::Block);
            }
            ExprKind::If(i) => {
                self.visit_expr(&i.condition);
                let then_end = i.else_branch.as_ref().map_or(span.end, |e| e.keyword.start);
                self.block_braces(Span::new(i.condition.span.end, then_end), &i.then_block, FlatShape::Block);
                if let Some(e) = &i.else_branch {
                    self.push(e.keyword, FlatShape::Keyword);
                    self.visit_expr(&e.body);
                }
            }
            ExprKind::Match(m) => {
                self.visit_expr(&m.value);
                let mut inner = Vec::new();
                for arm in &m.arms {
                    self.visit_pattern(&arm.pattern);
                    inner.push(arm.pattern.span);
                    if let Some(g) = &arm.guard {
                        self.visit_expr(g);
                        inner.push(g.span);
                    }
                    self.push(arm.arrow, FlatShape::Operator);
                    inner.push(arm.arrow);
                    self.visit_expr(&arm.body);
                    inner.push(arm.body.span);
                }
                match &m.value_block {
                    Some(b) => self.visit_expr(b),
                    None => self.gaps(m.block_span, inner.into_iter(), FlatShape::Block),
                }
            }
            ExprKind::For(f) => {
                self.var_decl(f.var.span);
                if let Some(ty) = &f.ty {
                    self.push(ty.span, FlatShape::Type);
                }
                self.push(f.in_keyword, FlatShape::Keyword);
                self.visit_expr(&f.iterable);
                self.block_braces(Span::new(f.iterable.span.end, span.end), &f.body, FlatShape::Block);
            }
            ExprKind::While(w) => {
                self.visit_expr(&w.condition);
                self.block_braces(Span::new(w.condition.span.end, span.end), &w.body, FlatShape::Block);
            }
            ExprKind::Loop(l) => {
                self.block_braces(Span::new(kw.end, span.end), &l.body, FlatShape::Block);
            }
            ExprKind::Break | ExprKind::Continue => {}
            ExprKind::Return(r) => {
                if let Some(v) = &r.value {
                    self.visit_expr(v);
                }
            }
            ExprKind::Try(t) => {
                let body_end = t.handlers.first().map_or(span.end, |h| h.keyword.start);
                self.block_braces(Span::new(kw.end, body_end), &t.body, FlatShape::Block);
                for h in &t.handlers {
                    self.push(h.keyword, FlatShape::Keyword);
                    self.visit_expr(&h.body);
                }
            }
            ExprKind::Where(w) => {
                self.visit_expr(&w.condition);
            }
            ExprKind::Garbage => self.push(span, FlatShape::Garbage),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_cover_source_in_order_without_overlap() {
        let src = "let x = [1 2] | each {|i| $i + 1 } # c\ndef f [a: int] { $a }";
        let ast = crate::parse(src).unwrap();
        let shapes = flatten(&ast);
        let mut last_end = 0;
        for (span, shape) in &shapes {
            assert!(span.start >= last_end, "overlap at {span} ({shape:?})");
            last_end = span.end;
        }
        let covered: usize = shapes.iter().map(|(s, _)| s.len()).sum();
        let significant = src.bytes().filter(|b| !b.is_ascii_whitespace()).count();
        assert!(covered >= significant, "covered {covered} of {significant} significant bytes");
        assert!(shapes.iter().any(|(_, s)| *s == FlatShape::Keyword));
        assert!(shapes.iter().any(|(_, s)| *s == FlatShape::Comment));
        assert!(shapes.iter().any(|(_, s)| *s == FlatShape::Closure));
    }
}
