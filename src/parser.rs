// src/parser.rs

use crate::ast::*;
use crate::lexer::{Token, TokenType};
use crate::span::Span;
use std::cell::Cell;

#[derive(Debug, Clone, PartialEq)]
pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    next_id: Cell<u32>,
    self_type_stack: Vec<Type>,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Parser {
            tokens,
            pos: 0,
            next_id: Cell::new(1),
            self_type_stack: Vec::new(),
        }
    }

    fn alloc_id(&self) -> NodeId {
        let id = NodeId(self.next_id.get());
        self.next_id.set(id.0 + 1);
        id
    }

    fn make_spanned<T>(&self, node: T) -> Spanned<T> {
        let token = self.peek_token();
        Spanned::new(node, token.span, self.alloc_id())
    }

    fn make_spanned_with_span<T>(&self, node: T, span: Span) -> Spanned<T> {
        Spanned::new(node, span, self.alloc_id())
    }

    fn make_expr(&self, kind: ExpressionKind) -> Expression {
        let span = self.peek_token().span;
        Spanned::new(kind, span, self.alloc_id())
    }

    fn make_stmt(&self, kind: StatementKind) -> Statement {
        let span = self.peek_token().span;
        Spanned::new(kind, span, self.alloc_id())
    }

    fn make_decl(&self, kind: DeclarationKind) -> Declaration {
        let span = self.peek_token().span;
        Spanned::new(kind, span, self.alloc_id())
    }

    fn make_type(&self, kind: TypeKind) -> Type {
        let span = self.peek_token().span;
        Spanned::new(kind, span, self.alloc_id())
    }

    fn make_pattern(&self, kind: PatternKind) -> Pattern {
        let span = self.peek_token().span;
        Spanned::new(kind, span, self.alloc_id())
    }

    pub fn parse_module(&mut self) -> Result<Module, String> {
        let start_span = self.peek_token().span;
        let mut declarations = Vec::new();
        let mut name = None;
        if self.match_token(TokenType::Namespace) {
            name = Some(self.namespace_path()?);
        }
        while !self.is_at_end() && self.peek_token().token_type != TokenType::Eof {
            declarations.push(self.declaration()?);
        }
        Ok(Module {
            name,
            declarations,
            span: start_span.join(self.last_token_span()),
        })
    }

    pub fn parse_expression_only(&mut self) -> Result<Expression, String> {
        let expr = self.expression()?;
        if self.peek_token().token_type != TokenType::Eof {
            return Err(format!(
                "Expected end of input, got {:?}",
                self.peek_token()
            ));
        }
        Ok(expr)
    }

    fn declaration(&mut self) -> Result<Declaration, String> {
        let token = self.peek_token();
        match token.token_type {
            TokenType::Fn => self.function_decl(),
            TokenType::Struct => self.struct_decl(),
            TokenType::Enum => self.enum_decl(),
            TokenType::Newtype => self.newtype_decl(),
            TokenType::Interface => self.interface_decl(),
            TokenType::Impl => self.impl_decl(),
            TokenType::Extend => self.extend_decl(),
            TokenType::Use => {
                self.advance_token();
                let path = self.use_path()?;
                self.consume(TokenType::Semicolon, "Expected ';' after use")?;
                Ok(self.make_decl(DeclarationKind::Use(path)))
            }
            _ => Err(format!("Unexpected token in declaration: {:?}", token)),
        }
    }

    fn parse_generics(&mut self) -> Result<Vec<GenericParam>, String> {
        if !self.match_token(TokenType::LessThan) {
            return Ok(Vec::new());
        }

        let mut generics = Vec::new();
        loop {
            let name = self.identifier_with_span()?;
            let span = name.span;
            generics.push(Spanned::new(
                GenericParamKind {
                    name,
                    bounds: Vec::new(),
                },
                span,
                self.alloc_id(),
            ));
            if !self.match_token(TokenType::Comma) {
                break;
            }
        }

        self.consume(
            TokenType::GreaterThan,
            "Expected '>' after generic parameters",
        )?;
        Ok(generics)
    }

    fn function_decl(&mut self) -> Result<Declaration, String> {
        self.advance_token();
        let name = self.identifier_with_span()?;
        let generics = self.parse_generics()?;
        self.consume(TokenType::LParen, "Expected '('")?;
        let mut params = Vec::new();
        while self.peek_token().token_type != TokenType::RParen {
            let p_name = self.identifier_with_span()?;
            let p_type = if p_name.name == "self"
                && self.peek_token().token_type != TokenType::Colon
                && !self.self_type_stack.is_empty()
            {
                self.self_type_stack.last().cloned().unwrap()
            } else {
                self.consume(TokenType::Colon, "Expected ':'")?;
                self.parse_type()?
            };
            params.push(Parameter {
                name: p_name,
                type_annotation: p_type,
                span: self.last_token_span(),
            });
            if !self.match_token(TokenType::Comma) {
                break;
            }
        }
        self.consume(TokenType::RParen, "Expected ')'")?;

        let mut return_type = None;
        if self.match_token(TokenType::ReturnType) {
            return_type = Some(self.parse_type()?);
        }

        let body = self.block()?;
        Ok(self.make_decl(DeclarationKind::Function {
            name,
            generics,
            params,
            return_type,
            body,
        }))
    }

    fn struct_decl(&mut self) -> Result<Declaration, String> {
        self.advance_token();
        let name = self.identifier_with_span()?;
        let generics = self.parse_generics()?;
        self.consume(TokenType::LBrace, "Expected '{'")?;
        let mut fields = Vec::new();
        while self.peek_token().token_type != TokenType::RBrace {
            let f_name = self.identifier_with_span()?;
            self.consume(TokenType::Colon, "Expected ':'")?;
            let f_type = self.parse_type()?;
            fields.push((f_name, f_type));
            if !self.match_token(TokenType::Comma) {
                break;
            }
        }
        self.consume(TokenType::RBrace, "Expected '}'")?;
        Ok(self.make_decl(DeclarationKind::Struct {
            name,
            generics,
            fields,
        }))
    }

    fn enum_decl(&mut self) -> Result<Declaration, String> {
        self.advance_token();
        let name = self.identifier_with_span()?;
        let generics = self.parse_generics()?;
        self.consume(TokenType::LBrace, "Expected '{'")?;
        let mut variants = Vec::new();
        while self.peek_token().token_type != TokenType::RBrace {
            let v_name = self.identifier_with_span()?;
            let payload = if self.match_token(TokenType::LParen) {
                let mut types = Vec::new();
                while self.peek_token().token_type != TokenType::RParen {
                    types.push(self.parse_type()?);
                    if !self.match_token(TokenType::Comma) {
                        break;
                    }
                }
                self.consume(TokenType::RParen, "Expected ')'")?;
                Some(self.make_spanned_with_span(
                    EnumVariantPayloadKind::Tuple(types),
                    self.last_token_span(),
                ))
            } else if self.match_token(TokenType::LBrace) {
                let mut fields = Vec::new();
                while self.peek_token().token_type != TokenType::RBrace {
                    let f_name = self.identifier_with_span()?;
                    self.consume(TokenType::Colon, "Expected ':'")?;
                    let f_type = self.parse_type()?;
                    fields.push((f_name, f_type));
                    if !self.match_token(TokenType::Comma) {
                        break;
                    }
                }
                self.consume(TokenType::RBrace, "Expected '}'")?;
                Some(self.make_spanned_with_span(
                    EnumVariantPayloadKind::Struct(fields),
                    self.last_token_span(),
                ))
            } else {
                None
            };
            let variant = EnumVariantKind {
                name: v_name,
                payload,
            };
            variants.push(self.make_spanned(variant));
        }
        self.consume(TokenType::RBrace, "Expected '}'")?;
        Ok(self.make_decl(DeclarationKind::Enum {
            name,
            generics,
            variants,
        }))
    }

    fn newtype_decl(&mut self) -> Result<Declaration, String> {
        self.advance_token();
        let name = self.identifier_with_span()?;
        self.consume(TokenType::Assign, "Expected '=' after newtype name")?;
        let type_alias = self.parse_type()?;
        self.match_token(TokenType::Semicolon);
        Ok(self.make_decl(DeclarationKind::Newtype { name, type_alias }))
    }

    fn use_path(&mut self) -> Result<UsePath, String> {
        let start_span = self.peek_token().span;
        let mut segments = Vec::new();
        let mut wildcard = false;
        loop {
            if self.match_token(TokenType::Star) {
                wildcard = true;
                break;
            }
            segments.push(self.identifier_with_span()?.name);
            if !self.match_token(TokenType::PathSep) {
                break;
            }
        }
        Ok(self.make_spanned_with_span(
            UsePathKind { segments, wildcard },
            start_span.join(self.last_token_span()),
        ))
    }

    fn namespace_path(&mut self) -> Result<String, String> {
        let mut segments = Vec::new();
        segments.push(self.identifier_with_span()?.name);
        while self.match_token(TokenType::PathSep) {
            segments.push(self.identifier_with_span()?.name);
        }
        Ok(segments.join("::"))
    }

    fn block(&mut self) -> Result<Block, String> {
        let start_span = self.peek_token().span;
        self.consume(TokenType::LBrace, "Expected '{'")?;
        let mut statements = Vec::new();
        let mut trailing_expression = None;
        while self.peek_token().token_type != TokenType::RBrace {
            if let Some(stmt) = self.statement()? {
                statements.push(stmt);
            } else {
                trailing_expression = Some(Box::new(self.expression()?));
                break;
            }
        }
        let end_span = self.consume(TokenType::RBrace, "Expected '}'")?.span;
        Ok(Block {
            statements,
            trailing_expression,
            span: start_span.join(end_span),
            block_id: self.alloc_id(),
        })
    }

    fn expression(&mut self) -> Result<Expression, String> {
        self.assignment()
    }

    fn assignment(&mut self) -> Result<Expression, String> {
        let expr = self.pipe()?;
        if self.match_token(TokenType::Assign) {
            let value = self.assignment()?;
            return Ok(self.make_expr(ExpressionKind::BinaryOp {
                op: Operator::Assign,
                left: Box::new(expr),
                right: Box::new(value),
            }));
        } else if self.match_token(TokenType::PlusAssign) {
            let value = self.assignment()?;
            return Ok(self.make_expr(ExpressionKind::BinaryOp {
                op: Operator::AddAssign,
                left: Box::new(expr),
                right: Box::new(value),
            }));
        } else if self.match_token(TokenType::MinusAssign) {
            let value = self.assignment()?;
            return Ok(self.make_expr(ExpressionKind::BinaryOp {
                op: Operator::SubAssign,
                left: Box::new(expr),
                right: Box::new(value),
            }));
        } else if self.match_token(TokenType::StarAssign) {
            let value = self.assignment()?;
            return Ok(self.make_expr(ExpressionKind::BinaryOp {
                op: Operator::MulAssign,
                left: Box::new(expr),
                right: Box::new(value),
            }));
        } else if self.match_token(TokenType::SlashAssign) {
            let value = self.assignment()?;
            return Ok(self.make_expr(ExpressionKind::BinaryOp {
                op: Operator::DivAssign,
                left: Box::new(expr),
                right: Box::new(value),
            }));
        }
        Ok(expr)
    }

    fn pipe(&mut self) -> Result<Expression, String> {
        let mut expr = self.equality()?;
        while self.match_token(TokenType::Pipe) {
            let right = self.equality()?;
            expr = self.make_expr(ExpressionKind::BinaryOp {
                op: Operator::Pipe,
                left: Box::new(expr),
                right: Box::new(right),
            });
        }
        Ok(expr)
    }

    fn equality(&mut self) -> Result<Expression, String> {
        let mut expr = self.bitwise()?;
        while let Some(op) = if self.match_token(TokenType::Equal) {
            Some(Operator::Eq)
        } else if self.match_token(TokenType::NotEqual) {
            Some(Operator::Ne)
        } else {
            None
        } {
            let right = self.comparison()?;
            expr = self.make_expr(ExpressionKind::BinaryOp {
                op,
                left: Box::new(expr),
                right: Box::new(right),
            });
        }
        Ok(expr)
    }

    fn bitwise(&mut self) -> Result<Expression, String> {
        let mut expr = self.comparison()?;
        while let Some(op) = if self.match_token(TokenType::BitwiseAnd) {
            Some(Operator::BitAnd)
        } else if self.match_token(TokenType::BitwiseOr) {
            Some(Operator::BitOr)
        } else if self.match_token(TokenType::BitwiseXor) {
            Some(Operator::BitXor)
        } else {
            None
        } {
            let right = self.comparison()?;
            expr = self.make_expr(ExpressionKind::BinaryOp {
                op,
                left: Box::new(expr),
                right: Box::new(right),
            });
        }
        Ok(expr)
    }

    fn comparison(&mut self) -> Result<Expression, String> {
        let mut expr = self.shift()?;
        while let Some(op) = if self.match_token(TokenType::LessThan) {
            Some(Operator::Lt)
        } else if self.match_token(TokenType::LessThanOrEqual) {
            Some(Operator::Le)
        } else if self.match_token(TokenType::GreaterThan) {
            Some(Operator::Gt)
        } else if self.match_token(TokenType::GreaterThanOrEqual) {
            Some(Operator::Ge)
        } else {
            None
        } {
            let right = self.shift()?;
            expr = self.make_expr(ExpressionKind::BinaryOp {
                op,
                left: Box::new(expr),
                right: Box::new(right),
            });
        }
        Ok(expr)
    }

    fn shift(&mut self) -> Result<Expression, String> {
        let mut expr = self.term()?;
        while let Some(op) = if self.match_token(TokenType::ShiftLeft) {
            Some(Operator::Shl)
        } else if self.match_token(TokenType::ShiftRight) {
            Some(Operator::Shr)
        } else {
            None
        } {
            let right = self.term()?;
            expr = self.make_expr(ExpressionKind::BinaryOp {
                op,
                left: Box::new(expr),
                right: Box::new(right),
            });
        }
        Ok(expr)
    }

    fn term(&mut self) -> Result<Expression, String> {
        let mut expr = self.factor()?;
        while let Some(op) = if self.match_token(TokenType::Plus) {
            Some(Operator::Add)
        } else if self.match_token(TokenType::Minus) {
            Some(Operator::Sub)
        } else {
            None
        } {
            let right = self.factor()?;
            expr = self.make_expr(ExpressionKind::BinaryOp {
                op,
                left: Box::new(expr),
                right: Box::new(right),
            });
        }
        Ok(expr)
    }

    fn factor(&mut self) -> Result<Expression, String> {
        let mut expr = self.unary()?;
        while let Some(op) = if self.match_token(TokenType::Star) {
            Some(Operator::Mul)
        } else if self.match_token(TokenType::Slash) {
            Some(Operator::Div)
        } else if self.match_token(TokenType::Modulo) {
            Some(Operator::Mod)
        } else {
            None
        } {
            let right = self.unary()?;
            expr = self.make_expr(ExpressionKind::BinaryOp {
                op,
                left: Box::new(expr),
                right: Box::new(right),
            });
        }
        Ok(expr)
    }

    fn unary(&mut self) -> Result<Expression, String> {
        if self.match_token(TokenType::Minus) {
            let expr = self.unary()?;
            return Ok(self.make_expr(ExpressionKind::UnaryOp {
                op: Operator::Sub,
                expr: Box::new(expr),
            }));
        }
        let mut expr = self.primary()?;
        while self.match_token(TokenType::Try) {
            expr = self.make_expr(ExpressionKind::TryOperator {
                expr: Box::new(expr),
            });
        }
        Ok(expr)
    }

    fn primary(&mut self) -> Result<Expression, String> {
        match self.peek_token().token_type {
            TokenType::IntLit => {
                let token = self.advance_token();
                let lex = token.lexeme.clone();
                let mut digits = String::new();
                let mut suffix: Option<String> = None;
                for ch in lex.chars() {
                    if ch.is_ascii_digit() || ch == '_' {
                        digits.push(ch);
                    } else {
                        suffix = Some(lex[digits.len()..].to_string());
                        break;
                    }
                }
                let digits_clean: String = digits.chars().filter(|c| *c != '_').collect();
                let val: i64 = digits_clean.parse().unwrap();
                let expr = Spanned::new(
                    ExpressionKind::Literal(Literal {
                        kind: LiteralKind::Int(val, suffix),
                        span: token.span,
                    }),
                    token.span,
                    self.alloc_id(),
                );
                self.primary_postfix(expr)
            }
            TokenType::FloatLit => {
                let token = self.advance_token();
                let lex = token.lexeme.clone();
                let mut idx = None;
                for (i, ch) in lex.chars().enumerate() {
                    if ch.is_ascii_alphabetic() {
                        idx = Some(i);
                        break;
                    }
                }
                let (num_part, suffix) = if let Some(i) = idx {
                    (lex[..i].to_string(), Some(lex[i..].to_string()))
                } else {
                    (lex.clone(), None)
                };
                let num_clean: String = num_part.chars().filter(|c| *c != '_').collect();
                let val: f64 = num_clean.parse().unwrap();
                let expr = Spanned::new(
                    ExpressionKind::Literal(Literal {
                        kind: LiteralKind::Float(val, suffix),
                        span: token.span,
                    }),
                    token.span,
                    self.alloc_id(),
                );
                self.primary_postfix(expr)
            }
            TokenType::StrLit => {
                let token = self.advance_token();
                let expr = Spanned::new(
                    ExpressionKind::Literal(Literal {
                        kind: LiteralKind::Str(token.lexeme),
                        span: token.span,
                    }),
                    token.span,
                    self.alloc_id(),
                );
                self.primary_postfix(expr)
            }
            TokenType::True => {
                let token = self.advance_token();
                let expr = Spanned::new(
                    ExpressionKind::Literal(Literal {
                        kind: LiteralKind::Bool(true),
                        span: token.span,
                    }),
                    token.span,
                    self.alloc_id(),
                );
                self.primary_postfix(expr)
            }
            TokenType::False => {
                let token = self.advance_token();
                let expr = Spanned::new(
                    ExpressionKind::Literal(Literal {
                        kind: LiteralKind::Bool(false),
                        span: token.span,
                    }),
                    token.span,
                    self.alloc_id(),
                );
                self.primary_postfix(expr)
            }
            TokenType::Identifier => {
                let identifier = self.identifier_with_span()?;
                let ident_expr = Spanned::new(
                    ExpressionKind::Identifier(identifier.clone()),
                    identifier.span,
                    self.alloc_id(),
                );
                if self.peek_token().token_type == TokenType::LBrace
                    && identifier
                        .name
                        .chars()
                        .next()
                        .map(|c| c.is_ascii_uppercase())
                        .unwrap_or(false)
                {
                    return self.struct_init(identifier);
                }
                if self.peek_token().token_type == TokenType::LParen {
                    self.advance_token();
                    let mut args = Vec::new();
                    while self.peek_token().token_type != TokenType::RParen {
                        args.push(self.expression()?);
                        if !self.match_token(TokenType::Comma) {
                            break;
                        }
                    }
                    self.consume(TokenType::RParen, "Expected ')' after arguments")?;
                    let span = identifier.span.join(self.last_token_span());
                    let expr = Spanned::new(
                        ExpressionKind::Call {
                            func: Box::new(ident_expr.clone()),
                            args,
                        },
                        span,
                        self.alloc_id(),
                    );
                    self.primary_postfix(expr)
                } else {
                    self.primary_postfix(ident_expr)
                }
            }
            TokenType::Match => self.match_expression(),
            TokenType::LParen => {
                self.advance_token();
                let expr = self.expression()?;
                self.consume(TokenType::RParen, "Expected ')'")?;
                self.primary_postfix(expr)
            }
            TokenType::LBracket => {
                let start_span = self.peek_token().span;
                self.advance_token();
                let mut elems = Vec::new();
                while self.peek_token().token_type != TokenType::RBracket {
                    elems.push(self.expression()?);
                    if !self.match_token(TokenType::Comma) {
                        break;
                    }
                }
                let end_span = self
                    .consume(TokenType::RBracket, "Expected ']' after array literal")?
                    .span;
                let span = start_span.join(end_span);
                let expr = Spanned::new(
                    ExpressionKind::Literal(Literal {
                        kind: LiteralKind::Array(elems),
                        span,
                    }),
                    span,
                    self.alloc_id(),
                );
                self.primary_postfix(expr)
            }
            TokenType::LBrace => self.merge_expression(),
            token => Err(format!("Expected expression, got {:?}", token)),
        }
    }

    fn struct_init(&mut self, name: Identifier) -> Result<Expression, String> {
        let start = name.span;
        self.consume(TokenType::LBrace, "Expected '{' after struct name")?;
        let mut fields = Vec::new();
        while self.peek_token().token_type != TokenType::RBrace {
            let f = self.identifier_with_span()?;
            self.consume(TokenType::Colon, "Expected ':' in struct init field")?;
            let v = self.expression()?;
            fields.push((f, v));
            if !self.match_token(TokenType::Comma) {
                break;
            }
        }
        let end = self
            .consume(TokenType::RBrace, "Expected '}' after struct init")?
            .span;
        Ok(self
            .make_spanned_with_span(ExpressionKind::StructInit { name, fields }, start.join(end)))
    }

    fn match_expression(&mut self) -> Result<Expression, String> {
        let start_span = self.peek_token().span;
        self.consume(TokenType::Match, "Expected 'match'")?;
        let scrutinee = self.expression()?;
        self.consume(TokenType::LBrace, "Expected '{' after match scrutinee")?;
        let mut arms = Vec::new();
        while self.peek_token().token_type != TokenType::RBrace {
            let pattern = self.parse_pattern()?;
            let mut guard = None;
            if self.match_token(TokenType::If) {
                guard = Some(self.expression()?);
            }
            self.consume(TokenType::MatchArm, "Expected '=>' in match arm")?;
            let body = self.expression()?;
            let arm_span = pattern.span.join(body.span);
            arms.push(self.make_spanned_with_span(
                MatchArmKind {
                    pattern,
                    guard,
                    body,
                },
                arm_span,
            ));
            self.match_token(TokenType::Comma);
        }
        let end_span = self
            .consume(TokenType::RBrace, "Expected '}' after match")?
            .span;
        Ok(self.make_spanned_with_span(
            ExpressionKind::Match {
                expr: Box::new(scrutinee),
                arms,
            },
            start_span.join(end_span),
        ))
    }

    fn interface_decl(&mut self) -> Result<Declaration, String> {
        self.advance_token();
        let name = self.identifier_with_span()?;
        let generics = self.parse_generics()?;
        self.consume(TokenType::LBrace, "Expected '{' after interface name")?;
        let mut methods = Vec::new();
        while self.peek_token().token_type != TokenType::RBrace {
            self.consume(TokenType::Fn, "Expected 'fn' in interface")?;
            let method_name = self.identifier_with_span()?;
            let method_generics = self.parse_generics()?;
            self.consume(TokenType::LParen, "Expected '('")?;
            let mut params = Vec::new();
            while self.peek_token().token_type != TokenType::RParen {
                let p_name = self.identifier_with_span()?;
                self.consume(TokenType::Colon, "Expected ':'")?;
                let p_type = self.parse_type()?;
                params.push(Parameter {
                    name: p_name,
                    type_annotation: p_type,
                    span: self.last_token_span(),
                });
                if !self.match_token(TokenType::Comma) {
                    break;
                }
            }
            self.consume(TokenType::RParen, "Expected ')'")?;
            let mut return_type = None;
            if self.match_token(TokenType::ReturnType) {
                return_type = Some(self.parse_type()?);
            }
            let span = method_name.span.join(self.last_token_span());
            methods.push(Spanned::new(
                FunctionSignatureKind {
                    name: method_name,
                    generics: method_generics,
                    params,
                    return_type,
                },
                span,
                self.alloc_id(),
            ));
        }
        let _end = self
            .consume(TokenType::RBrace, "Expected '}' after interface")?
            .span;
        Ok(self.make_decl(DeclarationKind::Interface {
            name,
            generics,
            methods,
        }))
    }

    fn impl_decl(&mut self) -> Result<Declaration, String> {
        self.advance_token();
        let first = self.parse_type()?;
        if self.match_token(TokenType::For) {
            let trait_name = first;
            let type_name = self.parse_type()?;
            let generics = self.parse_generics()?;
            self.consume(TokenType::LBrace, "Expected '{' after impl header")?;
            self.self_type_stack.push(type_name.clone());
            let mut methods = Vec::new();
            while self.peek_token().token_type != TokenType::RBrace {
                methods.push(self.function_decl()?);
            }
            self.self_type_stack.pop();
            self.consume(TokenType::RBrace, "Expected '}' after impl")?;
            Ok(self.make_decl(DeclarationKind::Impl {
                trait_name,
                type_name,
                generics,
                methods,
            }))
        } else {
            // Inherent impl: `impl Type { ... }` is parsed as an extension block.
            let type_constraint = first;
            let generics = Vec::new();
            self.consume(TokenType::LBrace, "Expected '{' after impl type")?;
            self.self_type_stack.push(type_constraint.clone());
            let mut methods = Vec::new();
            while self.peek_token().token_type != TokenType::RBrace {
                methods.push(self.function_decl()?);
            }
            self.self_type_stack.pop();
            self.consume(TokenType::RBrace, "Expected '}' after impl")?;
            Ok(self.make_decl(DeclarationKind::Extension {
                generics,
                type_constraint,
                methods,
            }))
        }
    }

    fn extend_decl(&mut self) -> Result<Declaration, String> {
        self.advance_token();
        let type_constraint = self.parse_type()?;
        let generics = Vec::new();
        self.consume(TokenType::LBrace, "Expected '{' after extend type")?;
        self.self_type_stack.push(type_constraint.clone());
        let mut methods = Vec::new();
        while self.peek_token().token_type != TokenType::RBrace {
            methods.push(self.function_decl()?);
        }
        self.self_type_stack.pop();
        self.consume(TokenType::RBrace, "Expected '}' after extend")?;
        Ok(self.make_decl(DeclarationKind::Extension {
            generics,
            type_constraint,
            methods,
        }))
    }

    fn primary_postfix(&mut self, mut expr: Expression) -> Result<Expression, String> {
        loop {
            if self.match_token(TokenType::Dot) {
                let field = self.identifier_with_span()?;
                expr = self.make_expr(ExpressionKind::FieldAccess {
                    base: Box::new(expr),
                    field,
                });
                continue;
            }
            if self.match_token(TokenType::LBracket) {
                let index = self.expression()?;
                self.consume(TokenType::RBracket, "Expected ']' after index access")?;
                expr = self.make_expr(ExpressionKind::IndexAccess {
                    base: Box::new(expr),
                    index: Box::new(index),
                });
                continue;
            }
            if self.match_token(TokenType::LParen) {
                let mut args = Vec::new();
                while self.peek_token().token_type != TokenType::RParen {
                    args.push(self.expression()?);
                    if !self.match_token(TokenType::Comma) {
                        break;
                    }
                }
                let end_span = self
                    .consume(TokenType::RParen, "Expected ')' after arguments")?
                    .span;
                let span = expr.span.join(end_span);
                expr = Spanned::new(
                    ExpressionKind::Call {
                        func: Box::new(expr),
                        args,
                    },
                    span,
                    self.alloc_id(),
                );
                continue;
            }
            break;
        }
        Ok(expr)
    }

    fn parse_type(&mut self) -> Result<Type, String> {
        // Handle `[T]`
        if self.peek_token().token_type == TokenType::LBracket {
            self.advance_token();
            let inner = self.parse_type()?;
            self.advance_token(); // ]
            return Ok(self.make_type(TypeKind {
                name: "Array".to_string(),
                generic_args: vec![inner],
            }));
        }

        // Handle `ref T` (ref is not a keyword token, it lexes as an identifier)
        if self.peek_token().token_type == TokenType::Identifier
            && self.peek_token().lexeme == "ref"
        {
            self.advance_token();
            let inner = self.parse_type()?;
            return Ok(self.make_type(TypeKind {
                name: "Ref".to_string(),
                generic_args: vec![inner],
            }));
        }

        // Handle `&T` reference syntax (BitwiseAnd token used as reference type prefix)
        if self.peek_token().token_type == TokenType::BitwiseAnd {
            self.advance_token();
            let inner = self.parse_type()?;
            return Ok(self.make_type(TypeKind {
                name: "Ref".to_string(),
                generic_args: vec![inner],
            }));
        }

        let name = self.identifier_with_span()?.name;
        let mut generic_args = Vec::new();
        if self.match_token(TokenType::LessThan) {
            loop {
                generic_args.push(self.parse_type()?);
                if !self.match_token(TokenType::Comma) {
                    break;
                }
            }
            self.consume(TokenType::GreaterThan, "Expected '>' after type arguments")?;
        }
        Ok(self.make_type(TypeKind { name, generic_args }))
    }

    fn merge_expression(&mut self) -> Result<Expression, String> {
        let start_span = self.peek_token().span;
        self.advance_token();
        let mut base = None;
        let mut fields = Vec::new();

        if self.match_token(TokenType::Spread) {
            let expr = self.expression()?;
            base = Some(Box::new(expr));
            self.match_token(TokenType::Comma);
        }

        while self.peek_token().token_type != TokenType::RBrace {
            let name = self.identifier_with_span()?;
            self.consume(TokenType::Colon, "Expected ':' in merge field")?;
            let value = self.expression()?;
            fields.push((name, value));
            if !self.match_token(TokenType::Comma) {
                break;
            }
        }

        let end_span = self
            .consume(TokenType::RBrace, "Expected '}' after merge expression")?
            .span;
        Ok(self.make_spanned_with_span(
            ExpressionKind::MergeExpression { base, fields },
            start_span.join(end_span),
        ))
    }

    fn statement(&mut self) -> Result<Option<Statement>, String> {
        let token = self.peek_token();
        match token.token_type {
            TokenType::Let => {
                self.advance_token();
                let mutable = self.match_token(TokenType::Mut);
                let name = self.identifier_with_span()?;
                let mut type_annotation = None;
                if self.match_token(TokenType::Colon) {
                    type_annotation = Some(self.parse_type()?);
                }
                self.consume(TokenType::Assign, "Expected '='")?;
                let initializer = self.expression()?;
                self.match_token(TokenType::Semicolon);
                Ok(Some(self.make_stmt(StatementKind::LetBinding {
                    mutable,
                    name,
                    type_annotation,
                    initializer,
                })))
            }
            TokenType::Return => {
                self.advance_token();
                let mut expr = None;
                if self.peek_token().token_type != TokenType::Semicolon
                    && self.peek_token().token_type != TokenType::RBrace
                {
                    expr = Some(self.expression()?);
                }
                self.match_token(TokenType::Semicolon);
                Ok(Some(self.make_stmt(StatementKind::Return(expr))))
            }
            TokenType::Conc => {
                self.advance_token();
                let body = self.block()?;
                Ok(Some(self.make_stmt(StatementKind::Conc { body })))
            }
            TokenType::If => {
                self.advance_token();
                if self.match_token(TokenType::Let) {
                    let pattern = self.parse_pattern()?;
                    self.consume(TokenType::Assign, "Expected '=' after if let pattern")?;
                    let matched = self.expression()?;
                    let then_branch = self.block()?;
                    let mut else_branch = None;
                    if self.match_token(TokenType::Else) {
                        // else must be an expression for IfLet; use block or nested if-let later.
                        let block = self.block()?;
                        let span = block.span;
                        else_branch = Some(Box::new(
                            self.make_spanned_with_span(ExpressionKind::Block(block), span),
                        ));
                    }
                    let span = token.span.join(self.last_token_span());
                    let expr = self.make_spanned_with_span(
                        ExpressionKind::IfLet {
                            pattern: Box::new(pattern),
                            expr: Box::new(matched),
                            then: then_branch,
                            else_branch,
                        },
                        span,
                    );
                    return Ok(Some(self.make_stmt(StatementKind::Expression(expr))));
                }
                let condition = self.expression()?;
                let then_branch = self.block()?;
                let mut else_branch = None;
                if self.match_token(TokenType::Else) {
                    if self.peek_token().token_type == TokenType::If {
                        let else_if_stmt = self.statement()?.ok_or("Expected else if statement")?;
                        else_branch = Some(self.make_spanned_with_span(
                            ElseBranchKind::If(Box::new(else_if_stmt.clone())),
                            else_if_stmt.span,
                        ));
                    } else {
                        let block = self.block()?;
                        let span = block.span;
                        else_branch =
                            Some(self.make_spanned_with_span(ElseBranchKind::Block(block), span));
                    }
                }
                Ok(Some(self.make_stmt(StatementKind::If {
                    condition,
                    then_branch,
                    else_branch,
                })))
            }
            TokenType::For => {
                let start_span = self.peek_token().span;
                self.advance_token();
                let pattern = self.parse_pattern()?;
                self.consume(TokenType::In, "Expected 'in' after for pattern")?;
                let iterator = self.expression()?;
                let body = self.block()?;
                Ok(Some(self.make_stmt(StatementKind::Loop {
                    kind: self.make_spanned_with_span(
                        LoopKindKind::For {
                            pattern,
                            iterator,
                            body: body.clone(),
                        },
                        start_span.join(self.last_token_span()),
                    ),
                    body,
                })))
            }
            TokenType::While => {
                let start_span = self.peek_token().span;
                self.advance_token();
                let condition = self.expression()?;
                let body = self.block()?;
                Ok(Some(self.make_stmt(StatementKind::Loop {
                    kind: self.make_spanned_with_span(
                        LoopKindKind::While {
                            condition,
                            body: body.clone(),
                        },
                        start_span.join(self.last_token_span()),
                    ),
                    body,
                })))
            }
            TokenType::Match => {
                let start_span = self.peek_token().span;
                let expr = self.match_expression()?;
                let ExpressionKind::Match {
                    expr: scrutinee,
                    arms,
                } = expr.node
                else {
                    return Err("internal: match_expression did not return Match".to_string());
                };
                self.match_token(TokenType::Semicolon);
                Ok(Some(self.make_spanned_with_span(
                    StatementKind::Match {
                        expr: *scrutinee,
                        arms,
                    },
                    start_span.join(self.last_token_span()),
                )))
            }
            _ => {
                let expr = self.expression()?;
                if self.match_token(TokenType::Semicolon) {
                    Ok(Some(self.make_stmt(StatementKind::Expression(expr))))
                } else {
                    Err("Expression statements must end in ';'. Trailing expressions are only allowed at block end.".to_string())
                }
            }
        }
    }

    fn identifier_with_span(&mut self) -> Result<Identifier, String> {
        let token = self.advance_token();
        if token.token_type == TokenType::Identifier {
            Ok(Identifier {
                name: token.lexeme,
                span: token.span,
            })
        } else {
            Err(format!("Expected identifier, got {:?}", token))
        }
    }

    fn parse_pattern(&mut self) -> Result<Pattern, String> {
        let mut pattern = self.parse_pattern_atom()?;
        while self.match_token(TokenType::BitwiseOr) {
            let right = self.parse_pattern_atom()?;
            let span = pattern.span.join(right.span);
            pattern = self
                .make_spanned_with_span(PatternKind::Or(Box::new(pattern), Box::new(right)), span);
        }
        Ok(pattern)
    }

    fn parse_pattern_atom(&mut self) -> Result<Pattern, String> {
        let token = self.peek_token();
        match token.token_type {
            TokenType::Identifier => {
                let id = self.identifier_with_span()?;
                if id.name == "_" {
                    return Ok(self.make_pattern(PatternKind::Wildcard));
                }
                if id.name == "Ok" || id.name == "Err" || id.name == "Some" {
                    if self.match_token(TokenType::LParen) {
                        let inner = self.parse_pattern()?;
                        self.consume(TokenType::RParen, "Expected ')' after variant pattern")?;
                        let enum_name = if id.name == "Some" {
                            "Option"
                        } else {
                            "Result"
                        };
                        return Ok(self.make_pattern(PatternKind::EnumVariant {
                            enum_name: Identifier {
                                name: enum_name.to_string(),
                                span: id.span,
                            },
                            variant_name: Identifier {
                                name: id.name,
                                span: id.span,
                            },
                            payload: Some(Box::new(inner)),
                        }));
                    }
                }
                if id.name == "None" {
                    return Ok(self.make_pattern(PatternKind::EnumVariant {
                        enum_name: Identifier {
                            name: "Option".to_string(),
                            span: id.span,
                        },
                        variant_name: Identifier {
                            name: "None".to_string(),
                            span: id.span,
                        },
                        payload: None,
                    }));
                }
                Ok(self.make_pattern(PatternKind::Identifier(id)))
            }
            TokenType::LParen => {
                let start = self.peek_token().span;
                self.advance_token();
                let first = self.parse_pattern()?;
                if self.match_token(TokenType::Comma) {
                    let mut parts = vec![first];
                    while self.peek_token().token_type != TokenType::RParen {
                        parts.push(self.parse_pattern()?);
                        if !self.match_token(TokenType::Comma) {
                            break;
                        }
                    }
                    let end = self
                        .consume(TokenType::RParen, "Expected ')' after tuple pattern")?
                        .span;
                    Ok(self.make_spanned_with_span(PatternKind::Tuple(parts), start.join(end)))
                } else {
                    self.consume(TokenType::RParen, "Expected ')'")?;
                    Ok(first)
                }
            }
            TokenType::LBracket => {
                let start = self.peek_token().span;
                self.advance_token();
                let mut parts = Vec::new();
                while self.peek_token().token_type != TokenType::RBracket {
                    parts.push(self.parse_pattern()?);
                    if !self.match_token(TokenType::Comma) {
                        break;
                    }
                }
                let end = self
                    .consume(TokenType::RBracket, "Expected ']' after array pattern")?
                    .span;
                Ok(self.make_spanned_with_span(PatternKind::Array(parts), start.join(end)))
            }
            TokenType::IntLit => {
                let token = self.advance_token();
                let lex = token.lexeme.clone();
                let mut digits = String::new();
                let mut suffix: Option<String> = None;
                for ch in lex.chars() {
                    if ch.is_ascii_digit() || ch == '_' {
                        digits.push(ch);
                    } else {
                        suffix = Some(lex[digits.len()..].to_string());
                        break;
                    }
                }
                let digits_clean: String = digits.chars().filter(|c| *c != '_').collect();
                let val: i64 = digits_clean
                    .parse()
                    .map_err(|e| format!("Invalid int literal: {}", e))?;
                Ok(self.make_pattern(PatternKind::Literal(Literal {
                    kind: LiteralKind::Int(val, suffix),
                    span: token.span,
                })))
            }
            TokenType::FloatLit => {
                let token = self.advance_token();
                let lex = token.lexeme.clone();
                let mut idx = None;
                for (i, ch) in lex.chars().enumerate() {
                    if ch.is_ascii_alphabetic() {
                        idx = Some(i);
                        break;
                    }
                }
                let (num_part, suffix) = if let Some(i) = idx {
                    (lex[..i].to_string(), Some(lex[i..].to_string()))
                } else {
                    (lex.clone(), None)
                };
                let num_clean: String = num_part.chars().filter(|c| *c != '_').collect();
                let val: f64 = num_clean
                    .parse()
                    .map_err(|e| format!("Invalid float literal: {}", e))?;
                Ok(self.make_pattern(PatternKind::Literal(Literal {
                    kind: LiteralKind::Float(val, suffix),
                    span: token.span,
                })))
            }
            TokenType::StrLit => {
                let token = self.advance_token();
                Ok(self.make_pattern(PatternKind::Literal(Literal {
                    kind: LiteralKind::Str(token.lexeme),
                    span: token.span,
                })))
            }
            TokenType::True => {
                let token = self.advance_token();
                Ok(self.make_pattern(PatternKind::Literal(Literal {
                    kind: LiteralKind::Bool(true),
                    span: token.span,
                })))
            }
            TokenType::False => {
                let token = self.advance_token();
                Ok(self.make_pattern(PatternKind::Literal(Literal {
                    kind: LiteralKind::Bool(false),
                    span: token.span,
                })))
            }
            _ => Err(format!("Unsupported pattern start: {:?}", token)),
        }
    }

    fn advance_token(&mut self) -> Token {
        let token = self.peek_token();
        self.pos += 1;
        token
    }

    fn peek_token(&self) -> Token {
        self.tokens.get(self.pos).cloned().unwrap_or_else(|| {
            self.tokens.last().cloned().unwrap_or_else(|| Token {
                token_type: TokenType::Eof,
                lexeme: "".to_string(),
                span: Span::default(),
            })
        })
    }

    fn last_token_span(&self) -> Span {
        self.tokens
            .get(self.pos.saturating_sub(1))
            .map_or(Span::default(), |t| t.span)
    }

    fn is_at_end(&self) -> bool {
        self.peek_token().token_type == TokenType::Eof
    }

    fn match_token(&mut self, ty: TokenType) -> bool {
        if self.peek_token().token_type == ty {
            self.advance_token();
            true
        } else {
            false
        }
    }

    fn consume(&mut self, ty: TokenType, msg: &str) -> Result<Token, String> {
        if self.peek_token().token_type == ty {
            Ok(self.advance_token())
        } else {
            Err(format!(
                "{}: Expected {:?}, got {:?}",
                msg,
                ty,
                self.peek_token()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;

    fn normalize_source(source: &str) -> String {
        let mut body = source.trim().to_string();
        if !body.starts_with("namespace main") {
            body = format!("namespace main\n{}", body);
        }
        if !body.contains("fn main") {
            body.push_str("\nfn main() -> Int32 { return 0; }");
        }
        body
    }

    fn parse_source(source: &str) -> Module {
        let tokens = Lexer::new(normalize_source(source)).tokenize();
        Parser::new(tokens).parse_module().unwrap()
    }

    fn parse_expr(source: &str) -> Expression {
        let tokens = Lexer::new(source.to_string()).tokenize();
        Parser::new(tokens).parse_expression_only().unwrap()
    }

    #[test]
    fn test_parse_struct() {
        let module = parse_source("struct User { name: Str, age: Int32 }");
        assert_eq!(module.declarations.len(), 2);
        if let DeclarationKind::Struct { name, fields, .. } = &module.declarations[0].node {
            assert_eq!(name.name, "User");
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].0.name, "name");
            assert_eq!(fields[1].0.name, "age");
        } else {
            panic!("Expected struct declaration");
        }
    }

    #[test]
    fn parses_method_call_postfix() {
        let expr = parse_expr("x.foo(1)");
        let ExpressionKind::Call { func, args } = &expr.node else {
            panic!("expected Call");
        };
        assert_eq!(args.len(), 1);
        let ExpressionKind::FieldAccess { base, field } = &func.node else {
            panic!("expected FieldAccess");
        };
        let ExpressionKind::Identifier(id) = &base.node else {
            panic!("expected base identifier");
        };
        assert_eq!(id.name, "x");
        assert_eq!(field.name, "foo");
    }

    #[test]
    fn test_parse_conc_block() {
        let source = "fn main() -> Int32 { conc { let x: Int32 = 0; } return 0; }";
        let module = parse_source(source);
        if let DeclarationKind::Function { body, .. } = &module.declarations[0].node {
            assert!(matches!(
                body.statements[0].node,
                StatementKind::Conc { .. }
            ));
        } else {
            panic!("Expected function declaration");
        }
    }

    #[test]
    fn test_parse_newtype_declaration() {
        let source = "newtype UserId = Int32";
        let module = parse_source(source);
        if let DeclarationKind::Newtype { name, type_alias } = &module.declarations[0].node {
            assert_eq!(name.name, "UserId");
            assert_eq!(type_alias.node.name, "Int32");
        } else {
            panic!("Expected newtype declaration");
        }
    }

    #[test]
    fn test_parse_merge_expression() {
        let source =
            "fn main() -> Int32 { let updated: User = { ...user, name: \"x\" }; return 0; }";
        let module = parse_source(source);
        if let DeclarationKind::Function { body, .. } = &module.declarations[0].node {
            if let StatementKind::LetBinding { initializer, .. } = &body.statements[0].node {
                assert!(matches!(
                    initializer.node,
                    ExpressionKind::MergeExpression { .. }
                ));
            } else {
                panic!("Expected let binding");
            }
        } else {
            panic!("Expected function declaration");
        }
    }

    #[test]
    fn test_parse_generic_type_arguments() {
        let source = "fn main() -> Result<Int32, Str> { return 0; }";
        let module = parse_source(source);
        if let DeclarationKind::Function { return_type, .. } = &module.declarations[0].node {
            let ty = return_type.as_ref().unwrap();
            assert_eq!(ty.node.name, "Result");
            assert_eq!(ty.node.generic_args.len(), 2);
            assert_eq!(ty.node.generic_args[0].node.name, "Int32");
            assert_eq!(ty.node.generic_args[1].node.name, "Str");
        } else {
            panic!("Expected function declaration");
        }
    }

    #[test]
    fn test_parse_generic_function_declaration() {
        let source = "fn id<T>(x: T) -> T { return x; }";
        let module = parse_source(source);
        if let DeclarationKind::Function {
            generics,
            params,
            return_type,
            ..
        } = &module.declarations[0].node
        {
            assert_eq!(generics.len(), 1);
            assert_eq!(generics[0].node.name.name, "T");
            assert_eq!(params[0].type_annotation.node.name, "T");
            assert_eq!(return_type.as_ref().unwrap().node.name, "T");
        } else {
            panic!("Expected generic function declaration");
        }
    }

    #[test]
    fn test_parse_if_statement() {
        let source = "fn main(flag: Bool) -> Int32 { if flag { return 1; } else { return 2; } }";
        let module = parse_source(source);
        if let DeclarationKind::Function { body, .. } = &module.declarations[0].node {
            assert!(matches!(body.statements[0].node, StatementKind::If { .. }));
        } else {
            panic!("Expected function declaration");
        }
    }

    #[test]
    fn test_parse_while_statement() {
        let source = "fn main(flag: Bool) -> Int32 { while flag { return 1; } return 0; }";
        let module = parse_source(source);
        if let DeclarationKind::Function { body, .. } = &module.declarations[0].node {
            assert!(matches!(
                body.statements[0].node,
                StatementKind::Loop { .. }
            ));
        } else {
            panic!("Expected function declaration");
        }
    }

    #[test]
    fn parse_namespace_main() {
        let source = "namespace main\nfn main() -> Int32 { return 0; }";
        let module = parse_source(source);
        assert_eq!(module.name.as_deref(), Some("main"));
    }

    #[test]
    fn namespace_rejected_if_not_main() {
        let source = "namespace foo\nfn main() -> Int32 { return 0; }";
        let tokens = Lexer::new(normalize_source(source)).tokenize();
        let mut parser = Parser::new(tokens);
        assert!(parser.parse_module().is_err());
    }

    #[test]
    fn test_parse_ref_type() {
        let source = "fn main() -> ref Int32 { return 0; }";
        let module = parse_source(source);
        if let DeclarationKind::Function { return_type, .. } = &module.declarations[0].node {
            let ty = return_type.as_ref().unwrap();
            assert_eq!(ty.node.name, "Ref");
            assert_eq!(ty.node.generic_args.len(), 1);
            assert_eq!(ty.node.generic_args[0].node.name, "Int32");
        } else {
            panic!("Expected function declaration");
        }
    }

    #[test]
    fn test_parse_match_statement_with_result_patterns() {
        let source = "fn main() -> Int32 { match foo() { Ok(x) => 1, Err(_) => 2, } return 0; }";
        let module = parse_source(source);
        if let DeclarationKind::Function { body, .. } = &module.declarations[0].node {
            assert!(matches!(
                body.statements[0].node,
                StatementKind::Match { .. }
            ));
        } else {
            panic!("Expected function declaration");
        }
    }

    #[test]
    fn test_parse_if_let_statement() {
        let source = "fn main() -> Int32 { if let Ok(x) = foo() { return 1; } else { return 2; } return 0; }";
        let module = parse_source(source);
        if let DeclarationKind::Function { body, .. } = &module.declarations[0].node {
            assert!(matches!(
                body.statements[0].node,
                StatementKind::Expression(_)
            ));
        } else {
            panic!("Expected function declaration");
        }
    }
}
