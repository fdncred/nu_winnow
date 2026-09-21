//! The formatter example must be idempotent and must not change the meaning
//! of a program: formatting then re-parsing yields a structurally identical
//! tree (spans aside) for every file in the corpus, with no comment lost.

#[path = "../examples/nufmt/format.rs"]
#[allow(dead_code)]
mod format;

use format::{Note, Options, format, format_with_notes};
use nu_winnow_parser::{parse, pretty};

/// The pretty-printed tree with all spans removed.
fn structure(src: &str) -> String {
    let ast = parse(src).unwrap_or_else(|e| panic!("{}", e.render(src, None)));
    let dump = pretty::dump(&ast);
    let mut out = String::new();
    for line in dump.lines() {
        if line.starts_with("Comments") {
            break;
        }
        let words: Vec<&str> = line
            .split(' ')
            .filter(|w| !(w.contains("..") && w.starts_with(|c: char| c.is_ascii_digit())))
            .filter(|w| !w.starts_with("comments=") && !w.starts_with("terminator="))
            .collect();
        out.push_str(&words.join(" "));
        out.push('\n');
    }
    out
}

fn check(name: &str, src: &str) {
    let options = Options::default();
    let once = format(src, &options).unwrap_or_else(|e| panic!("{name}: {}", e.render(src, Some(name))));
    let twice = format(&once, &options)
        .unwrap_or_else(|e| panic!("{name} (second pass): {}\n---\n{once}", e.render(&once, Some(name))));
    assert_eq!(once, twice, "{name}: formatting is not idempotent");
    assert_eq!(
        structure(src),
        structure(&once),
        "{name}: formatting changed the program structure\n--- formatted:\n{once}"
    );
    let before = parse(src).unwrap().comments.len();
    let after = parse(&once).unwrap().comments.len();
    assert_eq!(before, after, "{name}: comments were lost\n--- formatted:\n{once}");
}

#[test]
fn corpus_round_trips() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut count = 0;
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "nu") {
            let src = std::fs::read_to_string(&path).unwrap();
            check(&path.display().to_string(), &src);
            count += 1;
        }
    }
    assert!(count >= 10);
}

#[test]
fn normalises_whitespace() {
    let options = Options::default();
    let fmt = |s: &str| format(s, &options).unwrap();
    assert_eq!(fmt("ls|where size > 1kb   |   get name\n"), "ls | where size > 1kb | get name\n");
    assert_eq!(fmt("let   x  =  1  +  2  *  3"), "let x = 1 + 2 * 3\n");
    assert_eq!(fmt("cmd e>| lines"), "cmd e>| lines\n");
    assert_eq!(fmt("cmd o>  out.txt   e> err.txt"), "cmd o> out.txt e> err.txt\n");
    assert_eq!(fmt("[1,2,   3]"), "[1, 2, 3]\n");
    assert_eq!(fmt("{a:1,b:[x y]}"), "{a: 1, b: [x y]}\n");
    assert_eq!(fmt("def f [x:int,--flag(-f)] {$x}"), "def f [x: int, --flag(-f)] { $x }\n");
    assert_eq!(fmt("if $x {1} else {2}"), "if $x { 1 } else { 2 }\n");
    assert_eq!(fmt("def f [] {\nls\n| length\n}"), "def f [] {\n    ls\n        | length\n}\n");
    assert_eq!(fmt("ls\n| each {|x|\n$x\n}"), "ls\n    | each {|x|\n        $x\n    }\n");
    assert_eq!(fmt("[\n1\n# c\n2\n]"), "[\n    1\n    # c\n    2\n]\n");
    assert_eq!(fmt("ls # trailing\n\n\n\npwd"), "ls  # trailing\n\npwd\n");
    assert_eq!(fmt("each {|x|\n  $x\n}"), "each {|x|\n    $x\n}\n");
    assert_eq!(fmt("$\"a (1 + 1)\" | print"), "$\"a (1 + 1)\" | print\n");
}

/// A bare word in a `where` condition written without spaces around a
/// comparison (`size>1kb`) is one column name to Nushell, which is never
/// what was meant; the formatter writes the comparison and says so.
#[test]
fn splits_compact_row_conditions() {
    let options = Options::default();
    let fmt = |s: &str| format_with_notes(s, &options).unwrap();
    let (out, notes) = fmt("ls|where size>1kb|get name");
    assert_eq!(out, "ls | where size > 1kb | get name\n");
    assert_eq!(notes, [Note { offset: 9, message: "`size>1kb` written as the comparison `size > 1kb`".into() }]);
    // The rewrite is idempotent: the spaced form parses as a comparison.
    assert_eq!(fmt(&out), (out.clone(), vec![]));

    assert_eq!(fmt("ls | where size>=1kb and name=~Cargo").0, "ls | where size >= 1kb and name =~ Cargo\n");
    assert_eq!(fmt("ls | where not type==dir").0, "ls | where not type == dir\n");
    assert_eq!(fmt("ls | where modified<2024-01-01").0, "ls | where modified < 2024-01-01\n");
    assert_eq!(fmt("ls | where size > 1kb and name!=Cargo").0, "ls | where size > 1kb and name != Cargo\n");
    assert_eq!(fmt("ls | where size>1kb | where name=~Cargo").1.len(), 2);

    // Not a comparison, or not a row condition: left alone, no note.
    for src in [
        "ls | where \"size>1kb\"",
        "ls | where size>>1kb",
        "ls | where size==",
        "ls | where a=1",
        "ls | where name",
        "ls | where name =~ Cargo",
        "ls | where name == a>b",
        "echo size>1kb",
        "ls | where {|row| $row.size > 1kb }",
    ] {
        let (out, notes) = fmt(src);
        assert_eq!(out.trim_end(), src, "{src}");
        assert!(notes.is_empty(), "{src}");
    }
}
