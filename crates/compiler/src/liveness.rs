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

#[derive(Clone, Debug)]
pub struct DropInfo {
    pub name: String,

    pub is_heap: bool, // Track if it was allocated in a slab
}

pub struct LiveAnalyzer {
    stack: Vec<LiveSet>,
    errors: Vec<String>,
    structured_drops: HashMap<NodeId, Vec<DropInfo>>,
}

impl LiveAnalyzer {
    pub fn new() -> Self {
        Self {
            stack: Vec::new(),
            errors: Vec::new(),
            structured_drops: HashMap::new(),
        }
    }

    pub fn analyze_module(
        &mut self,
        module: &Module,
    ) -> Result<&HashMap<NodeId, Vec<DropInfo>>, Vec<String>> {
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
                // In analyze_module — remove the redundant record_drops call:
                if let Err(err) = self.analyze_block(body) {
                    self.errors.push(err);
                }
                let set = self.pop();
                // record param-scope drops under a different id
                self.record_drops(decl.id, &set);
            }
        }
        if self.errors.is_empty() {
            Ok(&self.structured_drops)
        } else {
            Err(self.errors.clone())
        }
    }

    /// Flat list of all drop names across all scopes (used by tests).
    pub fn drops(&self) -> Vec<String> {
        self.structured_drops
            .values()
            .flat_map(|v| v.iter().map(|d| d.name.clone()))
            .collect()
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
        self.record_drops(block.block_id, &set); // block.id from Spanned wrapper
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
                pattern,
                initializer,
                mutable,
                type_annotation,
                else_block,
            } => {
                // Initializer expressions consume any bindings they reference (move semantics for let-initializers)
                self.consume_identifiers_in_expression(initializer)?;
                let shared = type_annotation
                    .as_ref()
                    .map(|ty| is_ref_type(ty))
                    .unwrap_or(false);
                // insert_pattern_bindings walks the full pattern tree via
                // collect_pattern_binders, so Ok(x), Some(i), tuples, etc. all work.
                // For mutable or shared bindings we still need the single-name fast path
                // because insert_pattern_bindings always inserts as immutable/non-shared.
                if *mutable || shared {
                    if let Some(name) = pattern.get_identifier() {
                        self.insert_binding(name, "let binding", *mutable, shared)?;
                    }
                } else {
                    self.insert_pattern_bindings(pattern, "let binding")?;
                }
                // The else block runs when the pattern doesn't match — analyze it in a
                // fresh scope so it can't see the bindings introduced by the pattern.
                if let Some(else_blk) = else_block {
                    self.analyze_block(else_blk)?;
                }
                Ok(())
            }
            StatementKind::Expression(expr) => self.consume_identifiers_in_expression(expr),
            StatementKind::Return(Some(expr)) => self.consume_identifiers_in_expression(expr),
            StatementKind::Return(None) => Ok(()),
            StatementKind::Break | StatementKind::Continue => Ok(()),
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
                // Phase 1: find which outer bindings the conc body captures and consume them.
                let captures = self.collect_free_identifiers(body);
                for name in &captures {
                    // consume_identifier already skips mutables and shared (ref) bindings
                    for set in self.stack.iter_mut().rev() {
                        if set.bindings.contains_key(name)
                            || set.mutables.contains(name)
                            || set.shared.contains(name)
                        {
                            let _ = set.consume(name, "conc capture");
                            break;
                        }
                    }
                }

                // Phase 2: check the body in isolation with only the captured bindings in scope.
                let mut capture_set = LiveSet::new();
                for name in &captures {
                    // Re-insert captured bindings as shared — inside the conc body they are
                    // already "owned" by the closure; ref/non-ref distinction is resolved above.
                    capture_set.shared.insert(name.clone());
                }
                let isolated_stack = vec![capture_set];
                let mut body_analyzer = LiveAnalyzer {
                    stack: isolated_stack,
                    errors: Vec::new(),
                    structured_drops: HashMap::new(),
                };
                body_analyzer.analyze_block_no_drops(body)?;
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

                        self.ensure_loop_body_consistency(&base, &loop_stack, "loop")?;
                        self.merge_branch_result(&loop_stack);
                        return Ok(()); // skip the fallthrough pass on lines 281-283
                    }
                    LoopKindKind::While { condition, .. } => {
                        self.analyze_expression(condition)?;
                    }
                    _ => {}
                }
                let loop_stack = self.run_branch_block(body, base.clone())?;
                self.ensure_loop_body_consistency(&base, &loop_stack, "loop")?;
                self.merge_branch_result(&loop_stack);
                Ok(())
            }
            StatementKind::Match { expr, arms } => {
                self.analyze_expression(expr)?;
                let base = self.stack.clone();
                let mut branch_results = Vec::new();
                for arm in arms {
                    let stack = self.run_branch_match_arm(arm, base.clone())?;
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
            ExpressionKind::FieldAccess { base, .. } => {
                // Accessing a field or calling a method on a binding is a borrow, not a move.
                // Only recurse if the base itself is a complex expression that might contain
                // sub-expressions that need consuming (e.g. a call result).
                // A bare identifier as receiver must NOT be consumed.
                match &base.node {
                    ExpressionKind::Identifier(_) => Ok(()),
                    _ => self.analyze_expression(base),
                }
            }
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
                    // Match-arm patterns introduce new bindings that must be in-scope
                    // for the arm's guard and body.
                    self.push();
                    self.insert_pattern_bindings(&arm.node.pattern, "match binding")?;
                    if let Some(guard) = &arm.node.guard {
                        self.analyze_expression(guard)?;
                    }
                    self.analyze_expression(&arm.node.body)?;
                    self.pop();
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
                // Accessing a field or calling a method on a binding is a borrow, not a move.
                // Only recurse if the base itself is a complex expression that might contain
                // sub-expressions that need consuming (e.g. a call result).
                // A bare identifier as receiver must NOT be consumed.
                match &base.node {
                    ExpressionKind::Identifier(_) => Ok(()),
                    _ => self.consume_identifiers_in_expression(base),
                }
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
                    // Match-arm patterns introduce new bindings that must be in-scope
                    // for the arm's guard and body.
                    self.push();
                    self.insert_pattern_bindings(&arm.node.pattern, "match binding")?;
                    if let Some(guard) = &arm.node.guard {
                        self.consume_identifiers_in_expression(guard)?;
                    }
                    self.consume_identifiers_in_expression(&arm.node.body)?;
                    self.pop();
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
            structured_drops: HashMap::new(),
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
            structured_drops: HashMap::new(),
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
            structured_drops: HashMap::new(),
        };
        branch.analyze_expression(expr)?;
        if branch.errors.is_empty() {
            Ok(branch.stack)
        } else {
            Err(branch.errors.join("; "))
        }
    }

    fn collect_free_identifiers(&self, block: &Block) -> HashSet<String> {
        let mut free = HashSet::new();
        let mut bound = HashSet::new();
        self.collect_free_in_block(block, &mut bound, &mut free);
        // Remove names that aren't actually in the outer live set at all
        free.retain(|name| {
            self.stack.iter().any(|set| {
                set.bindings.contains_key(name)
                    || set.mutables.contains(name)
                    || set.shared.contains(name)
            })
        });
        free
    }

    fn collect_free_in_block(
        &self,
        block: &Block,
        bound: &mut HashSet<String>,
        free: &mut HashSet<String>,
    ) {
        for stmt in &block.statements {
            self.collect_free_in_stmt(stmt, bound, free);
        }
        if let Some(expr) = &block.trailing_expression {
            self.collect_free_in_expr(expr, bound, free);
        }
    }

    fn collect_free_in_stmt(
        &self,
        stmt: &Statement,
        bound: &mut HashSet<String>,
        free: &mut HashSet<String>,
    ) {
        match &stmt.node {
            StatementKind::LetBinding {
                pattern,
                initializer,
                ..
            } => {
                self.collect_free_in_expr(initializer, bound, free);
                if let Some(id) = pattern.get_identifier() {
                    bound.insert(id.name.clone());
                }
            }
            StatementKind::Expression(expr) | StatementKind::Return(Some(expr)) => {
                self.collect_free_in_expr(expr, bound, free);
            }
            StatementKind::Loop { body, .. } | StatementKind::Conc { body } => {
                self.collect_free_in_block(body, bound, free);
            }
            _ => {}
        }
    }

    fn collect_free_in_expr(
        &self,
        expr: &Expression,
        bound: &mut HashSet<String>,
        free: &mut HashSet<String>,
    ) {
        match &expr.node {
            ExpressionKind::Identifier(id) => {
                if !bound.contains(&id.name) {
                    free.insert(id.name.clone());
                }
            }
            ExpressionKind::BinaryOp { left, right, .. } => {
                self.collect_free_in_expr(left, bound, free);
                self.collect_free_in_expr(right, bound, free);
            }
            ExpressionKind::FieldAccess { base, .. } => {
                // Only the root identifier matters for capture; don't treat field name as free
                self.collect_free_in_expr(base, bound, free);
            }
            ExpressionKind::Call { func, args } => {
                self.collect_free_in_expr(func, bound, free);
                for arg in args {
                    self.collect_free_in_expr(arg, bound, free);
                }
            }
            ExpressionKind::Match { expr, arms } => {
                self.collect_free_in_expr(expr, bound, free);
                for arm in arms {
                    self.collect_free_in_expr(&arm.node.body, bound, free);
                }
            }
            ExpressionKind::Block(block) => self.collect_free_in_block(block, bound, free),
            _ => {}
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

    fn ensure_loop_body_consistency(
        &self,
        base: &[LiveSet],
        loop_stack: &[LiveSet],
        context: &str,
    ) -> Result<(), String> {
        if base.len() != loop_stack.len() {
            return Err(format!("Live-set depth mismatch in {}", context));
        }
        for (depth, base_set) in base.iter().enumerate() {
            for (name, base_binding) in &base_set.bindings {
                let after = loop_stack[depth].bindings.get(name).unwrap_or(base_binding);

                // If it's a parameter, it's a copy.
                if base_binding.origin == "parameter" {
                    continue;
                }

                // If it is in any shared set (including the one we are iterating on),
                // it's shared and thus safe.
                // ALSO, for now, if it is a `ref` type, let's just allow it for all
                // identifiers that were declared as `ref`.
                // For now, I will just print the bindings that are causing trouble.

                // A loop body must not consume a binding from the outer scope —
                // the loop might run zero times, so the binding would escape unconsumed.
                if !base_binding.consumed && after.consumed {
                    // Check if it is shared in any set
                    if base.iter().any(|s| s.shared.contains(name)) {
                        continue;
                    }
                    return Err(format!(
                        "Binding '{}' cannot be consumed inside a loop body in {}",
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

    fn record_drops(&mut self, block_id: NodeId, set: &LiveSet) {
        let mut drops = Vec::new();
        for (name, _origin, _span) in set.unconsumed() {
            // We check if this binding was a 'let' or 'mut' that
            // the codegen would have placed on the slab.
            let is_heap = set.mutables.contains(&name);
            drops.push(DropInfo { name, is_heap });
        }
        self.structured_drops.insert(block_id, drops);
    }

    fn analyze_pattern(&mut self, _pattern: &Pattern) -> Result<(), String> {
        // Patterns in for-loop bindings introduce new variables into scope;
        // for now we accept all patterns without tracking their introduced names.
        Ok(())
    }

    fn run_branch_match_arm(
        &self,
        arm: &MatchArm,
        stack: Vec<LiveSet>,
    ) -> Result<Vec<LiveSet>, String> {
        let mut branch = LiveAnalyzer {
            stack,
            errors: Vec::new(),
            structured_drops: HashMap::new(),
        };

        branch.push();
        if let Err(err) = branch.insert_pattern_bindings(&arm.node.pattern, "match binding") {
            branch.errors.push(err);
        }
        if let Some(guard) = &arm.node.guard {
            if let Err(err) = branch.analyze_expression(guard) {
                branch.errors.push(err);
            }
        }
        if let Err(err) = branch.analyze_expression(&arm.node.body) {
            branch.errors.push(err);
        }
        branch.pop();

        if branch.errors.is_empty() {
            Ok(branch.stack)
        } else {
            Err(branch.errors.join("; "))
        }
    }

    fn insert_pattern_bindings(&mut self, pattern: &Pattern, origin: &str) -> Result<(), String> {
        let mut binders: HashMap<String, Identifier> = HashMap::new();
        self.collect_pattern_binders(pattern, &mut binders);
        for (name, id) in binders {
            // Identifier pattern acts like a value-pattern if name already exists in outer scope.
            if self.name_in_outer_scopes(&name) {
                continue;
            }
            self.insert_binding(&id, origin, false, false)?;
        }
        Ok(())
    }

    fn name_in_outer_scopes(&self, name: &str) -> bool {
        if self.stack.len() < 2 {
            return false;
        }
        self.stack[..self.stack.len() - 1].iter().any(|set| {
            set.bindings.contains_key(name)
                || set.mutables.contains(name)
                || set.shared.contains(name)
        })
    }

    fn collect_pattern_binders(&self, pattern: &Pattern, out: &mut HashMap<String, Identifier>) {
        match &pattern.node {
            PatternKind::Wildcard | PatternKind::Literal(_) => {}
            PatternKind::Identifier(id) => {
                out.entry(id.name.clone()).or_insert_with(|| id.clone());
            }
            PatternKind::EnumVariant { payload, .. } => {
                if let Some(p) = payload {
                    self.collect_pattern_binders(p, out);
                }
            }
            PatternKind::Struct { fields, .. } => {
                for (_field, p) in fields {
                    self.collect_pattern_binders(p, out);
                }
            }
            PatternKind::Tuple(items) | PatternKind::Array(items) => {
                for p in items {
                    self.collect_pattern_binders(p, out);
                }
            }
            PatternKind::Or(a, b) => {
                self.collect_pattern_binders(a, out);
                self.collect_pattern_binders(b, out);
            }
            PatternKind::Guard { pattern, .. } => {
                // The guard expression is analyzed separately at the match-arm level.
                self.collect_pattern_binders(pattern, out);
            }
        }
    }
}

fn format_span(span: Span) -> String {
    format!("{}:{} ({}..{})", span.line, span.col, span.start, span.end)
}

fn is_ref_type(ty: &Type) -> bool {
    // Parser canonicalizes `ref T` (and `&T`) to a type named "Ref".
    // Keep accepting lowercase "ref" for any hand-constructed ASTs/tests.
    matches!(ty.node.name.as_str(), "Ref" | "ref")
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
            block_id: NodeId(1),
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
            Ok(_drop_map) => Ok(analyzer.drops()),
            Err(err) => Err(err),
        }
    }

    #[test]
    fn drop_unused_parameter() {
        let drops =
            analyze("fn unused(count: Int32) -> Int32 { let zero: Int32 = 0; return zero; }")
                .unwrap();
        println!("{:?}", drops);
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
        println!("{:?}", drops);
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
                        pattern: Spanned::new_dummy(
                            PatternKind::Identifier(mk_ident("value")),
                            dummy_span(),
                        ),
                        type_annotation: Some(mk_type("Int32")),
                        initializer: mk_expr(ExpressionKind::Literal(Literal {
                            kind: LiteralKind::Int(1, None),
                            span: dummy_span(),
                        })),
                        else_block: None,
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
                params: vec![],
                return_type: Some(mk_type("Int32")),
                body: mk_block(vec![
                    mk_stmt(StatementKind::LetBinding {
                        mutable: false,
                        pattern: Spanned::new_dummy(
                            PatternKind::Identifier(mk_ident("token")),
                            dummy_span(),
                        ),
                        type_annotation: Some(mk_type("Int32")),
                        initializer: mk_expr(ExpressionKind::Literal(Literal {
                            kind: LiteralKind::Int(1, None),
                            span: dummy_span(),
                        })),
                        else_block: None,
                    }),
                    mk_stmt(StatementKind::Loop {
                        kind: Spanned::new_dummy(
                            LoopKindKind::Block(loop_body.clone()),
                            dummy_span(),
                        ),
                        body: loop_body,
                    }),
                ]),
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
                        pattern: Spanned::new_dummy(
                            PatternKind::Identifier(mk_ident("value")),
                            dummy_span(),
                        ),
                        type_annotation: Some(mk_type("Int32")),
                        initializer: lit_int(1),
                        else_block: None,
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
                        pattern: Spanned::new_dummy(
                            PatternKind::Identifier(mk_ident("counter")),
                            dummy_span(),
                        ),
                        type_annotation: Some(mk_type("Int32")),
                        initializer: lit_int(1),
                        else_block: None,
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
                name: "Ref".into(),
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
                name: "Ref".into(),
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
                        pattern: Spanned::new_dummy(
                            PatternKind::Identifier(mk_ident("node")),
                            dummy_span(),
                        ),
                        type_annotation: Some(ref_node_type),
                        initializer: mk_expr(ExpressionKind::Placeholder("node".into())),
                        else_block: None,
                    }),
                    mk_stmt(StatementKind::LetBinding {
                        mutable: false,
                        pattern: Spanned::new_dummy(
                            PatternKind::Identifier(mk_ident("updated")),
                            dummy_span(),
                        ),
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
                        else_block: None,
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
