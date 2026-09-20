//! A human-readable tree dump of the AST, used by the `parse` example and
//! handy in tests: `println!("{}", nu_winnow_parser::pretty::dump(&ast))`.

use std::fmt::Write;

use crate::ast::*;
use crate::span::Span;

/// Render the AST as an indented tree, one node per line, with spans.
pub fn dump(ast: &Ast<'_>) -> String {
    let mut p = Printer { src: ast.source, out: String::new(), depth: 0 };
    p.block("Block", &ast.block);
    if !ast.comments.is_empty() {
        p.line(format_args!("Comments ({})", ast.comments.len()));
        p.depth += 1;
        for c in &ast.comments {
            p.line(format_args!("{} {:?}", c.span, c.text(ast.source)));
        }
        p.depth -= 1;
    }
    p.out
}

/// Render a single expression as an indented tree.
pub fn dump_expr(source: &str, expr: &Expr<'_>) -> String {
    let mut p = Printer { src: source, out: String::new(), depth: 0 };
    p.expr(expr);
    p.out
}

struct Printer<'a> {
    src: &'a str,
    out: String,
    depth: usize,
}

impl<'a> Printer<'a> {
    fn line(&mut self, args: std::fmt::Arguments<'_>) {
        for _ in 0..self.depth {
            self.out.push_str("  ");
        }
        let _ = self.out.write_fmt(args);
        self.out.push('\n');
    }

    fn text(&self, span: Span) -> &'a str {
        span.slice(self.src)
    }

    fn nested(&mut self, f: impl FnOnce(&mut Self)) {
        self.depth += 1;
        f(self);
        self.depth -= 1;
    }

    fn block(&mut self, label: &str, block: &Block<'a>) {
        self.line(format_args!("{label} {}", block.span));
        self.nested(|p| {
            for pipeline in &block.pipelines {
                p.pipeline(pipeline);
            }
        });
    }

    fn pipeline(&mut self, pipeline: &Pipeline<'a>) {
        let comments = pipeline.leading_comments.len() + pipeline.trailing_comments.len();
        let mut extra = String::new();
        if comments > 0 {
            let _ = write!(extra, " comments={comments}");
        }
        if let Some(t) = pipeline.terminator {
            let _ = write!(extra, " terminator={t}");
        }
        self.line(format_args!("Pipeline {}{extra}", pipeline.span));
        self.nested(|p| {
            for element in &pipeline.elements {
                if let Some(pipe) = element.pipe {
                    p.line(format_args!("| {pipe}"));
                }
                p.expr(&element.expr);
                if let Some(r) = &element.redirection {
                    p.redirection(r);
                }
            }
        });
    }

    fn redirection(&mut self, r: &Redirection<'a>) {
        let target = |p: &mut Self, t: &RedirectTarget<'a>| match t {
            RedirectTarget::File { op, path, .. } => {
                p.line(format_args!("Redirect {:?} {}", op.item, op.span));
                p.nested(|p| p.expr(path));
            }
            RedirectTarget::Pipe { op } => p.line(format_args!("Redirect {:?} {}", op.item, op.span)),
        };
        match r {
            Redirection::Single { target: t, .. } => target(self, t),
            Redirection::Separate { out, err } => {
                target(self, out);
                target(self, err);
            }
        }
    }

    fn signature(&mut self, sig: &Signature<'a>) {
        self.line(format_args!("Signature {}", sig.span));
        self.nested(|p| {
            for param in &sig.params {
                let kind = match &param.kind {
                    ParamKind::Positional { optional: false } => "positional".to_string(),
                    ParamKind::Positional { optional: true } => "optional".to_string(),
                    ParamKind::Rest => "rest".to_string(),
                    ParamKind::Flag { long, short } => format!(
                        "flag{}{}",
                        long.map(|l| format!(" --{}", l.item)).unwrap_or_default(),
                        short.map(|s| format!(" -{}", s.item)).unwrap_or_default()
                    ),
                };
                let mut extra = String::new();
                if let Some(ty) = &param.ty {
                    let _ = write!(extra, " : {}", p.text(ty.span));
                }
                if let Some(c) = param.completer {
                    let _ = write!(extra, " @{}", c.item);
                }
                if let Some(d) = &param.description {
                    let _ = write!(extra, " desc={:?}", d.body(p.src));
                }
                p.line(format_args!("Param {kind} `{}` {}{extra}", param.name.item, param.span));
                if let Some(d) = &param.default {
                    p.nested(|p| {
                        p.line(format_args!("Default"));
                        p.nested(|p| p.expr(d));
                    });
                }
            }
            for io in &sig.io_types {
                p.line(format_args!("IoType {} -> {}", p.text(io.input.span), p.text(io.output.span)));
            }
        });
    }

    fn args(&mut self, args: &[Arg<'a>]) {
        for arg in args {
            match arg {
                Arg::Positional(e) => self.expr(e),
                Arg::Flag(f) => {
                    let dashes = if f.long { "--" } else { "-" };
                    self.line(format_args!("Flag {dashes}{} {}", f.name, f.span));
                    if let Some(v) = &f.value {
                        self.nested(|p| p.expr(v));
                    }
                }
                Arg::Spread { dots, expr } => {
                    self.line(format_args!("Spread {dots}"));
                    self.nested(|p| p.expr(expr));
                }
                Arg::EndOfOptions(s) => self.line(format_args!("EndOfOptions {s}")),
            }
        }
    }

    fn members(&mut self, members: &[PathMember<'a>]) {
        for m in members {
            let name = match &m.kind {
                PathMemberKind::Int(i) => i.to_string(),
                PathMemberKind::String(s) => format!("{s:?}"),
            };
            let opt = if m.optional { "?" } else { "" };
            let ins = if m.insensitive { "!" } else { "" };
            self.line(format_args!("Member {name}{opt}{ins} {}", m.span));
        }
    }

    fn pattern(&mut self, pat: &Pattern<'a>) {
        match &pat.kind {
            PatternKind::Value(e) => {
                self.line(format_args!("Pattern value {}", pat.span));
                self.nested(|p| p.expr(e));
            }
            PatternKind::Variable(v) => self.line(format_args!("Pattern ${v} {}", pat.span)),
            PatternKind::Wildcard => self.line(format_args!("Pattern _ {}", pat.span)),
            PatternKind::List(items) => {
                self.line(format_args!("Pattern list {}", pat.span));
                self.nested(|p| {
                    for i in items {
                        p.pattern(i);
                    }
                });
            }
            PatternKind::Record(fields) => {
                self.line(format_args!("Pattern record {}", pat.span));
                self.nested(|p| {
                    for (name, pattern) in fields {
                        p.line(format_args!("Field {:?}", name.item));
                        p.nested(|p| p.pattern(pattern));
                    }
                });
            }
            PatternKind::Rest(name) => {
                self.line(format_args!(
                    "Pattern rest {} {}",
                    name.map(|n| format!("${}", n.item)).unwrap_or_default(),
                    pat.span
                ));
            }
            PatternKind::Or(alts) => {
                self.line(format_args!("Pattern or {}", pat.span));
                self.nested(|p| {
                    for a in alts {
                        p.pattern(a);
                    }
                });
            }
        }
    }

    fn expr(&mut self, e: &Expr<'a>) {
        let span = e.span;
        match &e.kind {
            ExprKind::Bool(b) => self.line(format_args!("Bool {b} {span}")),
            ExprKind::Nothing => self.line(format_args!("Nothing {span}")),
            ExprKind::Int(i) => self.line(format_args!("Int {i} {span}")),
            ExprKind::Float(f) => self.line(format_args!("Float {f} {span}")),
            ExprKind::String(s) => self.line(format_args!("String {:?} {:?} {span}", s.quote, s.value)),
            ExprKind::Interpolation(i) => {
                self.line(format_args!("Interpolation {:?} {span}", i.quote));
                self.nested(|p| {
                    for part in &i.parts {
                        match part {
                            InterpPart::Text { span, value } => p.line(format_args!("Text {value:?} {span}")),
                            InterpPart::Expr(e) => p.expr(e),
                        }
                    }
                });
            }
            ExprKind::Binary(b) => self.line(format_args!("Binary radix={} {:?} {span}", b.radix, b.bytes)),
            ExprKind::Duration(d) => self.line(format_args!("Duration {} {} {span}", d.value, d.unit.as_str())),
            ExprKind::Filesize(f) => self.line(format_args!("Filesize {} {} {span}", f.value, f.unit.as_str())),
            ExprKind::DateTime(t) => self.line(format_args!("DateTime {t} {span}")),
            ExprKind::Range(r) => {
                self.line(format_args!("Range {:?} {span}", r.inclusion));
                self.nested(|p| {
                    if let Some(f) = &r.from {
                        p.line(format_args!("From"));
                        p.nested(|p| p.expr(f));
                    }
                    if let Some(n) = &r.next {
                        p.line(format_args!("Next"));
                        p.nested(|p| p.expr(n));
                    }
                    if let Some(t) = &r.to {
                        p.line(format_args!("To"));
                        p.nested(|p| p.expr(t));
                    }
                });
            }
            ExprKind::Var(v) => self.line(format_args!("Var ${} {span}", v.name)),
            ExprKind::CellPath(c) => {
                self.line(format_args!("CellPath {span}"));
                self.nested(|p| p.members(&c.members));
            }
            ExprKind::FullCellPath(f) => {
                let implicit = if f.implicit_head { " implicit-head" } else { "" };
                self.line(format_args!("FullCellPath{implicit} {span}"));
                self.nested(|p| {
                    p.expr(&f.head);
                    p.members(&f.members);
                });
            }
            ExprKind::List(items) => {
                self.line(format_args!("List {span}"));
                self.nested(|p| {
                    for item in items {
                        match item {
                            ListItem::Item(e) => p.expr(e),
                            ListItem::Spread { dots, expr } => {
                                p.line(format_args!("Spread {dots}"));
                                p.nested(|p| p.expr(expr));
                            }
                        }
                    }
                });
            }
            ExprKind::Table(t) => {
                self.line(format_args!("Table {span}"));
                self.nested(|p| {
                    p.line(format_args!("Columns"));
                    p.nested(|p| p.expr(&t.columns));
                    for row in &t.rows {
                        p.line(format_args!("Row"));
                        p.nested(|p| p.expr(row));
                    }
                });
            }
            ExprKind::Record(items) => {
                self.line(format_args!("Record {span}"));
                self.nested(|p| {
                    for item in items {
                        match item {
                            RecordItem::Pair { key, value, .. } => {
                                p.line(format_args!("Pair"));
                                p.nested(|p| {
                                    p.expr(key);
                                    p.expr(value);
                                });
                            }
                            RecordItem::Spread { dots, expr } => {
                                p.line(format_args!("Spread {dots}"));
                                p.nested(|p| p.expr(expr));
                            }
                        }
                    }
                });
            }
            ExprKind::Closure(c) => {
                self.line(format_args!("Closure {span}"));
                self.nested(|p| {
                    if let Some(sig) = &c.params {
                        p.signature(sig);
                    }
                    p.block("Body", &c.body);
                });
            }
            ExprKind::Block(b) => self.block(&format!("BlockExpr {span}"), b),
            ExprKind::Subexpression(b) => self.block(&format!("Subexpression {span}"), b),
            ExprKind::BinaryOp(b) => {
                self.line(format_args!("BinaryOp {} {span}", b.op.item));
                self.nested(|p| {
                    p.expr(&b.lhs);
                    p.expr(&b.rhs);
                });
            }
            ExprKind::UnaryNot(n) => {
                self.line(format_args!("Not {span}"));
                self.nested(|p| p.expr(&n.expr));
            }
            ExprKind::Assignment(a) => {
                self.line(format_args!("Assignment {} {span}", a.op.item.as_str()));
                self.nested(|p| {
                    p.expr(&a.lhs);
                    p.block("Value", &a.rhs);
                });
            }
            ExprKind::Call(c) => {
                self.line(format_args!("Call `{}` {span}", c.head.name));
                self.nested(|p| p.args(&c.args));
            }
            ExprKind::ExternalCall(c) => {
                self.line(format_args!("ExternalCall {span}"));
                self.nested(|p| {
                    p.expr(&c.head);
                    for arg in &c.args {
                        match arg {
                            ExternalArg::Regular(e) => p.expr(e),
                            ExternalArg::Spread { dots, expr } => {
                                p.line(format_args!("Spread {dots}"));
                                p.nested(|p| p.expr(expr));
                            }
                        }
                    }
                });
            }
            ExprKind::EnvShorthand(e) => {
                self.line(format_args!("EnvShorthand {span}"));
                self.nested(|p| {
                    for v in &e.vars {
                        p.line(format_args!("Env {} {}", v.name.item, v.span));
                        p.nested(|p| p.expr(&v.value));
                    }
                    p.expr(&e.expr);
                });
            }
            ExprKind::AttributeBlock(a) => {
                self.line(format_args!("AttributeBlock {span}"));
                self.nested(|p| {
                    for attr in &a.attributes {
                        p.line(format_args!("Attribute `{}` {}", attr.name.item, attr.span));
                        p.nested(|p| p.args(&attr.args));
                    }
                    p.expr(&a.item);
                });
            }
            ExprKind::Let(b) | ExprKind::Mut(b) | ExprKind::Const(b) => {
                let kw = self.text(b.keyword);
                let ty = b.ty.as_ref().map(|t| format!(" : {}", self.text(t.span))).unwrap_or_default();
                self.line(format_args!("{kw} {}{ty} {span}", b.name.item));
                if let Some(value) = &b.value {
                    self.nested(|p| p.block("Value", value));
                }
            }
            ExprKind::Def(d) => {
                let flags: Vec<_> = d.flags.iter().map(|f| format!("{:?}", f.item)).collect();
                let flags = if flags.is_empty() { String::new() } else { format!(" [{}]", flags.join(", ")) };
                self.line(format_args!("Def `{}`{flags} {span}", d.name.item));
                self.nested(|p| {
                    p.signature(&d.signature);
                    p.block("Body", &d.body);
                });
            }
            ExprKind::Extern(x) => {
                self.line(format_args!("Extern `{}` {span}", x.name.item));
                self.nested(|p| p.signature(&x.signature));
            }
            ExprKind::Alias(a) => {
                self.line(format_args!("Alias `{}` {span}", a.name.item));
                self.nested(|p| p.expr(&a.value));
            }
            ExprKind::Use(u) => {
                self.line(format_args!("Use {span}"));
                self.nested(|p| {
                    p.expr(&u.module);
                    for m in &u.members {
                        match &m.kind {
                            UseMemberKind::Name(n) => p.line(format_args!("Member {n:?} {}", m.span)),
                            UseMemberKind::Glob => p.line(format_args!("Member * {}", m.span)),
                            UseMemberKind::List(names) => {
                                let names: Vec<_> = names.iter().map(|n| n.item.as_ref()).collect();
                                p.line(format_args!("Members {names:?} {}", m.span));
                            }
                        }
                    }
                });
            }
            ExprKind::Module(m) => {
                self.line(format_args!("Module {span}"));
                self.nested(|p| {
                    p.expr(&m.name);
                    if let Some(b) = &m.body {
                        p.block("Body", b);
                    }
                });
            }
            ExprKind::Export(x) => {
                self.line(format_args!("Export {span}"));
                self.nested(|p| p.expr(&x.item));
            }
            ExprKind::ExportEnv(x) => self.block(&format!("ExportEnv {span}"), &x.body),
            ExprKind::If(i) => {
                self.line(format_args!("If {span}"));
                self.nested(|p| {
                    p.line(format_args!("Condition"));
                    p.nested(|p| p.expr(&i.condition));
                    p.block("Then", &i.then_block);
                    if let Some(e) = &i.else_branch {
                        p.line(format_args!("Else {}", e.keyword));
                        p.nested(|p| p.expr(&e.body));
                    }
                });
            }
            ExprKind::Match(m) => {
                self.line(format_args!("Match {span}"));
                self.nested(|p| {
                    p.expr(&m.value);
                    for arm in &m.arms {
                        p.line(format_args!("Arm {}", arm.span));
                        p.nested(|p| {
                            p.pattern(&arm.pattern);
                            if let Some(g) = &arm.guard {
                                p.line(format_args!("Guard"));
                                p.nested(|p| p.expr(g));
                            }
                            p.expr(&arm.body);
                        });
                    }
                });
            }
            ExprKind::For(f) => {
                let ty = f.ty.as_ref().map(|t| format!(" : {}", self.text(t.span))).unwrap_or_default();
                self.line(format_args!("For ${}{ty} {span}", f.var.item));
                self.nested(|p| {
                    p.expr(&f.iterable);
                    p.block("Body", &f.body);
                });
            }
            ExprKind::While(w) => {
                self.line(format_args!("While {span}"));
                self.nested(|p| {
                    p.expr(&w.condition);
                    p.block("Body", &w.body);
                });
            }
            ExprKind::Loop(l) => self.block(&format!("Loop {span}"), &l.body),
            ExprKind::Break => self.line(format_args!("Break {span}")),
            ExprKind::Continue => self.line(format_args!("Continue {span}")),
            ExprKind::Return(r) => {
                self.line(format_args!("Return {span}"));
                if let Some(v) = &r.value {
                    self.nested(|p| p.expr(v));
                }
            }
            ExprKind::Try(t) => {
                self.line(format_args!("Try {span}"));
                self.nested(|p| {
                    p.block("Body", &t.body);
                    for h in &t.handlers {
                        p.line(format_args!("{:?} {}", h.kind, h.keyword));
                        p.nested(|p| p.expr(&h.body));
                    }
                });
            }
            ExprKind::Where(w) => {
                self.line(format_args!("Where {span}"));
                self.nested(|p| p.expr(&w.condition));
            }
            ExprKind::Garbage => self.line(format_args!("Garbage {span}")),
        }
    }
}
