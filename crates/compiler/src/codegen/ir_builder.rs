//! IR Builder - core state and emit helpers
//!
//! Maintains per-function emit state, context save/restore for conc trampolines,
//! and low-level line emission.

use crate::ast::*;
use crate::codegen::{int_suffix_to_llvm, IrFunction, IrModule, TypeRegistry, is_main, build_fn_annotations, annotation_ns_for_symbol, link_symbol_name};
use crate::liveness::DropInfo;
use crate::span::Span;
use crate::type_inference::{InferType, TypeVarId};
use std::collections::{HashMap, HashSet};

// ── Saved function context (for nested emission, e.g. conc trampolines) ───────

#[derive(Debug)]
pub struct FnContext {
    pub lines: Vec<String>,
    pub entry_allocas: Vec<String>,
    pub fn_name: Option<String>,
    pub ret_ty: String,
    pub locals: HashMap<String, String>,
    pub locals_type: HashMap<String, String>,
    pub mutable_vars: HashSet<String>,
    pub next_tmp: usize,
    pub chan_elem_tys: HashMap<String, String>,
}

// ── IR builder ────────────────────────────────────────────────────────────────

/// Main IR builder state. Contains emit state, variable tracking, and registries.
pub struct IrBuilder<'a> {
    // Emit state
    pub lines: Vec<String>,
    pub entry_allocas: Vec<String>,
    pub next_tmp: usize,
    pub next_label: usize,
    pub loop_labels: Vec<(String, String)>,
    pub current_fn_name: Option<String>,
    pub current_fn_ret_ty: String,

    // Variable tracking
    pub locals: HashMap<String, String>,
    pub locals_type: HashMap<String, String>,
    pub mutable_vars: HashSet<String>,
    pub chan_elem_tys: HashMap<String, String>,

    // Registries (stable across functions)
    pub reg: TypeRegistry,
    pub adt_structs: HashMap<String, String>,

    // External state
    pub types: Option<*const HashMap<NodeId, InferType>>,
    pub drop_map: &'a HashMap<NodeId, Vec<DropInfo>>,
    pub original_ns_by_symbol: &'a HashMap<String, String>,
    pub enum_variants: &'a HashMap<String, (String, Vec<TypeVarId>, Option<InferType>)>,

    // Completed trampolines waiting to be appended
    pub conc_functions: Vec<IrFunction>,
}

impl<'a> IrBuilder<'a> {
    pub fn new(
        drop_map: &'a HashMap<NodeId, Vec<DropInfo>>,
        original_ns_by_symbol: &'a HashMap<String, String>,
        enum_variants: &'a HashMap<String, (String, Vec<TypeVarId>, Option<InferType>)>,
    ) -> Self {
        Self {
            lines: Vec::new(),
            entry_allocas: Vec::new(),
            next_tmp: 0,
            next_label: 0,
            loop_labels: Vec::new(),
            current_fn_name: None,
            current_fn_ret_ty: "void".to_string(),
            locals: HashMap::new(),
            locals_type: HashMap::new(),
            mutable_vars: HashSet::new(),
            reg: TypeRegistry::new(),
            adt_structs: HashMap::new(),
            types: None,
            drop_map,
            original_ns_by_symbol,
            enum_variants,
            conc_functions: Vec::new(),
            chan_elem_tys: HashMap::new(),
        }
    }

    // ── Context save/restore ──────────────────────────────────────────────────

    pub fn save_context(&mut self) -> FnContext {
        FnContext {
            lines: std::mem::take(&mut self.lines),
            entry_allocas: std::mem::take(&mut self.entry_allocas),
            fn_name: self.current_fn_name.clone(),
            ret_ty: self.current_fn_ret_ty.clone(),
            locals: std::mem::take(&mut self.locals),
            locals_type: std::mem::take(&mut self.locals_type),
            mutable_vars: std::mem::take(&mut self.mutable_vars),
            next_tmp: self.next_tmp,
            chan_elem_tys: std::mem::take(&mut self.chan_elem_tys),
        }
    }

    pub fn restore_context(&mut self, ctx: FnContext) {
        self.lines = ctx.lines;
        self.entry_allocas = ctx.entry_allocas;
        self.current_fn_name = ctx.fn_name;
        self.current_fn_ret_ty = ctx.ret_ty;
        self.locals = ctx.locals;
        self.locals_type = ctx.locals_type;
        self.mutable_vars = ctx.mutable_vars;
        self.next_tmp = ctx.next_tmp;
        self.chan_elem_tys = ctx.chan_elem_tys;
    }

    pub fn reset_for_function(&mut self, name: &str, ret_ty: &str) {
        self.lines.clear();
        self.entry_allocas.clear();
        self.locals.clear();
        self.locals_type.clear();
        self.mutable_vars.clear();
        self.next_tmp = 0;
        self.current_fn_ret_ty = ret_ty.to_string();
        self.current_fn_name = Some(name.to_string());
        self.chan_elem_tys.clear();
    }

    // ── Low-level emitters ────────────────────────────────────────────────────

    pub fn emit(&mut self, line: String) {
        self.lines.push(line);
    }

    pub fn tmp(&mut self) -> String {
        let n = self.next_tmp;
        self.next_tmp += 1;
        format!("%t{}", n)
    }

    pub fn label(&mut self, prefix: &str) -> String {
        let n = self.next_label;
        self.next_label += 1;
        format!("{}_{}", prefix, n)
    }

    pub fn annotate_span(&mut self, span: &Span) {
        if *span != Span::default() {
            self.emit(format!(
                "  ; span {}..{} @ {}:{}",
                span.start, span.end, span.line, span.col
            ));
        }
    }

    /// Emit an alloca into the entry block regardless of the current basic block.
    pub fn emit_alloca(&mut self, tmp: &str, ty: &str) {
        let ty = if ty == "void" { "i32" } else { ty };
        self.entry_allocas
            .push(format!("  {} = alloca {}", tmp, ty));
    }

    /// Splice hoisted entry_allocas in right after the "entry:" label line.
    pub fn finish_function_ir(&mut self) -> String {
        let mut all = Vec::with_capacity(self.lines.len() + self.entry_allocas.len());
        if let Some(first) = self.lines.first() {
            all.push(first.clone());
        }
        all.extend(self.entry_allocas.drain(..));
        if self.lines.len() > 1 {
            all.extend(self.lines[1..].iter().cloned());
        }
        all.join("\n")
    }

    /// Load task from its alloca slot.
    pub fn emit_task_load(&mut self) -> String {
        if let Some(slot) = self.locals.get("task").cloned() {
            let t = self.tmp();
            self.emit(format!("  {} = load i8*, i8** {}", t, slot));
            t
        } else {
            "%task".to_string()
        }
    }

    // ── Captured variable analysis ────────────────────────────────────────────

    pub fn collect_captured_vars(&self, block: &Block) -> Vec<String> {
        let mut captured: Vec<String> = Vec::new();
        let mut defined = HashSet::new();

        // Visit each statement in order, adding let-bound names to `defined`
        // AFTER visiting the initializer — so `let x = x + 1` correctly
        // captures the outer `x` from the RHS before shadowing it.
        for stmt in &block.statements {
            self.visit_stmt_identifiers(stmt, &mut |name| {
                if !defined.contains(name) && !captured.iter().any(|s| s == name) {
                    captured.push(name.to_string());
                }
            });
            if let StatementKind::LetBinding { pattern, .. } = &stmt.node {
                if let Some(name) = pattern.get_identifier() {
                    defined.insert(name.name.clone());
                }
            }
        }

        // Visit trailing expression with all let bindings now in scope
        if let Some(expr) = &block.trailing_expression {
            self.visit_expr_identifiers(expr, &mut |name| {
                if !defined.contains(name) && !captured.iter().any(|s| s == name) {
                    captured.push(name.to_string());
                }
            });
        }

        // Remove global function names — they are called directly by symbol, not captured.
        // This covers both stdlib functions (printf, println, …) registered in func_sigs
        // and user-defined functions from the same module. Without this filter, any bare
        // function call inside a conc{} block would appear as a missing capture and emit
        // a BUG comment even though the generated IR is correct.
        captured.retain(|name| {
            !self.reg.func_sigs.contains_key(name.as_str())
                && !self.reg.extern_fns.contains(name.as_str())
        });

        captured
    }

    pub fn visit_stmt_identifiers(&self, stmt: &Statement, f: &mut dyn FnMut(&str)) {
        match &stmt.node {
            StatementKind::LetBinding { initializer, .. } => {
                self.visit_expr_identifiers(initializer, f)
            }
            StatementKind::Expression(expr) | StatementKind::Return(Some(expr)) => {
                self.visit_expr_identifiers(expr, f)
            }
            StatementKind::Match { expr, arms } => {
                self.visit_expr_identifiers(expr, f);
                for arm in arms {
                    if let Some(g) = &arm.node.guard {
                        self.visit_expr_identifiers(g, f);
                    }
                    // Add match-arm pattern bindings to scope so the body sees them as local
                    self.visit_pattern_identifiers(&arm.node.pattern, f);
                    self.visit_expr_identifiers(&arm.node.body, f);
                }
            }
            StatementKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.visit_expr_identifiers(condition, f);
                for s in &then_branch.statements {
                    self.visit_stmt_identifiers(s, f);
                }
                if let Some(t) = &then_branch.trailing_expression {
                    self.visit_expr_identifiers(t, f);
                }
                if let Some(eb) = else_branch {
                    match &eb.node {
                        ElseBranchKind::Block(b) => {
                            for s in &b.statements {
                                self.visit_stmt_identifiers(s, f);
                            }
                            if let Some(t) = &b.trailing_expression {
                                self.visit_expr_identifiers(t, f);
                            }
                        }
                        ElseBranchKind::If(stmt) => self.visit_stmt_identifiers(stmt, f),
                    }
                }
            }
            StatementKind::Loop { body, .. } | StatementKind::Conc { body } => {
                for s in &body.statements {
                    self.visit_stmt_identifiers(s, f);
                }
                if let Some(t) = &body.trailing_expression {
                    self.visit_expr_identifiers(t, f);
                }
            }
            _ => {}
        }
    }

    pub fn visit_pattern_identifiers(&self, pattern: &Pattern, f: &mut dyn FnMut(&str)) {
        match &pattern.node {
            PatternKind::Identifier(id) => f(&id.name),
            PatternKind::EnumVariant { payload, .. } => {
                if let Some(inner) = payload {
                    self.visit_pattern_identifiers(inner, f);
                }
            }
            PatternKind::Struct { fields, .. } => {
                for (_, pat) in fields {
                    self.visit_pattern_identifiers(pat, f);
                }
            }
            PatternKind::Tuple(parts) | PatternKind::Array(parts) => {
                for p in parts {
                    self.visit_pattern_identifiers(p, f);
                }
            }
            PatternKind::Or(left, right) => {
                self.visit_pattern_identifiers(left, f);
                self.visit_pattern_identifiers(right, f);
            }
            _ => {}
        }
    }

    pub fn visit_expr_identifiers(&self, expr: &Expression, f: &mut dyn FnMut(&str)) {
        if let ExpressionKind::Identifier(id) = &expr.node {
            f(&id.name);
        }
        match &expr.node {
            ExpressionKind::Cast { expr: inner, .. } => self.visit_expr_identifiers(inner, f),
            ExpressionKind::BinaryOp { left, right, .. } => {
                self.visit_expr_identifiers(left, f);
                self.visit_expr_identifiers(right, f);
            }
            ExpressionKind::UnaryOp { expr, .. } | ExpressionKind::TryOperator { expr } => {
                self.visit_expr_identifiers(expr, f)
            }
            ExpressionKind::Call { func, args } => {
                self.visit_expr_identifiers(func, f);
                for a in args {
                    self.visit_expr_identifiers(a, f);
                }
            }
            ExpressionKind::FieldAccess { base, .. } => self.visit_expr_identifiers(base, f),
            ExpressionKind::IndexAccess { base, index } => {
                self.visit_expr_identifiers(base, f);
                self.visit_expr_identifiers(index, f);
            }
            ExpressionKind::StructInit { fields, .. } => {
                for (_, e) in fields {
                    self.visit_expr_identifiers(e, f);
                }
            }
            ExpressionKind::MergeExpression { base, fields } => {
                if let Some(b) = base {
                    self.visit_expr_identifiers(b, f);
                }
                for (_, e) in fields {
                    self.visit_expr_identifiers(e, f);
                }
            }
            ExpressionKind::Pipe { left, right } => {
                self.visit_expr_identifiers(left, f);
                self.visit_expr_identifiers(right, f);
            }
            ExpressionKind::Match { expr, arms } => {
                self.visit_expr_identifiers(expr, f);
                for arm in arms {
                    if let Some(g) = &arm.node.guard {
                        self.visit_expr_identifiers(g, f);
                    }
                    self.visit_expr_identifiers(&arm.node.body, f);
                }
            }
            ExpressionKind::IfLet {
                expr,
                pattern,
                then,
                else_branch,
                ..
            } => {
                self.visit_expr_identifiers(expr, f);
                self.visit_pattern_identifiers(pattern, f);
                for s in &then.statements {
                    self.visit_stmt_identifiers(s, f);
                }
                if let Some(t) = &then.trailing_expression {
                    self.visit_expr_identifiers(t, f);
                }
                if let Some(e) = else_branch {
                    self.visit_expr_identifiers(e, f);
                }
            }
            ExpressionKind::Block(b) => {
                for s in &b.statements {
                    self.visit_stmt_identifiers(s, f);
                }
                if let Some(e) = &b.trailing_expression {
                    self.visit_expr_identifiers(e, f);
                }
            }
            _ => {}
        }
    }

    // ── Enum name resolution ─────────────────────────────────────────────────────
    // Uses enum_variants to look up the enum name from a variant name.
    // Falls back to the variant name if not found (for simple non-generic enums).

    pub fn enum_name_for_variant(&self, variant: &str) -> String {
        self.enum_variants
            .get(variant)
            .map(|(enum_name, _, _)| enum_name.clone())
            .unwrap_or_else(|| variant.to_string())
    }
    pub fn struct_field_info(&self, struct_name: &str, field_name: &str) -> (usize, String) {
        self.reg.struct_field_info(struct_name, field_name)
    }
    pub fn method_symbol_for_call(&self, base_ty: &str, method: &str) -> Option<String> {
        self.reg.method_symbol_for_call(base_ty, method)
    }

    pub fn value_llvm_type(&self, value: &str) -> Option<String> {
        if !value.starts_with('%') {
            return None;
        }
        for line in self.lines.iter().rev() {
            let t = line.trim_start();
            if !t.starts_with(&format!("{} = ", value)) {
                continue;
            }
            if let Some(rest) = t.strip_prefix(&format!("{} = call ", value)) {
                return rest.split_whitespace().next().map(|s| s.to_string());
            }
            if let Some(rest) = t.strip_prefix(&format!("{} = load ", value)) {
                return rest.split(',').next().map(|s| s.trim().to_string());
            }
            if let Some(rest) = t.strip_prefix(&format!("{} = extractvalue ", value)) {
                return rest.split_whitespace().next().map(|s| s.to_string());
            }
        }
        None
    }

    pub fn expr_llvm_type(&mut self, expr: &Expression) -> String {
        // Locals (most specific)
        if let ExpressionKind::Identifier(id) = &expr.node {
            if let Some(ty) = self.locals_type.get(&id.name) {
                return ty.clone();
            }
        }
        // Literals have unambiguous LLVM types — check these BEFORE the type
        // checker map, which can have NodeId collisions across merged modules.
        match &expr.node {
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Str(_),
                ..
            }) => return "%struct.Str*".to_string(),
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Bool(_),
                ..
            }) => return "i1".to_string(),
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Int(_, suffix),
                ..
            }) => return int_suffix_to_llvm(suffix.as_deref().unwrap_or("")).to_string(),
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Float(_, suffix),
                ..
            }) => {
                return if suffix.as_deref() == Some("f64") {
                    "double"
                } else {
                    "float"
                }
                .to_string()
            }
            _ => {}
        }
        // For Call expressions, always prefer func_sigs lookup BEFORE falling back
        // to the type checker map. The type checker's types HashMap is keyed by NodeId,
        // but parsed .ty files all start NodeId from 1, so expressions from different
        // files can have the same NodeId. This causes actual_inferred_type to return
        // the wrong type when two expressions share an ID and one was type-checked first.
        // func_sigs is populated from the .ll stdlib declarations and is safe from this.
        if let ExpressionKind::Call { func, .. } = &expr.node {
            if let ExpressionKind::Identifier(id) = &func.node {
                if id.name == "chan" {
                    return "i8*".to_string();
                }
                if let Some((ret_ty, c_param_types)) = self.reg.func_sigs.get(&id.name).cloned() {
                    if self.reg.out_result_funcs.contains(&id.name) {
                        if let Some(last) = c_param_types.last() {
                            return last.trim_end_matches('*').to_string();
                        }
                    }
                    return ret_ty;
                }
            }
            // Method calls (base.method()) have the exact same NodeId-collision
            // hazard as the free-function case above, but never got the same
            // func_sigs-first treatment — they fell straight through to
            // actual_inferred_type below. This is what broke Socket.split():
            // its real return type is SocketHalves, but a NodeId collision in
            // the type checker's map returned some unrelated "i32" instead,
            // which then corrupted the let-binding's alloca type and every
            // subsequent .read/.write field access downstream. Same fix,
            // same precedent: resolve via the mangled method symbol in
            // func_sigs before ever consulting the NodeId-keyed map.
            if let ExpressionKind::FieldAccess { base, field } = &func.node {
                let base_ty = self.expr_llvm_type(base);
                let struct_name = base_ty
                    .trim_start_matches("%struct.")
                    .trim_end_matches('*')
                    .to_string();
                let local_name = format!("__ty_method__{}__{}", struct_name, field.name);
                let rt_name = format!("__ty_rt__{}__{}", struct_name, field.name);
                if let Some((ret_ty, _)) = self
                    .reg
                    .func_sigs
                    .get(&local_name)
                    .cloned()
                    .or_else(|| self.reg.func_sigs.get(&rt_name).cloned())
                {
                    if self.reg.out_result_funcs.contains(&local_name)
                        || self.reg.out_result_funcs.contains(&rt_name)
                    {
                        // Out-pointer convention (e.g. Result<T,E>-returning
                        // methods): the real Typhoon-visible type isn't the
                        // declared C return type, same handling as the
                        // free-function branch above would give it.
                    } else {
                        return ret_ty;
                    }
                }
            }
        }
        // Cast: always return the target type, ignoring any inference
        // (type checker may store the inner type causing wrong width here).
        if let ExpressionKind::Cast { target_type, .. } = &expr.node {
            return self.lower_type(target_type);
        }
        // BinaryOp: result type matches the left operand's type. We MUST
        // resolve it here (rather than via the NodeId-keyed types map
        // below) because NodeId collisions across merged modules can return
        // a stray entry — observed as `nm % 10` in Buf.__push_int_inner
        // coming back typed `%struct.Str*`, which then cascaded into
        // `ptrtoint %struct.Str* ... to i8` and `store %struct.Str* ...,
        // %struct.Str** ...` for every subsequent arithmetic slot.
        if let ExpressionKind::BinaryOp { op, left, right } = &expr.node {
            // Comparisons yield i1 regardless of operand widths.
            let lhs_ty = self.expr_llvm_type(left);
            let rhs_ty = self.expr_llvm_type(right);
            let cmp = matches!(
                *op,
                Operator::Eq
                    | Operator::Ne
                    | Operator::Lt
                    | Operator::Gt
                    | Operator::Le
                    | Operator::Ge
                    | Operator::And
                    | Operator::Or
            ) && lhs_ty != "i1"
                && rhs_ty != "i1";
            if cmp {
                return "i1".to_string();
            }
            // Widen bool operands (i8 on codegen, i1 from locals)
            // to the other side's width for the comparison path
            // (already handled by the early i1 return above when
            // either side is i1).
            return lhs_ty;
        }
        // Type checker inference (can have NodeId collisions — only use for non-literals
        // and non-function-calls, which have their types resolved from func_sigs above).
        if let Some(ty) = self.actual_inferred_type(expr) {
            return self.lower_infer_type(&ty);
        }
        match &expr.node {
            ExpressionKind::StructInit { name, .. }
                if self.reg.opaque_structs.contains(&name.name) =>
            {
                format!("%struct.{}*", name.name)
            }
            ExpressionKind::StructInit { name, .. } => format!("%struct.{}", name.name),
            ExpressionKind::MergeExpression { base, .. } => base
                .as_ref()
                .map(|b| self.expr_llvm_type(b))
                .unwrap_or_else(|| "%struct.?".to_string()),
            ExpressionKind::FieldAccess { base, field } => {
                let base_ty = self.expr_llvm_type(base);
                let struct_name = base_ty.trim_start_matches("%struct.").to_string();
                self.struct_field_info(&struct_name, &field.name).1
            }
            ExpressionKind::Call { func, .. } => {
                if let ExpressionKind::FieldAccess { base, field } = &func.node {
                    let base_ty = self.expr_llvm_type(base);
                    if base_ty == "%struct.TyArray*" && field.name == "push" {
                        return "void".to_string();
                    }
                    if let Some(sym) = self.method_symbol_for_call(&base_ty, &field.name) {
                        return self
                            .reg
                            .func_sigs
                            .get(&sym)
                            .map(|(r, _)| r.clone())
                            .unwrap_or_else(|| "i32".to_string());
                    }
                } else if let ExpressionKind::Identifier(id) = &func.node {
                    if id.name == "chan" {
                        return "i8*".to_string();
                    }
                    if let Some((_, c_param_types)) = self.reg.func_sigs.get(&id.name).cloned() {
                        if self.reg.out_result_funcs.contains(&id.name) {
                            // C ABI holds the Result via trailing pointer.
                            // Return the pointed-to struct type so the value
                            // loaded out of the alloca has the right LLVM type.
                            if let Some(last) = c_param_types.last() {
                                return last.trim_end_matches('*').to_string();
                            }
                        }
                        if let Some((r, _)) = self.reg.func_sigs.get(&id.name) {
                            return r.clone();
                        }
                        return "i32".to_string();
                    }
                    return "i32".to_string();
                }
                "i32".to_string()
            }
            ExpressionKind::Block(b) => b
                .trailing_expression
                .as_ref()
                .map(|e| self.expr_llvm_type(e))
                .unwrap_or_else(|| "void".to_string()),
            ExpressionKind::TryOperator { expr } => self.expr_llvm_type(expr),
            _ => "i32".to_string(),
        }
    }

    // ── Type inference helpers ─────────────────────────────────────────────────────

    /// Get the inferred type for an expression from the type checker's map.
    pub fn inferred_expr_type(&self, expr: &Expression) -> Option<&InferType> {
        // SAFETY: types map lives longer than this builder call.
        let types = unsafe { &*self.types? };
        types.get(&expr.id)
    }

    /// Get the inferred type for an expression, preferring actual_inferred_type
    /// which handles casts and other adjustments.
    pub fn actual_inferred_type(&mut self, expr: &Expression) -> Option<InferType> {
        if let ExpressionKind::Cast { target_type, .. } = &expr.node {
            // For casts, return the target type as the inferred type
            let ty_name = target_type.node.name.clone();
            if target_type.node.generic_args.is_empty() {
                return Some(InferType::Con(ty_name));
            } else {
                let args: Vec<InferType> = target_type.node.generic_args
                    .iter()
                    .map(|a| InferType::Con(a.node.name.clone()))
                    .collect();
                return Some(InferType::App(ty_name, args));
            }
        }
        self.inferred_expr_type(expr).cloned()
    }

    /// Lower an InferType to an LLVM type string.
    pub fn lower_infer_type(&mut self, ty: &InferType) -> String {
        self.reg.lower_infer_type(ty)
    }

    /// Ensure enum layout for an inferred type (e.g., Option<T>, Result<T,E>).
    pub fn ensure_enum_layout_for_infer(&mut self, ty: &InferType) {
        let InferType::App(name, args) = ty else {
            return;
        };
        let Some(def) = self.reg.enum_defs.get(name).cloned() else {
            return;
        };
        if def.gen_params.len() != args.len() {
            return;
        }

        let llvm_args: Vec<String> = args.iter().map(|a| self.lower_infer_type(a)).collect();

        let opaque_structs = self.reg.opaque_structs.clone();
        let mut lower_payload =
            |payload: &EnumVariantPayloadKind, subst: &HashMap<String, String>| -> Option<String> {
                TypeRegistry::lower_enum_payload(payload, subst, &opaque_structs)
            };

        self.reg
            .ensure_enum_layout(&def, &llvm_args, &|_, _| String::new(), &mut lower_payload);
    }

    /// Ensure enum layout for an AST Type node.
    pub fn ensure_enum_layout_for_type(&mut self, ty: &Type) {
        let name = ty.node.name.as_str();
        let Some(def) = self.reg.enum_defs.get(name).cloned() else {
            return;
        };
        if def.gen_params.len() != ty.node.generic_args.len() {
            return;
        }
        let llvm_args: Vec<String> = ty
            .node
            .generic_args
            .iter()
            .map(|a| TypeRegistry::lower_type_with_opaque_structs(a, &self.reg.opaque_structs))
            .collect();
        let opaque_structs = self.reg.opaque_structs.clone();
        let mut lower_payload =
            |payload: &EnumVariantPayloadKind, subst: &HashMap<String, String>| -> Option<String> {
                TypeRegistry::lower_enum_payload(payload, subst, &opaque_structs)
            };
        self.reg
            .ensure_enum_layout(&def, &llvm_args, &|_, _| String::new(), &mut lower_payload);
    }

    /// Scan a declaration for ADTs (enums) that need layout emission.
    pub fn scan_decl_for_adts(&mut self, decl: &Declaration) {
        self.reg.scan_decl_for_adts(decl);
    }

    /// Ensure ADT layout for all types in an InferType.
    pub fn ensure_adt_for_infertype(&mut self, ty: &InferType) {
        self.reg.ensure_adt_for_infertype(ty);
    }

    // ── String literal emission ──────────────────────────────────────────────────

    pub fn emit_string(&mut self, s: &str) -> String {
        let (global, n) = if let Some(v) = self.reg.string_pool.get(s).cloned() {
            v
        } else {
            let id = self.reg.string_pool.len();
            let global = format!("@.str.{}", id);
            let bytes = s.as_bytes();
            let n = bytes.len() + 1;
            self.reg.extra_preamble.push(format!(
                "{} = private unnamed_addr constant [{} x i8] c\"{}\\00\"",
                global,
                n,
                crate::codegen::llvm_escape(bytes)
            ));
            let pair = (global.clone(), n);
            self.reg.string_pool.insert(s.to_string(), pair.clone());
            pair
        };
        let len = n - 1;

        let ptr_tmp = self.tmp();
        self.emit(format!(
            "  {} = getelementptr inbounds [{} x i8], [{} x i8]* {}, i32 0, i32 0",
            ptr_tmp, n, n, global
        ));

        let tv = self.emit_task_load();
        let raw = self.tmp();
        self.emit(format!(
            "  {} = call i8* @slab_alloc(i8* {}, i32 1)",
            raw, tv
        ));
        let str_val = self.tmp();
        self.emit(format!(
            "  {} = bitcast i8* {} to %struct.Str*",
            str_val, raw
        ));
        let ptr_field = self.tmp();
        self.emit(format!(
            "  {} = getelementptr inbounds %struct.Str, %struct.Str* {}, i32 0, i32 0",
            ptr_field, str_val
        ));
        self.emit(format!("  store i8* {}, i8** {}", ptr_tmp, ptr_field));
        let len_field = self.tmp();
        self.emit(format!(
            "  {} = getelementptr inbounds %struct.Str, %struct.Str* {}, i32 0, i32 1",
            len_field, str_val
        ));
        self.emit(format!("  store i32 {}, i32* {}", len, len_field));

        str_val
    }

    // ── Type lowering helpers ────────────────────────────────────────────────────

    pub fn lower_type(&self, ty: &Type) -> String {
        self.reg.lower_type(ty, &self.reg.opaque_structs)
    }

    /// Get the array element type from an inferred Array type.
    pub fn array_elem_type_from_infertype(&mut self, ty: &InferType) -> Option<String> {
        if let InferType::App(name, args) = ty {
            if name == "Array" && args.len() == 1 {
                return Some(self.lower_infer_type(&args[0]));
            }
        }
        None
    }

    /// Check if a return type needs the out-result ABI (size > 8 bytes).
    pub fn needs_out_result_abi(return_type: &Option<Type>) -> bool {
        TypeRegistry::needs_out_result_abi(return_type)
    }

    /// Get the zero value for a given LLVM type.
    pub fn zero_value(&self, ty: &str) -> String {
        self.reg.zero_value(ty)
    }

    /// Get the sizeof for an LLVM type.
    pub fn llvm_const_sizeof(&self, ty: &str) -> i64 {
        self.reg.llvm_const_sizeof(ty)
    }

    /// Get the alignof for an LLVM type.
    pub fn llvm_const_alignof(&self, ty: &str) -> i64 {
        self.reg.llvm_const_alignof(ty)
    }

    /// Widen a value from actual_ty to expected_ty.
    pub fn emit_widen(&mut self, val: &str, actual_ty: &str, expected_ty: &str) -> String {
        if actual_ty == expected_ty {
            return val.to_string();
        }
        if expected_ty.ends_with('*') && matches!(actual_ty, "i1" | "i8" | "i16" | "i32" | "i64") {
            if val == "0" {
                return "null".to_string();
            }
        }
        let int_rank = |t: &str| -> Option<u8> {
            match t {
                "i8" => Some(0),
                "i16" => Some(1),
                "i32" => Some(2),
                "i64" => Some(3),
                _ => None,
            }
        };
        let float_rank = |t: &str| -> Option<u8> {
            match t {
                "half" => Some(0),
                "float" => Some(1),
                "double" => Some(2),
                _ => None,
            }
        };
        if let (Some(a), Some(e)) = (int_rank(actual_ty), int_rank(expected_ty)) {
            if a < e {
                let tmp = self.tmp();
                self.emit(format!(
                    "  {} = sext {} {} to {}",
                    tmp, actual_ty, val, expected_ty
                ));
                return tmp;
            }
        }
        if let (Some(a), Some(e)) = (float_rank(actual_ty), float_rank(expected_ty)) {
            if a < e {
                let tmp = self.tmp();
                self.emit(format!(
                    "  {} = fpext {} {} to {}",
                    tmp, actual_ty, val, expected_ty
                ));
                return tmp;
            }
        }
        val.to_string()
    }

    // Pattern methods are defined in pattern.rs module
}

/// Entry point - lower a whole module to IR
impl<'a> IrBuilder<'a> {
    pub fn lower_module(
        module: &Module,
        types: &HashMap<NodeId, InferType>,
        specializations: &HashMap<(String, Vec<InferType>), String>,
        drop_map: &HashMap<NodeId, Vec<DropInfo>>,
        original_ns_by_symbol: &HashMap<String, String>,
        enum_variants: &'a HashMap<String, (String, Vec<TypeVarId>, Option<InferType>)>,
    ) -> IrModule {
        let mut b = IrBuilder::new(drop_map, original_ns_by_symbol, enum_variants);
        b.types = Some(types as *const _);
        b.collect_types(module);

    let mut all_functions: Vec<IrFunction> = module
        .declarations
        .iter()
        .filter_map(|decl| {
            let DeclarationKind::Function {
                name,
                return_type,
                body,
                params,
                generics,
                ..
            } = &decl.node
            else {
                return None;
            };
            if !generics.is_empty() {
                return None;
            }

            let ret_ty = return_type
                .as_ref()
                .map(|ty| b.lower_type(ty))
                .unwrap_or_else(|| "void".to_string());

            if is_main(&name.name) {
                let body_ir = b.emit_main_body(params, body);
                b.conc_functions.push(IrFunction {
                    name: "__ty_main_body".to_string(),
                    body: body_ir,
                    ret_type: "void".to_string(),
                    params: vec![
                        ("task".to_string(), "i8*".to_string()),
                        ("arg".to_string(), "i8*".to_string()),
                    ],
                    annotations: vec![],
                });
                Some(IrFunction {
                    name: "main".to_string(),
                    body: b.emit_bootstrap_main(),
                    ret_type: "i32".to_string(),
                    params: vec![],
                    annotations: vec![],
                })
            } else {
                let body_ir = b.emit_function(name, params, &ret_ty, body);
                let mut param_list: Vec<(String, String)> = params
                    .iter()
                    .map(|p| (p.name.name.clone(), b.lower_type(&p.type_annotation)))
                    .collect();
                param_list.insert(0, ("task".to_string(), "i8*".to_string()));
                let annotations = build_fn_annotations(
                    annotation_ns_for_symbol(
                        module.name.as_deref().unwrap_or(""),
                        &name.name,
                        b.original_ns_by_symbol,
                    ),
                    &name.name,
                    params,
                    return_type.as_ref(),
                );
                Some(IrFunction {
                    name: link_symbol_name(&name.name),
                    body: body_ir,
                    ret_type: ret_ty,
                    params: param_list,
                    annotations,
                })
            }
        })
        .collect();

    for ((_func_name, _concrete_types), spec_name) in specializations {
        let func_name = _func_name.clone();
        if let Some(decl) = module.declarations.iter().find(|d| {
            matches!(&d.node, DeclarationKind::Function { name, .. } if name.name == func_name)
        }) {
            if let DeclarationKind::Function { name, params, body, return_type, .. } = &decl.node {
                let ret_ty = return_type.as_ref()
                    .map(|ty| b.lower_type(ty,))
                    .unwrap_or_else(|| "void".to_string());
                let body_ir = b.emit_function(name, params, &ret_ty, body);
                let mut param_list: Vec<(String, String)> = params.iter()
                    .map(|p| (p.name.name.clone(), b.lower_type(&p.type_annotation,)))
                    .collect();
                param_list.insert(0, ("task".to_string(), "i8*".to_string()));
                let annotations = build_fn_annotations(
                        annotation_ns_for_symbol(
                            module.name.as_deref().unwrap_or(""),
                            name.name.as_str(),
                            b.original_ns_by_symbol,
                        ),
                        name.name.as_str(),
                        params,
                        return_type.as_ref(),
                    );
                all_functions.push(IrFunction {
                    name: spec_name.clone(), body: body_ir, ret_type: ret_ty, params: param_list,
                    annotations,
                });
            }
        }
    }

    all_functions.extend(b.conc_functions.drain(..));
    IrModule {
        functions: all_functions,
        preamble: b.reg.preamble(),
    }
}

    /// Emit the user's `main` body as a void coroutine named `__ty_main_body`.
    /// It receives `(task: i8*, arg: i8*)` like every other spawned trampoline,
    /// ignores `arg`, and returns void.  Scheduler init/shutdown are NOT emitted
    /// here — the thin bootstrap `main()` owns those.
    pub fn emit_main_body(&mut self, params: &[Parameter], body: &Block) -> String {
        self.reset_for_function("__ty_main_body", "void");
        self.emit("entry:".to_string());
        self.emit("  call void @ty_safepoint()".to_string());
        // Bind task and arg params (arg is unused but must be accepted).
        self.emit_function_param("task".to_string(), "i8*".to_string());
        self.emit_function_param("arg".to_string(), "i8*".to_string());

        // User params (main normally has none, but handle them anyway).
        for param in params {
            let ty = self.lower_type(&param.type_annotation);
            let slot = self.tmp();
            self.emit_alloca(&slot, &ty);
            if let Some(factory) = self.reg.default_factories.get(&ty).cloned() {
                let v = self.tmp();
                let args = self
                    .reg
                    .func_sigs
                    .get(&factory)
                    .filter(|(_, params)| params.is_empty())
                    .map_or("i8* %task", |_| "");
                self.emit(format!("  {} = call {} @{}({})", v, ty, factory, args));
                self.emit(format!("  store {} {}, {}* {}", ty, v, ty, slot));
            } else {
                let z = self.zero_value(&ty);
                self.emit(format!("  store {} {}, {}* {}", ty, z, ty, slot));
            }
            self.locals.insert(param.name.name.clone(), slot.clone());
            self.locals_type.insert(param.name.name.clone(), ty);
        }
        if !self.emit_block_stmts(body, "void") {
            self.emit("  ret void".to_string());
        }

        self.finish_function_ir()
    }

    /// Emit the thin C-style `main()` that:
    ///   1. initialises the arena, scheduler, and I/O subsystem
    ///   2. spawns `__ty_main_body` as a coroutine (Go-style: main IS a goroutine)
    ///   3. runs the scheduler to completion
    ///   4. tears down I/O and returns 0
    pub fn emit_bootstrap_main(&mut self) -> String {
        vec![
            "entry:".to_string(),
            // Arena + scheduler + I/O + Net init
            "  %arena = call i8* @slab_arena_new()".to_string(),
            "  call void @ty_sched_init()".to_string(),
            "  call void @ty_io_subsystem_init()".to_string(),
            "  call void @ty_net_init()".to_string(),
            // Cast __ty_main_body to i8* function pointer and spawn it
            "  %main_fn = bitcast void(i8*, i8*)* @__ty_main_body to i8*".to_string(),
            "  call i8* @ty_spawn(i8* %arena, i8* %main_fn, i8* null)".to_string(),
            // Run scheduler until all coroutines finish
            "  call void @ty_sched_run()".to_string(),
            "  call void @ty_sched_shutdown()".to_string(),
            "  call void @ty_net_shutdown()".to_string(),
            "  call void @ty_io_subsystem_shutdown()".to_string(),
            "  ret i32 0".to_string(),
        ]
        .join("\n")
    }
}
