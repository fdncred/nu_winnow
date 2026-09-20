//! The parser proper.
//!
//! Parsing happens in layers that mirror the language:
//!
//! 1. [`block`]: a lexed token stream is grouped into pipelines and commands
//!    (comments, `;`, newlines, `|`, redirections, assignment absorption).
//! 2. [`statement`]: one command's items are recognised as a keyword statement
//!    (`def`, `let`, `if`, ...), a call, an assignment, or a math expression.
//! 3. [`expr`]: math expressions with precedence, calls and their arguments.
//! 4. [`value`]: a single item becomes a literal, variable, path, collection,
//!    closure, block or subexpression, re-lexing its interior as needed.
//! 5. [`literal`], [`signature`], [`pattern`]: character-level parsers for
//!    literals, signatures/types and match patterns.
//!
//! All layers share [`St`], a copyable handle to the source text and the
//! mutable [`Shared`] state (collected comments, diagnostics, declared names).

pub(crate) mod block;
pub(crate) mod expr;
pub(crate) mod literal;
pub(crate) mod pattern;
pub(crate) mod signature;
pub(crate) mod statement;
pub(crate) mod value;

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::Arc;

use crate::ast::{Ast, Block, Comment};
use crate::error::Diagnostic;
use crate::lexer::{LexOptions, Token, TokenKind, lex};
use crate::span::Span;

/// Configuration for a parse.
///
/// The only knowledge the parser needs beyond the grammar is the set of known
/// multi-word command names, so that `str trim --left` is parsed as a call to
/// `str trim` rather than a call to `str` with a `trim` argument. Commands
/// defined in the file being parsed (`def "my cmd" ...`) are always recognised.
#[derive(Clone, Debug, Default)]
pub struct ParseConfig {
    commands: Arc<CommandSet>,
}

/// Known command names plus an index of the first words of multi-word names,
/// so that the common single-word head needs no string building at all.
#[derive(Debug, Default)]
struct CommandSet {
    names: HashSet<Box<str>>,
    prefixes: HashSet<Box<str>>,
}

impl CommandSet {
    fn insert(&mut self, name: &str) {
        if let Some((first, _)) = name.split_once(' ') {
            self.prefixes.insert(Box::from(first));
        }
        self.names.insert(Box::from(name));
    }
}

impl ParseConfig {
    /// A configuration that knows Nushell's built-in commands (when the
    /// `builtin-commands` feature is enabled, which it is by default).
    pub fn new() -> Self {
        #[cfg(feature = "builtin-commands")]
        {
            Self::with_commands(crate::builtin_commands::BUILTIN_COMMANDS.iter().copied())
        }
        #[cfg(not(feature = "builtin-commands"))]
        {
            Self::empty()
        }
    }

    /// A configuration that knows no commands: every call head is a single word.
    pub fn empty() -> Self {
        Self::default()
    }

    /// A configuration knowing exactly the given command names.
    pub fn with_commands<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::default().add_commands(names)
    }

    /// Add command names (e.g. from `use`d modules) to the known set.
    pub fn add_commands<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let set = Arc::make_mut(&mut self.commands);
        for name in names {
            set.insert(name.as_ref());
        }
        self
    }

    /// Whether `name` (with words separated by single spaces) is a known command.
    pub fn is_known(&self, name: &str) -> bool {
        self.commands.names.contains(name)
    }

    /// Whether `word` is the first word of some known multi-word command.
    pub fn is_prefix(&self, word: &str) -> bool {
        self.commands.prefixes.contains(word)
    }

    /// Number of known command names.
    pub fn len(&self) -> usize {
        self.commands.names.len()
    }

    /// `true` if no commands are known.
    pub fn is_empty(&self) -> bool {
        self.commands.names.is_empty()
    }
}

impl Clone for CommandSet {
    fn clone(&self) -> Self {
        Self { names: self.names.clone(), prefixes: self.prefixes.clone() }
    }
}

/// Mutable state shared by every parser layer during one parse.
#[derive(Debug)]
pub struct Shared {
    config: ParseConfig,
    comments: Vec<Comment>,
    diagnostics: Vec<Diagnostic>,
    /// Command names declared with `def`/`extern`/`alias` in enclosing blocks,
    /// innermost scope last.
    decl_scopes: Vec<CommandSet>,
}

/// A snapshot of the shared state, used to roll back speculative parses.
#[derive(Clone, Copy, Debug)]
pub struct Checkpoint {
    comments: usize,
    diagnostics: usize,
}

/// Copyable handle to the source and the shared state.
#[derive(Clone, Copy, Debug)]
pub struct St<'s, 'a> {
    /// The complete source text.
    pub src: &'a str,
    shared: &'s RefCell<Shared>,
}

impl<'s, 'a> St<'s, 'a> {
    /// The text of a span.
    #[inline]
    pub fn text(&self, span: Span) -> &'a str {
        span.slice(self.src)
    }

    /// The text of a token.
    #[inline]
    pub fn tok(&self, tok: &Token) -> &'a str {
        tok.span.slice(self.src)
    }

    /// Record a comment.
    pub fn comment(&self, span: Span) {
        self.shared.borrow_mut().comments.push(Comment { span });
    }

    /// Record every comment token in `tokens`.
    pub fn comments_from(&self, tokens: &[Token]) {
        let mut shared = self.shared.borrow_mut();
        shared
            .comments
            .extend(tokens.iter().filter(|t| t.kind == TokenKind::Comment).map(|t| Comment { span: t.span }));
    }

    /// Record a diagnostic (used for recovered errors).
    pub fn error(&self, d: Diagnostic) {
        self.shared.borrow_mut().diagnostics.push(d);
    }

    /// Snapshot the shared state.
    pub fn checkpoint(&self) -> Checkpoint {
        let shared = self.shared.borrow();
        Checkpoint { comments: shared.comments.len(), diagnostics: shared.diagnostics.len() }
    }

    /// Roll back to a snapshot (discarding comments and diagnostics recorded since).
    pub fn rollback(&self, cp: Checkpoint) {
        let mut shared = self.shared.borrow_mut();
        shared.comments.truncate(cp.comments);
        shared.diagnostics.truncate(cp.diagnostics);
    }

    /// Whether `name` is a known command (configured or declared in scope).
    pub fn is_known_command(&self, name: &str) -> bool {
        let shared = self.shared.borrow();
        shared.config.is_known(name) || shared.decl_scopes.iter().any(|scope| scope.names.contains(name))
    }

    /// Whether `word` starts some known multi-word command.
    pub fn is_command_prefix(&self, word: &str) -> bool {
        let shared = self.shared.borrow();
        shared.config.is_prefix(word) || shared.decl_scopes.iter().any(|scope| scope.prefixes.contains(word))
    }

    /// Whether `name` was declared with `def`/`extern`/`alias` in an enclosing block.
    pub fn is_declared_command(&self, name: &str) -> bool {
        self.shared.borrow().decl_scopes.iter().any(|scope| scope.names.contains(name))
    }

    /// Declare a command name in the innermost scope.
    pub fn declare_command(&self, name: &str) {
        let mut shared = self.shared.borrow_mut();
        if let Some(scope) = shared.decl_scopes.last_mut() {
            scope.insert(name);
        }
    }

    /// Enter a declaration scope.
    pub fn push_scope(&self) {
        self.shared.borrow_mut().decl_scopes.push(CommandSet::default());
    }

    /// Leave a declaration scope.
    pub fn pop_scope(&self) {
        self.shared.borrow_mut().decl_scopes.pop();
    }

    /// Lex `span` of the source with the given options, recording lexer errors.
    /// On error, returns the diagnostic (already recorded is *not* done here so
    /// callers can decide whether to record or propagate).
    pub fn lex_span(&self, span: Span, opts: LexOptions) -> Result<Vec<Token>, Diagnostic> {
        lex(self.text(span), span.start, opts)
    }
}

/// Parse `source` into an AST plus any diagnostics.
pub(crate) fn parse_source<'a>(source: &'a str, config: &ParseConfig) -> (Ast<'a>, Vec<Diagnostic>) {
    let shared = RefCell::new(Shared {
        config: config.clone(),
        comments: Vec::new(),
        diagnostics: Vec::new(),
        decl_scopes: vec![CommandSet::default()],
    });
    let st = St { src: source, shared: &shared };
    let full = Span::new(0, source.len());
    let shebang = source.starts_with("#!").then(|| {
        let end = source.find('\n').unwrap_or(source.len());
        Span::new(0, end)
    });
    let block = match st.lex_span(full, LexOptions::BLOCK) {
        Ok(tokens) => block::parse_block_tokens(st, &tokens, full),
        Err(d) => {
            st.error(d);
            Block { span: full, pipelines: Vec::new() }
        }
    };
    let mut shared = shared.into_inner();
    shared.comments.sort_by_key(|c| (c.span.start, c.span.end));
    shared.comments.dedup();
    shared.diagnostics.sort_by_key(|d| (d.span.start, d.span.end));
    (Ast { source, block, comments: shared.comments, shebang }, shared.diagnostics)
}
