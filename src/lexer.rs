use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Copy)]
pub enum TokenType {
    Let,
    Mut,
    Const,
    Fn,
    Struct,
    Enum,
    Interface,
    Impl,
    Extend,
    Newtype,
    Namespace,
    Match,
    If,
    Else,
    For,
    While,
    Return,
    In,
    Where,
    Conc,
    Select,
    Recv,
    Break,
    Continue,
    Unsafe,
    Use,
    True,
    False,
    As,

    Plus,
    Minus,
    Star,
    Slash,
    Modulo,
    Equal,
    NotEqual,
    LessThan,
    GreaterThan,
    LessThanOrEqual,
    GreaterThanOrEqual,
    LogicalAnd,
    LogicalOr,
    LogicalNot,
    BitwiseAnd,
    BitwiseOr,
    BitwiseXor,
    ShiftLeft,
    ShiftRight,
    Assign,
    PlusAssign,
    MinusAssign,
    StarAssign,
    SlashAssign,
    Pipe,
    Try,
    Spread,
    ReturnType,
    MatchArm,
    PathSep,
    Borrow,

    IntLit,
    FloatLit,
    BoolLit,
    StrLit,
    Identifier,

    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Colon,
    Semicolon,
    At,

    Eof,
    Error,
}

use crate::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub token_type: TokenType,
    pub lexeme: String,
    pub span: Span,
}

pub struct Lexer {
    input: Vec<char>,
    current_offset: usize,
    start_offset: usize,
    line: usize,
    col: usize,
    keywords: HashMap<String, TokenType>,
}

impl Lexer {
    pub fn new(input: String) -> Self {
        let mut keywords = HashMap::new();
        let kw_list = [
            ("let", TokenType::Let),
            ("mut", TokenType::Mut),
            ("const", TokenType::Const),
            ("fn", TokenType::Fn),
            ("struct", TokenType::Struct),
            ("enum", TokenType::Enum),
            ("interface", TokenType::Interface),
            ("impl", TokenType::Impl),
            ("extend", TokenType::Extend),
            ("newtype", TokenType::Newtype),
            ("namespace", TokenType::Namespace),
            ("match", TokenType::Match),
            ("if", TokenType::If),
            ("else", TokenType::Else),
            ("for", TokenType::For),
            ("while", TokenType::While),
            ("return", TokenType::Return),
            ("in", TokenType::In),
            ("where", TokenType::Where),
            ("conc", TokenType::Conc),
            ("select", TokenType::Select),
            ("recv", TokenType::Recv),
            ("break", TokenType::Break),
            ("continue", TokenType::Continue),
            ("unsafe", TokenType::Unsafe),
            ("use", TokenType::Use),
            ("true", TokenType::True),
            ("false", TokenType::False),
            ("as", TokenType::As),
        ];
        for (name, ty) in kw_list {
            keywords.insert(name.to_string(), ty);
        }

        Lexer {
            input: input.chars().collect(),
            current_offset: 0,
            start_offset: 0,
            line: 1,
            col: 1,
            keywords,
        }
    }

    pub fn tokenize(&mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        while !self.is_at_end() {
            self.skip_whitespace();
            if self.is_at_end() {
                break;
            }
            tokens.push(self.next_token());
        }
        // Create EOF token with the span of the last character or a default span if empty
        let eof_span = if tokens.is_empty() {
            Span::new(
                self.current_offset,
                self.current_offset,
                self.line,
                self.col,
            )
        } else {
            let last_token = tokens.last().unwrap();
            Span::new(
                last_token.span.end,
                last_token.span.end,
                self.line,
                self.col,
            )
        };
        tokens.push(Token {
            token_type: TokenType::Eof,
            lexeme: "".to_string(),
            span: eof_span,
        });
        tokens
    }

    fn next_token(&mut self) -> Token {
        self.start_offset = self.current_offset;
        let c = self.advance();

        let ty = match c {
            '(' => TokenType::LParen,
            ')' => TokenType::RParen,
            '{' => TokenType::LBrace,
            '}' => TokenType::RBrace,
            '[' => TokenType::LBracket,
            ']' => TokenType::RBracket,
            ',' => TokenType::Comma,
            ';' => TokenType::Semicolon,
            '.' => {
                if self.match_char('.') {
                    if self.match_char('.') {
                        TokenType::Spread
                    } else {
                        TokenType::Error
                    }
                } else {
                    TokenType::Dot
                }
            }
            '+' => {
                if self.match_char('=') {
                    TokenType::PlusAssign
                } else {
                    TokenType::Plus
                }
            }
            '-' => {
                if self.match_char('>') {
                    TokenType::ReturnType
                } else if self.match_char('=') {
                    TokenType::MinusAssign
                } else {
                    TokenType::Minus
                }
            }
            '*' => {
                if self.match_char('=') {
                    TokenType::StarAssign
                } else {
                    TokenType::Star
                }
            }
            '/' => {
                if self.match_char('=') {
                    TokenType::SlashAssign
                } else {
                    TokenType::Slash
                }
            }
            '%' => TokenType::Modulo,
            '=' => {
                if self.match_char('=') {
                    TokenType::Equal
                } else if self.match_char('>') {
                    TokenType::MatchArm
                } else {
                    TokenType::Assign
                }
            }
            '!' => {
                if self.match_char('=') {
                    TokenType::NotEqual
                } else {
                    TokenType::LogicalNot
                }
            }
            '<' => {
                if self.match_char('=') {
                    TokenType::LessThanOrEqual
                } else if self.match_char('<') {
                    TokenType::ShiftLeft
                } else {
                    TokenType::LessThan
                }
            }
            '>' => {
                if self.match_char('=') {
                    TokenType::GreaterThanOrEqual
                } else if self.match_char('>') {
                    TokenType::ShiftRight
                } else {
                    TokenType::GreaterThan
                }
            }
            '&' => {
                if self.match_char('&') {
                    TokenType::LogicalAnd
                } else {
                    TokenType::BitwiseAnd
                }
            }
            '|' => {
                if self.match_char('|') {
                    TokenType::LogicalOr
                } else if self.match_char('>') {
                    TokenType::Pipe
                } else {
                    TokenType::BitwiseOr
                }
            }
            '^' => TokenType::BitwiseXor,
            '?' => TokenType::Try,
            ':' => {
                if self.match_char(':') {
                    TokenType::PathSep
                } else {
                    TokenType::Colon
                }
            }
            '@' => TokenType::At,
            '"' => return self.string_lit(),
            _ if c.is_ascii_digit() => return self.number_lit(c),
            _ if c.is_alphabetic() || c == '_' => return self.identifier(c),
            _ => TokenType::Error,
        };
        Token {
            token_type: ty,
            lexeme: self.input[self.start_offset..self.current_offset]
                .iter()
                .collect(),
            span: Span::new(self.start_offset, self.current_offset, self.line, self.col),
        }
    }

    fn string_lit(&mut self) -> Token {
        let mut val = String::new();
        while !self.is_at_end() && self.peek() != '"' {
            if self.peek() == '\n' {
                self.line += 1;
                self.col = 1;
            }
            val.push(self.advance());
        }
        if self.is_at_end() {
            return Token {
                token_type: TokenType::Error,
                lexeme: val,
                span: Span::new(self.start_offset, self.current_offset, self.line, self.col),
            };
        }
        self.advance(); // consume closing "
        Token {
            token_type: TokenType::StrLit,
            lexeme: val,
            span: Span::new(self.start_offset, self.current_offset, self.line, self.col),
        }
    }

    fn number_lit(&mut self, first: char) -> Token {
        let mut lexeme = first.to_string();
        let mut is_float = false;
        while !self.is_at_end() && (self.peek().is_ascii_digit() || self.peek() == '_') {
            lexeme.push(self.advance());
        }
        if self.peek() == '.' && self.peek_next().is_ascii_digit() {
            is_float = true;
            lexeme.push(self.advance());
            while self.peek().is_ascii_digit() || self.peek() == '_' {
                lexeme.push(self.advance());
            }
        }
        // Handle suffixes
        if !self.is_at_end() && (self.peek() == 'i' || self.peek() == 'u' || self.peek() == 'f') {
            lexeme.push(self.advance());
            while !self.is_at_end() && self.peek().is_ascii_alphanumeric() {
                lexeme.push(self.advance());
            }
        }
        Token {
            token_type: if is_float {
                TokenType::FloatLit
            } else {
                TokenType::IntLit
            },
            lexeme,
            span: Span::new(self.start_offset, self.current_offset, self.line, self.col),
        }
    }

    fn identifier(&mut self, first: char) -> Token {
        let mut lexeme = first.to_string();
        while !self.is_at_end() && (self.peek().is_ascii_alphanumeric() || self.peek() == '_') {
            lexeme.push(self.advance());
        }
        let ty = *self.keywords.get(&lexeme).unwrap_or(&TokenType::Identifier);
        Token {
            token_type: ty,
            lexeme,
            span: Span::new(self.start_offset, self.current_offset, self.line, self.col),
        }
    }

    fn skip_whitespace(&mut self) {
        while !self.is_at_end() {
            match self.peek() {
                ' ' | '\r' | '\t' => {
                    self.advance();
                }
                '\n' => {
                    self.advance();
                }
                '/' if self.peek_next() == '/' => {
                    while !self.is_at_end() && self.peek() != '\n' {
                        self.advance();
                    }
                }
                '/' if self.peek_next() == '*' => {
                    self.advance();
                    self.advance();
                    while !self.is_at_end() && !(self.peek() == '*' && self.peek_next() == '/') {
                        if self.advance() == '\n' {
                            self.line += 1;
                            self.col = 1;
                        }
                    }
                    if !self.is_at_end() {
                        self.advance();
                        self.advance();
                    }
                }
                _ => break,
            }
        }
    }

    fn is_at_end(&self) -> bool {
        self.current_offset >= self.input.len()
    }
    fn peek(&self) -> char {
        if self.is_at_end() {
            '\0'
        } else {
            self.input[self.current_offset]
        }
    }
    fn peek_next(&self) -> char {
        if self.current_offset + 1 >= self.input.len() {
            '\0'
        } else {
            self.input[self.current_offset + 1]
        }
    }
    fn advance(&mut self) -> char {
        let c = self.input[self.current_offset];
        self.current_offset += 1;
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        c
    }
    fn match_char(&mut self, expected: char) -> bool {
        if self.is_at_end() || self.input[self.current_offset] != expected {
            return false;
        }
        self.advance();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keywords() {
        let input = "let mut const fn struct enum interface impl extend newtype match if else for while return in where conc select recv unsafe use true false as";
        let mut lexer = Lexer::new(input.to_string());
        let tokens = lexer.tokenize();
        let expected = vec![
            TokenType::Let,
            TokenType::Mut,
            TokenType::Const,
            TokenType::Fn,
            TokenType::Struct,
            TokenType::Enum,
            TokenType::Interface,
            TokenType::Impl,
            TokenType::Extend,
            TokenType::Newtype,
            TokenType::Match,
            TokenType::If,
            TokenType::Else,
            TokenType::For,
            TokenType::While,
            TokenType::Return,
            TokenType::In,
            TokenType::Where,
            TokenType::Conc,
            TokenType::Select,
            TokenType::Recv,
            TokenType::Unsafe,
            TokenType::Use,
            TokenType::True,
            TokenType::False,
            TokenType::As,
            TokenType::Eof,
        ];
        for (i, ty) in expected.iter().enumerate() {
            assert_eq!(tokens[i].token_type, *ty, "Mismatch at index {}", i);
        }
    }

    #[test]
    fn test_operators() {
        let input =
            "+ - * / % == != < > <= >= && || ! & | ^ << >> = += -= *= /= |> ? ... -> => :: @";
        let mut lexer = Lexer::new(input.to_string());
        let tokens = lexer.tokenize();
        let expected = vec![
            TokenType::Plus,
            TokenType::Minus,
            TokenType::Star,
            TokenType::Slash,
            TokenType::Modulo,
            TokenType::Equal,
            TokenType::NotEqual,
            TokenType::LessThan,
            TokenType::GreaterThan,
            TokenType::LessThanOrEqual,
            TokenType::GreaterThanOrEqual,
            TokenType::LogicalAnd,
            TokenType::LogicalOr,
            TokenType::LogicalNot,
            TokenType::BitwiseAnd,
            TokenType::BitwiseOr,
            TokenType::BitwiseXor,
            TokenType::ShiftLeft,
            TokenType::ShiftRight,
            TokenType::Assign,
            TokenType::PlusAssign,
            TokenType::MinusAssign,
            TokenType::StarAssign,
            TokenType::SlashAssign,
            TokenType::Pipe,
            TokenType::Try,
            TokenType::Spread,
            TokenType::ReturnType,
            TokenType::MatchArm,
            TokenType::PathSep,
            TokenType::At,
            TokenType::Eof,
        ];
        for (i, ty) in expected.iter().enumerate() {
            assert_eq!(tokens[i].token_type, *ty, "Mismatch at index {}", i);
        }
    }

    #[test]
    fn test_literals() {
        let input = "42 42i64 42u8 3.14 3.14f64 \"hello\" \"Hi {name}\"";
        let mut lexer = Lexer::new(input.to_string());
        let tokens = lexer.tokenize();

        assert_eq!(tokens[0].token_type, TokenType::IntLit);
        assert_eq!(tokens[0].lexeme, "42");

        assert_eq!(tokens[1].token_type, TokenType::IntLit);
        assert_eq!(tokens[1].lexeme, "42i64");

        assert_eq!(tokens[2].token_type, TokenType::IntLit);
        assert_eq!(tokens[2].lexeme, "42u8");

        assert_eq!(tokens[3].token_type, TokenType::FloatLit);
        assert_eq!(tokens[3].lexeme, "3.14");

        assert_eq!(tokens[4].token_type, TokenType::FloatLit);
        assert_eq!(tokens[4].lexeme, "3.14f64");

        assert_eq!(tokens[5].token_type, TokenType::StrLit);
        assert_eq!(tokens[5].lexeme, "hello");

        assert_eq!(tokens[6].token_type, TokenType::StrLit);
        assert_eq!(tokens[6].lexeme, "Hi {name}");
    }

    #[test]
    fn test_comments() {
        let input = "let x = 5; // line comment\n/* block\ncomment */ let y = 10;";
        let mut lexer = Lexer::new(input.to_string());
        let tokens = lexer.tokenize();

        assert_eq!(tokens[0].token_type, TokenType::Let);
        assert_eq!(tokens[1].token_type, TokenType::Identifier);
        assert_eq!(tokens[2].token_type, TokenType::Assign);
        assert_eq!(tokens[3].token_type, TokenType::IntLit);
        assert_eq!(tokens[4].token_type, TokenType::Semicolon);

        assert_eq!(tokens[5].token_type, TokenType::Let);
        assert_eq!(tokens[6].token_type, TokenType::Identifier);
        assert_eq!(tokens[7].token_type, TokenType::Assign);
        assert_eq!(tokens[8].token_type, TokenType::IntLit);
        assert_eq!(tokens[9].token_type, TokenType::Semicolon);
    }

    #[test]
    fn test_let_pattern_matching() {
        let input = "let Some(x) = func()";
        let mut lexer = Lexer::new(input.to_string());
        let tokens = lexer.tokenize();

        let expected = vec![
            TokenType::Let,
            TokenType::Identifier, // Some
            TokenType::LParen,
            TokenType::Identifier, // x
            TokenType::RParen,
            TokenType::Assign,
            TokenType::Identifier, // func
            TokenType::LParen,
            TokenType::RParen,
            TokenType::Eof,
        ];
        for (i, ty) in expected.iter().enumerate() {
            assert_eq!(tokens[i].token_type, *ty, "Mismatch at index {}", i);
        }
    }
}
