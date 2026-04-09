use crate::ast::*;
use crate::span::Span;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
struct LiveBinding {
    consumed: bool,
    origin: String,
    span: Span,
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
                    span: name.span,
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
                Err(format!(
                    "Binding '{}' already consumed ({}) [span {}]",
                    name,
                    context,
                    format_span(binding.span)
                ))
            } else {
                binding.consumed = true;
                Ok(())
            }
        } else {
            Err(format!("Binding '{}' not found for {}", name, context))
        }
    }

    fn unconsumed(&self) -> Vec<(String, String, Span)> {
        self.bindings
            .iter()
            .filter_map(|(name, binding)| {
                if !binding.consumed {
                    Some((name.clone(), binding.origin.clone(), binding.span))
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
            if let DeclarationKind::Function { params, body, .. } = &decl.node {
                self.push();
                for param in params {
                    let shared = is_ref_type(&param.type_annotation);
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
            "Binding '{}' not found while {} (span {})",
            name.name,
            context,
            format_span(name.span)
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
        self.analyze_statement(stmt)
    }

    fn analyze_statement(&mut self, stmt: &Statement) -> Result<(), String> {
        match &stmt.node {
            StatementKind::LetBinding {
                name,
                initializer,
                mutable,
                type_annotation,
                ..
            } => {
                // Initializer expressions consume any bindings they reference (move semantics for let-initializers)
                self.consume_identifiers_in_expression(initializer)?;
                let shared = type_annotation
                    .as_ref()
                    .map(|ty| is_ref_type(ty))
                    .unwrap_or(false);
                self.insert_binding(name, "let binding", *mutable, shared)
            }
            StatementKind::Expression(expr) => self.consume_identifiers_in_expression(expr),
            StatementKind::Return(Some(expr)) => self.consume_identifiers_in_expression(expr),
            StatementKind::Return(None) => Ok(()),
            StatementKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.analyze_expression(condition)?;
                let base = self.stack.clone();
                let then_stack = self.run_branch_block(then_branch, base.clone())?;
                let else_stack = if let Some(eb) = else_branch {
                    match &eb.node {
                        ElseBranchKind::Block(block) => {
                            self.run_branch_block(block, base.clone())?
                        }
                        ElseBranchKind::If(stmt) => self.run_branch_stmt(stmt, base.clone())?,
                    }
                } else {
                    base.clone()
                };
                self.ensure_branch_consistency(&base, &then_stack, &else_stack, "if")?;
                self.merge_branch_result(&then_stack);
                Ok(())
            }
            StatementKind::Conc { body } => {
                let base = self.stack.clone();
                let conc_stack = self.run_branch_block(body, base)?;
                self.merge_branch_result(&conc_stack);
                Ok(())
            }
            StatementKind::Loop { kind, body } => {
                let base = self.stack.clone();
                match &kind.node {
                    LoopKindKind::For {
                        pattern, iterator, ..
                    } => {
                        self.analyze_expression(iterator)?;

                        // Pass 1: validate body with loop var in scope (unchanged)
                        self.push();
                        if let PatternKind::Identifier(id) = &pattern.node {
                            self.insert_binding(id, "for loop", false, false)?;
                        }
                        self.analyze_block_no_drops(body)?;
                        self.pop();

                        // Pass 2 (branch-consistency): seed the stack with `x` so the body can resolve it
                        let mut for_base = base.clone();
                        let mut for_scope = LiveSet::new();
                        if let PatternKind::Identifier(id) = &pattern.node {
                            // ignore duplicate-binding error; this is a fresh scope
                            let _ = for_scope.insert(id, "for loop", false, false);
                        }
                        for_base.push(for_scope);

                        let mut loop_stack = self.run_branch_block(body, for_base)?;

                        // Strip the for-scope level we added — it isn't part of `base`
                        loop_stack.pop();

                        self.ensure_branch_consistency(&base, &loop_stack, &base, "loop")?;
                        self.merge_branch_result(&loop_stack);
                        return Ok(()); // skip the fallthrough pass on lines 281-283
                    }
                    LoopKindKind::While { condition, .. } => {
                        self.analyze_expression(condition)?;
                    }
                    _ => {}
                }
                let loop_stack = self.run_branch_block(body, base.clone())?;
                self.ensure_branch_consistency(&base, &loop_stack, &base, "loop")?;
                self.merge_branch_result(&loop_stack);
                Ok(())
            }
            StatementKind::Match { expr, arms } => {
                self.analyze_expression(expr)?;
                let base = self.stack.clone();
                let mut branch_results = Vec::new();
                for arm in arms {
                    let stack = self.run_branch_expr(&arm.node.body, base.clone())?;
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
            _ => Err("Unsupported statement type".to_string()),
        }
    }

    fn analyze_expression(&mut self, expr: &Expression) -> Result<(), String> {
        match &expr.node {
            ExpressionKind::Identifier(_id) => Ok(()),
            ExpressionKind::Block(block) => self.analyze_block(block),
            ExpressionKind::StructInit { fields, .. } => {
                for (_, expr) in fields {
                    self.analyze_expression(expr)?;
                }
                Ok(())
            }
            ExpressionKind::Call { func, args } => {
                // If the callee is a plain identifier, only consume it as a live binding
                // if it actually exists in the live set (i.e. it's a closure / callable
                // stored in a local variable). Top-level function names are never inserted
                // into the live set, so we skip them silently rather than erroring.
                if let ExpressionKind::Identifier(id) = &func.node {
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
            ExpressionKind::BinaryOp { left, right, .. } => {
                self.analyze_expression(left)?;
                self.analyze_expression(right)?;
                Ok(())
            }
            ExpressionKind::UnaryOp { expr, .. } => self.analyze_expression(expr),
            ExpressionKind::FieldAccess { base, .. } => self.analyze_expression(base),
            ExpressionKind::IndexAccess { base, index } => {
                self.analyze_expression(base)?;
                self.analyze_expression(index)?;
                Ok(())
            }
            ExpressionKind::MergeExpression { base, fields } => {
                if let Some(base_expr) = base {
                    self.analyze_expression(base_expr)?;
                }
                for (_, expr) in fields {
                    self.analyze_expression(expr)?;
                }
                Ok(())
            }
            ExpressionKind::Match { expr, arms } => {
                self.analyze_expression(expr)?;
                for arm in arms {
                    self.analyze_expression(&arm.node.body)?;
                }
                Ok(())
            }
            ExpressionKind::Pipe { left, right } => {
                self.analyze_expression(left)?;
                self.analyze_expression(right)?;
                Ok(())
            }
            ExpressionKind::TryOperator { expr } => self.analyze_expression(expr),
            ExpressionKind::IfLet {
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
            ExpressionKind::Literal(_) => Ok(()),
            ExpressionKind::Placeholder(_) => Ok(()),
        }
    }

    fn consume_identifiers_in_expression(&mut self, expr: &Expression) -> Result<(), String> {
        match &expr.node {
            ExpressionKind::Identifier(id) => self.consume_identifier(id, "initializer"),
            ExpressionKind::Block(block) => self.analyze_block_no_drops(block),
            ExpressionKind::StructInit { fields, .. } => {
                for (_, expr) in fields {
                    self.consume_identifiers_in_expression(expr)?;
                }
                Ok(())
            }
            ExpressionKind::Call { func, args } => {
                if let ExpressionKind::Identifier(id) = &func.node {
                    let is_live_binding = self.stack.iter().any(|set| {
                        set.bindings.contains_key(&id.name)
                            || set.mutables.contains(&id.name)
                            || set.shared.contains(&id.name)
                    });
                    if is_live_binding {
                        self.consume_identifier(id, "call")?;
                    }
                } else {
                    self.consume_identifiers_in_expression(func)?;
                }
                for arg in args {
                    self.consume_identifiers_in_expression(arg)?;
                }
                Ok(())
            }
            ExpressionKind::BinaryOp { left, right, .. } => {
                self.consume_identifiers_in_expression(left)?;
                self.consume_identifiers_in_expression(right)?;
                Ok(())
            }
            ExpressionKind::UnaryOp { expr, .. } => self.consume_identifiers_in_expression(expr),
            ExpressionKind::FieldAccess { base, .. } => {
                self.consume_identifiers_in_expression(base)
            }
            ExpressionKind::IndexAccess { base, index } => {
                self.consume_identifiers_in_expression(base)?;
                self.consume_identifiers_in_expression(index)?;
                Ok(())
            }
            ExpressionKind::MergeExpression { base, fields } => {
                if let Some(base_expr) = base {
                    self.consume_identifiers_in_expression(base_expr)?;
                }
                for (_, expr) in fields {
                    self.consume_identifiers_in_expression(expr)?;
                }
                Ok(())
            }
            ExpressionKind::Match { expr, arms } => {
                self.consume_identifiers_in_expression(expr)?;
                for arm in arms {
                    self.consume_identifiers_in_expression(&arm.node.body)?;
                }
                Ok(())
            }
            ExpressionKind::Pipe { left, right } => {
                self.consume_identifiers_in_expression(left)?;
                self.consume_identifiers_in_expression(right)?;
                Ok(())
            }
            ExpressionKind::TryOperator { expr } => self.consume_identifiers_in_expression(expr),
            ExpressionKind::IfLet {
                expr,
                then,
                else_branch,
                ..
            } => {
                self.consume_identifiers_in_expression(expr)?;
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
            ExpressionKind::Literal(_) => Ok(()),
            ExpressionKind::Placeholder(_) => Ok(()),
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
        for (name, origin, span) in set.unconsumed() {
            self.drops.push(format!(
                "Drop '{}' (origin: {}; span: {})",
                name,
                origin,
                format_span(span)
            ));
        }
    }

    fn analyze_pattern(&mut self, _pattern: &Pattern) -> Result<(), String> {
        // Patterns in for-loop bindings introduce new variables into scope;
        // for now we accept all patterns without tracking their introduced names.
        Ok(())
    }
}

fn format_span(span: Span) -> String {
    format!("{}:{} ({}..{})", span.line, span.col, span.start, span.end)
}

fn is_ref_type(ty: &Type) -> bool {
    ty.node.name.eq("ref")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::span::Span;

    fn dummy_span() -> Span {
        Span::new(0, 0, 0, 0)
    }

    fn mk_ident(name: &str) -> Identifier {
        Identifier {
            name: name.into(),
            span: dummy_span(),
        }
    }

    fn mk_stmt(kind: StatementKind) -> Statement {
        Spanned::new_dummy(kind, dummy_span())
    }

    fn mk_expr(kind: ExpressionKind) -> Expression {
        Spanned::new_dummy(kind, dummy_span())
    }

    fn mk_type(name: &str) -> Type {
        Spanned::new_dummy(
            TypeKind {
                name: name.into(),
                generic_args: vec![],
            },
            dummy_span(),
        )
    }

    fn mk_param(name: &str, ty: &str) -> Parameter {
        Parameter {
            name: mk_ident(name),
            type_annotation: mk_type(ty),
            span: dummy_span(),
        }
    }

    fn mk_decl(kind: DeclarationKind) -> Declaration {
        Spanned::new_dummy(kind, dummy_span())
    }

    fn mk_else(kind: ElseBranchKind) -> ElseBranch {
        Spanned::new_dummy(kind, dummy_span())
    }

    fn mk_block(statements: Vec<Statement>) -> Block {
        Block {
            statements,
            trailing_expression: None,
            span: dummy_span(),
        }
    }

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
            declarations: vec![mk_decl(DeclarationKind::Function {
                name: mk_ident("cond"),
                generics: vec![],
                params: vec![mk_param("flag", "Bool")],
                return_type: Some(mk_type("Int32")),
                body: mk_block(vec![
                    mk_stmt(StatementKind::LetBinding {
                        mutable: false,
                        name: mk_ident("value"),
                        type_annotation: Some(mk_type("Int32")),
                        initializer: mk_expr(ExpressionKind::Literal(Literal {
                            kind: LiteralKind::Int(1, None),
                            span: dummy_span(),
                        })),
                    }),
                    mk_stmt(StatementKind::If {
                        condition: mk_expr(ExpressionKind::Identifier(mk_ident("flag"))),
                        then_branch: mk_block(vec![mk_stmt(StatementKind::Return(Some(mk_expr(
                            ExpressionKind::Identifier(mk_ident("value")),
                        ))))]),
                        else_branch: Some(mk_else(ElseBranchKind::Block(mk_block(vec![mk_stmt(
                            StatementKind::Return(Some(mk_expr(ExpressionKind::Literal(
                                Literal {
                                    kind: LiteralKind::Int(0, None),
                                    span: dummy_span(),
                                },
                            )))),
                        )])))),
                    }),
                ]),
            })],
            span: dummy_span(),
        };

        let mut analyzer = LiveAnalyzer::new();
        let err = analyzer.analyze_module(&module).unwrap_err();
        assert!(err
            .iter()
            .any(|msg| msg.contains("consumed inconsistently")));
    }

    #[test]
    fn loop_consumption_rejected() {
        let loop_body = mk_block(vec![mk_stmt(StatementKind::Return(Some(mk_expr(
            ExpressionKind::Identifier(mk_ident("token")),
        ))))]);
        let module = Module {
            name: None,
            declarations: vec![mk_decl(DeclarationKind::Function {
                name: mk_ident("looping"),
                generics: vec![],
                params: vec![mk_param("token", "Int32")],
                return_type: Some(mk_type("Int32")),
                body: Block {
                    statements: vec![mk_stmt(StatementKind::Loop {
                        kind: Spanned::new_dummy(
                            LoopKindKind::Block(loop_body.clone()),
                            dummy_span(),
                        ),
                        body: loop_body,
                    })],
                    trailing_expression: Some(Box::new(mk_expr(ExpressionKind::Literal(
                        Literal {
                            kind: LiteralKind::Int(0, None),
                            span: dummy_span(),
                        },
                    )))),
                    span: dummy_span(),
                },
            })],
            span: dummy_span(),
        };

        let mut analyzer = LiveAnalyzer::new();
        let err = analyzer.analyze_module(&module).unwrap_err();
        assert!(err.iter().any(|msg| msg.contains("Binding")));
    }

    #[test]
    fn conc_consumes_captured_bindings() {
        let lit_int = |n: i64| {
            mk_expr(ExpressionKind::Literal(Literal {
                kind: LiteralKind::Int(n, None),
                span: dummy_span(),
            }))
        };
        let module = Module {
            name: None,
            declarations: vec![mk_decl(DeclarationKind::Function {
                name: mk_ident("conc"),
                generics: vec![],
                params: vec![],
                return_type: Some(mk_type("Int32")),
                body: mk_block(vec![
                    mk_stmt(StatementKind::LetBinding {
                        mutable: false,
                        name: mk_ident("value"),
                        type_annotation: Some(mk_type("Int32")),
                        initializer: lit_int(1),
                    }),
                    mk_stmt(StatementKind::Conc {
                        body: mk_block(vec![mk_stmt(StatementKind::Expression(mk_expr(
                            ExpressionKind::Identifier(mk_ident("value")),
                        )))]),
                    }),
                    mk_stmt(StatementKind::Return(Some(mk_expr(
                        ExpressionKind::Identifier(mk_ident("value")),
                    )))),
                ]),
            })],
            span: dummy_span(),
        };

        let mut analyzer = LiveAnalyzer::new();
        let err = analyzer.analyze_module(&module).unwrap_err();
        assert!(err.iter().any(|msg| msg.contains("already consumed")));
    }

    #[test]
    fn let_mut_not_tracked() {
        let lit_int = |n: i64| {
            mk_expr(ExpressionKind::Literal(Literal {
                kind: LiteralKind::Int(n, None),
                span: dummy_span(),
            }))
        };
        let module = Module {
            name: None,
            declarations: vec![mk_decl(DeclarationKind::Function {
                name: mk_ident("mut_ok"),
                generics: vec![],
                params: vec![],
                return_type: Some(mk_type("Int32")),
                body: mk_block(vec![
                    mk_stmt(StatementKind::LetBinding {
                        mutable: true,
                        name: mk_ident("counter"),
                        type_annotation: Some(mk_type("Int32")),
                        initializer: lit_int(1),
                    }),
                    mk_stmt(StatementKind::Expression(mk_expr(
                        ExpressionKind::Identifier(mk_ident("counter")),
                    ))),
                    mk_stmt(StatementKind::Return(Some(mk_expr(
                        ExpressionKind::Identifier(mk_ident("counter")),
                    )))),
                ]),
            })],
            span: dummy_span(),
        };

        let mut analyzer = LiveAnalyzer::new();
        assert!(analyzer.analyze_module(&module).is_ok());
    }

    #[test]
    fn ref_binding_not_consumed() {
        // ref<Int32> parameter — needs a generic type arg
        let ref_type = Spanned::new_dummy(
            TypeKind {
                name: "ref".into(),
                generic_args: vec![mk_type("Int32")],
            },
            dummy_span(),
        );
        let param = Parameter {
            name: mk_ident("data"),
            type_annotation: ref_type,
            span: dummy_span(),
        };
        let module = Module {
            name: None,
            declarations: vec![mk_decl(DeclarationKind::Function {
                name: mk_ident("shared"),
                generics: vec![],
                params: vec![param],
                return_type: Some(mk_type("Int32")),
                body: mk_block(vec![
                    mk_stmt(StatementKind::Expression(mk_expr(
                        ExpressionKind::Identifier(mk_ident("data")),
                    ))),
                    mk_stmt(StatementKind::Return(Some(mk_expr(
                        ExpressionKind::Identifier(mk_ident("data")),
                    )))),
                ]),
            })],
            span: dummy_span(),
        };

        let mut analyzer = LiveAnalyzer::new();
        assert!(analyzer.analyze_module(&module).is_ok());
    }

    #[test]
    fn merge_on_ref_is_shared() {
        let lit_int = |n: i64| {
            mk_expr(ExpressionKind::Literal(Literal {
                kind: LiteralKind::Int(n, None),
                span: dummy_span(),
            }))
        };
        let ref_node_type = Spanned::new_dummy(
            TypeKind {
                name: "ref".into(),
                generic_args: vec![mk_type("Node")],
            },
            dummy_span(),
        );
        let module = Module {
            name: None,
            declarations: vec![mk_decl(DeclarationKind::Function {
                name: mk_ident("merge_ref"),
                generics: vec![],
                params: vec![],
                return_type: Some(mk_type("Int32")),
                body: mk_block(vec![
                    mk_stmt(StatementKind::LetBinding {
                        mutable: false,
                        name: mk_ident("node"),
                        type_annotation: Some(ref_node_type),
                        initializer: mk_expr(ExpressionKind::Placeholder("node".into())),
                    }),
                    mk_stmt(StatementKind::LetBinding {
                        mutable: false,
                        name: mk_ident("updated"),
                        type_annotation: None,
                        initializer: mk_expr(ExpressionKind::MergeExpression {
                            base: Some(Box::new(mk_expr(ExpressionKind::Identifier(mk_ident(
                                "node",
                            ))))),
                            fields: vec![(
                                mk_ident("child"),
                                mk_expr(ExpressionKind::Identifier(mk_ident("node"))),
                            )],
                        }),
                    }),
                    mk_stmt(StatementKind::Return(Some(lit_int(0)))),
                ]),
            })],
            span: dummy_span(),
        };

        let mut analyzer = LiveAnalyzer::new();
        assert!(analyzer.analyze_module(&module).is_ok());
    }
}
