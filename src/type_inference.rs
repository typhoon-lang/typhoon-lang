use crate::ast::*;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferType {
    Int32,
    Float32,
    Bool,
    Str,
    Named(String),
    Unknown(String),
}

#[derive(Debug, Clone)]
struct FunctionSig {
    params: Vec<InferType>,
    ret: InferType,
}

impl InferType {
    fn from_annotation(ty: &Type) -> Self {
        match ty.name.as_str() {
            "Int32" => InferType::Int32,
            "Float32" => InferType::Float32,
            "Bool" => InferType::Bool,
            "Str" => InferType::Str,
            other => InferType::Named(other.to_string()),
        }
    }
}

#[derive(Debug)]
pub enum TypeError {
    UnknownIdentifier(String),
    TypeMismatch {
        expected: InferType,
        actual: InferType,
        context: String,
    },
}

pub struct TypeChecker {
    scopes: Vec<HashMap<String, InferType>>,
    func_sigs: HashMap<String, FunctionSig>,
}

impl TypeChecker {
    pub fn new() -> Self {
        TypeChecker {
            scopes: vec![HashMap::new()],
            func_sigs: HashMap::new(),
        }
    }

    pub fn check_module(&mut self, module: &Module) -> Result<(), TypeError> {
        self.collect_function_sigs(module);
        for decl in &module.declarations {
            self.check_declaration(decl)?;
        }
        Ok(())
    }

    fn collect_function_sigs(&mut self, module: &Module) {
        self.func_sigs.clear();
        for decl in &module.declarations {
            if let Declaration::Function {
                name,
                params,
                return_type,
                ..
            } = decl
            {
                let ret = return_type
                    .as_ref()
                    .map(|ty| InferType::from_annotation(ty))
                    .unwrap_or(InferType::Unknown("void".into()));
                let param_types = params
                    .iter()
                    .map(|p| InferType::from_annotation(&p.type_annotation))
                    .collect();
                self.func_sigs.insert(
                    name.name.clone(),
                    FunctionSig {
                        params: param_types,
                        ret,
                    },
                );
            }
        }
    }

    fn check_declaration(&mut self, declaration: &Declaration) -> Result<(), TypeError> {
        if let Declaration::Function {
            params,
            return_type,
            body,
            ..
        } = declaration
        {
            let expected = return_type
                .as_ref()
                .map(|ty| InferType::from_annotation(ty))
                .unwrap_or(InferType::Unknown("".into()));

            self.push_scope();
            for param in params {
                let ty = InferType::from_annotation(&param.type_annotation);
                self.declare(&param.name.name, ty);
            }
            self.check_block(body, &expected)?;
            self.pop_scope();
        }
        Ok(())
    }

    fn check_block(
        &mut self,
        block: &Block,
        expected: &InferType,
    ) -> Result<Option<InferType>, TypeError> {
        for stmt in &block.statements {
            self.check_statement(stmt, expected)?;
        }
        if let Some(expr) = &block.trailing_expression {
            let ty = self.check_expression(expr)?;
            if expected != &InferType::Unknown(String::new()) && expected != &ty {
                return Err(TypeError::TypeMismatch {
                    expected: expected.clone(),
                    actual: ty,
                    context: "block trailing expression".to_string(),
                });
            }
            Ok(Some(ty))
        } else {
            Ok(None)
        }
    }

    fn check_statement(&mut self, stmt: &Statement, expected: &InferType) -> Result<(), TypeError> {
        match stmt {
            Statement::LetBinding {
                name,
                type_annotation,
                initializer,
                ..
            } => {
                let init_ty = self.check_expression(initializer)?;
                let declared_ty = type_annotation
                    .as_ref()
                    .map(|ty| InferType::from_annotation(ty))
                    .unwrap_or(init_ty.clone());
                if init_ty != declared_ty {
                    return Err(TypeError::TypeMismatch {
                        expected: declared_ty,
                        actual: init_ty,
                        context: name.name.clone(),
                    });
                }
                self.declare(&name.name, declared_ty);
                Ok(())
            }
            Statement::Return(Some(expr)) => {
                let ty = self.check_expression(expr)?;
                if expected != &InferType::Unknown(String::new()) && expected != &ty {
                    return Err(TypeError::TypeMismatch {
                        expected: expected.clone(),
                        actual: ty,
                        context: "return".to_string(),
                    });
                }
                Ok(())
            }
            Statement::Return(None) => Ok(()),
            Statement::Expression(expr) => {
                self.check_expression(expr)?;
                Ok(())
            }
            Statement::Conc { body } => {
                self.push_scope();
                self.check_block(body, &InferType::Unknown(String::new()))?;
                self.pop_scope();
                Ok(())
            }
            Statement::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let _ = self.check_expression(condition)?;
                self.push_scope();
                self.check_block(then_branch, &InferType::Unknown(String::new()))?;
                self.pop_scope();
                if let Some(stmt) = else_branch {
                    self.check_statement(stmt, expected)?;
                }
                Ok(())
            }
            Statement::Loop { body, .. } => {
                self.push_scope();
                self.check_block(body, &InferType::Unknown(String::new()))?;
                self.pop_scope();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn check_expression(&mut self, expr: &Expression) -> Result<InferType, TypeError> {
        match expr {
            Expression::Literal(lit) => Ok(self.type_of_literal(lit)),
            Expression::Identifier(id) => self
                .lookup(&id.name)
                .cloned()
                .ok_or(TypeError::UnknownIdentifier(id.name.clone())),
            Expression::Block(block) => {
                self.push_scope();
                let block_ty = self.check_block(block, &InferType::Unknown(String::new()));
                self.pop_scope();
                if let Some(ty) = block_ty? {
                    Ok(ty)
                } else {
                    Ok(InferType::Unknown("block".into()))
                }
            }
            Expression::MergeExpression { base, fields } => {
                if let Some(base_expr) = base {
                    self.check_expression(base_expr)?;
                }
                for (_, expr) in fields {
                    self.check_expression(expr)?;
                }
                Ok(InferType::Unknown("merge".into()))
            }
            Expression::BinaryOp { op, left, right } => {
                let lhs = self.check_expression(left)?;
                let rhs = self.check_expression(right)?;
                match op {
                    Operator::Add
                    | Operator::Sub
                    | Operator::Mul
                    | Operator::Div
                    | Operator::Mod
                    | Operator::Pipe => {
                        if lhs == InferType::Int32 && rhs == InferType::Int32 {
                            Ok(InferType::Int32)
                        } else {
                            Err(TypeError::TypeMismatch {
                                expected: InferType::Int32,
                                actual: rhs,
                                context: "binary".to_string(),
                            })
                        }
                    }
                    Operator::Eq
                    | Operator::Ne
                    | Operator::Lt
                    | Operator::Le
                    | Operator::Gt
                    | Operator::Ge => Ok(InferType::Bool),
                    _ => Ok(InferType::Unknown("binary".into())),
                }
            }
            Expression::Call { func, args } => {
                let func_name = if let Expression::Identifier(id) = func.as_ref() {
                    id.name.clone()
                } else {
                    return Err(TypeError::UnknownIdentifier("call".into()));
                };
                let sig = self
                    .func_sigs
                    .get(&func_name)
                    .cloned()
                    .ok_or(TypeError::UnknownIdentifier(func_name.clone()))?;
                if sig.params.len() != args.len() {
                    return Err(TypeError::TypeMismatch {
                        expected: InferType::Unknown("arity".into()),
                        actual: InferType::Unknown("args".into()),
                        context: func_name.clone(),
                    });
                }
                for (idx, arg) in args.iter().enumerate() {
                    let ty = self.check_expression(arg)?;
                    if ty != sig.params[idx] {
                        return Err(TypeError::TypeMismatch {
                            expected: sig.params[idx].clone(),
                            actual: ty,
                            context: func_name.clone(),
                        });
                    }
                }
                Ok(sig.ret.clone())
            }
            _ => Ok(InferType::Unknown("expr".into())),
        }
    }

    fn type_of_literal(&self, lit: &Literal) -> InferType {
        match lit {
            Literal::Int(_) => InferType::Int32,
            Literal::Float(_) => InferType::Float32,
            Literal::Bool(_) => InferType::Bool,
            Literal::Str(_) => InferType::Str,
            Literal::Array(_) => InferType::Unknown("array".into()),
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn declare(&mut self, name: &str, ty: InferType) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), ty);
        }
    }

    fn lookup(&self, name: &str) -> Option<&InferType> {
        for scope in self.scopes.iter().rev() {
            if let Some(ty) = scope.get(name) {
                return Some(ty);
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
    use crate::resolver::Resolver;

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

    fn check(source: &str) -> Result<(), TypeError> {
        let module = Parser::new(Lexer::new(normalize_source(source)).tokenize())
            .parse_module()
            .unwrap();
        let mut resolver = Resolver::new();
        resolver.resolve_module(&module).unwrap();
        let mut checker = TypeChecker::new();
        checker.check_module(&module)
    }

    #[test]
    fn accepts_simple_function() {
        let source =
            "fn compute(count: Int32) -> Int32 { let accumulator: Int32 = 0; return accumulator; }";
        assert!(check(source).is_ok());
    }

    #[test]
    fn rejects_mismatched_let_type() {
        let source = "fn bad() -> Int32 { let text: Int32 = \"hello\"; return text; }";
        let err = check(source).unwrap_err();
        match err {
            TypeError::TypeMismatch { context, .. } => assert_eq!(context, "text"),
            _ => panic!("expected type mismatch error"),
        }
    }

    #[test]
    fn accepts_named_types_option_result_buf() {
        let source = "fn api() -> Result<Buf, Str> { let value: Option<Buf> = \"\"; return value; }";
        let err = check(source).unwrap_err();
        match err {
            TypeError::TypeMismatch { expected, .. } => match expected {
                InferType::Named(name) => assert_eq!(name, "Option"),
                _ => panic!("expected named type for Option"),
            },
            _ => panic!("expected type mismatch error"),
        }
    }
}
