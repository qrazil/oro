//! Unit tests for the Oro lexer, with an emphasis on the INDENT/DEDENT engine.

use super::{Lexer, LexError, TokenKind};
use TokenKind::*;

/// Lex `src`, asserting success, and return just the token kinds.
fn kinds(src: &str) -> Vec<TokenKind> {
    Lexer::new(src)
        .tokenize()
        .expect("expected successful lex")
        .into_iter()
        .map(|t| t.kind)
        .collect()
}

/// Lex `src`, asserting failure, and return the error.
fn err(src: &str) -> LexError {
    Lexer::new(src)
        .tokenize()
        .expect_err("expected a lex error")
}

fn ident(s: &str) -> TokenKind {
    Ident(s.to_string())
}

#[test]
fn empty_input_is_just_eof() {
    assert_eq!(kinds(""), vec![Eof]);
}

#[test]
fn only_blank_and_comment_lines_produce_no_structure() {
    assert_eq!(kinds("\n\n   \n# just a comment\n\n"), vec![Eof]);
}

#[test]
fn simple_assignment() {
    assert_eq!(
        kinds("x = 1\n"),
        vec![ident("x"), Eq, Int("1".into()), Newline, Eof]
    );
}

#[test]
fn no_trailing_newline_still_terminates_line() {
    // Missing final newline must still yield a Newline before Eof.
    assert_eq!(
        kinds("x = 1"),
        vec![ident("x"), Eq, Int("1".into()), Newline, Eof]
    );
}

#[test]
fn keywords_are_recognised() {
    assert_eq!(
        kinds("if True and not False:\n    pass\n"),
        vec![
            If, True, And, Not, False, Colon, Newline, Indent, Pass, Newline, Dedent, Eof
        ]
    );
}

#[test]
fn none_literal_and_identifier_named_like_prefix() {
    // `f` alone is an identifier, `None` is a literal.
    assert_eq!(
        kinds("f = None\n"),
        vec![ident("f"), Eq, None, Newline, Eof]
    );
}

#[test]
fn multichar_operators() {
    assert_eq!(
        kinds("a //= b\n"),
        // `//=` is not in the frozen operator set: `//` then `=`.
        vec![ident("a"), DoubleSlash, Eq, ident("b"), Newline, Eof]
    );
    assert_eq!(
        kinds("a ** b == c != d <= e >= f -> g += h\n"),
        vec![
            ident("a"),
            DoubleStar,
            ident("b"),
            EqEq,
            ident("c"),
            NotEq,
            ident("d"),
            LtEq,
            ident("e"),
            GtEq,
            ident("f"),
            Arrow,
            ident("g"),
            PlusEq,
            ident("h"),
            Newline,
            Eof
        ]
    );
}

#[test]
fn numbers_int_and_float() {
    assert_eq!(
        kinds("1 2.5 .5 1. 1e9 2.5e-3 6E+2\n"),
        vec![
            Int("1".into()),
            Float("2.5".into()),
            Float(".5".into()),
            Float("1.".into()),
            Float("1e9".into()),
            Float("2.5e-3".into()),
            Float("6E+2".into()),
            Newline,
            Eof
        ]
    );
}

#[test]
fn string_escapes_are_decoded() {
    assert_eq!(
        kinds(r#""a\tb\nc\"d""#),
        vec![Str("a\tb\nc\"d".into()), Newline, Eof]
    );
}

#[test]
fn fstring_is_a_single_raw_token() {
    assert_eq!(
        kinds("f\"hi {name}\\n\"\n"),
        // f-string keeps raw inner text, escapes preserved verbatim.
        vec![FString("hi {name}\\n".into()), Newline, Eof]
    );
}

#[test]
fn unterminated_string_errors() {
    assert_eq!(err("\"oops\n").message, "unterminated string literal");
    assert_eq!(err("\"oops").message, "unterminated string literal");
}

#[test]
fn nested_blocks() {
    let src = "if x:\n    a\n    if y:\n        b\n    c\nd\n";
    assert_eq!(
        kinds(src),
        vec![
            If,
            ident("x"),
            Colon,
            Newline,
            Indent,
            ident("a"),
            Newline,
            If,
            ident("y"),
            Colon,
            Newline,
            Indent,
            ident("b"),
            Newline,
            Dedent,
            ident("c"),
            Newline,
            Dedent,
            ident("d"),
            Newline,
            Eof
        ]
    );
}

#[test]
fn dedent_by_multiple_levels_at_once() {
    let src = "if a:\n    if b:\n        c\nd\n";
    assert_eq!(
        kinds(src),
        vec![
            If,
            ident("a"),
            Colon,
            Newline,
            Indent,
            If,
            ident("b"),
            Colon,
            Newline,
            Indent,
            ident("c"),
            Newline,
            Dedent,
            Dedent,
            ident("d"),
            Newline,
            Eof
        ]
    );
}

#[test]
fn blank_and_comment_lines_inside_block_are_ignored() {
    let src = "if a:\n    x\n\n    # a comment at block indent\n# comment at col 0\n    y\n";
    assert_eq!(
        kinds(src),
        vec![
            If,
            ident("a"),
            Colon,
            Newline,
            Indent,
            ident("x"),
            Newline,
            ident("y"),
            Newline,
            Dedent,
            Eof
        ]
    );
}

#[test]
fn trailing_comment_on_content_line() {
    assert_eq!(
        kinds("x = 1  # set x\n"),
        vec![ident("x"), Eq, Int("1".into()), Newline, Eof]
    );
}

#[test]
fn tab_in_leading_whitespace_is_hard_error() {
    let e = err("if x:\n\tpass\n");
    assert_eq!(
        e.message,
        "tabs are not permitted for indentation, use spaces"
    );
    assert_eq!(e.line, 2);
    assert_eq!(e.col, 1);
}

#[test]
fn tab_after_spaces_in_leading_whitespace_is_error() {
    let e = err("if x:\n  \tpass\n");
    assert_eq!(
        e.message,
        "tabs are not permitted for indentation, use spaces"
    );
    assert_eq!(e.line, 2);
    assert_eq!(e.col, 3);
}

#[test]
fn tab_between_tokens_is_allowed() {
    // A tab that is not leading whitespace is ordinary whitespace.
    assert_eq!(
        kinds("x\t=\t1\n"),
        vec![ident("x"), Eq, Int("1".into()), Newline, Eof]
    );
}

#[test]
fn inconsistent_dedent_is_error() {
    // Dedent lands between stack levels 0 and 8.
    let src = "if a:\n        x\n    y\n";
    let e = err(src);
    assert_eq!(e.message, "inconsistent indentation");
    assert_eq!(e.line, 3);
}

#[test]
fn implicit_line_joining_across_brackets() {
    let src = "x = [1,\n     2,\n\t3]\n";
    assert_eq!(
        kinds(src),
        vec![
            ident("x"),
            Eq,
            LBracket,
            Int("1".into()),
            Comma,
            Int("2".into()),
            Comma,
            Int("3".into()),
            RBracket,
            Newline,
            Eof
        ]
    );
}

#[test]
fn implicit_join_nested_brackets() {
    let src = "f(a,\n  {b: [c,\n      d]})\n";
    assert_eq!(
        kinds(src),
        vec![
            ident("f"),
            LParen,
            ident("a"),
            Comma,
            LBrace,
            ident("b"),
            Colon,
            LBracket,
            ident("c"),
            Comma,
            ident("d"),
            RBracket,
            RBrace,
            RParen,
            Newline,
            Eof
        ]
    );
}

#[test]
fn eof_with_unclosed_blocks_drains_dedents() {
    // Three open indentation levels, no trailing dedent lines.
    let src = "if a:\n    if b:\n        c\n";
    assert_eq!(
        kinds(src),
        vec![
            If,
            ident("a"),
            Colon,
            Newline,
            Indent,
            If,
            ident("b"),
            Colon,
            Newline,
            Indent,
            ident("c"),
            Newline,
            Dedent,
            Dedent,
            Eof
        ]
    );
}

#[test]
fn eof_without_trailing_newline_emits_newline_then_dedents() {
    // Same as above but the final line lacks a newline.
    let src = "if a:\n    c";
    assert_eq!(
        kinds(src),
        vec![
            If,
            ident("a"),
            Colon,
            Newline,
            Indent,
            ident("c"),
            Newline,
            Dedent,
            Eof
        ]
    );
}

#[test]
fn token_positions_are_tracked() {
    let toks = Lexer::new("ab = 12\n").tokenize().unwrap();
    // ab at 1:1, = at 1:4, 12 at 1:6
    assert_eq!((toks[0].line, toks[0].col), (1, 1));
    assert_eq!((toks[1].line, toks[1].col), (1, 4));
    assert_eq!((toks[2].line, toks[2].col), (1, 6));
}

#[test]
fn indent_token_position_points_past_whitespace() {
    let toks = Lexer::new("if x:\n    y\n").tokenize().unwrap();
    let indent = toks.iter().find(|t| t.kind == Indent).unwrap();
    assert_eq!((indent.line, indent.col), (2, 5));
}

#[test]
fn semicolons_and_multiple_statements() {
    assert_eq!(
        kinds("a; b\n"),
        vec![ident("a"), Semicolon, ident("b"), Newline, Eof]
    );
}

#[test]
fn crlf_line_endings_behave_like_lf() {
    assert_eq!(
        kinds("if x:\r\n    y\r\n"),
        vec![
            If,
            ident("x"),
            Colon,
            Newline,
            Indent,
            ident("y"),
            Newline,
            Dedent,
            Eof
        ]
    );
}

#[test]
fn unexpected_character_is_error() {
    let e = err("a $ b\n");
    assert_eq!(e.message, "unexpected character '$'");
}

#[test]
fn at_sign_tokenizes() {
    // `@` is no longer a lex error: it becomes an `At` token so the parser can
    // emit a designed "decorators are not supported" message.
    assert_eq!(kinds("@f\n"), vec![At, ident("f"), Newline, Eof]);
}
