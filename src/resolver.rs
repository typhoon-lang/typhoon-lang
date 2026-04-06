use crate::ast::*;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeclId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScopeId(pub usize);

#[derive(Debug)]
struct Scope {
    parent: Option<ScopeId>,
    symbols: HashMap<String, DeclId>,
}

pub struct Resolver {
    scopes: Vec<Scope>,
    decls: Vec<String>,
}

impl Resolver {
    pub fn new() -> Self {
        Resolver {
            scopes: Vec::new(),
            decls: Vec::new(),
        }
    }

    pub fn resolve_module(&mut self, module: &Module) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        let root = self.enter_scope(None);
        for decl in &module.declarations {
            if let Err(err) = self.declare_from_decl(root, decl) {
                errors.push(err);
            }
        }
        for decl in &module.declarations {
            if let Err(err) = self.resolve_declaration(root, decl) {
                errors.push(err);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    fn enter_scope(&mut self, parent: Option<ScopeId>) -> ScopeId {
        let id = ScopeId(self.scopes.len());
        self.scopes.push(Scope {
            parent,
            symbols: HashMap::new(),
        });
        id
    }

    fn declare(&mut self, scope: ScopeId, identifier: Identifier) -> Result<DeclId, String> {
        let symbols = &mut self.scopes[scope.0].symbols;
        if symbols.contains_key(&identifier.name) {
            Err(format!("Duplicate declaration of '{}'", identifier.name))
        } else {
            let decl_id = DeclId(self.decls.len());
            symbols.insert(identifier.name.clone(), decl_id);
            self.decls.push(identifier.name.clone());
            Ok(decl_id)
        }
    }

    fn resolve_declaration(
        &mut self,
        scope: ScopeId,
        declaration: &Declaration,
    ) -> Result<(), String> {
        match declaration {
            Declaration::Function { params, body, .. } => {
                let fn_scope = self.enter_scope(Some(scope));
                for param in params {
                    self.declare(fn_scope, param.name.clone())?;
                }
                self.resolve_block(fn_scope, body)
            }
            Declaration::Struct { .. } | Declaration::Enum { .. } | Declaration::Newtype { .. } => {
                Ok(())
            }
            Declaration::Use(path) => {
                if let Some(segment) = path.segments.last() {
                    self.declare(
                        scope,
                        Identifier {
                            name: segment.clone(),
                        },
                    )?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn declare_from_decl(
        &mut self,
        scope: ScopeId,
        declaration: &Declaration,
    ) -> Result<DeclId, String> {
        match declaration {
            Declaration::Function { name, .. }
            | Declaration::Struct { name, .. }
            | Declaration::Enum { name, .. }
            | Declaration::Newtype { name, .. } => self.declare(scope, name.clone()),
            Declaration::Use(path) => {
                if let Some(segment) = path.segments.last() {
                    self.declare(
                        scope,
                        Identifier {
                            name: segment.clone(),
                        },
                    )
                } else {
                    Ok(DeclId(0))
                }
            }
            _ => Ok(DeclId(0)),
        }
    }

    fn resolve_block(&mut self, scope: ScopeId, block: &Block) -> Result<(), String> {
        for stmt in &block.statements {
            self.resolve_statement(scope, stmt)?;
        }
        if let Some(expr) = &block.trailing_expression {
            self.resolve_expression(scope, expr)?;
        }
        Ok(())
    }

    fn resolve_statement(&mut self, scope: ScopeId, stmt: &Statement) -> Result<(), String> {
        match stmt {
            Statement::LetBinding {
                name, initializer, ..
            } => {
                self.resolve_expression(scope, initializer)?;
                self.declare(scope, name.clone()).map(|_| ())
            }
            Statement::Expression(expr) => self.resolve_expression(scope, expr),
            Statement::Return(Some(expr)) => self.resolve_expression(scope, expr),
            Statement::Return(None) => Ok(()),
            Statement::Conc { body } => {
                let conc_scope = self.enter_scope(Some(scope));
                self.resolve_block(conc_scope, body)
            }
            Statement::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.resolve_expression(scope, condition)?;
                let then_scope = self.enter_scope(Some(scope));
                self.resolve_block(then_scope, then_branch)?;
                if let Some(stmt) = else_branch {
                    self.resolve_statement(scope, stmt)?;
                }
                Ok(())
            }
            Statement::Loop { body, .. } => {
                let loop_scope = self.enter_scope(Some(scope));
                self.resolve_block(loop_scope, body)
            }
            _ => Ok(()),
        }
    }

    fn resolve_expression(&mut self, scope: ScopeId, expr: &Expression) -> Result<(), String> {
        match expr {
            Expression::Identifier(id) => {
                if self.lookup(scope, &id.name).is_none() {
                    Err(format!("Unresolved identifier '{}'", id.name))
                } else {
                    Ok(())
                }
            }
            Expression::Block(block) => {
                let block_scope = self.enter_scope(Some(scope));
                self.resolve_block(block_scope, block)
            }
            Expression::MergeExpression { base, fields } => {
                if let Some(base_expr) = base {
                    self.resolve_expression(scope, base_expr)?;
                }
                for (_, expr) in fields {
                    self.resolve_expression(scope, expr)?;
                }
                Ok(())
            }
            Expression::Call { func, args } => {
                // Resolve the callee — for plain function calls this is an Identifier,
                // but we go through resolve_expression so method/closure calls work too.
                // Note: top-level function names live in the root scope via declare_from_decl,
                // so they will resolve correctly here.
                self.resolve_expression(scope, func)?;
                for arg in args {
                    self.resolve_expression(scope, arg)?;
                }
                Ok(())
            }
            Expression::BinaryOp { left, right, .. } => {
                self.resolve_expression(scope, left)?;
                self.resolve_expression(scope, right)
            }
            Expression::UnaryOp { expr, .. } => self.resolve_expression(scope, expr),
            _ => Ok(()),
        }
    }

    fn lookup(&self, mut scope: ScopeId, name: &str) -> Option<DeclId> {
        loop {
            if let Some(decl_id) = self.scopes[scope.0].symbols.get(name) {
                return Some(*decl_id);
            }
            if let Some(parent) = self.scopes[scope.0].parent {
                scope = parent;
            } else {
                break;
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

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

    fn resolve(source: &str) -> Resolver {
        let module = Parser::new(Lexer::new(normalize_source(source)).tokenize())
            .parse_module()
            .unwrap();
        let mut resolver = Resolver::new();
        resolver.resolve_module(&module).unwrap();
        resolver
    }

    #[test]
    fn resolves_parameters_and_let_bindings() {
        let resolver = resolve(
            "fn compute(count: Int32) -> Int32 { let accumulator: Int32 = 0; return accumulator; }",
        );
        assert_eq!(resolver.decls.len(), 3);
    }

    #[test]
    fn errors_on_unknown_identifier() {
        let tokens =
            Lexer::new("namespace main\nfn main() -> Int32 { return missing; }".to_string())
                .tokenize();
        let module = Parser::new(tokens).parse_module().unwrap();
        let mut resolver = Resolver::new();
        let err = resolver.resolve_module(&module).unwrap_err();
        assert!(err
            .iter()
            .any(|msg| msg.contains("Unresolved identifier 'missing'")));
    }

    #[test]
    fn rejects_duplicate_declarations() {
        let tokens =
            Lexer::new("namespace main\nstruct Foo {} struct Foo {}".to_string()).tokenize();
        let module = Parser::new(tokens).parse_module().unwrap();
        let mut resolver = Resolver::new();
        let err = resolver.resolve_module(&module).unwrap_err();
        assert!(err.iter().any(|msg| msg.contains("Duplicate declaration")));
    }
}
