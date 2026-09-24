//! A visitor for walking the AST.
//!
//! Implement [`Visitor`] and override the `visit_*` methods you care about;
//! each default implementation calls the matching `walk_*` function, which
//! visits the node's children. This is the easiest way to build tools such as
//! formatters, linters or symbol indexes on top of the tree.

use super::*;

/// Walks every node of an AST.
#[allow(unused_variables)]
pub trait Visitor<'a> {
    /// Visit a block.
    fn visit_block(&mut self, block: &Block<'a>) {
        walk_block(self, block);
    }
    /// Visit a pipeline.
    fn visit_pipeline(&mut self, pipeline: &Pipeline<'a>) {
        walk_pipeline(self, pipeline);
    }
    /// Visit a pipeline element.
    fn visit_element(&mut self, element: &PipelineElement<'a>) {
        walk_element(self, element);
    }
    /// Visit an expression.
    fn visit_expr(&mut self, expr: &Expr<'a>) {
        walk_expr(self, expr);
    }
    /// Visit a signature.
    fn visit_signature(&mut self, sig: &Signature<'a>) {
        walk_signature(self, sig);
    }
    /// Visit a parameter.
    fn visit_param(&mut self, param: &Param<'a>) {
        walk_param(self, param);
    }
    /// Visit a type annotation.
    fn visit_type(&mut self, ty: &TypeAnnotation<'a>) {
        walk_type(self, ty);
    }
    /// Visit a match pattern.
    fn visit_pattern(&mut self, pattern: &Pattern<'a>) {
        walk_pattern(self, pattern);
    }
    /// Visit a cell-path member.
    fn visit_path_member(&mut self, member: &PathMember<'a>) {}
    /// Visit a comment attached to a pipeline or parameter.
    fn visit_comment(&mut self, comment: &Comment) {}
    /// Visit a redirection.
    fn visit_redirection(&mut self, redirection: &Redirection<'a>) {
        walk_redirection(self, redirection);
    }
}

/// Visit the children of a block.
pub fn walk_block<'a, V: Visitor<'a> + ?Sized>(v: &mut V, block: &Block<'a>) {
    for p in &block.pipelines {
        v.visit_pipeline(p);
    }
}

/// Visit the children of a pipeline.
pub fn walk_pipeline<'a, V: Visitor<'a> + ?Sized>(v: &mut V, pipeline: &Pipeline<'a>) {
    for c in &pipeline.leading_comments {
        v.visit_comment(c);
    }
    for e in &pipeline.elements {
        v.visit_element(e);
    }
    for c in &pipeline.trailing_comments {
        v.visit_comment(c);
    }
}

/// Visit the children of a pipeline element.
pub fn walk_element<'a, V: Visitor<'a> + ?Sized>(v: &mut V, element: &PipelineElement<'a>) {
    v.visit_expr(&element.expr);
    if let Some(r) = &element.redirection {
        v.visit_redirection(r);
    }
}

/// Visit the children of a redirection.
pub fn walk_redirection<'a, V: Visitor<'a> + ?Sized>(v: &mut V, redirection: &Redirection<'a>) {
    let mut target = |t: &RedirectTarget<'a>| {
        if let RedirectTarget::File { path, .. } = t {
            v.visit_expr(path);
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

/// Visit the children of an expression.
pub fn walk_expr<'a, V: Visitor<'a> + ?Sized>(v: &mut V, expr: &Expr<'a>) {
    match &expr.kind {
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
        | ExprKind::Break
        | ExprKind::Continue
        | ExprKind::Garbage => {}
        ExprKind::Interpolation(i) => {
            for part in &i.parts {
                if let InterpPart::Expr(e) = part {
                    v.visit_expr(e);
                }
            }
        }
        ExprKind::Range(r) => {
            if let Some(e) = &r.from {
                v.visit_expr(e);
            }
            if let Some(e) = &r.next {
                v.visit_expr(e);
            }
            if let Some(e) = &r.to {
                v.visit_expr(e);
            }
        }
        ExprKind::CellPath(p) => {
            for m in &p.members {
                v.visit_path_member(m);
            }
        }
        ExprKind::FullCellPath(p) => {
            v.visit_expr(&p.head);
            for m in &p.members {
                v.visit_path_member(m);
            }
        }
        ExprKind::List(items) => {
            for item in items {
                match item {
                    ListItem::Item(e) | ListItem::Spread { expr: e, .. } => v.visit_expr(e),
                }
            }
        }
        ExprKind::Table(t) => {
            v.visit_expr(&t.columns);
            for row in &t.rows {
                v.visit_expr(row);
            }
        }
        ExprKind::Record(items) => {
            for item in items {
                match item {
                    RecordItem::Pair { key, value, .. } => {
                        v.visit_expr(key);
                        v.visit_expr(value);
                    }
                    RecordItem::Spread { expr, .. } => v.visit_expr(expr),
                }
            }
        }
        ExprKind::Closure(c) => {
            if let Some(sig) = &c.params {
                v.visit_signature(sig);
            }
            v.visit_block(&c.body);
        }
        ExprKind::Block(b) | ExprKind::Subexpression(b) => v.visit_block(b),
        ExprKind::BinaryOp(b) => {
            v.visit_expr(&b.lhs);
            v.visit_expr(&b.rhs);
        }
        ExprKind::UnaryNot(n) => v.visit_expr(&n.expr),
        ExprKind::Assignment(a) => {
            v.visit_expr(&a.lhs);
            v.visit_block(&a.rhs);
        }
        ExprKind::Call(c) => walk_args(v, &c.args),
        ExprKind::DynamicCall(d) => {
            v.visit_expr(&d.head);
            walk_args(v, &d.args);
        }
        ExprKind::ExternalCall(c) => {
            v.visit_expr(&c.head);
            for arg in &c.args {
                match arg {
                    ExternalArg::Regular(e) | ExternalArg::Spread { expr: e, .. } => v.visit_expr(e),
                }
            }
        }
        ExprKind::EnvShorthand(e) => {
            for var in &e.vars {
                v.visit_expr(&var.value);
            }
            v.visit_expr(&e.expr);
        }
        ExprKind::AttributeBlock(a) => {
            for attr in &a.attributes {
                walk_args(v, &attr.args);
            }
            v.visit_expr(&a.item);
        }
        ExprKind::Let(b) | ExprKind::Mut(b) | ExprKind::Const(b) => {
            if let Some(ty) = &b.ty {
                v.visit_type(ty);
            }
            if let Some(value) = &b.value {
                v.visit_block(value);
            }
        }
        ExprKind::Def(d) => {
            v.visit_signature(&d.signature);
            if let Some(p) = &d.body_params {
                v.visit_signature(p);
            }
            v.visit_block(&d.body);
        }
        ExprKind::Extern(e) => v.visit_signature(&e.signature),
        ExprKind::Alias(a) => {
            if let Some(v_) = &a.value {
                v.visit_expr(v_);
            }
        }
        ExprKind::Use(u) => {
            v.visit_expr(&u.module);
            for m in &u.members {
                if let UseMemberKind::Ignored(e) = &m.kind {
                    v.visit_expr(e);
                }
            }
        }
        ExprKind::Module(m) => {
            v.visit_expr(&m.name);
            if let Some(b) = &m.body {
                v.visit_block(b);
            }
        }
        ExprKind::Export(e) => v.visit_expr(&e.item),
        ExprKind::ExportEnv(e) => v.visit_block(&e.body),
        ExprKind::If(i) => {
            v.visit_expr(&i.condition);
            v.visit_block(&i.then_block);
            if let Some(e) = &i.else_branch {
                v.visit_expr(&e.body);
            }
        }
        ExprKind::Match(m) => {
            v.visit_expr(&m.value);
            for arm in &m.arms {
                v.visit_pattern(&arm.pattern);
                if let Some(g) = &arm.guard {
                    v.visit_expr(g);
                }
                v.visit_expr(&arm.body);
            }
            if let Some(b) = &m.value_block {
                v.visit_expr(b);
            }
        }
        ExprKind::For(f) => {
            if let Some(ty) = &f.ty {
                v.visit_type(ty);
            }
            v.visit_expr(&f.iterable);
            v.visit_block(&f.body);
        }
        ExprKind::While(w) => {
            v.visit_expr(&w.condition);
            v.visit_block(&w.body);
        }
        ExprKind::Loop(l) => v.visit_block(&l.body),
        ExprKind::Return(r) => {
            if let Some(e) = &r.value {
                v.visit_expr(e);
            }
        }
        ExprKind::Try(t) => {
            v.visit_block(&t.body);
            for h in &t.handlers {
                v.visit_expr(&h.body);
            }
        }
        ExprKind::Where(w) => v.visit_expr(&w.condition),
    }
}

fn walk_args<'a, V: Visitor<'a> + ?Sized>(v: &mut V, args: &[Arg<'a>]) {
    for arg in args {
        match arg {
            Arg::Positional(e) | Arg::Spread { expr: e, .. } => v.visit_expr(e),
            Arg::Flag(f) => {
                if let Some(e) = &f.value {
                    v.visit_expr(e);
                }
            }
            Arg::EndOfOptions(_) => {}
        }
    }
}

/// Visit the children of a signature.
pub fn walk_signature<'a, V: Visitor<'a> + ?Sized>(v: &mut V, sig: &Signature<'a>) {
    for p in &sig.params {
        v.visit_param(p);
    }
    for io in &sig.io_types {
        v.visit_type(&io.input);
        v.visit_type(&io.output);
    }
}

/// Visit the children of a parameter.
pub fn walk_param<'a, V: Visitor<'a> + ?Sized>(v: &mut V, param: &Param<'a>) {
    if let Some(ty) = &param.ty {
        v.visit_type(ty);
    }
    if let Some(d) = &param.default {
        v.visit_expr(d);
    }
    for c in &param.description {
        v.visit_comment(c);
    }
}

/// Visit the children of a type annotation.
pub fn walk_type<'a, V: Visitor<'a> + ?Sized>(v: &mut V, ty: &TypeAnnotation<'a>) {
    match &ty.kind {
        TypeKind::List(Some(inner)) => v.visit_type(inner),
        TypeKind::Record(fields) | TypeKind::Table(fields) => {
            for f in fields {
                v.visit_type(&f.ty);
            }
        }
        TypeKind::OneOf(types) => {
            for t in types {
                v.visit_type(t);
            }
        }
        _ => {}
    }
}

/// Visit the children of a pattern.
pub fn walk_pattern<'a, V: Visitor<'a> + ?Sized>(v: &mut V, pattern: &Pattern<'a>) {
    match &pattern.kind {
        PatternKind::Value(e) => v.visit_expr(e),
        PatternKind::Variable(_) | PatternKind::Wildcard | PatternKind::Rest(_) => {}
        PatternKind::List(items) | PatternKind::Or(items) => {
            for p in items {
                v.visit_pattern(p);
            }
        }
        PatternKind::Record(fields) => {
            for (_, p) in fields {
                v.visit_pattern(p);
            }
        }
    }
}
