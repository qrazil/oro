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
    /// A string literal, plus whether it was written as `r"..."`. The flag
    /// carries no semantics — a raw string has already been decoded — but the
    /// formatter needs it to reprint `r"\d+"` instead of `"\\d+"`.
    Str(String, bool),
    /// Bytes literal (`b"..."`), escapes already decoded to octets, plus
    /// whether it was written as `rb"..."`. The flag carries no semantics; the
    /// formatter needs it to reprint `rb"\d+"` instead of `b"\\d+"`.
    Bytes(Vec<u8>, bool),
    /// f-string literal. A single token for now: the raw inner text is kept
    /// verbatim (no escape processing, no interpolation parsing).
    FString(String),
    /// `true`, `false` and `null`. The variants keep Python's names because
    /// `None` is also `Option::None` here; only the source spelling moved.
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
    /// Reserved, and never part of an expression: `is` was cut, and the parser
    /// rejects this token with a message naming `==`.
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
    FatArrow,    // => (lambda)
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

/// A `#`-to-end-of-line comment, captured on the side (never inserted into the
/// main [`Token`] stream, so the parser is unaffected). Consumed by `src/fmt.rs`
/// to reproduce comments in formatted output.
#[derive(Debug, Clone, PartialEq)]
pub struct Comment {
    /// 1-based line number of the leading `#`.
    pub line: usize,
    /// 1-based column of the leading `#`.
    pub col: usize,
    /// The raw comment text, `#` through end of line, unmodified.
    pub text: String,
    /// True if this comment sits inside an open `(`/`[`/`{` — i.e. inside a
    /// multi-line bracketed expression, where a formatter cannot safely
    /// reattach it to a specific sub-expression.
    pub in_brackets: bool,
    /// True if a content token already appeared earlier on this same source
    /// line (a trailing/inline comment), as opposed to a comment alone on its
    /// own line.
    pub inline: bool,
}
