use crate::ast::*;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
struct LiveBinding {
    consumed: bool,
    origin: String,
}

#[derive(Clone, Debug)]
struct LiveSet {
    bindings: HashMap<String, LiveBinding>,
    mutables: HashSet<String>,
    shared: HashSet<String>,
}

impl LiveSet {
    fn new() -> Self {
        Self {
            bindings: HashMap::new(),
            mutables: HashSet::new(),
            shared: HashSet::new(),
        }
    }

    fn insert(
        &mut self,
        name: &Identifier,
        origin: &str,
        mutable: bool,
        shared: bool,
    ) -> Result<(), String> {
        if self.bindings.contains_key(&name.name)
            || self.mutables.contains(&name.name)
            || self.shared.contains(&name.name)
        {
            Err(format!("Duplicate binding '{}' in this scope", name.name))
        } else if shared {
            self.shared.insert(name.name.clone());
            Ok(())
        } else if mutable {
            self.mutables.insert(name.name.clone());
            Ok(())
        } else {
            self.bindings.insert(
                name.name.clone(),
                LiveBinding {
                    consumed: false,
                    origin: origin.to_string(),
                },
            );
            Ok(())
        }
    }

    fn consume(&mut self, name: &str, context: &str) -> Result<(), String> {
        if self.mutables.contains(name) || self.shared.contains(name) {
            return Ok(());
        }
        if let Some(binding) = self.bindings.get_mut(name) {
            if binding.consumed {
                Err(format!("Binding '{}' already consumed ({})", name, context))
            } else {
                binding.consumed = true;
                Ok(())
            }
        } else {
            Err(format!("Binding '{}' not found for {}", name, context))
        }
    }

    fn unconsumed(&self) -> Vec<(String, String)> {
        self.bindings
            .iter()
            .filter_map(|(name, binding)| {
                if !binding.consumed {
                    Some((name.clone(), binding.origin.clone()))
                } else {
                    None
                }
            })
            .collect()
    }
}

pub struct LiveAnalyzer {
    stack: Vec<LiveSet>,
    errors: Vec<String>,
    drops: Vec<String>,
}

impl LiveAnalyzer {
    pub fn new() -> Self {
        Self {
            stack: Vec::new(),
            errors: Vec::new(),
            drops: Vec::new(),
        }
    }

    pub fn analyze_module(&mut self, module: &Module) -> Result<(), Vec<String>> {
        for decl in &module.declarations {
            if let Declaration::Function { params, body, .. } = decl {
                self.push();
                for param in params {
                    let shared = param.type_annotation.name == "ref";
                    if let Err(err) = self.insert_binding(&param.name, "parameter", false, shared) {
                        self.errors.push(err);
                        break;
                    }
                }
                if let Err(err) = self.analyze_block(body) {
                    self.errors.push(err);
                }
                let set = self.pop();
                self.record_drops(&set);
            }
        }
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(self.errors.clone())
        }
    }

    pub fn drops(&self) -> &[String] {
        &self.drops
    }

    fn push(&mut self) {
        self.stack.push(LiveSet::new());
    }

    fn pop(&mut self) -> LiveSet {
        self.stack.pop().expect("live set stack underflow")
    }

    fn current(&mut self) -> &mut LiveSet {
        self.stack
            .last_mut()
            .expect("live analyzer must have an active set")
    }

    fn insert_binding(
        &mut self,
        name: &Identifier,
        origin: &str,
        mutable: bool,
        shared: bool,
    ) -> Result<(), String> {
        self.current().insert(name, origin, mutable, shared)
    }

    fn consume_identifier(&mut self, name: &Identifier, context: &str) -> Result<(), String> {
        for set in self.stack.iter_mut().rev() {
            if set.bindings.contains_key(&name.name)
                || set.mutables.contains(&name.name)
                || set.shared.contains(&name.name)
            {
                return set.consume(&name.name, context);
            }
        }
        Err(format!(
            "Binding '{}' not found while {}",
            name.name, context
        ))
    }

    fn analyze_block(&mut self, block: &Block) -> Result<(), String> {
        self.push();
        let result = (|| {
            for stmt in &block.statements {
                self.analyze_statement(stmt)?;
            }
            if let Some(expr) = &block.trailing_expression {
                self.analyze_expression(expr)?;
            }
            Ok(())
        })();
        let set = self.pop();
        self.record_drops(&set);
        result
    }

    fn analyze_block_no_drops(&mut self, block: &Block) -> Result<(), String> {
        self.push();
        let result = (|| {
            for stmt in &block.statements {
                self.analyze_statement(stmt)?;
            }
            if let Some(expr) = &block.trailing_expression {
                self.analyze_expression(expr)?;
            }
            Ok(())
        })();
        let _ = self.pop();
        result
    }

    fn analyze_statement_no_drops(&mut self, stmt: &Statement) -> Result<(), String> {
        match stmt {
            Statement::If { .. } | Statement::Match { .. } | Statement::Loop { .. } => {
                self.analyze_statement(stmt)
            }
            _ => self.analyze_statement(stmt),
        }
    }

    fn analyze_statement(&mut self, stmt: &Statement) -> Result<(), String> {
        match stmt {
            Statement::LetBinding {
                name,
                initializer,
                mutable,
                type_annotation,
                ..
            } => {
                self.analyze_expression(initializer)?;
                let shared = type_annotation
                    .as_ref()
                    .map(|ty| ty.name == "ref")
                    .unwrap_or(false);
                self.insert_binding(name, "let binding", *mutable, shared)
            }
            Statement::Expression(expr) => self.analyze_expression(expr),
            Statement::Return(Some(expr)) => self.analyze_expression(expr),
            Statement::Return(None) => Ok(()),
            Statement::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.analyze_expression(condition)?;
                let base = self.stack.clone();
                let then_stack = self.run_branch_block(then_branch, base.clone())?;
                let else_stack = if let Some(else_stmt) = else_branch {
                    self.run_branch_stmt(else_stmt, base.clone())?
                } else {
                    base.clone()
                };
                self.ensure_branch_consistency(&base, &then_stack, &else_stack, "if")?;
                self.merge_branch_result(&then_stack);
                Ok(())
            }
            Statement::Conc { body } => {
                let base = self.stack.clone();
                let conc_stack = self.run_branch_block(body, base)?;
                self.merge_branch_result(&conc_stack);
                Ok(())
            }
            Statement::Match { arms, .. } => {
                let base = self.stack.clone();
                let mut branch_results = Vec::new();
                for arm in arms {
                    let stack = self.run_branch_expr(&arm.body, base.clone())?;
                    branch_results.push(stack);
                }
                if let Some(first) = branch_results.first() {
                    for (idx, branch) in branch_results.iter().enumerate().skip(1) {
                        self.ensure_branch_consistency(
                            &base,
                            first,
                            branch,
                            &format!("match arm {}", idx),
                        )?;
                    }
                    self.merge_branch_result(first);
                }
                Ok(())
            }
            Statement::Loop { body, .. } => {
                let base = self.stack.clone();
                let loop_stack = self.run_branch_block(body, base.clone())?;
                self.ensure_branch_consistency(&base, &loop_stack, &base, "loop")?;
                self.merge_branch_result(&base);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn analyze_expression(&mut self, expr: &Expression) -> Result<(), String> {
        match expr {
            Expression::Identifier(id) => self.consume_identifier(id, "expression"),
            Expression::Block(block) => self.analyze_block(block),
            Expression::StructInit { fields, .. } => {
                for (_, expr) in fields {
                    self.analyze_expression(expr)?;
                }
                Ok(())
            }
            Expression::Call { func, args } => {
                // If the callee is a plain identifier, only consume it as a live binding
                // if it actually exists in the live set (i.e. it's a closure / callable
                // stored in a local variable). Top-level function names are never inserted
                // into the live set, so we skip them silently rather than erroring.
                if let Expression::Identifier(id) = func.as_ref() {
                    let is_live_binding = self.stack.iter().any(|set| {
                        set.bindings.contains_key(&id.name)
                            || set.mutables.contains(&id.name)
                            || set.shared.contains(&id.name)
                    });
                    if is_live_binding {
                        self.consume_identifier(id, "call")?;
                    }
                    // else: top-level function name — nothing to consume
                } else {
                    // Complex callee (e.g. field access, closure expression) — recurse normally.
                    self.analyze_expression(func)?;
                }
                for arg in args {
                    self.analyze_expression(arg)?;
                }
                Ok(())
            }
            Expression::BinaryOp { left, right, .. } => {
                self.analyze_expression(left)?;
                self.analyze_expression(right)?;
                Ok(())
            }
            Expression::UnaryOp { expr, .. } => self.analyze_expression(expr),
            Expression::FieldAccess { base, .. } => self.analyze_expression(base),
            Expression::IndexAccess { base, index } => {
                self.analyze_expression(base)?;
                self.analyze_expression(index)?;
                Ok(())
            }
            Expression::MergeExpression { base, fields } => {
                if let Some(base_expr) = base {
                    self.analyze_expression(base_expr)?;
                }
                for (_, expr) in fields {
                    self.analyze_expression(expr)?;
                }
                Ok(())
            }
            Expression::Match { expr, arms } => {
                self.analyze_expression(expr)?;
                for arm in arms {
                    self.analyze_expression(&arm.body)?;
                }
                Ok(())
            }
            Expression::Pipe { left, right } => {
                self.analyze_expression(left)?;
                self.analyze_expression(right)?;
                Ok(())
            }
            Expression::TryOperator { expr } => self.analyze_expression(expr),
            Expression::IfLet {
                expr,
                then,
                else_branch,
                ..
            } => {
                self.analyze_expression(expr)?;
                let base = self.stack.clone();
                let then_stack = self.run_branch_block(then, base.clone())?;
                let else_stack = if let Some(else_expr) = else_branch {
                    self.run_branch_expr(else_expr, base.clone())?
                } else {
                    base.clone()
                };
                self.ensure_branch_consistency(&base, &then_stack, &else_stack, "if let")?;
                self.merge_branch_result(&then_stack);
                Ok(())
            }
            Expression::Literal(_) => Ok(()),
            Expression::Placeholder(_) => Ok(()),
        }
    }

    fn run_branch_block(&self, block: &Block, stack: Vec<LiveSet>) -> Result<Vec<LiveSet>, String> {
        let mut branch = LiveAnalyzer {
            stack,
            errors: Vec::new(),
            drops: Vec::new(),
        };
        branch.analyze_block_no_drops(block)?;
        if branch.errors.is_empty() {
            Ok(branch.stack)
        } else {
            Err(branch.errors.join("; "))
        }
    }

    fn run_branch_stmt(
        &self,
        stmt: &Statement,
        stack: Vec<LiveSet>,
    ) -> Result<Vec<LiveSet>, String> {
        let mut branch = LiveAnalyzer {
            stack,
            errors: Vec::new(),
            drops: Vec::new(),
        };
        branch.analyze_statement_no_drops(stmt)?;
        if branch.errors.is_empty() {
            Ok(branch.stack)
        } else {
            Err(branch.errors.join("; "))
        }
    }

    fn run_branch_expr(
        &self,
        expr: &Expression,
        stack: Vec<LiveSet>,
    ) -> Result<Vec<LiveSet>, String> {
        let mut branch = LiveAnalyzer {
            stack,
            errors: Vec::new(),
            drops: Vec::new(),
        };
        branch.analyze_expression(expr)?;
        if branch.errors.is_empty() {
            Ok(branch.stack)
        } else {
            Err(branch.errors.join("; "))
        }
    }

    fn ensure_branch_consistency(
        &self,
        base: &[LiveSet],
        then_stack: &[LiveSet],
        else_stack: &[LiveSet],
        context: &str,
    ) -> Result<(), String> {
        if base.len() != then_stack.len() || base.len() != else_stack.len() {
            return Err(format!("Live-set depth mismatch in {}", context));
        }
        for (depth, base_set) in base.iter().enumerate() {
            for (name, base_binding) in &base_set.bindings {
                let then_binding = then_stack[depth].bindings.get(name).unwrap_or(base_binding);
                let else_binding = else_stack[depth].bindings.get(name).unwrap_or(base_binding);
                if then_binding.consumed != else_binding.consumed {
                    return Err(format!(
                        "Binding '{}' consumed inconsistently across branches in {}",
                        name, context
                    ));
                }
            }
        }
        Ok(())
    }

    fn merge_branch_result(&mut self, merged: &[LiveSet]) {
        if merged.len() != self.stack.len() {
            return;
        }
        for (depth, set) in merged.iter().enumerate() {
            for (name, binding) in &set.bindings {
                if let Some(current) = self.stack[depth].bindings.get_mut(name) {
                    current.consumed = binding.consumed;
                }
            }
        }
    }

    fn record_drops(&mut self, set: &LiveSet) {
        for (name, origin) in set.unconsumed() {
            self.drops
                .push(format!("Drop '{}' (origin: {})", name, origin));
        }
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

    fn parse(source: &str) -> Module {
        let tokens = Lexer::new(normalize_source(source)).tokenize();
        Parser::new(tokens).parse_module().unwrap()
    }

    fn analyze(source: &str) -> Result<Vec<String>, Vec<String>> {
        let module = parse(source);
        let mut analyzer = LiveAnalyzer::new();
        match analyzer.analyze_module(&module) {
            Ok(()) => Ok(analyzer.drops().to_vec()),
            Err(err) => Err(err),
        }
    }

    #[test]
    fn drop_unused_parameter() {
        let drops =
            analyze("fn unused(count: Int32) -> Int32 { let zero: Int32 = 0; return zero; }")
                .unwrap();
        assert!(drops.iter().any(|msg| msg.contains("count")));
    }

    #[test]
    fn double_consumption_error() {
        let errors = analyze(
            "fn twice() -> Int32 { let value: Int32 = 0; let copy: Int32 = value; return value; }",
        )
        .unwrap_err();
        assert!(errors.iter().any(|msg| msg.contains("already consumed")));
    }

    #[test]
    fn drop_records_unconsumed_let() {
        let drops =
            analyze("fn leftover() -> Int32 { let temporary: Int32 = 42; return 0; }").unwrap();
        assert!(drops.iter().any(|msg| msg.contains("temporary")));
    }

    #[test]
    fn conditional_consumption_mismatch() {
        let module = Module {
            name: None,
            declarations: vec![Declaration::Function {
                name: Identifier {
                    name: "cond".into(),
                },
                generics: vec![],
                params: vec![Parameter {
                    name: Identifier {
                        name: "flag".into(),
                    },
                    type_annotation: Type {
                        name: "Bool".into(),
                        generic_args: vec![],
                    },
                }],
                return_type: Some(Type {
                    name: "Int32".into(),
                    generic_args: vec![],
                }),
                body: Block {
                    statements: vec![
                        Statement::LetBinding {
                            mutable: false,
                            name: Identifier {
                                name: "value".into(),
                            },
                            type_annotation: Some(Type {
                                name: "Int32".into(),
                                generic_args: vec![],
                            }),
                            initializer: Expression::Literal(Literal::Int(1)),
                        },
                        Statement::If {
                            condition: Expression::Identifier(Identifier {
                                name: "flag".into(),
                            }),
                            then_branch: Block {
                                statements: vec![Statement::Return(Some(Expression::Identifier(
                                    Identifier {
                                        name: "value".into(),
                                    },
                                )))],
                                trailing_expression: None,
                            },
                            else_branch: Some(Box::new(Statement::Return(Some(
                                Expression::Literal(Literal::Int(0)),
                            )))),
                        },
                    ],
                    trailing_expression: None,
                },
            }],
        };

        let mut analyzer = LiveAnalyzer::new();
        let err = analyzer.analyze_module(&module).unwrap_err();
        assert!(err
            .iter()
            .any(|msg| msg.contains("consumed inconsistently")));
    }

    #[test]
    fn loop_consumption_rejected() {
        let loop_body = Block {
            statements: vec![Statement::Return(Some(Expression::Identifier(
                Identifier {
                    name: "token".into(),
                },
            )))],
            trailing_expression: None,
        };
        let module = Module {
            name: None,
            declarations: vec![Declaration::Function {
                name: Identifier {
                    name: "looping".into(),
                },
                generics: vec![],
                params: vec![Parameter {
                    name: Identifier {
                        name: "token".into(),
                    },
                    type_annotation: Type {
                        name: "Int32".into(),
                        generic_args: vec![],
                    },
                }],
                return_type: Some(Type {
                    name: "Int32".into(),
                    generic_args: vec![],
                }),
                body: Block {
                    statements: vec![Statement::Loop {
                        kind: LoopKind::Block(loop_body.clone()),
                        body: loop_body,
                    }],
                    trailing_expression: Some(Box::new(Expression::Literal(Literal::Int(0)))),
                },
            }],
        };

        let mut analyzer = LiveAnalyzer::new();
        let err = analyzer.analyze_module(&module).unwrap_err();
        assert!(err.iter().any(|msg| msg.contains("Binding")));
    }

    #[test]
    fn conc_consumes_captured_bindings() {
        let module = Module {
            name: None,
            declarations: vec![Declaration::Function {
                name: Identifier {
                    name: "conc".into(),
                },
                generics: vec![],
                params: vec![],
                return_type: Some(Type {
                    name: "Int32".into(),
                    generic_args: vec![],
                }),
                body: Block {
                    statements: vec![
                        Statement::LetBinding {
                            mutable: false,
                            name: Identifier {
                                name: "value".into(),
                            },
                            type_annotation: Some(Type {
                                name: "Int32".into(),
                                generic_args: vec![],
                            }),
                            initializer: Expression::Literal(Literal::Int(1)),
                        },
                        Statement::Conc {
                            body: Block {
                                statements: vec![Statement::Expression(Expression::Identifier(
                                    Identifier {
                                        name: "value".into(),
                                    },
                                ))],
                                trailing_expression: None,
                            },
                        },
                        Statement::Return(Some(Expression::Identifier(Identifier {
                            name: "value".into(),
                        }))),
                    ],
                    trailing_expression: None,
                },
            }],
        };

        let mut analyzer = LiveAnalyzer::new();
        let err = analyzer.analyze_module(&module).unwrap_err();
        assert!(err.iter().any(|msg| msg.contains("already consumed")));
    }

    #[test]
    fn let_mut_not_tracked() {
        let module = Module {
            name: None,
            declarations: vec![Declaration::Function {
                name: Identifier {
                    name: "mut_ok".into(),
                },
                generics: vec![],
                params: vec![],
                return_type: Some(Type {
                    name: "Int32".into(),
                    generic_args: vec![],
                }),
                body: Block {
                    statements: vec![
                        Statement::LetBinding {
                            mutable: true,
                            name: Identifier {
                                name: "counter".into(),
                            },
                            type_annotation: Some(Type {
                                name: "Int32".into(),
                                generic_args: vec![],
                            }),
                            initializer: Expression::Literal(Literal::Int(1)),
                        },
                        Statement::Expression(Expression::Identifier(Identifier {
                            name: "counter".into(),
                        })),
                        Statement::Return(Some(Expression::Identifier(Identifier {
                            name: "counter".into(),
                        }))),
                    ],
                    trailing_expression: None,
                },
            }],
        };

        let mut analyzer = LiveAnalyzer::new();
        assert!(analyzer.analyze_module(&module).is_ok());
    }

    #[test]
    fn ref_binding_not_consumed() {
        let module = Module {
            name: None,
            declarations: vec![Declaration::Function {
                name: Identifier {
                    name: "shared".into(),
                },
                generics: vec![],
                params: vec![Parameter {
                    name: Identifier {
                        name: "data".into(),
                    },
                    type_annotation: Type {
                        name: "ref".into(),
                        generic_args: vec![Type {
                            name: "Int32".into(),
                            generic_args: vec![],
                        }],
                    },
                }],
                return_type: Some(Type {
                    name: "Int32".into(),
                    generic_args: vec![],
                }),
                body: Block {
                    statements: vec![
                        Statement::Expression(Expression::Identifier(Identifier {
                            name: "data".into(),
                        })),
                        Statement::Return(Some(Expression::Identifier(Identifier {
                            name: "data".into(),
                        }))),
                    ],
                    trailing_expression: None,
                },
            }],
        };

        let mut analyzer = LiveAnalyzer::new();
        assert!(analyzer.analyze_module(&module).is_ok());
    }

    #[test]
    fn merge_on_ref_is_shared() {
        let module = Module {
            name: None,
            declarations: vec![Declaration::Function {
                name: Identifier {
                    name: "merge_ref".into(),
                },
                generics: vec![],
                params: vec![],
                return_type: Some(Type {
                    name: "Int32".into(),
                    generic_args: vec![],
                }),
                body: Block {
                    statements: vec![
                        Statement::LetBinding {
                            mutable: false,
                            name: Identifier {
                                name: "node".into(),
                            },
                            type_annotation: Some(Type {
                                name: "ref".into(),
                                generic_args: vec![Type {
                                    name: "Node".into(),
                                    generic_args: vec![],
                                }],
                            }),
                            initializer: Expression::Placeholder("node".into()),
                        },
                        Statement::LetBinding {
                            mutable: false,
                            name: Identifier {
                                name: "updated".into(),
                            },
                            type_annotation: None,
                            initializer: Expression::MergeExpression {
                                base: Some(Box::new(Expression::Identifier(Identifier {
                                    name: "node".into(),
                                }))),
                                fields: vec![(
                                    Identifier {
                                        name: "child".into(),
                                    },
                                    Expression::Identifier(Identifier {
                                        name: "node".into(),
                                    }),
                                )],
                            },
                        },
                        Statement::Return(Some(Expression::Literal(Literal::Int(0)))),
                    ],
                    trailing_expression: None,
                },
            }],
        };

        let mut analyzer = LiveAnalyzer::new();
        assert!(analyzer.analyze_module(&module).is_ok());
    }
}
