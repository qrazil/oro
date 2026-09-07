//! Token definitions for the Oro lexer.

/// The kind of a lexical token.
///
/// Numeric literals carry their raw source text rather than a parsed value:
/// integer literals may exceed `i64` (Oro promotes to bignum on overflow), so
/// the decision of how to store the value belongs to a later stage, not the
/// lexer.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // --- Literals ---
    /// Integer literal, raw text (e.g. `"42"`).
    Int(String),
    /// Floating-point literal, raw text (e.g. `"3.14"`, `"1e9"`).
    Float(String),
    /// String literal with escape sequences already decoded.
    Str(String),
    /// f-string literal. A single token for now: the raw inner text is kept
    /// verbatim (no escape processing, no interpolation parsing).
    FString(String),
    True,
    False,
    None,

    // --- Identifiers ---
    Ident(String),

    // --- Keywords ---
    If,
    Elif,
    Else,
    For,
    While,
    In,
    Def,
    Class,
    Return,
    Break,
    Continue,
    Try,
    Except,
    Finally,
    Raise,
    Import,
    As,
    Yield,
    And,
    Or,
    Not,
    Is,
    Pass,

    // --- Operators & punctuation ---
    Plus,        // +
    Minus,       // -
    Star,        // *
    Slash,       // /
    DoubleSlash, // //
    Percent,     // %
    DoubleStar,  // **
    Eq,          // =
    PlusEq,      // +=
    MinusEq,     // -=
    StarEq,      // *=
    SlashEq,     // /=
    EqEq,        // ==
    NotEq,       // !=
    Lt,          // <
    Gt,          // >
    LtEq,        // <=
    GtEq,        // >=
    LParen,      // (
    RParen,      // )
    LBracket,    // [
    RBracket,    // ]
    LBrace,      // {
    RBrace,      // }
    Pipe,        // |
    Comma,       // ,
    Dot,         // .
    Colon,       // :
    Semicolon,   // ;
    Arrow,       // ->
    At,          // @

    // --- Structural ---
    Newline,
    Indent,
    Dedent,
    Eof,
}

/// A token together with its 1-based source position.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number (counted in Unicode scalar values).
    pub col: usize,
}

impl Token {
    pub fn new(kind: TokenKind, line: usize, col: usize) -> Self {
        Token { kind, line, col }
    }
}
