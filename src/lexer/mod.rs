//! Hand-written lexer for Oro.
//!
//! The lexer turns UTF-8 source text into a flat stream of [`Token`]s,
//! including the synthetic `Indent`/`Dedent`/`Newline` tokens that encode
//! Oro's significant, brace-free block structure.
//!
//! Design notes that are deliberate, not accidental:
//!
//! * **Tabs are rejected outright in leading whitespace.** This designs out the
//!   entire class of tab/space indentation-ambiguity bugs. A tab elsewhere on a
//!   line (between tokens) is treated as ordinary whitespace.
//! * **Implicit line joining inside brackets.** While any of `(`, `[`, `{` is
//!   open, newlines and indentation are suppressed entirely.
//! * Numeric literals are captured as raw text; see [`TokenKind`].

mod token;

pub use token::{Comment, Token, TokenKind};

use std::fmt;

/// An error produced while lexing, with a 1-based source position.
#[derive(Debug, Clone, PartialEq)]
pub struct LexError {
    pub message: String,
    pub line: usize,
    pub col: usize,
}

impl LexError {
    fn new(message: impl Into<String>, line: usize, col: usize) -> Self {
        LexError {
            message: message.into(),
            line,
            col,
        }
    }
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.col, self.message)
    }
}

impl std::error::Error for LexError {}

/// The lexer. Construct with [`Lexer::new`] and consume with
/// [`Lexer::tokenize`].
pub struct Lexer {
    /// Source as a vector of chars. Carriage returns are stripped up front so
    /// CRLF and LF line endings behave identically.
    chars: Vec<char>,
    pos: usize,
    /// 1-based line of the next char to read.
    line: usize,
    /// 1-based column of the next char to read.
    col: usize,
    /// Stack of indentation widths; always starts with `0`.
    indent_stack: Vec<usize>,
    /// Depth of currently open `(`/`[`/`{` brackets.
    bracket_depth: usize,
    /// True when positioned at the logical start of a line, i.e. indentation
    /// still needs to be measured.
    line_start: bool,
    /// Whether any content token has been emitted since the last `Newline`.
    line_has_tokens: bool,
    tokens: Vec<Token>,
    /// Comments captured on the side; see [`Comment`]. Never affects the token
    /// stream the parser sees.
    comments: Vec<Comment>,
}

impl Lexer {
    pub fn new(source: &str) -> Self {
        Lexer {
            chars: source.chars().filter(|&c| c != '\r').collect(),
            pos: 0,
            line: 1,
            col: 1,
            indent_stack: vec![0],
            bracket_depth: 0,
            line_start: true,
            line_has_tokens: false,
            tokens: Vec::new(),
            comments: Vec::new(),
        }
    }

    /// Tokenize the entire input, returning the token stream (always terminated
    /// by a single `Eof`) or the first [`LexError`]. Comments are discarded;
    /// use [`Lexer::tokenize_with_comments`] to keep them.
    pub fn tokenize(self) -> Result<Vec<Token>, LexError> {
        self.tokenize_with_comments().map(|(tokens, _)| tokens)
    }

    /// Tokenize the entire input, returning the token stream (always terminated
    /// by a single `Eof`) together with every `#`-comment encountered, or the
    /// first [`LexError`]. The token stream is identical to [`Lexer::tokenize`]
    /// — comments are never inserted into it.
    pub fn tokenize_with_comments(mut self) -> Result<(Vec<Token>, Vec<Comment>), LexError> {
        self.run()?;
        Ok((self.tokens, self.comments))
    }

    fn run(&mut self) -> Result<(), LexError> {
        loop {
            if self.bracket_depth == 0 && self.line_start {
                self.handle_indentation()?;
            }

            match self.peek() {
                None => {
                    self.finish_eof();
                    break;
                }
                Some('\n') => {
                    if self.bracket_depth == 0 {
                        if self.line_has_tokens {
                            self.push(TokenKind::Newline);
                            self.line_has_tokens = false;
                        }
                        self.advance();
                        self.line_start = true;
                    } else {
                        // Implicit line joining: swallow the newline.
                        self.advance();
                    }
                }
                Some(' ') | Some('\t') => {
                    // Inter-token whitespace (tabs are only illegal in leading
                    // whitespace, which is handled in `handle_indentation`).
                    self.advance();
                }
                Some('#') => self.skip_comment(),
                Some(_) => self.scan_token()?,
            }
        }
        Ok(())
    }

    // --- Indentation handling ------------------------------------------------

    /// Called at the logical start of a line (outside brackets). Skips blank and
    /// comment-only lines without emitting anything, then measures the
    /// indentation of the next content line and emits the appropriate
    /// `Indent`/`Dedent` tokens.
    fn handle_indentation(&mut self) -> Result<(), LexError> {
        loop {
            let mut width = 0usize;
            loop {
                match self.peek() {
                    Some(' ') => {
                        width += 1;
                        self.advance();
                    }
                    Some('\t') => {
                        return Err(self.error(
                            "tabs are not permitted for indentation, use spaces",
                        ));
                    }
                    _ => break,
                }
            }

            match self.peek() {
                // Indentation of a line that never materialised: leave EOF to
                // the main loop.
                None => return Ok(()),
                // Blank line: no tokens, keep scanning.
                Some('\n') => {
                    self.advance();
                }
                // Comment-only line: no tokens, keep scanning.
                Some('#') => {
                    self.skip_comment();
                    if self.peek() == Some('\n') {
                        self.advance();
                    }
                }
                // A real content line: reconcile against the indent stack.
                Some(_) => {
                    let top = *self.indent_stack.last().unwrap();
                    if width > top {
                        self.indent_stack.push(width);
                        self.push(TokenKind::Indent);
                    } else if width < top {
                        while width < *self.indent_stack.last().unwrap() {
                            self.indent_stack.pop();
                            self.push(TokenKind::Dedent);
                        }
                        if width != *self.indent_stack.last().unwrap() {
                            return Err(self.error("inconsistent indentation"));
                        }
                    }
                    self.line_start = false;
                    return Ok(());
                }
            }
        }
    }

    /// At end of input: terminate any dangling logical line, drain the indent
    /// stack, and emit `Eof`.
    fn finish_eof(&mut self) {
        if self.line_has_tokens {
            self.push(TokenKind::Newline);
            self.line_has_tokens = false;
        }
        while *self.indent_stack.last().unwrap() > 0 {
            self.indent_stack.pop();
            self.push(TokenKind::Dedent);
        }
        self.push(TokenKind::Eof);
    }

    // --- Token scanning ------------------------------------------------------

    /// Scan a single content token. `peek()` is guaranteed to be a non-newline,
    /// non-whitespace, non-`#` character.
    fn scan_token(&mut self) -> Result<(), LexError> {
        let c = self.peek().unwrap();

        // A digit, or a leading `.` immediately followed by a digit (e.g. `.5`).
        if c.is_ascii_digit()
            || (c == '.' && matches!(self.peek2(), Some(d) if d.is_ascii_digit()))
        {
            self.scan_number();
            Ok(())
        } else if (c == 'f' || c == 'F') && matches!(self.peek2(), Some('\'') | Some('"')) {
            self.advance(); // consume the `f`/`F` prefix
            self.scan_string(true, false)
        } else if (c == 'r' || c == 'R') && matches!(self.peek2(), Some('\'') | Some('"')) {
            // A raw string: backslashes are literal (no escape processing) — the
            // natural way to write regex patterns.
            self.advance(); // consume the `r`/`R` prefix
            self.scan_string(false, true)
        } else if c == '\'' || c == '"' {
            self.scan_string(false, false)
        } else if c == '_' || c.is_ascii_alphabetic() {
            self.scan_ident();
            Ok(())
        } else {
            self.scan_operator()
        }
    }

    fn scan_number(&mut self) {
        let (sl, sc) = (self.line, self.col);
        let mut s = String::new();
        let mut is_float = false;

        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            s.push(self.advance().unwrap());
        }

        // A `.` only starts a fractional part when a digit follows it. Without
        // that lookahead `42.to_str()` lexes as the float `42.` followed by a
        // stray name — which matters a great deal now that conversion is spelled
        // as a method on the value.
        if self.peek() == Some('.') && matches!(self.peek2(), Some(c) if c.is_ascii_digit()) {
            is_float = true;
            s.push(self.advance().unwrap());
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                s.push(self.advance().unwrap());
            }
        }

        // Optional exponent, only consumed if it is well-formed.
        if matches!(self.peek(), Some('e') | Some('E')) {
            let after = self.peek2();
            let valid = matches!(after, Some(c) if c.is_ascii_digit())
                || (matches!(after, Some('+') | Some('-'))
                    && matches!(self.peek_at(2), Some(c) if c.is_ascii_digit()));
            if valid {
                is_float = true;
                s.push(self.advance().unwrap()); // e / E
                if matches!(self.peek(), Some('+') | Some('-')) {
                    s.push(self.advance().unwrap());
                }
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    s.push(self.advance().unwrap());
                }
            }
        }

        let kind = if is_float {
            TokenKind::Float(s)
        } else {
            TokenKind::Int(s)
        };
        self.push_at(kind, sl, sc);
        self.line_has_tokens = true;
    }

    fn scan_string(&mut self, is_f: bool, is_raw: bool) -> Result<(), LexError> {
        let (sl, sc) = (self.line, self.col);
        let quote = self.advance().unwrap(); // opening ' or "
        let mut value = String::new();

        loop {
            match self.peek() {
                None | Some('\n') => {
                    return Err(LexError::new("unterminated string literal", sl, sc));
                }
                Some(c) if c == quote => {
                    self.advance();
                    break;
                }
                Some('\\') => {
                    self.advance();
                    match self.peek() {
                        None => {
                            return Err(LexError::new("unterminated string literal", sl, sc));
                        }
                        Some(e) => {
                            self.advance();
                            // f-strings and raw strings keep the backslash
                            // verbatim (f-strings decode later in codegen; raw
                            // strings never decode). A backslash still escapes a
                            // closing quote for termination, but is preserved.
                            if is_f || is_raw {
                                value.push('\\');
                                value.push(e);
                            } else if let Some(c) = simple_escape(e) {
                                value.push(c);
                            } else if let Some(width) = hex_escape_width(e) {
                                let mut digits = String::new();
                                for _ in 0..width {
                                    match self.peek() {
                                        Some(d) if d.is_ascii_hexdigit() => {
                                            digits.push(self.advance().unwrap())
                                        }
                                        _ => break,
                                    }
                                }
                                match decode_hex_escape(e, &digits) {
                                    Ok(c) => value.push(c),
                                    Err(msg) => return Err(LexError::new(msg, sl, sc)),
                                }
                            } else {
                                return Err(LexError::new(unknown_escape_message(e), sl, sc));
                            }
                        }
                    }
                }
                Some(c) => {
                    self.advance();
                    value.push(c);
                }
            }
        }

        let kind = if is_f {
            TokenKind::FString(value)
        } else {
            TokenKind::Str(value, is_raw)
        };
        self.push_at(kind, sl, sc);
        self.line_has_tokens = true;
        Ok(())
    }

    fn scan_ident(&mut self) {
        let (sl, sc) = (self.line, self.col);
        let mut s = String::new();
        while matches!(self.peek(), Some(c) if c == '_' || c.is_ascii_alphanumeric()) {
            s.push(self.advance().unwrap());
        }
        let kind = keyword_kind(&s).unwrap_or(TokenKind::Ident(s));
        self.push_at(kind, sl, sc);
        self.line_has_tokens = true;
    }

    fn scan_operator(&mut self) -> Result<(), LexError> {
        let (sl, sc) = (self.line, self.col);
        let c = self.advance().unwrap();
        let kind = match c {
            '+' => {
                if self.eat('=') {
                    TokenKind::PlusEq
                } else {
                    TokenKind::Plus
                }
            }
            '-' => {
                if self.eat('=') {
                    TokenKind::MinusEq
                } else if self.eat('>') {
                    TokenKind::Arrow
                } else {
                    TokenKind::Minus
                }
            }
            '*' => {
                if self.eat('*') {
                    TokenKind::DoubleStar
                } else if self.eat('=') {
                    TokenKind::StarEq
                } else {
                    TokenKind::Star
                }
            }
            '/' => {
                if self.eat('/') {
                    TokenKind::DoubleSlash
                } else if self.eat('=') {
                    TokenKind::SlashEq
                } else {
                    TokenKind::Slash
                }
            }
            '%' => TokenKind::Percent,
            '=' => {
                if self.eat('=') {
                    TokenKind::EqEq
                } else if self.eat('>') {
                    TokenKind::FatArrow
                } else {
                    TokenKind::Eq
                }
            }
            '!' => {
                if self.eat('=') {
                    TokenKind::NotEq
                } else {
                    return Err(LexError::new("unexpected character '!'", sl, sc));
                }
            }
            '<' => {
                if self.eat('=') {
                    TokenKind::LtEq
                } else {
                    TokenKind::Lt
                }
            }
            '>' => {
                if self.eat('=') {
                    TokenKind::GtEq
                } else {
                    TokenKind::Gt
                }
            }
            '(' => {
                self.bracket_depth += 1;
                TokenKind::LParen
            }
            ')' => {
                self.bracket_depth = self.bracket_depth.saturating_sub(1);
                TokenKind::RParen
            }
            '[' => {
                self.bracket_depth += 1;
                TokenKind::LBracket
            }
            ']' => {
                self.bracket_depth = self.bracket_depth.saturating_sub(1);
                TokenKind::RBracket
            }
            '{' => {
                self.bracket_depth += 1;
                TokenKind::LBrace
            }
            '}' => {
                self.bracket_depth = self.bracket_depth.saturating_sub(1);
                TokenKind::RBrace
            }
            // `|` is tokenized (not rejected here) so the parser can emit a
            // designed diagnostic — chiefly the "or-patterns are not supported"
            // message inside a `case`. It is not a binary operator in Oro.
            '|' => TokenKind::Pipe,
            ',' => TokenKind::Comma,
            '.' => TokenKind::Dot,
            ':' => TokenKind::Colon,
            ';' => TokenKind::Semicolon,
            // `@` is tokenized (not rejected here) so the parser can emit a
            // designed "decorators are not supported" diagnostic.
            '@' => TokenKind::At,
            other => {
                return Err(LexError::new(
                    format!("unexpected character '{other}'"),
                    sl,
                    sc,
                ));
            }
        };
        self.push_at(kind, sl, sc);
        self.line_has_tokens = true;
        Ok(())
    }

    fn skip_comment(&mut self) {
        let (sl, sc) = (self.line, self.col);
        let in_brackets = self.bracket_depth > 0;
        let inline = self.line_has_tokens;
        let mut text = String::new();
        while let Some(c) = self.peek() {
            if c == '\n' {
                break;
            }
            text.push(c);
            self.advance();
        }
        self.comments.push(Comment { line: sl, col: sc, text, in_brackets, inline });
    }

    // --- Low-level cursor helpers -------------------------------------------

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied();
        match c {
            Some('\n') => {
                self.pos += 1;
                self.line += 1;
                self.col = 1;
            }
            Some(_) => {
                self.pos += 1;
                self.col += 1;
            }
            None => {}
        }
        c
    }

    fn eat(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn error(&self, message: impl Into<String>) -> LexError {
        LexError::new(message, self.line, self.col)
    }

    fn push(&mut self, kind: TokenKind) {
        self.tokens.push(Token::new(kind, self.line, self.col));
    }

    fn push_at(&mut self, kind: TokenKind, line: usize, col: usize) {
        self.tokens.push(Token::new(kind, line, col));
    }
}

/// Map an identifier string to its keyword token, or `None` if it is an
/// ordinary identifier.
fn keyword_kind(s: &str) -> Option<TokenKind> {
    use TokenKind::*;
    Some(match s {
        "if" => If,
        "elif" => Elif,
        "else" => Else,
        "for" => For,
        "while" => While,
        "in" => In,
        "def" => Def,
        "class" => Class,
        "return" => Return,
        "break" => Break,
        "continue" => Continue,
        "try" => Try,
        "except" => Except,
        "finally" => Finally,
        "raise" => Raise,
        "import" => Import,
        "as" => As,
        "yield" => Yield,
        "and" => And,
        "or" => Or,
        "not" => Not,
        "is" => Is,
        "pass" => Pass,
        "True" => True,
        "False" => False,
        "None" => None,
        _ => return Option::None,
    })
}

#[cfg(test)]
mod tests;

/// The single-character escapes. Shared with the f-string decoder in codegen so
/// the two can never disagree about what `"\t"` means.
pub fn simple_escape(e: char) -> Option<char> {
    Some(match e {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        '\\' => '\\',
        '\'' => '\'',
        '"' => '"',
        '0' => '\0',
        'a' => '\u{7}',
        'b' => '\u{8}',
        'f' => '\u{c}',
        'v' => '\u{b}',
        _ => return None,
    })
}

/// How many hex digits `\x`, `\u` and `\U` take.
pub fn hex_escape_width(e: char) -> Option<usize> {
    Some(match e {
        'x' => 2,
        'u' => 4,
        'U' => 8,
        _ => return None,
    })
}

/// Decode `\xNN` / `\uNNNN` / `\UNNNNNNNN` given exactly its digits.
pub fn decode_hex_escape(kind: char, digits: &str) -> Result<char, String> {
    let width = hex_escape_width(kind).expect("not a hex escape");
    if digits.len() != width || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "`\\{kind}` needs exactly {width} hex digits, found `{digits}`"
        ));
    }
    let n = u32::from_str_radix(digits, 16).map_err(|e| e.to_string())?;
    char::from_u32(n).ok_or_else(|| {
        format!("`\\{kind}{digits}` is not a valid character (out of range, or a surrogate)")
    })
}

/// The message for an escape Oro does not recognise. Unknown escapes used to
/// decode to backslash-plus-letter, which meant `"a\bb"` was four characters in
/// Oro and three in Python — the same syntax quietly meaning different things.
/// Rejecting is the only option that does not silently corrupt data.
pub fn unknown_escape_message(e: char) -> String {
    format!(
        "unknown escape `\\{e}` — Oro's escapes are \\n \\t \\r \\a \\b \\f \\v \\0 \\\\ \\' \\\" \\xNN \\uNNNN \\UNNNNNNNN; \
         write `\\\\{e}` for a literal backslash, or use a raw string r\"...\""
    )
}
