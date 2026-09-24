//! The state every parser function shares during one parse.
//!
//! [`WorkingSet`] plays the part of nu-parser's `StateWorkingSet`, and its
//! methods carry the same names where the two overlap: `get_span_contents`,
//! `error`, `enter_scope`/`exit_scope`, `find_decl` and `add_predecl`. Parser
//! functions take it as their first argument, `working_set: &WorkingSet<'a>`,
//! exactly as nu-parser's do. Everything it collects on the side (comments,
//! ignored text, diagnostics, declared command names) sits behind a
//! [`RefCell`], so the combinators that capture it can share one `&WorkingSet`.

use std::cell::RefCell;

use crate::ast::Comment;
use crate::error::Diagnostic;
use crate::lex::{Token, TokenContents};
use crate::span::Span;

use super::{CommandSet, ParseConfig};

/// Where a command name known to the parser comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclKind {
    /// Declared with `def`, `extern` or `alias` in an enclosing block.
    Declared,
    /// One of the configured (built-in) commands of [`ParseConfig`].
    Builtin,
}

/// The source text, the known command names, and what one parse collects.
#[derive(Debug)]
pub struct WorkingSet<'a> {
    /// The complete source text; every span indexes into it.
    pub source: &'a str,
    config: ParseConfig,
    comments: RefCell<Vec<Comment>>,
    /// Source text nu-parser accepts and discards (see [`crate::ast::Ast::ignored`]).
    ignored: RefCell<Vec<Span>>,
    parse_errors: RefCell<Vec<Diagnostic>>,
    /// Command names declared with `def`/`extern`/`alias` in enclosing blocks,
    /// innermost scope last.
    scopes: RefCell<Vec<CommandSet>>,
}

/// What a finished parse collected besides the tree.
pub struct Collected {
    pub comments: Vec<Comment>,
    pub ignored: Vec<Span>,
    pub parse_errors: Vec<Diagnostic>,
}

impl<'a> WorkingSet<'a> {
    /// A working set for parsing `source` with the command names of `config`.
    pub fn new(source: &'a str, config: &ParseConfig) -> Self {
        Self {
            source,
            config: config.clone(),
            comments: RefCell::new(Vec::new()),
            ignored: RefCell::new(Vec::new()),
            parse_errors: RefCell::new(Vec::new()),
            scopes: RefCell::new(vec![CommandSet::default()]),
        }
    }

    /// The source text of `span`.
    #[inline]
    pub fn get_span_contents(&self, span: Span) -> &'a str {
        span.slice(self.source)
    }

    /// Record a diagnostic (a recovered error: parsing goes on).
    pub fn error(&self, diagnostic: Diagnostic) {
        self.parse_errors.borrow_mut().push(diagnostic);
    }

    /// Record a comment.
    pub fn add_comment(&self, span: Span) {
        self.comments.borrow_mut().push(Comment { span });
    }

    /// Record every comment token in `tokens`.
    pub fn add_comments(&self, tokens: &[Token]) {
        let comments = tokens.iter().filter(|token| token.contents == TokenContents::Comment);
        self.comments.borrow_mut().extend(comments.map(|token| Comment { span: token.span }));
    }

    /// Record text that nu-parser accepts and discards (see [`crate::ast::Ast::ignored`]).
    pub fn add_ignored(&self, span: Span) {
        if !span.is_empty() {
            self.ignored.borrow_mut().push(span);
        }
    }

    /// Drop the ignored text recorded from `offset` on: a statement that turns
    /// out to be a help call is parsed again as an ordinary call.
    pub fn remove_ignored_from(&self, offset: usize) {
        self.ignored.borrow_mut().retain(|span| span.start < offset);
    }

    /// Where `name` is known from, if it is a command: a declaration in an
    /// enclosing block shadows a built-in command of the same name.
    pub fn find_decl(&self, name: &str) -> Option<DeclKind> {
        if self.is_declared(name) {
            Some(DeclKind::Declared)
        } else if self.config.is_known(name) {
            Some(DeclKind::Builtin)
        } else {
            None
        }
    }

    /// Whether `name` was declared with `def`, `extern` or `alias` in an
    /// enclosing block (and so shadows a built-in command of that name).
    pub fn is_declared(&self, name: &str) -> bool {
        self.scopes.borrow().iter().any(|scope| scope.names.contains(name))
    }

    /// Whether `word` is the first word of some known multi-word command.
    pub fn is_decl_name_prefix(&self, word: &str) -> bool {
        self.config.is_prefix(word) || self.scopes.borrow().iter().any(|scope| scope.prefixes.contains(word))
    }

    /// Whether a table of built-in commands is configured. Rules that need to
    /// know whether a command exists apply only when it is.
    pub fn has_builtin_decls(&self) -> bool {
        !self.config.is_empty()
    }

    /// Whether `name` is one of the configured built-in commands, whatever
    /// the file declares.
    pub fn is_builtin_decl(&self, name: &str) -> bool {
        self.config.is_known(name)
    }

    /// Declare a command name in the innermost scope before its block is
    /// parsed, so calls to it resolve (nu's `add_predecl`).
    pub fn add_predecl(&self, name: &str) {
        if let Some(scope) = self.scopes.borrow_mut().last_mut() {
            scope.insert(name);
        }
    }

    /// Enter a declaration scope (a block, closure or module body).
    pub fn enter_scope(&self) {
        self.scopes.borrow_mut().push(CommandSet::default());
    }

    /// Leave the innermost declaration scope.
    pub fn exit_scope(&self) {
        self.scopes.borrow_mut().pop();
    }

    /// The comments, ignored text and diagnostics, each sorted by position.
    /// Comments and ignored spans are de-duplicated; diagnostics are kept as
    /// reported.
    pub fn into_collected(self) -> Collected {
        let mut comments = self.comments.into_inner();
        comments.sort_by_key(|comment| (comment.span.start, comment.span.end));
        comments.dedup();
        let mut ignored = self.ignored.into_inner();
        ignored.sort_by_key(|span| (span.start, span.end));
        ignored.dedup();
        let mut parse_errors = self.parse_errors.into_inner();
        parse_errors.sort_by_key(|diagnostic| (diagnostic.span.start, diagnostic.span.end));
        Collected { comments, ignored, parse_errors }
    }
}
