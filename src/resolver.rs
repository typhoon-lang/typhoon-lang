use crate::ast::*;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeclId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScopeId(pub usize);

#[derive(Debug, Clone)]
pub enum DeclInfo {
    Function,
    Struct {
        fields: HashMap<String, TypeKind>,
    },
    Enum {
        variants: HashMap<String, EnumVariantInfo>,
    },
    Newtype {
        aliased_type: TypeKind,
    },
    Use,
    Unresolved,
}

#[derive(Debug, Clone)]
pub struct EnumVariantInfo {
    pub name: String,
    pub payload: Option<EnumVariantPayloadKind>,
}

#[derive(Debug)]
struct Scope {
    parent: Option<ScopeId>,
    symbols: HashMap<String, DeclId>,
}

pub struct Resolver {
    scopes: Vec<Scope>,
    decls: HashMap<DeclId, DeclInfo>,
    next_decl_id: usize,
}

impl Resolver {
    pub fn new() -> Self {
        Resolver {
            scopes: Vec::new(),
            decls: HashMap::new(),
            next_decl_id: 0,
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
            let decl_id = DeclId(self.next_decl_id);
            self.next_decl_id += 1;
            symbols.insert(identifier.name.clone(), decl_id);
            self.decls.insert(decl_id, DeclInfo::Unresolved);
            Ok(decl_id)
        }
    }

    fn resolve_declaration(
        &mut self,
        scope: ScopeId,
        declaration: &Declaration,
    ) -> Result<(), String> {
        match &declaration.node {
            DeclarationKind::Function {
                generics,
                params,
                body,
                return_type,
                ..
            } => {
                let type_params = self.generic_type_params(generics);
                for param in params {
                    if let Err(err) = self.resolve_type(scope, &param.type_annotation, &type_params)
                    {
                        return Err(err);
                    }
                }
                if let Some(ret) = return_type {
                    if let Err(err) = self.resolve_type(scope, ret, &type_params) {
                        return Err(err);
                    }
                }

                let fn_scope = self.enter_scope(Some(scope));
                for param in params {
                    self.declare(fn_scope, param.name.clone())?;
                }
                self.resolve_block(fn_scope, body)
            }
            DeclarationKind::Struct {
                generics, fields, ..
            } => {
                let type_params = self.generic_type_params(generics);
                for (_name, ty) in fields {
                    self.resolve_type(scope, ty, &type_params)?;
                }
                Ok(())
            }
            DeclarationKind::Enum {
                generics, variants, ..
            } => {
                let type_params = self.generic_type_params(generics);
                for variant in variants {
                    if let Some(payload) = &variant.node.payload {
                        match &payload.node {
                            EnumVariantPayloadKind::Tuple(types) => {
                                for ty in types {
                                    self.resolve_type(scope, ty, &type_params)?;
                                }
                            }
                            EnumVariantPayloadKind::Struct(fields) => {
                                for (_id, ty) in fields {
                                    self.resolve_type(scope, ty, &type_params)?;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Ok(())
            }
            DeclarationKind::Newtype { type_alias, .. } => {
                self.resolve_type(scope, type_alias, &HashSet::new())
            }
            DeclarationKind::Use(_) => Ok(()),
            _ => Ok(()),
        }
    }

    fn internal_name(&self, name: &str) -> bool {
        let internal = [
            // flow control
            "break", "continue", // type
            "Ok", "Err", "Some", "None", "chan", // stdio
            "print", "println", "printf", "fprint", "fprintln", "fprintf", "sprint", "sprintln",
            "sprintf", "scan", "scanf", "fscan", "fscanf", "sscan", "sscanf",
        ];
        return internal.contains(&name);
    }

    fn resolve_type(
        &self,
        scope: ScopeId,
        ty: &crate::ast::Type,
        type_params: &HashSet<String>,
    ) -> Result<(), String> {
        for arg in &ty.node.generic_args {
            self.resolve_type(scope, arg, type_params)?;
        }

        let primitives = [
            "Int8", "Int16", "Int32", "Int64", "Float16", "Float32", "Float64", "Bool", "Str",
            "Char", "Byte",
        ];

        let name = &ty.node.name;
        if primitives.contains(&name.as_str()) {
            return Ok(());
        }

        if type_params.contains(name) {
            return Ok(());
        }

        let common_named = [
            "Option", "Result", "Buf", "Map", "Set", "Node", "Ref", "Array", "Chan", "chan", "ref",
            // Runtime-provided capability/resource types
            "Network", "Listener", "Socket",
        ];
        if common_named.contains(&name.as_str()) {
            return Ok(());
        }

        if let Some(decl_id) = self.lookup(scope, name) {
            if let Some(DeclInfo::Newtype { aliased_type }) = self.decls.get(&decl_id) {
                return self.resolve_type(
                    scope,
                    &Spanned::new_dummy(aliased_type.clone(), ty.span),
                    type_params,
                );
            }
            Ok(())
        } else {
            Err(format!(
                "Unknown type '{}', expected a struct/enum/newtype or builtin",
                name
            ))
        }
    }

    fn declare_from_decl(
        &mut self,
        scope: ScopeId,
        declaration: &Declaration,
    ) -> Result<DeclId, String> {
        match &declaration.node {
            DeclarationKind::Function { name, .. } => {
                let decl_id = self.declare(scope, name.clone())?;
                self.decls.insert(decl_id, DeclInfo::Function);
                Ok(decl_id)
            }
            DeclarationKind::Struct { name, fields, .. } => {
                let decl_id = self.declare(scope, name.clone())?;
                let mut field_map = HashMap::new();
                for (field_name_id, field_type) in fields {
                    field_map.insert(field_name_id.name.clone(), field_type.node.clone());
                }
                self.decls
                    .insert(decl_id, DeclInfo::Struct { fields: field_map });
                Ok(decl_id)
            }
            DeclarationKind::Enum { name, variants, .. } => {
                let decl_id = self.declare(scope, name.clone())?;
                let mut variant_map = HashMap::new();
                for variant in variants {
                    variant_map.insert(
                        variant.node.name.name.clone(),
                        EnumVariantInfo {
                            name: variant.node.name.name.clone(),
                            payload: variant.node.payload.clone().map(|p| p.node),
                        },
                    );
                }
                self.decls.insert(
                    decl_id,
                    DeclInfo::Enum {
                        variants: variant_map,
                    },
                );
                Ok(decl_id)
            }
            DeclarationKind::Newtype { name, type_alias } => {
                let decl_id = self.declare(scope, name.clone())?;
                self.decls.insert(
                    decl_id,
                    DeclInfo::Newtype {
                        aliased_type: type_alias.node.clone(),
                    },
                );
                Ok(decl_id)
            }
            DeclarationKind::Use(path) => {
                if let Some(segment) = path.node.segments.last() {
                    let decl_id = self.declare(
                        scope,
                        Identifier {
                            name: segment.clone(),
                            span: path.span,
                        },
                    )?;
                    self.decls.insert(decl_id, DeclInfo::Use);
                }
                Ok(DeclId(0))
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
        match &stmt.node {
            StatementKind::LetBinding {
                name,
                initializer,
                type_annotation,
                ..
            } => {
                self.declare(scope, name.clone())?;
                if let Some(ty) = type_annotation {
                    self.resolve_type(scope, ty, &HashSet::new())?;
                }
                self.resolve_expression(scope, initializer)?;
                Ok(())
            }
            StatementKind::Expression(expr) => self.resolve_expression(scope, expr),
            StatementKind::Return(Some(expr)) => self.resolve_expression(scope, expr),
            StatementKind::Return(None) => Ok(()),
            StatementKind::Conc { body } => {
                let conc_scope = self.enter_scope(Some(scope));
                self.resolve_block(conc_scope, body)
            }
            StatementKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.resolve_expression(scope, condition)?;
                let then_scope = self.enter_scope(Some(scope));
                self.resolve_block(then_scope, then_branch)?;
                if let Some(else_branch) = else_branch {
                    match &else_branch.node {
                        ElseBranchKind::Block(block) => {
                            let else_scope = self.enter_scope(Some(scope));
                            self.resolve_block(else_scope, block)?;
                        }
                        ElseBranchKind::If(if_stmt) => {
                            self.resolve_statement(scope, if_stmt)?;
                        }
                    }
                }
                Ok(())
            }
            StatementKind::Loop { kind, body } => match &kind.node {
                LoopKindKind::For {
                    pattern,
                    iterator,
                    body: _,
                } => {
                    self.resolve_expression(scope, iterator)?;
                    let loop_scope = self.enter_scope(Some(scope));
                    self.declare_pattern(loop_scope, pattern)?;
                    self.resolve_block(loop_scope, body)
                }
                LoopKindKind::While { condition, body: _ } => {
                    self.resolve_expression(scope, condition)?;
                    let loop_scope = self.enter_scope(Some(scope));
                    self.resolve_block(loop_scope, body)
                }
                LoopKindKind::Block(block) => {
                    let loop_scope = self.enter_scope(Some(scope));
                    self.resolve_block(loop_scope, block)
                }
            },
            _ => Ok(()),
        }
    }

    fn resolve_expression(&mut self, scope: ScopeId, expr: &Expression) -> Result<(), String> {
        match &expr.node {
            ExpressionKind::Identifier(id) => {
                // Allow built-in identifiers
                match self.internal_name(&id.name) {
                    true => Ok(()),
                    false => {
                        if self.lookup(scope, &id.name).is_none() {
                            Err(format!("Unresolved identifier '{}'", id.name))
                        } else {
                            Ok(())
                        }
                    }
                }
            }
            ExpressionKind::Block(block) => {
                let block_scope = self.enter_scope(Some(scope));
                self.resolve_block(block_scope, block)
            }
            ExpressionKind::MergeExpression { base, fields } => {
                if let Some(base_expr) = base {
                    self.resolve_expression(scope, base_expr)?;
                }
                for (_, expr) in fields {
                    self.resolve_expression(scope, expr)?;
                }
                Ok(())
            }
            ExpressionKind::Call { func, args } => {
                self.resolve_expression(scope, func)?;
                for arg in args {
                    self.resolve_expression(scope, arg)?;
                }
                Ok(())
            }
            ExpressionKind::BinaryOp { left, right, .. } => {
                self.resolve_expression(scope, left)?;
                self.resolve_expression(scope, right)
            }
            ExpressionKind::UnaryOp { expr, .. } => self.resolve_expression(scope, expr),
            ExpressionKind::FieldAccess { base, field: _ } => {
                self.resolve_expression(scope, base)?;
                Ok(())
            }
            ExpressionKind::Literal(lit) => match &lit.kind {
                LiteralKind::Array(elems) => {
                    for elem in elems {
                        self.resolve_expression(scope, elem)?;
                    }
                    Ok(())
                }
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }

    fn generic_type_params(&self, generics: &[GenericParam]) -> HashSet<String> {
        generics
            .iter()
            .map(|param| param.node.name.name.clone())
            .collect()
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

    fn declare_pattern(&mut self, scope: ScopeId, pattern: &Pattern) -> Result<(), String> {
        match &pattern.node {
            PatternKind::Wildcard => Ok(()),
            PatternKind::Identifier(id) => self.declare(scope, id.clone()).map(|_| ()),
            PatternKind::Tuple(elems) | PatternKind::Array(elems) => {
                for p in elems {
                    self.declare_pattern(scope, p)?;
                }
                Ok(())
            }
            PatternKind::Literal(_) => Ok(()),
            PatternKind::Struct { fields, .. } => {
                for (_id, p) in fields {
                    self.declare_pattern(scope, p)?;
                }
                Ok(())
            }
            PatternKind::Or(_a, _b) => Ok(()),
            PatternKind::Guard { pattern: p, .. } => self.declare_pattern(scope, p),
            PatternKind::EnumVariant {
                payload: Some(p), ..
            } => self.declare_pattern(scope, p),
            PatternKind::EnumVariant { payload: None, .. } => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn resolve(source: &str) -> Resolver {
        let module = Parser::new(Lexer::new(source.trim().to_string()).tokenize())
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
        // compute, count, and accumulator.
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
        // Use normalize_source so parse_module gets the required `fn main`.
        // The duplicate Foo structs should still cause a resolver error.
        let tokens = Lexer::new(
            "namespace main\nstruct Foo {} struct Foo {}\nfn main() -> Int32 { return 0; }"
                .to_string(),
        )
        .tokenize();
        let module = Parser::new(tokens).parse_module().unwrap();
        let mut resolver = Resolver::new();
        let err = resolver.resolve_module(&module).unwrap_err();
        assert!(err.iter().any(|msg| msg.contains("Duplicate declaration")));
    }

    #[test]
    fn resolves_generic_type_params_in_function_signatures() {
        let resolver = resolve("fn id<T>(x: T) -> T { return x; }");
        assert!(resolver.decls.len() >= 2);
    }
}
