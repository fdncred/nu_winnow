//! Construct-by-construct coverage of the Nushell grammar.
//!
//! Each test parses a snippet and asserts on the shape of the resulting AST.
//! Snippets are drawn from the Nushell book, the standard library and the
//! reference parser's own test-suite.

use nu_winnow_parser::ast::*;
use nu_winnow_parser::lexer::{AssignOp, RedirectOp, RedirectSource};
use nu_winnow_parser::{ErrorKind, ParseConfig, ParseError, Span, parse, parse_lenient, parse_with};

// --- helpers ------------------------------------------------------------------

fn ok(src: &str) -> Ast<'_> {
    match parse(src) {
        Ok(ast) => ast,
        Err(e) => panic!("failed to parse {src:?}:\n{}", e.render(src, None)),
    }
}

fn err(src: &str) -> ParseError {
    match parse(src) {
        Ok(ast) => panic!("expected {src:?} to fail, got:\n{}", nu_winnow_parser::pretty::dump(&ast)),
        Err(e) => e,
    }
}

/// The single expression of a one-statement, one-element source.
fn expr<'a>(ast: &'a Ast<'a>) -> &'a Expr<'a> {
    assert_eq!(ast.block.pipelines.len(), 1, "expected one pipeline in {:?}", ast.source);
    let p = &ast.block.pipelines[0];
    assert_eq!(p.elements.len(), 1, "expected one element in {:?}", ast.source);
    &p.elements[0].expr
}

fn elements<'a>(ast: &'a Ast<'a>) -> &'a [PipelineElement<'a>] {
    assert_eq!(ast.block.pipelines.len(), 1);
    &ast.block.pipelines[0].elements
}

fn text<'a>(ast: &Ast<'a>, span: Span) -> &'a str {
    ast.text(span)
}

macro_rules! kind {
    ($expr:expr, $pat:pat) => {
        match &$expr.kind {
            $pat => {}
            other => panic!("expected {}, got {other:?}", stringify!($pat)),
        }
    };
    ($expr:expr, $pat:pat => $body:expr) => {
        match &$expr.kind {
            $pat => $body,
            other => panic!("expected {}, got {other:?}", stringify!($pat)),
        }
    };
}

fn call<'a>(e: &'a Expr<'a>) -> &'a Call<'a> {
    kind!(e, ExprKind::Call(c) => c)
}

fn string<'e>(e: &'e Expr<'_>) -> (&'e str, Quote) {
    kind!(e, ExprKind::String(s) => (s.value.as_ref(), s.quote))
}

// --- pipelines and statements -----------------------------------------------------

#[test]
fn empty_and_whitespace_only() {
    assert!(ok("").block.pipelines.is_empty());
    assert!(ok("\n\n   \n").block.pipelines.is_empty());
    assert!(ok("; ;\n;").block.pipelines.is_empty());
    assert!(ok("# just a comment").block.pipelines.is_empty());
}

#[test]
fn simple_call_and_pipeline() {
    let ast = ok("ls -l --all | where size > 1kb | get name\n");
    let els = elements(&ast);
    assert_eq!(els.len(), 3);
    let c = call(&els[0].expr);
    assert_eq!(c.head.name, "ls");
    assert_eq!(c.args.len(), 2);
    match (&c.args[0], &c.args[1]) {
        (Arg::Flag(l), Arg::Flag(all)) => {
            assert_eq!((l.name, l.long), ("l", false));
            assert_eq!((all.name, all.long), ("all", true));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(text(&ast, els[1].pipe.unwrap()), "|");
    kind!(els[1].expr, ExprKind::Where(_));
    assert_eq!(call(&els[2].expr).head.name, "get");
    assert_eq!(string(call(&els[2].expr).positionals().next().unwrap()), ("name", Quote::Bare));
}

#[test]
fn statements_separated_by_semicolons_and_newlines() {
    let ast = ok("ls; pwd\ncd ..;\n\n\nls");
    assert_eq!(ast.block.pipelines.len(), 4);
    assert_eq!(text(&ast, ast.block.pipelines[0].terminator.unwrap()), ";");
    assert!(ast.block.pipelines[1].terminator.is_none());
    assert_eq!(text(&ast, ast.block.pipelines[2].terminator.unwrap()), ";");
    assert_eq!(string(call(&ast.block.pipelines[2].elements[0].expr).positionals().next().unwrap()).0, "..");
}

#[test]
fn multiline_pipelines_with_leading_pipes_and_comments() {
    let src = "ls\n  | where size > 1kb # big ones\n  # another comment\n  | # trailing\n  | get name\n";
    let ast = ok(src);
    let p = &ast.block.pipelines[0];
    assert_eq!(p.elements.len(), 3);
    assert_eq!(p.trailing_comments.len(), 3);
    assert_eq!(ast.comments.len(), 3);
    let ast = ok("ls |\n  length");
    assert_eq!(elements(&ast).len(), 2);
    let ast = ok("ls | # comment\n length");
    assert_eq!(elements(&ast).len(), 2);
    let ast = ok("( | str join)");
    let sub = kind!(expr(&ast), ExprKind::Subexpression(b) => b);
    assert_eq!(call(&sub.pipelines[0].elements[0].expr).head.name, "str join");
    let ast = ok("ls | | length");
    assert_eq!(elements(&ast).len(), 2);
}

#[test]
fn leading_comments_attach_to_pipeline() {
    let src = "# doc line one\n# doc line two\ndef foo [] { }\n\n# detached\n\nls\n";
    let ast = ok(src);
    let def = &ast.block.pipelines[0];
    assert_eq!(def.leading_comments.len(), 2);
    assert_eq!(def.leading_comments[0].body(src), "doc line one");
    let ls = &ast.block.pipelines[1];
    assert!(ls.leading_comments.is_empty(), "a blank line detaches comments");
    assert_eq!(ast.comments.len(), 3);
}

#[test]
fn shebang() {
    let ast = ok("#!/usr/bin/env nu\nls\n");
    assert_eq!(text(&ast, ast.shebang.unwrap()), "#!/usr/bin/env nu");
    assert_eq!(ast.block.pipelines.len(), 1);
}

#[test]
fn math_at_pipeline_head_and_precedence() {
    let ast = ok("1 + 2 * 3 - 4 / 2");
    // ((1 + (2 * 3)) - (4 / 2))
    let top = kind!(expr(&ast), ExprKind::BinaryOp(b) => b);
    assert_eq!(top.op.item, Operator::Math(Math::Subtract));
    let lhs = kind!(top.lhs, ExprKind::BinaryOp(b) => b);
    assert_eq!(lhs.op.item, Operator::Math(Math::Add));
    let mul = kind!(lhs.rhs, ExprKind::BinaryOp(b) => b);
    assert_eq!(mul.op.item, Operator::Math(Math::Multiply));
    let rhs = kind!(top.rhs, ExprKind::BinaryOp(b) => b);
    assert_eq!(rhs.op.item, Operator::Math(Math::Divide));
}

#[test]
fn pow_is_right_associative_and_others_left() {
    let ast = ok("2 ** 3 ** 2");
    let top = kind!(expr(&ast), ExprKind::BinaryOp(b) => b);
    kind!(top.lhs, ExprKind::Int(2));
    let rhs = kind!(top.rhs, ExprKind::BinaryOp(b) => b);
    assert_eq!(rhs.op.item, Operator::Math(Math::Pow));
    let ast = ok("10 - 3 - 2");
    let top = kind!(expr(&ast), ExprKind::BinaryOp(b) => b);
    kind!(top.rhs, ExprKind::Int(2));
    kind!(top.lhs, ExprKind::BinaryOp(_));
}

#[test]
fn all_operators_parse() {
    let ops = [
        "+",
        "-",
        "*",
        "/",
        "//",
        "mod",
        "**",
        "++",
        "==",
        "!=",
        "<",
        "<=",
        ">",
        ">=",
        "=~",
        "!~",
        "like",
        "not-like",
        "in",
        "not-in",
        "has",
        "not-has",
        "starts-with",
        "not-starts-with",
        "ends-with",
        "not-ends-with",
        "and",
        "or",
        "xor",
        "bit-or",
        "bit-xor",
        "bit-and",
        "bit-shl",
        "bit-shr",
    ];
    for op in ops {
        let src = format!("$a {op} $b");
        let ast = ok(&src);
        let b = kind!(expr(&ast), ExprKind::BinaryOp(b) => b);
        assert_eq!(text(&ast, b.op.span), op);
        assert_eq!(b.op.item, Operator::from_spelling(op).unwrap());
    }
}

#[test]
fn boolean_precedence_and_not() {
    let ast = ok("not $a and $b or $c > 1");
    // ((not $a) and $b) or ($c > 1)
    let top = kind!(expr(&ast), ExprKind::BinaryOp(b) => b);
    assert_eq!(top.op.item, Operator::Boolean(Boolean::Or));
    let and = kind!(top.lhs, ExprKind::BinaryOp(b) => b);
    assert_eq!(and.op.item, Operator::Boolean(Boolean::And));
    kind!(and.lhs, ExprKind::UnaryNot(_));
    let ast = ok("not not true");
    let outer = kind!(expr(&ast), ExprKind::UnaryNot(n) => n);
    kind!(outer.expr, ExprKind::UnaryNot(_));
    let ast = ok("$x == not false");
    let b = kind!(expr(&ast), ExprKind::BinaryOp(b) => b);
    kind!(b.rhs, ExprKind::UnaryNot(_));
}

#[test]
fn if_and_match_as_math_operands() {
    let ast = ok("1 + if true { 2 } else { 3 }");
    let b = kind!(expr(&ast), ExprKind::BinaryOp(b) => b);
    kind!(b.rhs, ExprKind::If(_));
    let ast = ok("let x = if $a { 1 } else if $b { 2 } else { 3 }");
    let binding = kind!(expr(&ast), ExprKind::Let(b) => b);
    let value = &binding.value.as_ref().unwrap().pipelines[0].elements[0].expr;
    let i = kind!(value, ExprKind::If(i) => i);
    let else_if = kind!(i.else_branch.as_ref().unwrap().body, ExprKind::If(i) => i);
    kind!(else_if.else_branch.as_ref().unwrap().body, ExprKind::Block(_));
}

#[test]
fn barewords_in_argument_position_are_strings() {
    let ast = ok("echo 1 + 1 not true");
    let c = call(expr(&ast));
    let args: Vec<_> = c.positionals().collect();
    kind!(args[0], ExprKind::Int(1));
    assert_eq!(string(args[1]), ("+", Quote::Bare));
    assert_eq!(string(args[3]), ("not", Quote::Bare));
    kind!(args[4], ExprKind::Bool(true));
    let ast = ok("echo 1+1 a=b foo.txt ~/x *.rs");
    for arg in call(expr(&ast)).positionals() {
        kind!(arg, ExprKind::String(_));
    }
}

#[test]
fn unknown_operator_hints() {
    let e = err("1 ^ 2");
    assert!(matches!(e.primary().kind, ErrorKind::UnknownOperator(_)));
    assert!(e.primary().help.as_deref().unwrap().contains("**"));
    let e = err("1 +");
    assert!(matches!(e.primary().kind, ErrorKind::Expected(_)));
    let e = err("$x foo");
    assert!(matches!(e.primary().kind, ErrorKind::Expected("operator")));
}

// --- literals -----------------------------------------------------------------------

#[test]
fn numbers() {
    let cases: &[(&str, ExprKind<'_>)] = &[
        ("42", ExprKind::Int(42)),
        ("-42", ExprKind::Int(-42)),
        ("+7", ExprKind::Int(7)),
        ("1_000", ExprKind::Int(1000)),
        ("0xff", ExprKind::Int(255)),
        ("0o17", ExprKind::Int(15)),
        ("0b1010", ExprKind::Int(10)),
        ("1.5", ExprKind::Float(1.5)),
        ("-.5", ExprKind::Float(-0.5)),
        ("1e3", ExprKind::Float(1000.0)),
        ("inf", ExprKind::Float(f64::INFINITY)),
        ("true", ExprKind::Bool(true)),
        ("false", ExprKind::Bool(false)),
        ("null", ExprKind::Nothing),
    ];
    for (src, expected) in cases {
        let ast = ok(src);
        assert_eq!(&expr(&ast).kind, expected, "{src}");
    }
    let ast = ok("NaN");
    kind!(expr(&ast), ExprKind::Float(f) => assert!(f.is_nan()));
}

#[test]
fn units_and_datetimes() {
    let ast = ok("1.5sec");
    kind!(expr(&ast), ExprKind::Duration(d) => assert_eq!(*d, Duration { value: 1.5, unit: DurationUnit::Second }));
    let ast = ok("10kib");
    kind!(expr(&ast), ExprKind::Filesize(f) => {
        assert_eq!(f.unit, FilesizeUnit::KiB);
        assert_eq!(f.to_bytes(), Some(10240));
    });
    let ast = ok("1\u{00B5}s");
    kind!(expr(&ast), ExprKind::Duration(d) => assert_eq!(d.to_nanoseconds(), Some(1000)));
    let ast = ok("2024-01-02T03:04:05+05:00");
    kind!(expr(&ast), ExprKind::DateTime("2024-01-02T03:04:05+05:00"));
    let ast = ok("2024-01-02");
    kind!(expr(&ast), ExprKind::DateTime(_));
    // `5NS` is not a duration: units are case-sensitive, so it is a bare word.
    let ast = ok("echo 5NS");
    kind!(call(expr(&ast)).positionals().next().unwrap(), ExprKind::String(_));
    assert!(parse("echo 1..2sec").is_err(), "a unit suffix with a non-numeric value is an error, as in Nushell");
}

#[test]
fn binary_literals() {
    let ast = ok("0x[ff 00, 1a]");
    kind!(expr(&ast), ExprKind::Binary(b) => assert_eq!((b.radix, b.bytes.as_slice()), (16, &[0xff, 0x00, 0x1a][..])));
    let ast = ok("0b[1010]");
    kind!(expr(&ast), ExprKind::Binary(b) => assert_eq!(b.bytes, vec![0b1010]));
    let ast = ok("0o[377]");
    kind!(expr(&ast), ExprKind::Binary(b) => assert_eq!(b.bytes, vec![255]));
    let ast = ok("0x[\n  de ad # comment\n  be ef\n]");
    kind!(expr(&ast), ExprKind::Binary(b) => assert_eq!(b.bytes, vec![0xde, 0xad, 0xbe, 0xef]));
    assert!(matches!(err("0x[zz]").primary().kind, ErrorKind::InvalidLiteral { kind: "binary", .. }));
    assert!(parse("0o[777]").is_err());
}

#[test]
fn strings_all_quote_styles() {
    let ast = ok(r#"echo 'single' "dou\"ble\n" `back tick` r#'raw 'q' #'# bare"#);
    let args: Vec<_> = call(expr(&ast)).positionals().collect();
    assert_eq!(string(args[0]), ("single", Quote::Single));
    assert_eq!(string(args[1]), ("dou\"ble\n", Quote::Double));
    assert_eq!(string(args[2]), ("back tick", Quote::Backtick));
    assert_eq!(string(args[3]), ("raw 'q' #", Quote::Raw(1)));
    assert_eq!(string(args[4]), ("bare", Quote::Bare));
    let ast = ok("r##'a'#b'##");
    assert_eq!(string(expr(&ast)), ("a'#b", Quote::Raw(2)));
    // Nushell keeps embedded quotes as long as the last quote ends the word.
    let ast = ok(r#""[{name: "John"}]""#);
    assert_eq!(string(expr(&ast)).0, r#"[{name: "John"}]"#);
    let ast = ok("'it''s'");
    assert_eq!(string(expr(&ast)).0, "it''s");
    let ast = ok("echo foo\"bar\"");
    assert_eq!(string(call(expr(&ast)).positionals().next().unwrap()), ("foo\"bar\"", Quote::Bare));
}

#[test]
fn string_escapes() {
    let ast = ok(r#""\u{1F600}\x41\e\t\\\"\/\(\)\{\}\$\^\#\|\~\a\b\f\r\0""#);
    let (value, _) = string(expr(&ast));
    assert_eq!(value, "😀A\u{1b}\t\\\"/(){}$^#|~\u{07}\u{08}\u{0c}\r\0");
    let e = err(r#""\q""#);
    assert!(matches!(e.primary().kind, ErrorKind::InvalidLiteral { kind: "string", .. }));
    assert!(parse(r#""\u{110000}""#).is_err());
    assert!(parse(r#""\x4""#).is_err());
    // Single quotes have no escapes.
    let ast = ok(r"'a\nb'");
    assert_eq!(string(expr(&ast)).0, r"a\nb");
}

#[test]
fn string_errors() {
    assert!(matches!(err("\"abc\"def").primary().kind, ErrorKind::ExtraTokens));
    assert!(matches!(err("echo 'abc").primary().kind, ErrorKind::Unclosed { delimiter: "'", .. }));
    assert!(matches!(err("echo $\"abc").primary().kind, ErrorKind::Unclosed { .. }));
}

#[test]
fn interpolation() {
    let src = r#"$"hello (1 + 1) and ($x | str upcase) \(literal\) (")")""#;
    let ast = ok(src);
    let i = kind!(expr(&ast), ExprKind::Interpolation(i) => i);
    assert_eq!(i.quote, Quote::Double);
    let texts: Vec<_> = i
        .parts
        .iter()
        .filter_map(|p| match p {
            InterpPart::Text { value, .. } => Some(value.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["hello ", " and ", " (literal) "]);
    let exprs = i.parts.iter().filter(|p| matches!(p, InterpPart::Expr(_))).count();
    assert_eq!(exprs, 3);
    match &i.parts[1] {
        InterpPart::Expr(e) => {
            let sub = kind!(e, ExprKind::Subexpression(b) => b);
            kind!(sub.pipelines[0].elements[0].expr, ExprKind::BinaryOp(_));
        }
        other => panic!("{other:?}"),
    }
    let ast = ok("$'no (escapes) here'");
    let i = kind!(expr(&ast), ExprKind::Interpolation(i) => i);
    assert_eq!(i.quote, Quote::Single);
    assert_eq!(i.parts.len(), 3);
    // Bare-word interpolation.
    let ast = ok("echo foo(1 + 1)bar");
    let arg = call(expr(&ast)).positionals().next().unwrap();
    let i = kind!(arg, ExprKind::Interpolation(i) => i);
    assert_eq!(i.quote, Quote::Bare);
    assert_eq!(i.parts.len(), 3);
    // Quotes inside an interpolated string (HTML attributes).
    let ast = ok("$\"<meta charset=\"utf-8\"> ($x)\"");
    let i = kind!(expr(&ast), ExprKind::Interpolation(i) => i);
    assert_eq!(i.parts.len(), 2);
    // Nested interpolation inside a subexpression.
    let ast = ok(r#"$"a ($"b (1)")""#);
    kind!(expr(&ast), ExprKind::Interpolation(_));
}

// --- variables and cell paths ------------------------------------------------------

#[test]
fn variables_and_cell_paths() {
    let ast = ok("$x");
    kind!(expr(&ast), ExprKind::Var(Var { name: "x" }));
    let ast = ok("$in");
    kind!(expr(&ast), ExprKind::Var(v) => assert!(v.is_in()));
    let ast = ok("$env.PATH.0?");
    let p = kind!(expr(&ast), ExprKind::FullCellPath(p) => p);
    kind!(p.head, ExprKind::Var(v) => assert!(v.is_env()));
    assert_eq!(p.members.len(), 2);
    assert_eq!(p.members[0].kind, PathMemberKind::String("PATH".into()));
    assert_eq!(p.members[1].kind, PathMemberKind::Int(0));
    assert!(p.members[1].optional);
    assert_eq!(text(&ast, p.members[1].span), "0?");
    let ast = ok("$x.\"a b\".'c.d'.Name!.e?!");
    let p = kind!(expr(&ast), ExprKind::FullCellPath(p) => p);
    assert_eq!(p.members[0].kind, PathMemberKind::String("a b".into()));
    assert_eq!(p.members[1].kind, PathMemberKind::String("c.d".into()));
    assert!(p.members[2].insensitive && !p.members[2].optional);
    assert!(p.members[3].insensitive && p.members[3].optional);
    let ast = ok("$nu.os-info.name");
    kind!(expr(&ast), ExprKind::FullCellPath(_));
    // Trailing dot is tolerated like Nushell.
    let ast = ok("$x.a.");
    let p = kind!(expr(&ast), ExprKind::FullCellPath(p) => p);
    assert_eq!(p.members.len(), 1);
}

#[test]
fn cell_path_literals_and_heads() {
    let ast = ok("$.a.0.b?");
    let c = kind!(expr(&ast), ExprKind::CellPath(c) => c);
    assert_eq!(c.members.len(), 3);
    let ast = ok("$.");
    kind!(expr(&ast), ExprKind::CellPath(c) => assert!(c.members.is_empty()));
    let ast = ok("(ls).name");
    let p = kind!(expr(&ast), ExprKind::FullCellPath(p) => p);
    kind!(p.head, ExprKind::Subexpression(_));
    let ast = ok("[1 2 3].1");
    let p = kind!(expr(&ast), ExprKind::FullCellPath(p) => p);
    kind!(p.head, ExprKind::List(_));
    let ast = ok("{a: 1}.a");
    let p = kind!(expr(&ast), ExprKind::FullCellPath(p) => p);
    kind!(p.head, ExprKind::Record(_));
    let ast = ok("{}.foo?");
    kind!(expr(&ast), ExprKind::FullCellPath(_));
    let e = err("$x.a?.b?.c!!");
    assert!(matches!(e.primary().kind, ErrorKind::Expected(_)));
    assert!(parse("$x.-1").is_err());
    assert!(parse("$foo-bar").is_err());
}

#[test]
fn ranges() {
    let ast = ok("1..10");
    let r = kind!(expr(&ast), ExprKind::Range(r) => r);
    assert_eq!(r.inclusion, RangeInclusion::Inclusive);
    kind!(r.from.as_ref().unwrap(), ExprKind::Int(1));
    kind!(r.to.as_ref().unwrap(), ExprKind::Int(10));
    assert!(r.next.is_none());
    let ast = ok("0..<10");
    let r = kind!(expr(&ast), ExprKind::Range(r) => r);
    assert_eq!(r.inclusion, RangeInclusion::RightExclusive);
    assert_eq!(text(&ast, r.op_span), "..<");
    let ast = ok("1..=10");
    assert_eq!(text(&ast, kind!(expr(&ast), ExprKind::Range(r) => r).op_span), "..=");
    let ast = ok("1..3..10");
    let r = kind!(expr(&ast), ExprKind::Range(r) => r);
    kind!(r.next.as_ref().unwrap(), ExprKind::Int(3));
    assert_eq!(text(&ast, r.next_op_span.unwrap()), "..");
    let ast = ok("..5");
    let r = kind!(expr(&ast), ExprKind::Range(r) => r);
    assert!(r.from.is_none());
    let ast = ok("5..");
    let r = kind!(expr(&ast), ExprKind::Range(r) => r);
    assert!(r.to.is_none());
    let ast = ok("-5..5");
    kind!(expr(&ast), ExprKind::Range(_));
    let ast = ok("1.5..3");
    kind!(expr(&ast), ExprKind::Range(_));
    let ast = ok("$x..$y.len");
    let r = kind!(expr(&ast), ExprKind::Range(r) => r);
    kind!(r.from.as_ref().unwrap(), ExprKind::Var(_));
    kind!(r.to.as_ref().unwrap(), ExprKind::FullCellPath(_));
    let ast = ok("0..($n - 1)");
    let r = kind!(expr(&ast), ExprKind::Range(r) => r);
    kind!(r.to.as_ref().unwrap(), ExprKind::Subexpression(_));
    let ast = ok("(1)..3");
    kind!(expr(&ast), ExprKind::Range(_));
    // Not ranges.
    let ast = ok("cd ..");
    assert_eq!(string(call(expr(&ast)).positionals().next().unwrap()).0, "..");
    let ast = ok("echo a..b");
    kind!(call(expr(&ast)).positionals().next().unwrap(), ExprKind::String(_));
}

// --- collections ---------------------------------------------------------------------

#[test]
fn lists() {
    let ast = ok("[1, 2 3\n 4 # comment\n ...$rest, ...[5 6] ...(seq 7 8)]");
    let items = kind!(expr(&ast), ExprKind::List(l) => l);
    assert_eq!(items.len(), 7);
    assert!(matches!(items[4], ListItem::Spread { .. }));
    assert!(matches!(items[5], ListItem::Spread { .. }));
    assert!(matches!(items[6], ListItem::Spread { .. }));
    assert_eq!(ast.comments.len(), 1);
    let ast = ok("[a b c]");
    let items = kind!(expr(&ast), ExprKind::List(l) => l);
    assert!(items.iter().all(|i| matches!(i, ListItem::Item(Expr { kind: ExprKind::String(_), .. }))));
    let ast = ok("[1 + 1]");
    assert_eq!(kind!(expr(&ast), ExprKind::List(l) => l).len(), 3);
    let ast = ok("[]");
    assert!(kind!(expr(&ast), ExprKind::List(l) => l).is_empty());
    // Nushell tolerates `|` and `;` between items, and bare operators are words.
    let ast = ok("[a | b]");
    assert_eq!(kind!(expr(&ast), ExprKind::List(l) => l).len(), 2);
    let ast = ok("[Assignment, =, Assign]");
    assert_eq!(kind!(expr(&ast), ExprKind::List(l) => l).len(), 3);
    let ast = ok("[...foo]");
    let items = kind!(expr(&ast), ExprKind::List(l) => l);
    assert!(matches!(items[0], ListItem::Item(_)), "`...foo` is a bare word, not a spread");
}

#[test]
fn tables() {
    let ast = ok("[[a b]; [1 2] [3 4], [5 6]]");
    let t = kind!(expr(&ast), ExprKind::Table(t) => t);
    assert_eq!(kind!(t.columns, ExprKind::List(l) => l).len(), 2);
    assert_eq!(t.rows.len(), 3);
    let ast = ok("[[a b];\n  [1 2]\n  [3 4]\n]");
    kind!(expr(&ast), ExprKind::Table(t) => assert_eq!(t.rows.len(), 2));
    let ast = ok("[[a b]]");
    kind!(expr(&ast), ExprKind::List(_));
    assert!(parse("[[a]; 1]").is_err());
}

#[test]
fn records() {
    let src =
        "{a: 1, b: \"two\" c: [3], \"d e\": {f: 4}\n g: null # comment\n ...$spread, $key: 5, (1 + 1): 6, 7: 8, h:9}";
    let ast = ok(src);
    let items = kind!(expr(&ast), ExprKind::Record(r) => r);
    assert_eq!(items.len(), 10);
    let keys: Vec<&str> = items
        .iter()
        .map(|i| match i {
            RecordItem::Pair { key, .. } => text(&ast, key.span),
            RecordItem::Spread { .. } => "...",
        })
        .collect();
    assert_eq!(keys, vec!["a", "b", "c", "\"d e\"", "g", "...", "$key", "(1 + 1)", "7", "h"]);
    match &items[6] {
        RecordItem::Pair { key, .. } => kind!(key, ExprKind::Var(_)),
        _ => panic!(),
    }
    match &items[7] {
        RecordItem::Pair { key, .. } => kind!(key, ExprKind::Subexpression(_)),
        _ => panic!(),
    }
    match &items[8] {
        RecordItem::Pair { key, value, .. } => {
            assert_eq!(string(key).0, "7");
            kind!(value, ExprKind::Int(8));
        }
        _ => panic!(),
    }
    assert_eq!(ast.comments.len(), 1);
    let ast = ok("{}");
    assert!(kind!(expr(&ast), ExprKind::Record(r) => r).is_empty());
    // Like nu, a bare word containing `:` is refused as a record key or value.
    assert!(parse("{a: http://x.y}").is_err());
    assert!(parse("{ :: x }").is_err());
    let ast = ok("{a: \"http://x.y\"}");
    kind!(expr(&ast), ExprKind::Record(_));
    let ast = ok("{a 1}");
    kind!(expr(&ast), ExprKind::Closure(_));
    assert!(parse("{a: 1 + 1}").is_err());
}

#[test]
fn closures_and_blocks() {
    let ast = ok("{|x, y: int| $x + $y }");
    let c = kind!(expr(&ast), ExprKind::Closure(c) => c);
    let sig = c.params.as_ref().unwrap();
    assert_eq!(text(&ast, sig.span), "|x, y: int|");
    assert_eq!(sig.params.len(), 2);
    assert_eq!(sig.params[1].name.item, "y");
    assert!(matches!(sig.params[1].ty.as_ref().unwrap().kind, TypeKind::Int));
    assert_eq!(c.body.pipelines.len(), 1);
    let ast = ok("{ || 1 }");
    let c = kind!(expr(&ast), ExprKind::Closure(c) => c);
    assert!(c.params.as_ref().unwrap().params.is_empty());
    let ast = ok("{ print hi }");
    kind!(expr(&ast), ExprKind::Closure(c) => assert!(c.params.is_none()));
    let ast = ok("{ $in | length }");
    kind!(expr(&ast), ExprKind::Closure(_));
    let ast = ok("each { |it|\n  # comment\n  $it * 2\n}");
    let c = kind!(call(expr(&ast)).positionals().next().unwrap(), ExprKind::Closure(c) => c);
    assert_eq!(c.body.pipelines.len(), 1);
    assert_eq!(c.body.pipelines[0].leading_comments.len(), 1);
    let ast = ok("do { print a; print b }");
    let c = kind!(call(expr(&ast)).positionals().next().unwrap(), ExprKind::Closure(c) => c);
    assert_eq!(c.body.pipelines.len(), 2);
    assert!(matches!(err("if true {|x| 1 }").primary().kind, ErrorKind::Expected("block")));
}

#[test]
fn subexpressions_span_lines() {
    let ast = ok("(\n  ls\n  | sort-by modified\n  --reverse # comment\n  | length\n)");
    let sub = kind!(expr(&ast), ExprKind::Subexpression(b) => b);
    assert_eq!(sub.pipelines.len(), 1);
    assert_eq!(sub.pipelines[0].elements.len(), 3);
    let ast = ok("(1 +\n 2)");
    kind!(kind!(expr(&ast), ExprKind::Subexpression(b) => b).pipelines[0].elements[0].expr, ExprKind::BinaryOp(_));
    let ast = ok("(ls; pwd)");
    assert_eq!(kind!(expr(&ast), ExprKind::Subexpression(b) => b).pipelines.len(), 2);
}

// --- calls -------------------------------------------------------------------------------

#[test]
fn multiword_commands_and_flags() {
    let ast = ok("str trim --left --char=x -abc -- --not-a-flag -5 ...$args ...[1 2]");
    let c = call(expr(&ast));
    assert_eq!(c.head.name, "str trim");
    assert_eq!(text(&ast, c.head.span), "str trim");
    let describe: Vec<String> = c
        .args
        .iter()
        .map(|a| match a {
            Arg::Flag(f) => format!("flag:{}:{}:{}", f.name, f.long, f.value.is_some()),
            Arg::Positional(e) => format!("pos:{}", text(&ast, e.span)),
            Arg::Spread { expr, .. } => format!("spread:{}", text(&ast, expr.span)),
            Arg::EndOfOptions(_) => "--".into(),
        })
        .collect();
    assert_eq!(
        describe,
        vec![
            "flag:left:true:false",
            "flag:char:true:true",
            "flag:abc:false:false",
            "--",
            "flag:not-a-flag:true:false",
            "pos:-5",
            "spread:$args",
            "spread:[1 2]"
        ]
    );
    kind!(c.flag("char").unwrap().value.as_ref().unwrap(), ExprKind::String(_));
    let ast = ok("into int | date to-timezone utc | attr example");
    let names: Vec<_> = elements(&ast).iter().map(|e| call(&e.expr).head.name.to_string()).collect();
    assert_eq!(names, vec!["into int", "date to-timezone", "attr example"]);
}

#[test]
fn user_defined_multiword_commands_resolve() {
    let src = "def \"my cmd\" [x] { $x }\nmy cmd 1\ndef later [] { }\nlater\n";
    let ast = ok(src);
    assert_eq!(call(&ast.block.pipelines[1].elements[0].expr).head.name, "my cmd");
    assert_eq!(call(&ast.block.pipelines[1].elements[0].expr).args.len(), 1);
    // Without a known table, `str trim` is `str` with an argument.
    let ast = parse_with("str trim", &ParseConfig::empty()).unwrap();
    assert_eq!(call(expr(&ast)).head.name, "str");
    let ast = parse_with("my cmd 1", &ParseConfig::with_commands(["my cmd"])).unwrap();
    assert_eq!(call(expr(&ast)).head.name, "my cmd");
}

#[test]
fn definitions_cannot_use_parser_keywords() {
    for name in ["loop", "if", "def", "where", "run", "\"break\""] {
        let e = err(&format!("def {name} [x] {{ $x }}"));
        assert!(matches!(e.primary().kind, ErrorKind::Message(_)), "{name}: {e}");
    }
    assert!(parse("extern \"while\" []").is_err());
    assert!(parse("alias if = ls").is_err());
    // A module command shadowing a keyword is still a call once configured.
    let ast = parse_with("do-it 1 2", &ParseConfig::with_commands(["do-it"])).unwrap();
    assert_eq!(call(expr(&ast)).args.len(), 2);
}

#[test]
fn external_calls() {
    let ast = ok("^git commit -m 'msg' --all $file (pwd)/x ...$rest [a b] {c: 1}");
    let e = kind!(expr(&ast), ExprKind::ExternalCall(e) => e);
    assert_eq!(text(&ast, e.caret), "^");
    assert_eq!(string(&e.head), ("git", Quote::Bare));
    assert_eq!(e.args.len(), 9);
    let regular: Vec<&Expr<'_>> = e
        .args
        .iter()
        .filter_map(|a| match a {
            ExternalArg::Regular(e) => Some(e),
            _ => None,
        })
        .collect();
    assert_eq!(string(regular[1]), ("-m", Quote::Bare));
    assert_eq!(string(regular[2]), ("msg", Quote::Single));
    kind!(regular[4], ExprKind::Var(_));
    kind!(regular[5], ExprKind::Interpolation(i) => assert_eq!(i.parts.len(), 2));
    kind!(regular[6], ExprKind::List(_));
    kind!(regular[7], ExprKind::Record(_));
    assert!(matches!(e.args[6], ExternalArg::Spread { .. }));
    // Quoted parts keep parentheses literal.
    let ast = ok("^gh api -f query='q($x) { y }'");
    let e = kind!(expr(&ast), ExprKind::ExternalCall(e) => e);
    match &e.args[2] {
        ExternalArg::Regular(a) => assert_eq!(string(a).0, "query=q($x) { y }"),
        other => panic!("{other:?}"),
    }
    let ast = ok("^$cmd --x");
    kind!(kind!(expr(&ast), ExprKind::ExternalCall(e) => e).head, ExprKind::Var(_));
    let ast = ok("^(which ls | get 0.path)");
    kind!(kind!(expr(&ast), ExprKind::ExternalCall(e) => e).head, ExprKind::Subexpression(_));
}

#[test]
fn env_shorthand() {
    let ast = ok("FOO=bar BAZ=$x EMPTY= cmd arg");
    let e = kind!(expr(&ast), ExprKind::EnvShorthand(e) => e);
    assert_eq!(e.vars.len(), 3);
    assert_eq!(e.vars[0].name.item, "FOO");
    assert_eq!(string(&e.vars[0].value).0, "bar");
    kind!(e.vars[1].value, ExprKind::Var(_));
    assert_eq!(string(&e.vars[2].value).0, "");
    assert_eq!(call(&e.expr).head.name, "cmd");
    let ast = ok("echo a=b");
    kind!(expr(&ast), ExprKind::Call(_));
    assert!(parse("FOO=bar").is_err());
}

#[test]
fn where_row_conditions() {
    let ast = ok("where size > 1kb and name =~ 'x' or not active");
    let w = kind!(expr(&ast), ExprKind::Where(w) => w);
    let or = kind!(w.condition, ExprKind::BinaryOp(b) => b);
    let and = kind!(or.lhs, ExprKind::BinaryOp(b) => b);
    let size = kind!(and.lhs, ExprKind::BinaryOp(b) => b);
    let path = kind!(size.lhs, ExprKind::FullCellPath(p) => p);
    assert!(path.implicit_head);
    kind!(path.head, ExprKind::Var(Var { name: "it" }));
    assert!(path.head.span.is_empty());
    // Only the left operand of each operator is expanded, as in Nushell.
    let not = kind!(or.rhs, ExprKind::UnaryNot(n) => n);
    kind!(not.expr, ExprKind::String(_));
    let ast = ok("where not active");
    let not = kind!(kind!(expr(&ast), ExprKind::Where(w) => w).condition, ExprKind::UnaryNot(n) => n);
    kind!(not.expr, ExprKind::FullCellPath(p) => assert!(p.implicit_head));
    let ast = ok("where name.first == 'a'");
    let w = kind!(expr(&ast), ExprKind::Where(w) => w);
    let cmp = kind!(w.condition, ExprKind::BinaryOp(b) => b);
    assert_eq!(kind!(cmp.lhs, ExprKind::FullCellPath(p) => p).members.len(), 2);
    let ast = ok("where {|x| $x > 1 }");
    kind!(kind!(expr(&ast), ExprKind::Where(w) => w).condition, ExprKind::Closure(_));
    let ast = ok("where $it.size > 1kb");
    let w = kind!(expr(&ast), ExprKind::Where(w) => w);
    let cmp = kind!(w.condition, ExprKind::BinaryOp(b) => b);
    kind!(cmp.lhs, ExprKind::FullCellPath(p) => assert!(!p.implicit_head));
    let ast = ok("where active");
    kind!(kind!(expr(&ast), ExprKind::Where(w) => w).condition, ExprKind::FullCellPath(p) => assert!(p.implicit_head));
    let ast = ok("where ($it.a | is-empty)");
    kind!(kind!(expr(&ast), ExprKind::Where(w) => w).condition, ExprKind::Subexpression(_));
}

// --- declarations ------------------------------------------------------------------------

#[test]
fn let_mut_const() {
    let ast = ok("let x = 1 + 1 | into string");
    let b = kind!(expr(&ast), ExprKind::Let(b) => b);
    assert_eq!(b.name.item, "x");
    assert_eq!(text(&ast, b.eq.unwrap()), "=");
    let value = b.value.as_ref().unwrap();
    assert_eq!(value.pipelines[0].elements.len(), 2);
    assert_eq!(text(&ast, value.span), "1 + 1 | into string");
    let ast = ok("mut y: list<int> = []");
    let b = kind!(expr(&ast), ExprKind::Mut(b) => b);
    assert!(matches!(b.ty.as_ref().unwrap().kind, TypeKind::List(Some(_))));
    let ast = ok("const c: record<a: int, b: string> = {a: 1, b: x}");
    let b = kind!(expr(&ast), ExprKind::Const(b) => b);
    assert!(matches!(b.ty.as_ref().unwrap().kind, TypeKind::Record(ref f) if f.len() == 2));
    let ast = ok("let $z = 1");
    assert_eq!(kind!(expr(&ast), ExprKind::Let(b) => b).name.item, "z");
    let ast = ok("let input");
    assert!(kind!(expr(&ast), ExprKind::Let(b) => b).value.is_none());
    let ast = ok("let x = if true { 1 } else { 2 }");
    kind!(expr(&ast), ExprKind::Let(_));
    let ast = ok("let x = ls\n  | length");
    assert_eq!(kind!(expr(&ast), ExprKind::Let(b) => b).value.as_ref().unwrap().pipelines[0].elements.len(), 2);
    let ast = ok("let x = ^cmd o> file");
    let value = kind!(expr(&ast), ExprKind::Let(b) => b).value.as_ref().unwrap();
    assert!(value.pipelines[0].elements[0].redirection.is_some());
    assert!(parse("let x += 1").is_err());
    assert!(parse("let x =").is_err());
    assert!(parse("let 'a b' = 1").is_err());
}

#[test]
fn assignments() {
    let ast = ok("$x = 1");
    let a = kind!(expr(&ast), ExprKind::Assignment(a) => a);
    assert_eq!(a.op.item, AssignOp::Assign);
    kind!(a.lhs, ExprKind::Var(_));
    let ast = ok("$env.PATH ++= [/bin]");
    let a = kind!(expr(&ast), ExprKind::Assignment(a) => a);
    assert_eq!(a.op.item, AssignOp::ConcatAssign);
    kind!(a.lhs, ExprKind::FullCellPath(_));
    let ast = ok("$x.a.0 += 1 | into int");
    let a = kind!(expr(&ast), ExprKind::Assignment(a) => a);
    assert_eq!(a.rhs.pipelines[0].elements.len(), 2);
    for op in ["-=", "*=", "/="] {
        ok(&format!("$x {op} 2"));
    }
    assert!(parse("1 = 2").is_err());
    assert!(parse("$x =").is_err());
}

#[test]
fn def_forms() {
    let src = "def --env --wrapped \"my cmd\" [\n  a: int, # the a\n  b?: string = \"x\"\n  --flag(-f): int = 3\n  -s\n  --long (-l)\n  ...rest: any\n  name: string@completer\n]: [int -> string, nothing -> nothing] {\n  $a\n}";
    let ast = ok(src);
    let d = kind!(expr(&ast), ExprKind::Def(d) => d);
    assert_eq!(d.name.item, "my cmd");
    assert_eq!(d.flags.iter().map(|f| f.item).collect::<Vec<_>>(), vec![DefFlag::Env, DefFlag::Wrapped]);
    let sig = &d.signature;
    let names: Vec<_> = sig.params.iter().map(|p| p.name.item).collect();
    assert_eq!(names, vec!["a", "b", "flag", "s", "long", "rest", "name"]);
    assert!(matches!(sig.params[0].kind, ParamKind::Positional { optional: false }));
    assert_eq!(sig.params[0].description.unwrap().body(src), "the a");
    assert!(matches!(sig.params[1].kind, ParamKind::Positional { optional: true }));
    assert!(sig.params[1].default.is_some());
    match &sig.params[2].kind {
        ParamKind::Flag { long, short } => {
            assert_eq!(long.unwrap().item, "flag");
            assert_eq!(short.unwrap().item, 'f');
        }
        other => panic!("{other:?}"),
    }
    kind!(sig.params[2].default.as_ref().unwrap(), ExprKind::Int(3));
    assert!(matches!(sig.params[3].kind, ParamKind::Flag { long: None, short: Some(_) }));
    assert!(matches!(sig.params[4].kind, ParamKind::Flag { long: Some(_), short: Some(_) }));
    assert!(matches!(sig.params[5].kind, ParamKind::Rest));
    assert_eq!(sig.params[6].completer.unwrap().item, "completer");
    assert_eq!(sig.io_types.len(), 2);
    assert!(matches!(sig.io_types[0].input.kind, TypeKind::Int));
    assert!(matches!(sig.io_types[1].output.kind, TypeKind::Nothing));
    assert_eq!(d.body.pipelines.len(), 1);
    let ast = ok("def foo [] { }");
    let d = kind!(expr(&ast), ExprKind::Def(d) => d);
    assert!(d.body.pipelines.is_empty());
    assert!(d.signature.io_types.is_empty());
    let ast = ok("def foo (a b) { }");
    assert_eq!(kind!(expr(&ast), ExprKind::Def(d) => d).signature.params.len(), 2);
    let ast = ok("def foo []: nothing -> string { }");
    assert_eq!(kind!(expr(&ast), ExprKind::Def(d) => d).signature.io_types.len(), 1);
    let ast = ok("def foo [] : nothing -> string { }");
    assert_eq!(kind!(expr(&ast), ExprKind::Def(d) => d).signature.io_types.len(), 1);
    let ast = ok("def foo []: [\n  list<any> -> nothing,\n  record<a: int b: string> -> table<x: path>\n] { }");
    assert_eq!(kind!(expr(&ast), ExprKind::Def(d) => d).signature.io_types.len(), 2);
    assert!(parse("def foo [] int -> int { }").is_err());
    assert!(parse("def foo { }").is_err());
    assert!(parse("def foo [x: unknown] { }").is_err());
    assert!(parse("def foo [x: block] { }").is_err());
    assert!(parse("def foo [x: int = ] { }").is_err());
}

#[test]
fn types_in_signatures() {
    let src = "def f [a: list<record<x: int, y: list<string>>>, b: oneof<int, string>, c: table, d: record, e: list, f: closure, g: cell-path, h: glob, i: path, j: directory, k: duration, l: filesize, m: datetime, n: binary, o: bool, p: float, q: number, r: range, s: any, t: nothing, u: error, v: external_arg] { }";
    let ast = ok(src);
    let d = kind!(expr(&ast), ExprKind::Def(d) => d);
    assert_eq!(d.signature.params.len(), 22);
    match &d.signature.params[0].ty.as_ref().unwrap().kind {
        TypeKind::List(Some(inner)) => match &inner.kind {
            TypeKind::Record(fields) => {
                assert_eq!(fields.len(), 2);
                assert!(matches!(fields[1].ty.kind, TypeKind::List(Some(_))));
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
    assert!(matches!(d.signature.params[1].ty.as_ref().unwrap().kind, TypeKind::OneOf(ref t) if t.len() == 2));
}

#[test]
fn extern_alias_module_use_export() {
    let ast = ok("extern gh [--repo: string, ...args]: nothing -> string");
    let x = kind!(expr(&ast), ExprKind::Extern(x) => x);
    assert_eq!(x.name.item, "gh");
    assert_eq!(x.signature.params.len(), 2);
    let ast = ok("alias ll = ls -l");
    let a = kind!(expr(&ast), ExprKind::Alias(a) => a);
    assert_eq!(a.name.item, "ll");
    assert_eq!(call(&a.value).head.name, "ls");
    // Nushell accepts a pipe here and treats it as a word.
    let ast = ok("alias ll = ls | length");
    let a = kind!(expr(&ast), ExprKind::Alias(a) => a);
    assert_eq!(call(&a.value).args.len(), 2);
    let ast = ok(
        "module m {\n  export def f [] { 2 }\n  export const c = 1\n  export-env { $env.A = 1 }\n  export alias g = f\n  export use other *\n  export module inner { }\n  export extern e []\n}",
    );
    let m = kind!(expr(&ast), ExprKind::Module(m) => m);
    assert_eq!(string(&m.name).0, "m");
    let body = m.body.as_ref().unwrap();
    assert_eq!(body.pipelines.len(), 7);
    for (i, expected) in ["def", "const", "export-env", "alias", "use", "module", "extern"].iter().enumerate() {
        let e = &body.pipelines[i].elements[0].expr;
        match (&e.kind, *expected) {
            (ExprKind::ExportEnv(_), "export-env") => {}
            (ExprKind::Export(x), _) => {
                let inner = match &x.item.kind {
                    ExprKind::Def(_) => "def",
                    ExprKind::Const(_) => "const",
                    ExprKind::Alias(_) => "alias",
                    ExprKind::Use(_) => "use",
                    ExprKind::Module(_) => "module",
                    ExprKind::Extern(_) => "extern",
                    other => panic!("{other:?}"),
                };
                assert_eq!(inner, *expected);
            }
            other => panic!("{other:?}"),
        }
    }
    let ast = ok("module ./path/to/mod.nu");
    assert!(kind!(expr(&ast), ExprKind::Module(m) => m).body.is_none());
    assert!(parse("export foo").is_err());
}

#[test]
fn use_forms() {
    let ast = ok("use std/log");
    let u = kind!(expr(&ast), ExprKind::Use(u) => u);
    assert_eq!(string(&u.module).0, "std/log");
    assert!(u.members.is_empty());
    let ast = ok("use std [log, assert]");
    let u = kind!(expr(&ast), ExprKind::Use(u) => u);
    match &u.members[0].kind {
        UseMemberKind::List(names) => {
            assert_eq!(names.iter().map(|n| n.item.as_ref()).collect::<Vec<_>>(), vec!["log", "assert"])
        }
        other => panic!("{other:?}"),
    }
    let ast = ok("use std *");
    assert!(matches!(kind!(expr(&ast), ExprKind::Use(u) => u).members[0].kind, UseMemberKind::Glob));
    let ast = ok("use std log info");
    let u = kind!(expr(&ast), ExprKind::Use(u) => u);
    assert_eq!(u.members.len(), 2);
    let ast = ok("use \"path with space.nu\" name");
    kind!(expr(&ast), ExprKind::Use(_));
    let ast = ok("use null");
    kind!(kind!(expr(&ast), ExprKind::Use(u) => u).module, ExprKind::Nothing);
    assert!(parse("use std * log").is_err());
    let ast = ok(
        "hide std log\nhide-env FOO\nsource file.nu\nsource-env env.nu\noverlay use ./foo.nu as bar --prefix\noverlay new x\noverlay hide\noverlay list\nplugin use query",
    );
    for p in &ast.block.pipelines {
        kind!(p.elements[0].expr, ExprKind::Call(_));
    }
    assert_eq!(call(&ast.block.pipelines[4].elements[0].expr).head.name, "overlay use");
}

#[test]
fn attributes() {
    let src = "# doc\n@example \"add\" { 1 + 1 } --result 2 # why\n@search-terms math plus\n@category math\nexport def add [] { }\n";
    let ast = ok(src);
    let a = kind!(expr(&ast), ExprKind::AttributeBlock(a) => a);
    assert_eq!(a.attributes.len(), 3);
    assert_eq!(a.attributes[0].name.item, "example");
    assert_eq!(a.attributes[0].args.len(), 4, "`--result 2` is a flag followed by a positional");
    assert_eq!(a.attributes[1].name.item, "search-terms");
    assert_eq!(text(&ast, a.attributes[1].name.span), "search-terms");
    kind!(a.item, ExprKind::Export(_));
    assert_eq!(ast.comments.len(), 2);
    let ast = ok("@deprecated\ndef old [] { }");
    kind!(expr(&ast), ExprKind::AttributeBlock(_));
    assert!(parse("@example x\nls").is_err());
    assert!(parse("@example x\n").is_err());
    // Like nu, nothing may come between the attributes and the definition.
    assert!(parse("@example x\n\ndef f [] { }").is_err());
    assert!(parse("@example x\n# doc\ndef f [] { }").is_err());
}

// --- control flow --------------------------------------------------------------------

#[test]
fn if_forms() {
    let ast = ok("if $x > 1 { a } else if $y { b } else { c }");
    let i = kind!(expr(&ast), ExprKind::If(i) => i);
    kind!(i.condition, ExprKind::BinaryOp(_));
    assert_eq!(i.then_block.pipelines.len(), 1);
    let e = i.else_branch.as_ref().unwrap();
    assert_eq!(text(&ast, e.keyword), "else");
    let inner = kind!(e.body, ExprKind::If(i) => i);
    kind!(inner.else_branch.as_ref().unwrap().body, ExprKind::Block(_));
    let ast = ok("if true { 1 }");
    assert!(kind!(expr(&ast), ExprKind::If(i) => i).else_branch.is_none());
    let ast = ok("if (ls | is-empty) { } else (print no)");
    kind!(kind!(expr(&ast), ExprKind::If(i) => i).else_branch.as_ref().unwrap().body, ExprKind::Subexpression(_));
    let ast = ok("if ($a and\n $b) { }");
    kind!(expr(&ast), ExprKind::If(_));
    assert!(parse("if $a and\n $b { }").is_err(), "a newline ends the statement outside parentheses");
    assert!(parse("if { }").is_err());
    assert!(parse("if true").is_err());
    assert!(parse("if true { } else").is_err());
    assert!(parse("if true 1").is_err());
}

#[test]
fn match_forms() {
    let src = "match $x {\n  1 | 2 => \"low\",\n  3..5 => { print mid; \"mid\" }\n  $n if $n > 100 => \"huge\"\n  [$a, $b, ..$rest] => $a\n  [.. $last] => $last\n  {name: $name, age: 3} => $name\n  {$shorthand} => 1\n  \"str\" => 2\n  (1 + 1) => 3\n  {a: 1} => {b: 2}\n  _ => {|| 1}\n  null => print\n}";
    let ast = ok(src);
    let m = kind!(expr(&ast), ExprKind::Match(m) => m);
    kind!(m.value, ExprKind::Var(_));
    assert_eq!(m.arms.len(), 12);
    let kinds: Vec<&str> = m
        .arms
        .iter()
        .map(|a| match &a.pattern.kind {
            PatternKind::Value(_) => "value",
            PatternKind::Variable(_) => "var",
            PatternKind::Wildcard => "_",
            PatternKind::List(_) => "list",
            PatternKind::Record(_) => "record",
            PatternKind::Rest(_) => "rest",
            PatternKind::Or(_) => "or",
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["or", "value", "var", "list", "list", "record", "record", "value", "value", "record", "_", "value"]
    );
    assert!(m.arms[2].guard.is_some());
    assert_eq!(text(&ast, m.arms[2].arrow), "=>");
    kind!(m.arms[1].body, ExprKind::Block(b) => assert_eq!(b.pipelines.len(), 2));
    match &m.arms[3].pattern.kind {
        PatternKind::List(items) => {
            assert!(matches!(items[2].kind, PatternKind::Rest(Some(r)) if r.item == "rest"));
        }
        _ => panic!(),
    }
    match &m.arms[4].pattern.kind {
        PatternKind::List(items) => assert!(matches!(items[0].kind, PatternKind::Rest(None))),
        _ => panic!(),
    }
    match &m.arms[6].pattern.kind {
        PatternKind::Record(fields) => {
            assert_eq!(fields[0].0.item, "shorthand");
            assert!(matches!(fields[0].1.kind, PatternKind::Variable("shorthand")));
        }
        _ => panic!(),
    }
    kind!(m.arms[9].body, ExprKind::Record(_));
    kind!(m.arms[10].body, ExprKind::Closure(_));
    kind!(m.arms[11].body, ExprKind::Call(_));
    assert!(parse("match $x { 1 }").is_err());
    assert!(parse("match $x { 1 => }").is_err());
    assert!(parse("match $x { $a if => 1 }").is_err());
}

#[test]
fn loops_and_jumps() {
    let ast = ok("for x in [1 2 3] { print $x }");
    let f = kind!(expr(&ast), ExprKind::For(f) => f);
    assert_eq!(f.var.item, "x");
    assert_eq!(text(&ast, f.in_keyword), "in");
    kind!(f.iterable, ExprKind::List(_));
    let ast = ok("for x: int in 1..10 { }");
    assert!(matches!(kind!(expr(&ast), ExprKind::For(f) => f).ty.as_ref().unwrap().kind, TypeKind::Int));
    let ast = ok("for $y in $list { }");
    assert_eq!(kind!(expr(&ast), ExprKind::For(f) => f).var.item, "y");
    let ast = ok("while $i < 10 { $i += 1; if $i == 5 { break } else { continue } }");
    let w = kind!(expr(&ast), ExprKind::While(w) => w);
    assert_eq!(w.body.pipelines.len(), 2);
    let ast = ok("loop { break }");
    kind!(kind!(expr(&ast), ExprKind::Loop(l) => l).body.pipelines[0].elements[0].expr, ExprKind::Break);
    let ast = ok("def f [] { return 5 }");
    let d = kind!(expr(&ast), ExprKind::Def(d) => d);
    kind!(d.body.pipelines[0].elements[0].expr, ExprKind::Return(r) => kind!(r.value.as_ref().unwrap(), ExprKind::Int(5)));
    let ast = ok("return");
    kind!(expr(&ast), ExprKind::Return(r) => assert!(r.value.is_none()));
    assert!(parse("for x [1] { }").is_err());
    assert!(parse("break 1").is_err());
}

#[test]
fn try_forms() {
    let ast = ok("try { risky } catch { |e| print $e.msg } finally { cleanup }");
    let t = kind!(expr(&ast), ExprKind::Try(t) => t);
    assert_eq!(t.body.pipelines.len(), 1);
    kind!(t.catch().unwrap().body, ExprKind::Closure(c) => assert_eq!(c.params.as_ref().unwrap().params.len(), 1));
    kind!(t.finally().unwrap().body, ExprKind::Closure(_));
    let ast = ok("try { 1 }");
    let t = kind!(expr(&ast), ExprKind::Try(t) => t);
    assert!(t.handlers.is_empty());
    let ast = ok("try { 1 } catch $handler");
    kind!(kind!(expr(&ast), ExprKind::Try(t) => t).catch().unwrap().body, ExprKind::Var(_));
    let ast = ok("try { 1 } finally { 2 } catch { 3 }");
    assert_eq!(kind!(expr(&ast), ExprKind::Try(t) => t).handlers.len(), 2);
    // Two handlers of the same kind are accepted, as in Nushell; a third is not.
    ok("try { 1 } catch { 2 } catch { 3 }");
    assert!(parse("try { 1 } catch { 2 } catch { 3 } finally { 4 }").is_err());
    assert!(parse("try { 1 } foo { 2 }").is_err());
}

// --- redirections -----------------------------------------------------------------------

#[test]
fn redirections() {
    let ast = ok("cmd o> out.txt e>> err.txt");
    let r = elements(&ast)[0].redirection.as_ref().unwrap();
    match r {
        Redirection::Separate { out, err } => {
            assert!(matches!(out, RedirectTarget::File { append: false, .. }));
            assert!(matches!(err, RedirectTarget::File { append: true, .. }));
            assert_eq!(text(&ast, err.op_span()), "e>>");
        }
        other => panic!("{other:?}"),
    }
    let ast = ok("cmd o+e> all.txt | next");
    match elements(&ast)[0].redirection.as_ref().unwrap() {
        Redirection::Single { source: RedirectSource::StdoutAndStderr, target: RedirectTarget::File { path, .. } } => {
            assert_eq!(string(path).0, "all.txt");
        }
        other => panic!("{other:?}"),
    }
    let ast = ok("cmd e>| lines | length");
    let els = elements(&ast);
    assert_eq!(els.len(), 3);
    match els[0].redirection.as_ref().unwrap() {
        Redirection::Single { source: RedirectSource::Stderr, target: RedirectTarget::Pipe { op } } => {
            assert_eq!(op.item, RedirectOp::ErrPipe);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(text(&ast, els[1].pipe.unwrap()), "e>|");
    for op in ["out>", "err>", "out+err>", "err+out>", "o+e>>", "out+err>|", "e+o>"] {
        ok(&format!("cmd {op} x"));
    }
    assert!(parse("cmd o> a o> b").is_err());
    assert!(parse("o> file").is_err());
    assert!(parse("cmd o>").is_err());
    assert!(parse("def f [] { } o> x").is_err());
}

#[test]
fn bashisms_are_reported_with_help() {
    let e = err("a && b");
    assert!(matches!(e.primary().kind, ErrorKind::ShellSyntax { found: "&&", .. }));
    let e = err("a || b");
    assert!(matches!(e.primary().kind, ErrorKind::ShellSyntax { found: "||", .. }));
    let e = err("cmd 2> err");
    assert!(matches!(e.primary().kind, ErrorKind::ShellSyntax { found: "2>", .. }));
    let e = err("cmd 2>&1");
    assert!(matches!(e.primary().kind, ErrorKind::ShellSyntax { found: "2>&1", .. }));
    let e = err("cmd o>| x");
    assert!(matches!(e.primary().kind, ErrorKind::ShellSyntax { found: "o>|", .. }));
}

// --- errors, recovery and spans ------------------------------------------------------

#[test]
fn unclosed_delimiters_report_opener() {
    let e = err("let x = [1 2\nlet y = 2");
    match &e.primary().kind {
        ErrorKind::Unclosed { delimiter: "]", open } => assert_eq!(text_of("let x = [1 2\nlet y = 2", *open), "["),
        other => panic!("{other:?}"),
    }
    let e = err("def f [] {");
    assert!(matches!(e.primary().kind, ErrorKind::Unclosed { delimiter: "}", .. }));
    let e = err("echo )");
    assert!(matches!(e.primary().kind, ErrorKind::Unbalanced { found: ")", .. }));
}

fn text_of(src: &str, span: Span) -> &str {
    span.slice(src)
}

#[test]
fn recovery_keeps_parsing_later_statements() {
    let src = "ls\nlet = 1\ndef f [] { let x }\n1 +\npwd\n";
    let (ast, diagnostics) = parse_lenient(src, &ParseConfig::new());
    assert_eq!(ast.block.pipelines.len(), 5);
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    assert!(ast.block.pipelines[1].elements[0].expr.is_garbage());
    assert!(ast.block.pipelines[3].elements[0].expr.is_garbage());
    assert_eq!(call(&ast.block.pipelines[4].elements[0].expr).head.name, "pwd");
    // Errors inside nested blocks are reported without failing the enclosing statement.
    let src = "def f [] {\n  1 +\n  ls\n}\npwd";
    let (ast, diagnostics) = parse_lenient(src, &ParseConfig::new());
    assert_eq!(diagnostics.len(), 1);
    kind!(ast.block.pipelines[0].elements[0].expr, ExprKind::Def(d) => assert_eq!(d.body.pipelines.len(), 2));
    assert!(parse(src).is_err());
}

#[test]
fn diagnostics_render_with_context() {
    let src = "def foo [x: int {\n  1\n}";
    let e = err(src);
    let rendered = e.render(src, Some("t.nu"));
    assert!(rendered.contains("--> t.nu:"), "{rendered}");
    assert!(rendered.contains('^'));
    let e = err("def foo [x:] { }");
    assert_eq!(e.primary().context.first().copied(), Some("signature"));
}

/// Every node's span must lie inside its parent's span and on char boundaries.
struct SpanChecker<'s> {
    src: &'s str,
    stack: Vec<Span>,
    count: usize,
}

impl<'s> SpanChecker<'s> {
    fn check(&mut self, span: Span, what: &str) {
        assert!(span.start <= span.end && span.end <= self.src.len(), "{what}: bad span {span}");
        assert!(
            self.src.is_char_boundary(span.start) && self.src.is_char_boundary(span.end),
            "{what}: {span} not on char boundary"
        );
        if let Some(parent) = self.stack.last() {
            assert!(
                parent.start <= span.start && span.end <= parent.end,
                "{what}: {span} outside parent {parent} in {:?}",
                self.src
            );
        }
        self.count += 1;
    }
}

impl<'a> Visitor<'a> for SpanChecker<'_> {
    fn visit_pipeline(&mut self, p: &Pipeline<'a>) {
        self.check(p.span, "pipeline");
        self.stack.push(p.span);
        walk_pipeline(self, p);
        self.stack.pop();
    }
    fn visit_element(&mut self, e: &PipelineElement<'a>) {
        self.check(e.span, "element");
        self.stack.push(e.span);
        nu_winnow_parser::ast::walk_expr(self, &e.expr);
        self.stack.pop();
    }
    fn visit_expr(&mut self, e: &Expr<'a>) {
        self.check(e.span, "expr");
        self.stack.push(e.span);
        walk_expr(self, e);
        self.stack.pop();
    }
    fn visit_signature(&mut self, s: &Signature<'a>) {
        self.check(s.span, "signature");
        self.stack.push(s.span);
        walk_signature(self, s);
        self.stack.pop();
    }
    fn visit_param(&mut self, p: &Param<'a>) {
        self.check(p.span, "param");
        self.stack.push(p.span);
        nu_winnow_parser::ast::walk_param(self, p);
        self.stack.pop();
    }
    fn visit_pattern(&mut self, p: &Pattern<'a>) {
        self.check(p.span, "pattern");
        self.stack.push(p.span);
        walk_pattern(self, p);
        self.stack.pop();
    }
    fn visit_path_member(&mut self, m: &PathMember<'a>) {
        self.check(m.span, "member");
    }
    fn visit_comment(&mut self, c: &Comment) {
        let saved = std::mem::take(&mut self.stack);
        self.check(c.span, "comment");
        self.stack = saved;
    }
}

#[test]
fn spans_are_nested_and_on_char_boundaries() {
    let src = include_str!("corpus/kitchen_sink.nu");
    let ast = ok(src);
    let mut checker = SpanChecker { src, stack: vec![], count: 0 };
    checker.visit_block(&ast.block);
    assert!(checker.count > 200, "walked {} nodes", checker.count);
    // Literal spellings survive.
    struct Strings<'s>(&'s str);
    impl<'a> Visitor<'a> for Strings<'_> {
        fn visit_expr(&mut self, e: &Expr<'a>) {
            if let ExprKind::String(s) = &e.kind {
                let t = e.span.slice(self.0);
                match s.quote {
                    Quote::Single => assert!(t.starts_with('\'') && t.ends_with('\'')),
                    Quote::Double => assert!(t.starts_with('"') && t.ends_with('"')),
                    Quote::Backtick => assert!(t.starts_with('`')),
                    Quote::Raw(_) => assert!(t.starts_with("r#")),
                    Quote::Bare => assert_eq!(t, s.value),
                }
            }
            walk_expr(self, e);
        }
    }
    Strings(src).visit_block(&ast.block);
}

#[test]
fn unicode_source() {
    let ast = ok("let 🧼 = 'ünïcödé'; print $\"héllo (🧼)\" # cömment é");
    assert_eq!(ast.block.pipelines.len(), 2);
    assert_eq!(kind!(ast.block.pipelines[0].elements[0].expr, ExprKind::Let(b) => b).name.item, "🧼");
}
