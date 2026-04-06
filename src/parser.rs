// src/parser.rs

use crate::ast::*;
use crate::lexer::{Token, TokenType};

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Parser { tokens, pos: 0 }
    }

    pub fn parse_module(&mut self) -> Result<Module, String> {
        let mut declarations = Vec::new();
        let mut module_name = None;
        if self.match_token(TokenType::Namespace) {
            let path = self.namespace_path()?;
            if path != "main" {
                return Err(format!("Only 'namespace main' is allowed; got '{}'", path));
            }
            module_name = Some(path);
        }
        while !self.is_at_end() && self.peek().token_type != TokenType::Eof {
            declarations.push(self.declaration()?);
        }
        if module_name.as_deref() != Some("main") {
            return Err("Missing 'namespace main' declaration".to_string());
        }
        if !declarations
            .iter()
            .any(|decl| matches!(decl, Declaration::Function { name, .. } if name.name == "main"))
        {
            return Err("Namespace main must define fn main".to_string());
        }
        Ok(Module {
            name: module_name,
            declarations,
        })
    }

    fn declaration(&mut self) -> Result<Declaration, String> {
        let token = self.peek();
        match token.token_type {
            TokenType::Fn => self.function_decl(),
            TokenType::Struct => self.struct_decl(),
            TokenType::Enum => self.enum_decl(),
            TokenType::Newtype => self.newtype_decl(),
            TokenType::Use => {
                self.advance();
                let path = self.use_path()?;
                self.consume(TokenType::Semicolon, "Expected ';' after use")?;
                Ok(Declaration::Use(path))
            }
            _ => Err(format!("Unexpected token in declaration: {:?}", token)),
        }
    }

    fn function_decl(&mut self) -> Result<Declaration, String> {
        self.advance(); // consume fn
        let name = self.identifier()?;
        self.consume(TokenType::LParen, "Expected '('")?;
        let mut params = Vec::new();
        while self.peek().token_type != TokenType::RParen {
            let p_name = self.identifier()?;
            self.consume(TokenType::Colon, "Expected ':'")?;
            let p_type = self.parse_type()?;
            params.push(Parameter {
                name: p_name,
                type_annotation: p_type,
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
        Ok(Declaration::Function {
            name,
            generics: vec![],
            params,
            return_type,
            body,
        })
    }

    fn struct_decl(&mut self) -> Result<Declaration, String> {
        self.advance(); // consume struct
        let name = self.identifier()?;
        self.consume(TokenType::LBrace, "Expected '{'")?;
        let mut fields = Vec::new();
        while self.peek().token_type != TokenType::RBrace {
            let f_name = self.identifier()?;
            self.consume(TokenType::Colon, "Expected ':'")?;
            let f_type = self.parse_type()?;
            fields.push((f_name, f_type));
            if !self.match_token(TokenType::Comma) {
                break;
            }
        }
        self.consume(TokenType::RBrace, "Expected '}'")?;
        Ok(Declaration::Struct {
            name,
            generics: vec![],
            fields,
        })
    }

    fn enum_decl(&mut self) -> Result<Declaration, String> {
        self.advance(); // consume enum
        let name = self.identifier()?;
        self.consume(TokenType::LBrace, "Expected '{'")?;
        let mut variants = Vec::new();
        while self.peek().token_type != TokenType::RBrace {
            let v_name = self.identifier()?;
            let payload = if self.match_token(TokenType::LParen) {
                let mut types = Vec::new();
                while self.peek().token_type != TokenType::RParen {
                    types.push(self.parse_type()?);
                    if !self.match_token(TokenType::Comma) {
                        break;
                    }
                }
                self.consume(TokenType::RParen, "Expected ')'")?;
                Some(EnumVariantPayload::Tuple(types))
            } else if self.match_token(TokenType::LBrace) {
                let mut fields = Vec::new();
                while self.peek().token_type != TokenType::RBrace {
                    let f_name = self.identifier()?;
                    self.consume(TokenType::Colon, "Expected ':'")?;
                    let f_type = self.parse_type()?;
                    fields.push((f_name, f_type));
                    if !self.match_token(TokenType::Comma) {
                        break;
                    }
                }
                self.consume(TokenType::RBrace, "Expected '}'")?;
                Some(EnumVariantPayload::Struct(fields))
            } else {
                None
            };
            variants.push(EnumVariant {
                name: v_name,
                payload,
            });
            if !self.match_token(TokenType::Comma) {
                break;
            }
        }
        self.consume(TokenType::RBrace, "Expected '}'")?;
        Ok(Declaration::Enum {
            name,
            generics: vec![],
            variants,
        })
    }

    fn newtype_decl(&mut self) -> Result<Declaration, String> {
        self.advance(); // consume newtype
        let name = self.identifier()?;
        self.consume(TokenType::Assign, "Expected '=' after newtype name")?;
        let type_alias = self.parse_type()?;
        Ok(Declaration::Newtype { name, type_alias })
    }

    fn use_path(&mut self) -> Result<UsePath, String> {
        let mut segments = Vec::new();
        let mut wildcard = false;
        loop {
            if self.match_token(TokenType::Star) {
                wildcard = true;
                break;
            }
            segments.push(self.identifier()?.name);
            if !self.match_token(TokenType::PathSep) {
                break;
            }
        }
        Ok(UsePath { segments, wildcard })
    }

    fn namespace_path(&mut self) -> Result<String, String> {
        let mut segments = Vec::new();
        segments.push(self.identifier()?.name);
        while self.match_token(TokenType::PathSep) {
            segments.push(self.identifier()?.name);
        }
        Ok(segments.join("::"))
    }

    fn block(&mut self) -> Result<Block, String> {
        self.consume(TokenType::LBrace, "Expected '{'")?;
        let mut statements = Vec::new();
        let mut trailing_expression = None;
        while self.peek().token_type != TokenType::RBrace {
            if let Some(stmt) = self.statement()? {
                statements.push(stmt);
            } else {
                // Potential trailing expression
                trailing_expression = Some(Box::new(self.expression()?));
                break;
            }
        }
        self.consume(TokenType::RBrace, "Expected '}'")?;
        Ok(Block {
            statements,
            trailing_expression,
        })
    }

    fn statement(&mut self) -> Result<Option<Statement>, String> {
        let token = self.peek();
        match token.token_type {
            TokenType::Let => {
                self.advance();
                let mutable = self.match_token(TokenType::Mut);
                let name = self.identifier()?;
                let mut type_annotation = None;
                if self.match_token(TokenType::Colon) {
                    type_annotation = Some(self.parse_type()?);
                }
                self.consume(TokenType::Assign, "Expected '='")?;
                let initializer = self.expression()?;
                self.match_token(TokenType::Semicolon); // optional semicolon
                Ok(Some(Statement::LetBinding {
                    mutable,
                    name,
                    type_annotation,
                    initializer,
                }))
            }
            TokenType::Return => {
                self.advance();
                let mut expr = None;
                if self.peek().token_type != TokenType::Semicolon
                    && self.peek().token_type != TokenType::RBrace
                {
                    expr = Some(self.expression()?);
                }
                self.match_token(TokenType::Semicolon);
                Ok(Some(Statement::Return(expr)))
            }
            TokenType::Conc => {
                self.advance();
                let body = self.block()?;
                Ok(Some(Statement::Conc { body }))
            }
            TokenType::If => {
                self.advance();
                let condition = self.expression()?;
                let then_branch = self.block()?;
                let mut else_branch = None;
                if self.match_token(TokenType::Else) {
                    let else_block = self.block()?;
                    else_branch = Some(Box::new(Statement::Expression(Expression::Block(
                        else_block,
                    ))));
                }
                Ok(Some(Statement::If {
                    condition,
                    then_branch,
                    else_branch,
                }))
            }
            TokenType::While => {
                self.advance();
                let condition = self.expression()?;
                let body = self.block()?;
                Ok(Some(Statement::Loop {
                    kind: LoopKind::While {
                        condition,
                        body: body.clone(),
                    },
                    body,
                }))
            }
            _ => {
                // If it looks like an expression that ends in a semicolon, it's a statement.
                // This is a simplification.
                let expr = self.expression()?;
                if self.match_token(TokenType::Semicolon) {
                    Ok(Some(Statement::Expression(expr)))
                } else {
                    // Backtrack would be better, but we return None to signify it's a trailing expr
                    // Since we can't easily backtrack the entire expression with this simple parser,
                    // we assume that if no semicolon, it's intended to be trailing if it's the last thing.
                    // This is fragile.
                    Err("Expression statements must end in ';'. Trailing expressions are only allowed at block end.".to_string())
                }
            }
        }
    }

    fn expression(&mut self) -> Result<Expression, String> {
        self.equality()
    }

    fn equality(&mut self) -> Result<Expression, String> {
        let mut expr = self.comparison()?;
        while let Some(op) = if self.match_token(TokenType::Equal) {
            Some(Operator::Eq)
        } else if self.match_token(TokenType::NotEqual) {
            Some(Operator::Ne)
        } else {
            None
        } {
            let right = self.comparison()?;
            expr = Expression::BinaryOp {
                op,
                left: Box::new(expr),
                right: Box::new(right),
            };
        }
        Ok(expr)
    }

    fn comparison(&mut self) -> Result<Expression, String> {
        let mut expr = self.term()?;
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
            let right = self.term()?;
            expr = Expression::BinaryOp {
                op,
                left: Box::new(expr),
                right: Box::new(right),
            };
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
            expr = Expression::BinaryOp {
                op,
                left: Box::new(expr),
                right: Box::new(right),
            };
        }
        Ok(expr)
    }

    fn factor(&mut self) -> Result<Expression, String> {
        let mut expr = self.unary()?;
        while let Some(op) = if self.match_token(TokenType::Star) {
            Some(Operator::Mul)
        } else if self.match_token(TokenType::Slash) {
            Some(Operator::Div)
        } else {
            None
        } {
            let right = self.unary()?;
            expr = Expression::BinaryOp {
                op,
                left: Box::new(expr),
                right: Box::new(right),
            };
        }
        Ok(expr)
    }

    fn unary(&mut self) -> Result<Expression, String> {
        if self.match_token(TokenType::Minus) {
            let expr = self.unary()?;
            return Ok(Expression::UnaryOp {
                op: Operator::Sub,
                expr: Box::new(expr),
            });
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expression, String> {
        let token = self.advance();
        match token.token_type {
            TokenType::IntLit => Ok(Expression::Literal(Literal::Int(
                token.lexeme.parse().unwrap(),
            ))),
            TokenType::FloatLit => Ok(Expression::Literal(Literal::Float(
                token.lexeme.parse().unwrap(),
            ))),
            TokenType::StrLit => Ok(Expression::Literal(Literal::Str(token.lexeme))),
            TokenType::True => Ok(Expression::Literal(Literal::Bool(true))),
            TokenType::False => Ok(Expression::Literal(Literal::Bool(false))),
            TokenType::Identifier => {
                let ident = Expression::Identifier(Identifier { name: token.lexeme });
                // Check for a function call: identifier followed by '('
                if self.peek().token_type == TokenType::LParen {
                    self.advance(); // consume '('
                    let mut args = Vec::new();
                    while self.peek().token_type != TokenType::RParen {
                        args.push(self.expression()?);
                        if !self.match_token(TokenType::Comma) {
                            break;
                        }
                    }
                    self.consume(TokenType::RParen, "Expected ')' after arguments")?;
                    Ok(Expression::Call {
                        func: Box::new(ident),
                        args,
                    })
                } else {
                    Ok(ident)
                }
            }
            TokenType::LParen => {
                let expr = self.expression()?;
                self.consume(TokenType::RParen, "Expected ')'")?;
                Ok(expr)
            }
            TokenType::LBrace => self.merge_expression(),
            _ => Err(format!("Expected expression, got {:?}", token)),
        }
    }

    fn parse_type(&mut self) -> Result<Type, String> {
        let name = self.identifier()?.name;
        if name == "ref" {
            let inner = self.parse_type()?;
            return Ok(Type {
                name,
                generic_args: vec![inner],
            });
        }
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
        Ok(Type { name, generic_args })
    }

    fn merge_expression(&mut self) -> Result<Expression, String> {
        let mut base = None;
        let mut fields = Vec::new();

        if self.match_token(TokenType::Spread) {
            let expr = self.expression()?;
            base = Some(Box::new(expr));
            self.match_token(TokenType::Comma);
        }

        while self.peek().token_type != TokenType::RBrace {
            let name = self.identifier()?;
            self.consume(TokenType::Colon, "Expected ':' in merge field")?;
            let value = self.expression()?;
            fields.push((name, value));
            if !self.match_token(TokenType::Comma) {
                break;
            }
        }

        self.consume(TokenType::RBrace, "Expected '}' after merge expression")?;
        Ok(Expression::MergeExpression { base, fields })
    }

    fn identifier(&mut self) -> Result<Identifier, String> {
        let token = self.advance();
        if token.token_type == TokenType::Identifier {
            Ok(Identifier { name: token.lexeme })
        } else {
            Err(format!("Expected identifier, got {:?}", token))
        }
    }

    fn advance(&mut self) -> Token {
        let t = self.peek();
        if t.token_type != TokenType::Eof {
            self.pos += 1;
        }
        t
    }

    fn peek(&self) -> Token {
        self.tokens
            .get(self.pos)
            .cloned()
            .unwrap_or_else(|| self.tokens.last().cloned().unwrap())
    }

    fn is_at_end(&self) -> bool {
        self.peek().token_type == TokenType::Eof
    }

    fn match_token(&mut self, ty: TokenType) -> bool {
        if self.peek().token_type == ty {
            self.advance();
            true
        } else {
            false
        }
    }

    fn consume(&mut self, ty: TokenType, msg: &str) -> Result<Token, String> {
        if self.peek().token_type == ty {
            Ok(self.advance())
        } else {
            Err(format!("{}: Expected {:?}, got {:?}", msg, ty, self.peek()))
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

    #[test]
    fn test_parse_struct() {
        let module = parse_source("struct User { name: Str, age: Int32 }");
        assert_eq!(module.declarations.len(), 1);
        if let Declaration::Struct { name, fields, .. } = &module.declarations[0] {
            assert_eq!(name.name, "User");
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].0.name, "name");
            assert_eq!(fields[1].0.name, "age");
        } else {
            panic!("Expected struct declaration");
        }
    }

    #[test]
    fn test_parse_conc_block() {
        let source = "fn main() -> Int32 { conc { let x: Int32 = 0; } return 0; }";
        let module = parse_source(source);
        if let Declaration::Function { body, .. } = &module.declarations[0] {
            assert!(matches!(body.statements[0], Statement::Conc { .. }));
        } else {
            panic!("Expected function declaration");
        }
    }

    #[test]
    fn test_parse_newtype_declaration() {
        let source = "newtype UserId = Int32";
        let module = parse_source(source);
        if let Declaration::Newtype { name, type_alias } = &module.declarations[0] {
            assert_eq!(name.name, "UserId");
            assert_eq!(type_alias.name, "Int32");
        } else {
            panic!("Expected newtype declaration");
        }
    }

    #[test]
    fn test_parse_merge_expression() {
        let source =
            "fn main() -> Int32 { let updated: User = { ...user, name: \"x\" }; return 0; }";
        let module = parse_source(source);
        if let Declaration::Function { body, .. } = &module.declarations[0] {
            if let Statement::LetBinding { initializer, .. } = &body.statements[0] {
                assert!(matches!(initializer, Expression::MergeExpression { .. }));
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
        if let Declaration::Function { return_type, .. } = &module.declarations[0] {
            let ty = return_type.as_ref().unwrap();
            assert_eq!(ty.name, "Result");
            assert_eq!(ty.generic_args.len(), 2);
            assert_eq!(ty.generic_args[0].name, "Int32");
            assert_eq!(ty.generic_args[1].name, "Str");
        } else {
            panic!("Expected function declaration");
        }
    }

    #[test]
    fn test_parse_if_statement() {
        let source = "fn main(flag: Bool) -> Int32 { if flag { return 1; } else { return 2; } }";
        let module = parse_source(source);
        if let Declaration::Function { body, .. } = &module.declarations[0] {
            assert!(matches!(body.statements[0], Statement::If { .. }));
        } else {
            panic!("Expected function declaration");
        }
    }

    #[test]
    fn test_parse_while_statement() {
        let source = "fn main(flag: Bool) -> Int32 { while flag { return 1; } return 0; }";
        let module = parse_source(source);
        if let Declaration::Function { body, .. } = &module.declarations[0] {
            assert!(matches!(body.statements[0], Statement::Loop { .. }));
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
        let mut parser = Parser::new(Lexer::new(source.to_string()).tokenize());
        assert!(parser.parse_module().is_err());
    }

    #[test]
    fn test_parse_ref_type() {
        let source = "fn main() -> ref Int32 { return 0; }";
        let module = parse_source(source);
        if let Declaration::Function { return_type, .. } = &module.declarations[0] {
            let ty = return_type.as_ref().unwrap();
            assert_eq!(ty.name, "ref");
            assert_eq!(ty.generic_args.len(), 1);
            assert_eq!(ty.generic_args[0].name, "Int32");
        } else {
            panic!("Expected function declaration");
        }
    }

    #[test]
    fn test_parse_ref_struct_field() {
        let source = "struct Node { child: ref Node }";
        let module = parse_source(source);
        if let Declaration::Struct { fields, .. } = &module.declarations[0] {
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].0.name, "child");
            assert_eq!(fields[0].1.name, "ref");
            assert_eq!(fields[0].1.generic_args.len(), 1);
            assert_eq!(fields[0].1.generic_args[0].name, "Node");
        } else {
            panic!("Expected struct declaration");
        }
    }

    #[test]
    fn test_parse_function_with_let_and_return_from_spec() {
        // Mirrors the "Let Binding" and "Function Declaration" snippets from spec.md §5-6.
        let source = "fn compute_total(count: Int32) -> Int32 { let accumulator: Int32 = 0; return accumulator; }";
        let module = parse_source(source);
        assert_eq!(module.declarations.len(), 1);

        if let Declaration::Function {
            name,
            params,
            return_type,
            body,
            ..
        } = &module.declarations[0]
        {
            assert_eq!(name.name, "compute_total");
            assert_eq!(params.len(), 1);
            assert_eq!(params[0].name.name, "count");
            assert_eq!(params[0].type_annotation.name, "Int32");
            assert_eq!(return_type.as_ref().unwrap().name, "Int32");

            assert_eq!(body.statements.len(), 2);
            if let Statement::LetBinding {
                mutable,
                name,
                type_annotation,
                initializer,
            } = &body.statements[0]
            {
                assert!(!mutable);
                assert_eq!(name.name, "accumulator");
                assert_eq!(type_annotation.as_ref().unwrap().name, "Int32");
                assert_eq!(initializer, &Expression::Literal(Literal::Int(0)));
            } else {
                panic!("expected let binding as first statement");
            }

            if let Statement::Return(Some(expr)) = &body.statements[1] {
                assert_eq!(
                    expr,
                    &Expression::Identifier(Identifier {
                        name: "accumulator".to_string()
                    })
                );
            } else {
                panic!("expected return statement as second statement");
            }
        } else {
            panic!("expected function declaration");
        }
    }

    #[test]
    fn test_parse_use_declarations_from_spec_examples() {
        // Based on the "Use Declaration" examples in spec.md §5.
        let source = "use std::collections::Map; use myapp::models::*;";
        let module = parse_source(source);
        assert_eq!(module.declarations.len(), 2);

        if let Declaration::Use(path) = &module.declarations[0] {
            assert_eq!(
                path.segments,
                vec![
                    "std".to_string(),
                    "collections".to_string(),
                    "Map".to_string()
                ]
            );
            assert!(!path.wildcard);
        } else {
            panic!("expected first declaration to be a use path");
        }

        if let Declaration::Use(path) = &module.declarations[1] {
            assert_eq!(
                path.segments,
                vec!["myapp".to_string(), "models".to_string()]
            );
            assert!(path.wildcard);
        } else {
            panic!("expected second declaration to be a wildcard use");
        }
    }

    #[test]
    fn test_parse_enum_variants_from_spec() {
        // Mirrors the enum examples documented in spec.md §5.
        let source = r#"
enum Command {
  Quit,
  Move { x: Int32, y: Int32 },
  Write(Str),
}
"#;

        let module = parse_source(source);
        assert_eq!(module.declarations.len(), 1);

        if let Declaration::Enum { name, variants, .. } = &module.declarations[0] {
            assert_eq!(name.name, "Command");
            assert_eq!(variants.len(), 3);

            assert_eq!(variants[0].name.name, "Quit");
            assert!(variants[0].payload.is_none());

            assert_eq!(variants[1].name.name, "Move");
            if let Some(EnumVariantPayload::Struct(fields)) = &variants[1].payload {
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[0].0.name, "x");
                assert_eq!(fields[0].1.name, "Int32");
                assert_eq!(fields[1].0.name, "y");
                assert_eq!(fields[1].1.name, "Int32");
            } else {
                panic!("expected struct payload on Move variant");
            }

            assert_eq!(variants[2].name.name, "Write");
            if let Some(EnumVariantPayload::Tuple(types)) = &variants[2].payload {
                assert_eq!(types.len(), 1);
                assert_eq!(types[0].name, "Str");
            } else {
                panic!("expected tuple payload on Write variant");
            }
        } else {
            panic!("expected enum declaration");
        }
    }
}
