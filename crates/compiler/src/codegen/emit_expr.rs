//! Expression emission
//!
//! Handles emission of all expression kinds: literals, identifiers, calls,
//! binary ops, field access, index access, casts, match, if-let, etc.

use crate::ast::*;
use crate::codegen::ir_builder::IrBuilder;
use crate::codegen::typeregistry::TypeRegistry;
use crate::codegen::{
    array_elem_type_from_str, is_no_task_intrinsic, link_symbol_name, parse_enum_from_mangled,
    runtime_intrinsic_name,
};
use crate::type_inference::InferType;

impl<'a> IrBuilder<'a> {
    // ── Expression emission ───────────────────────────────────────────────────

    pub fn emit_expr(&mut self, expr: &Expression) -> String {
        match &expr.node {
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Int(v, _),
                ..
            }) => v.to_string(),
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Bool(v),
                ..
            }) => if *v { "1" } else { "0" }.to_string(),
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Str(v),
                ..
            }) => self.emit_string(v),
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Array(elems),
                ..
            }) => {
                let elem_ty = self.reg.infer_elem_ty(elems);
                let array_ty = format!("[{} x {}]", elems.len(), elem_ty);
                let alloca = self.tmp();
                self.emit_alloca(&alloca, &array_ty);
                for (i, elem) in elems.iter().enumerate() {
                    let val = self.emit_expr(elem);
                    let gep = self.tmp();
                    self.emit(format!(
                        "  {} = getelementptr inbounds {}, {}* {}, i32 0, i32 {}",
                        gep, array_ty, array_ty, alloca, i
                    ));
                    self.emit(format!("  store {} {}, {}* {}", elem_ty, val, elem_ty, gep));
                }

                let raw = self.tmp();
                self.emit(format!(
                    "  {} = bitcast {}* {} to i8*",
                    raw, array_ty, alloca
                ));

                let out = self.tmp();
                let elem_size = self.reg.llvm_const_sizeof(&elem_ty);
                let align = self.reg.llvm_const_alignof(&elem_ty);
                let tv = self.emit_task_load();
                self.emit(format!("  {} = call %struct.TyArray* @ty_array_from_fixed(i8* {}, i8* {}, i64 {}, i64 {}, i64 {})", out, tv, raw, elems.len(), elem_size, align));
                out
            }
            ExpressionKind::Identifier(id) => {
                if let Some(slot) = self.locals.get(&id.name).cloned() {
                    let ty = self
                        .locals_type
                        .get(&id.name)
                        .cloned()
                        .unwrap_or_else(|| "i32".to_string());
                    let tmp = self.tmp();
                    self.emit(format!("  {} = load {}, {}* {}", tmp, ty, ty, slot));
                    return tmp;
                }
                "0".to_string()
            }
            ExpressionKind::StructInit { name, fields } => {
                let struct_ty = format!("%struct.{}", name.name);
                let mut cur = "undef".to_string();
                for (field_name, field_expr) in fields {
                    let val = self.emit_expr(field_expr);
                    let (idx, fty) = self.reg.struct_field_info(&name.name, &field_name.name);
                    let next = self.tmp();
                    self.emit(format!(
                        "  {} = insertvalue {} {}, {} {}, {}",
                        next, struct_ty, cur, fty, val, idx
                    ));
                    cur = next;
                }
                cur
            }
            ExpressionKind::MergeExpression { base, fields } => {
                let (mut cur, base_ty) = match base {
                    Some(b) => (self.emit_expr(b), self.expr_llvm_type(b)),
                    None => ("undef".to_string(), "%struct.?".to_string()),
                };
                let struct_name = base_ty.trim_start_matches("%struct.").to_string();
                for (field_name, field_expr) in fields {
                    let val = self.emit_expr(field_expr);
                    let (idx, fty) = self.reg.struct_field_info(&struct_name, &field_name.name);
                    let next = self.tmp();
                    self.emit(format!(
                        "  {} = insertvalue {} {}, {} {}, {}",
                        next, base_ty, cur, fty, val, idx
                    ));
                    cur = next;
                }
                cur
            }
            ExpressionKind::FieldAccess { base, field } => {
                let base_val = self.emit_expr(base);
                let base_ty = self.expr_llvm_type(base);
                let struct_name = base_ty.trim_start_matches("%struct.").to_string();
                let (idx, _) = self.reg.struct_field_info(&struct_name, &field.name);
                let tmp = self.tmp();
                self.emit(format!(
                    "  {} = extractvalue {} {}, {}",
                    tmp, base_ty, base_val, idx
                ));
                tmp
            }
            ExpressionKind::IndexAccess { base, index } => self.emit_index(expr, base, index),
            ExpressionKind::Call { func, args } => self.emit_call(expr, func, args),
            ExpressionKind::TryOperator { expr } => self.emit_expr(expr),
            ExpressionKind::Match { expr, arms } => self.emit_match_expression(expr, arms),
            ExpressionKind::BinaryOp { op, left, right } => self.emit_binop(op, left, right),
            ExpressionKind::UnaryOp { op, expr: inner } => {
                let v = self.emit_expr(inner);
                let ty = self.expr_llvm_type(inner);
                let tmp = self.tmp();
                match op {
                    Operator::Not => {
                        if ty == "i1" {
                            self.emit(format!("  {} = xor i1 {}, 1", tmp, v));
                        } else {
                            // Fallback: treat as int-like; compare to 0.
                            self.emit(format!("  {} = icmp eq {} {}, 0", tmp, ty, v));
                        }
                    }
                    Operator::Sub => {
                        if matches!(ty.as_str(), "half" | "float" | "double") {
                            self.emit(format!("  {} = fsub {} 0.0, {}", tmp, ty, v));
                        } else {
                            self.emit(format!("  {} = sub {} 0, {}", tmp, ty, v));
                        }
                    }
                    _ => return "0".to_string(),
                }
                tmp
            }
            ExpressionKind::IfLet {
                pattern,
                expr: matched,
                then,
                else_branch,
            } => self.emit_if_let(expr, pattern, matched, then, else_branch.as_deref()),
            ExpressionKind::Cast {
                expr: inner,
                target_type,
            } => self.emit_cast(inner, target_type),
            ExpressionKind::Placeholder(_) => "0".to_string(),
            _ => "0".to_string(),
        }
    }

    // ── Cast (as) ─────────────────────────────────────────────────────────────

    fn emit_cast(&mut self, inner: &Expression, target_type: &Type) -> String {
        let src_val = self.emit_expr(inner);
        let src_ty = self.expr_llvm_type(inner);
        let dst_ty = self.reg.lower_type(target_type, &self.reg.opaque_structs);

        // Recover the ACTUAL emitted type from the last emitted instruction.
        let actual_src_ty = match &inner.node {
            ExpressionKind::Call { .. } => self
                .lines
                .iter()
                .rev()
                .find_map(|l| {
                    let t = l.trim_start();
                    if t.starts_with(&format!("{} = ", src_val)) {
                        let after_eq = t.strip_prefix(&format!("{} = ", src_val)).unwrap_or("");
                        if let Some(rest) = after_eq.strip_prefix("call ") {
                            return rest.split_whitespace().next().map(|s| s.to_string());
                        }
                        if let Some(rest) = after_eq.strip_prefix("load ") {
                            return rest.split(',').next().map(|s| s.trim().to_string());
                        }
                        return after_eq.split_whitespace().next().map(|s| s.to_string());
                    }
                    None
                })
                .unwrap_or_else(|| src_ty.clone()),
            _ => src_ty.clone(),
        };

        if actual_src_ty == dst_ty {
            return src_val;
        }

        let int_bits = |t: &str| -> Option<u32> {
            match t {
                "i1" => Some(1),
                "i8" => Some(8),
                "i16" => Some(16),
                "i32" => Some(32),
                "i64" => Some(64),
                _ => None,
            }
        };
        let float_bits = |t: &str| -> Option<u32> {
            match t {
                "half" => Some(16),
                "float" => Some(32),
                "double" => Some(64),
                _ => None,
            }
        };

        let tmp = self.tmp();
        let instr = match (
            int_bits(&actual_src_ty),
            int_bits(&dst_ty),
            float_bits(&actual_src_ty),
            float_bits(&dst_ty),
        ) {
            (Some(s), Some(d), _, _) if s < d => {
                format!(
                    "  {} = sext {} {} to {}",
                    tmp, actual_src_ty, src_val, dst_ty
                )
            }
            (Some(s), Some(d), _, _) if s > d => {
                format!(
                    "  {} = trunc {} {} to {}",
                    tmp, actual_src_ty, src_val, dst_ty
                )
            }
            (_, _, Some(s), Some(d)) if s < d => {
                format!(
                    "  {} = fpext {} {} to {}",
                    tmp, actual_src_ty, src_val, dst_ty
                )
            }
            (_, _, Some(s), Some(d)) if s > d => {
                format!(
                    "  {} = fptrunc {} {} to {}",
                    tmp, actual_src_ty, src_val, dst_ty
                )
            }
            (Some(_), _, _, Some(_)) => {
                format!(
                    "  {} = sitofp {} {} to {}",
                    tmp, actual_src_ty, src_val, dst_ty
                )
            }
            (_, Some(_), Some(_), _) => {
                format!(
                    "  {} = fptosi {} {} to {}",
                    tmp, actual_src_ty, src_val, dst_ty
                )
            }
            _ if actual_src_ty.ends_with('*') && dst_ty.ends_with('*') => {
                format!(
                    "  {} = bitcast {} {} to {}",
                    tmp, actual_src_ty, src_val, dst_ty
                )
            }
            (Some(_), _, _, _) if dst_ty.ends_with('*') => {
                format!(
                    "  {} = inttoptr {} {} to {}",
                    tmp, actual_src_ty, src_val, dst_ty
                )
            }
            (_, Some(_), _, _) if actual_src_ty.ends_with('*') => {
                format!(
                    "  {} = ptrtoint {} {} to {}",
                    tmp, actual_src_ty, src_val, dst_ty
                )
            }
            _ => {
                format!(
                    "  {} = bitcast {} {} to {}",
                    tmp, actual_src_ty, src_val, dst_ty
                )
            }
        };
        self.emit(instr);
        tmp
    }

    // ── Binary operations ─────────────────────────────────────────────────────

    fn emit_binop(&mut self, op: &Operator, left: &Expression, right: &Expression) -> String {
        if *op == Operator::Assign {
            let (slot, lval_ty) = self.resolve_lvalue(left);
            let rhs_val = self.emit_expr(right);
            self.emit(format!(
                "  store {} {}, {}* {}",
                lval_ty, rhs_val, lval_ty, slot
            ));
            return rhs_val;
        }

        if matches!(
            op,
            Operator::AddAssign | Operator::SubAssign | Operator::MulAssign | Operator::DivAssign
        ) {
            return self.emit_assign_op(op, left, right);
        }

        if *op == Operator::Pipe {
            return self.emit_pipe(left, right);
        }

        let ty = self.expr_llvm_type(left);
        let lhs_raw = self.emit_expr(left);
        let lhs_ty = self.expr_llvm_type(left);
        let lhs = self.emit_widen(&lhs_raw, &lhs_ty, &ty);
        let rhs_raw = self.emit_expr(right);
        let rhs_ty = self.expr_llvm_type(right);
        let rhs = self.emit_widen(&rhs_raw, &rhs_ty, &ty);
        let dst = self.tmp();
        let instr = self.arith_instr(op, &ty, &lhs, &rhs, &dst);
        self.emit(instr);
        dst
    }

    fn arith_instr(&self, op: &Operator, ty: &str, lhs: &str, rhs: &str, dst: &str) -> String {
        let is_float = matches!(ty, "float" | "double" | "half");
        let is_bool = ty == "i1";
        if is_float {
            match op {
                Operator::Add | Operator::AddAssign => {
                    format!("  {} = fadd {} {}, {}", dst, ty, lhs, rhs)
                }
                Operator::Sub | Operator::SubAssign => {
                    format!("  {} = fsub {} {}, {}", dst, ty, lhs, rhs)
                }
                Operator::Mul | Operator::MulAssign => {
                    format!("  {} = fmul {} {}, {}", dst, ty, lhs, rhs)
                }
                Operator::Div | Operator::DivAssign => {
                    format!("  {} = fdiv {} {}, {}", dst, ty, lhs, rhs)
                }
                Operator::Mod => format!("  {} = frem {} {}, {}", dst, ty, lhs, rhs),
                Operator::Eq => format!("  {} = fcmp oeq {} {}, {}", dst, ty, lhs, rhs),
                Operator::Ne => format!("  {} = fcmp one {} {}, {}", dst, ty, lhs, rhs),
                Operator::Lt => format!("  {} = fcmp olt {} {}, {}", dst, ty, lhs, rhs),
                Operator::Gt => format!("  {} = fcmp ogt {} {}, {}", dst, ty, lhs, rhs),
                Operator::Le => format!("  {} = fcmp ole {} {}, {}", dst, ty, lhs, rhs),
                Operator::Ge => format!("  {} = fcmp oge {} {}, {}", dst, ty, lhs, rhs),
                _ => format!("  {} = fadd {} {}, {}", dst, ty, lhs, rhs),
            }
        } else if is_bool {
            match op {
                Operator::And | Operator::BitAnd => format!("  {} = and i1 {}, {}", dst, lhs, rhs),
                Operator::Or | Operator::BitOr => format!("  {} = or i1 {}, {}", dst, lhs, rhs),
                Operator::Eq => format!("  {} = icmp eq i1 {}, {}", dst, lhs, rhs),
                Operator::Ne => format!("  {} = icmp ne i1 {}, {}", dst, lhs, rhs),
                _ => format!("  {} = or i1 {}, {}", dst, lhs, rhs),
            }
        } else {
            match op {
                Operator::Add | Operator::AddAssign => {
                    format!("  {} = add {} {}, {}", dst, ty, lhs, rhs)
                }
                Operator::Sub | Operator::SubAssign => {
                    format!("  {} = sub {} {}, {}", dst, ty, lhs, rhs)
                }
                Operator::Mul | Operator::MulAssign => {
                    format!("  {} = mul {} {}, {}", dst, ty, lhs, rhs)
                }
                Operator::Div | Operator::DivAssign => {
                    format!("  {} = sdiv {} {}, {}", dst, ty, lhs, rhs)
                }
                Operator::Mod => format!("  {} = srem {} {}, {}", dst, ty, lhs, rhs),
                Operator::Eq => format!("  {} = icmp eq {} {}, {}", dst, ty, lhs, rhs),
                Operator::Ne => format!("  {} = icmp ne {} {}, {}", dst, ty, lhs, rhs),
                Operator::Lt => format!("  {} = icmp slt {} {}, {}", dst, ty, lhs, rhs),
                Operator::Gt => format!("  {} = icmp sgt {} {}, {}", dst, ty, lhs, rhs),
                Operator::Le => format!("  {} = icmp sle {} {}, {}", dst, ty, lhs, rhs),
                Operator::Ge => format!("  {} = icmp sge {} {}, {}", dst, ty, lhs, rhs),
                Operator::And => format!("  {} = and {} {}, {}", dst, ty, lhs, rhs),
                Operator::Or => format!("  {} = or {} {}, {}", dst, ty, lhs, rhs),
                Operator::BitAnd => format!("  {} = and {} {}, {}", dst, ty, lhs, rhs),
                Operator::BitOr => format!("  {} = or {} {}, {}", dst, ty, lhs, rhs),
                Operator::BitXor => format!("  {} = xor {} {}, {}", dst, ty, lhs, rhs),
                Operator::Shl => format!("  {} = shl {} {}, {}", dst, ty, lhs, rhs),
                Operator::Shr => format!("  {} = lshr {} {}, {}", dst, ty, lhs, rhs),
                _ => format!("  {} = add {} {}, {}", dst, ty, lhs, rhs),
            }
        }
    }

    fn emit_assign_op(&mut self, op: &Operator, left: &Expression, right: &Expression) -> String {
        let (slot, lval_ty) = self.resolve_lvalue(left);
        let lhs_raw = self.tmp();
        self.emit(format!(
            "  {} = load {}, {}* {}",
            lhs_raw, lval_ty, lval_ty, slot
        ));
        let lhs_val = self.emit_widen(&lhs_raw, &lval_ty, &lval_ty);
        let rhs_raw = self.emit_expr(right);
        let rhs_ty = self.expr_llvm_type(right);
        let rhs_val = self.emit_widen(&rhs_raw, &rhs_ty, &lval_ty);
        let res = self.tmp();
        let instr = self.arith_instr(op, &lval_ty, &lhs_val, &rhs_val, &res);
        self.emit(instr);
        self.emit(format!(
            "  store {} {}, {}* {}",
            lval_ty, res, lval_ty, slot
        ));
        res
    }

    /// Resolve an lvalue expression to its (alloca_slot, element_type).
    fn resolve_lvalue(&mut self, expr: &Expression) -> (String, String) {
        match &expr.node {
            ExpressionKind::Identifier(id) => {
                let slot = self.locals.get(&id.name).cloned().unwrap_or_else(|| {
                    self.emit(format!("  ; undefined lvalue: {}", id.name));
                    "null ; UNDEFINED".to_string()
                });
                let ty = self
                    .locals_type
                    .get(&id.name)
                    .cloned()
                    .unwrap_or_else(|| "i32".to_string());
                (slot, ty)
            }
            ExpressionKind::IndexAccess { base, index } => {
                let (base_ptr, array_ty) = match &base.node {
                    ExpressionKind::Identifier(id) => (
                        self.locals
                            .get(&id.name)
                            .cloned()
                            .unwrap_or(id.name.clone()),
                        self.locals_type
                            .get(&id.name)
                            .cloned()
                            .unwrap_or_else(|| "[0 x i32]".to_string()),
                    ),
                    _ => (self.emit_expr(base), "[0 x i32]".to_string()),
                };
                let elem_ty = array_elem_type_from_str(&array_ty);
                let idx_val = self.emit_expr(index);
                let gep = self.tmp();
                self.emit(format!(
                    "  {} = getelementptr inbounds {}, {}* {}, i32 0, i32 {}",
                    gep, array_ty, array_ty, base_ptr, idx_val
                ));
                (gep, elem_ty)
            }
            ExpressionKind::FieldAccess { base, field } => {
                let (base_ptr, base_ty) = match &base.node {
                    ExpressionKind::Identifier(id) => (
                        self.locals
                            .get(&id.name)
                            .cloned()
                            .unwrap_or(id.name.clone()),
                        self.locals_type
                            .get(&id.name)
                            .cloned()
                            .unwrap_or_else(|| "%struct.?".to_string()),
                    ),
                    _ => (self.emit_expr(base), "%struct.?".to_string()),
                };
                let struct_name = base_ty.trim_start_matches("%struct.").to_string();
                let (idx, fty) = self.reg.struct_field_info(&struct_name, &field.name);
                let gep = self.tmp();
                self.emit(format!(
                    "  {} = getelementptr inbounds {}, {}* {}, i32 0, i32 {}",
                    gep, base_ty, base_ty, base_ptr, idx
                ));
                (gep, fty)
            }
            _ => ("0".to_string(), "i32".to_string()),
        }
    }

    fn emit_pipe(&mut self, left: &Expression, right: &Expression) -> String {
        let ExpressionKind::Call { func, args } = &right.node else {
            self.emit_expr(left);
            self.emit_expr(right);
            return "0".to_string();
        };
        let ExpressionKind::Identifier(id) = &func.node else {
            self.emit_expr(left);
            self.emit_expr(right);
            return "0".to_string();
        };

        let lhs = self.emit_expr(left);
        let lhs_ty = self.expr_llvm_type(left);
        let (ret_ty, param_types) = self
            .reg
            .func_sigs
            .get(&id.name)
            .cloned()
            .unwrap_or_else(|| ("i32".to_string(), vec![]));

        let no_task = is_no_task_intrinsic(&id.name) || self.reg.extern_fns.contains(&id.name);
        let mut arg_pairs = Vec::new();
        if !no_task {
            let tv = self.emit_task_load();
            arg_pairs.push(format!("i8* {}", tv));
        }

        let first_user_ty = if no_task {
            param_types.get(0)
        } else {
            param_types.get(1)
        }
        .cloned()
        .unwrap_or(lhs_ty);
        arg_pairs.push(format!("{} {}", first_user_ty, lhs));

        let offset = if no_task { 1 } else { 2 };
        for (i, a) in args.iter().enumerate() {
            let v = self.emit_expr(a);
            let actual_ty = self.expr_llvm_type(a);
            let t = param_types
                .get(i + offset)
                .cloned()
                .unwrap_or_else(|| "i32".to_string());
            let v = self.emit_widen(&v, &actual_ty, &t);
            arg_pairs.push(format!("{} {}", t, v));
        }
        let tmp = self.tmp();
        self.emit(format!(
            "  {} = call {} @{}({})",
            tmp,
            ret_ty,
            id.name,
            arg_pairs.join(", ")
        ));
        tmp
    }

    // ── Index access ──────────────────────────────────────────────────────────

    fn emit_index(&mut self, expr: &Expression, base: &Expression, index: &Expression) -> String {
        let base_val = self.emit_expr(base);
        let base_ty = self.expr_llvm_type(base);
        let idx_val = self.emit_expr(index);

        let Some((opt_ty, elem_ty)) = self.option_type_for_index(expr) else {
            return "0".to_string();
        };

        if base_ty == "%struct.TyArray*" {
            let idx64 = self.tmp();
            self.emit(format!("  {} = sext i32 {} to i64", idx64, idx_val));
            let raw_ptr = self.tmp();
            self.emit(format!(
                "  {} = call i8* @ty_array_get_ptr(%struct.TyArray* {}, i64 {})",
                raw_ptr, base_val, idx64
            ));
            return self.emit_some_none_from_i8_ptr(&opt_ty, &elem_ty, &raw_ptr);
        }

        // Fixed array
        let (base_ptr, array_ty) = match &base.node {
            ExpressionKind::Identifier(id) => (
                self.locals
                    .get(&id.name)
                    .cloned()
                    .unwrap_or(id.name.clone()),
                self.locals_type
                    .get(&id.name)
                    .cloned()
                    .unwrap_or_else(|| "[0 x i32]".to_string()),
            ),
            _ => (base_val, base_ty),
        };
        if !array_ty.starts_with('[') {
            return "0".to_string();
        }

        let len = self.reg.fixed_array_len(&array_ty).unwrap_or(0);
        let in_bounds = self.tmp();
        self.emit(format!(
            "  {} = icmp ult i32 {}, {}",
            in_bounds, idx_val, len
        ));
        let some_lbl = self.label("idx_some");
        let none_lbl = self.label("idx_none");
        let merge_lbl = self.label("idx_merge");
        self.emit(format!(
            "  br i1 {}, label %{}, label %{}",
            in_bounds, some_lbl, none_lbl
        ));

        self.emit(format!("{}:", some_lbl));
        let gep = self.tmp();
        self.emit(format!(
            "  {} = getelementptr inbounds {}, {}* {}, i32 0, i32 {}",
            gep, array_ty, array_ty, base_ptr, idx_val
        ));
        let loaded = self.tmp();
        self.emit(format!(
            "  {} = load {}, {}* {}",
            loaded, elem_ty, elem_ty, gep
        ));
        let some_val = self.emit_enum_value(&opt_ty, "Some", Some((&elem_ty, &loaded)));
        self.emit(format!("  br label %{}", merge_lbl));

        self.emit(format!("{}:", none_lbl));
        let none_val = self.emit_enum_value(&opt_ty, "None", None);
        self.emit(format!("  br label %{}", merge_lbl));

        self.emit(format!("{}:", merge_lbl));
        let phi = self.tmp();
        self.emit(format!(
            "  {} = phi {} [ {}, %{} ], [ {}, %{} ]",
            phi, opt_ty, some_val, some_lbl, none_val, none_lbl
        ));
        phi
    }

    // ── Call emission ─────────────────────────────────────────────────────────

    fn emit_call(
        &mut self,
        call_expr: &Expression,
        func: &Expression,
        args: &[Expression],
    ) -> String {
        if let ExpressionKind::FieldAccess { base, field } = &func.node {
            return self.emit_method_call(call_expr, base, field, args);
        }
        if let ExpressionKind::Identifier(id) = &func.node {
            return self.emit_free_call(call_expr, id, args);
        }
        "0".to_string()
    }

    fn emit_method_call(
        &mut self,
        call_expr: &Expression,
        base: &Expression,
        field: &Identifier,
        args: &[Expression],
    ) -> String {
        let base_val = self.emit_expr(base);
        let base_ty = self.expr_llvm_type(base);

        if base_ty == "i8*" {
            match field.name.as_str() {
                "send" => return self.emit_chan_send(&base_val, args),
                "recv" => return self.emit_chan_recv(call_expr, &base_val),
                "try_recv" => return self.emit_chan_try_recv(call_expr, &base_val),
                _ => {}
            }
        }

        if base_ty == "%struct.Str*" {
            match field.name.as_str() {
                "length" => return self.emit_str_length(&base_val),
                "at" => return self.emit_str_at(&base_val, args),
                _ => {}
            }
        }

        if base_ty == "%struct.TyArray*" && field.name == "push" {
            return self.emit_array_push(&base_val, args);
        }

        if let Some(method_sym) = self.reg.method_symbol_for_call(&base_ty, &field.name) {
            return self.emit_user_method_call(call_expr, &method_sym, &base_val, &base_ty, args);
        }

        "0".to_string()
    }

    fn emit_str_length(&mut self, str_val: &str) -> String {
        let len_field = self.tmp();
        self.emit(format!(
            "  {} = getelementptr inbounds %struct.Str, %struct.Str* {}, i32 0, i32 1",
            len_field, str_val
        ));
        let result = self.tmp();
        self.emit(format!("  {} = load i32, i32* {}", result, len_field));
        result
    }

    fn emit_str_at(&mut self, str_val: &str, args: &[Expression]) -> String {
        let idx_val = args
            .first()
            .map(|a| self.emit_expr(a))
            .unwrap_or_else(|| "0".to_string());

        let ptr_field = self.tmp();
        self.emit(format!(
            "  {} = getelementptr inbounds %struct.Str, %struct.Str* {}, i32 0, i32 0",
            ptr_field, str_val
        ));
        let ptr = self.tmp();
        self.emit(format!("  {} = load i8*, i8** {}", ptr, ptr_field));
        let byte_ptr = self.tmp();
        self.emit(format!(
            "  {} = getelementptr inbounds i8, i8* {}, i64 {}",
            byte_ptr, ptr, idx_val
        ));
        let result = self.tmp();
        self.emit(format!("  {} = load i8, i8* {}", result, byte_ptr));
        result
    }

    fn emit_chan_send(&mut self, chan_val: &str, args: &[Expression]) -> String {
        if let Some(arg0) = args.first() {
            let val = self.emit_expr(arg0);
            let val_ty = self
                .actual_inferred_type(arg0)
                .map(|t| self.reg.lower_infer_type(&t))
                .filter(|t| t != "void")
                .or_else(|| {
                    let t = self.expr_llvm_type(arg0);
                    if t != "void" {
                        Some(t)
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    self.lines
                        .iter()
                        .rev()
                        .find(|l| {
                            l.trim_start().starts_with(&format!("{} =", val))
                                || (l.contains("= load ") && l.contains(&val))
                        })
                        .and_then(|l| {
                            l.trim_start()
                                .strip_prefix(&format!("{} = load ", val))
                                .and_then(|rest| rest.split(',').next())
                                .map(|t| t.trim().to_string())
                        })
                        .filter(|t| t != "void")
                })
                .unwrap_or_else(|| "i32".to_string());
            let slot = self.tmp();
            self.emit_alloca(&slot, &val_ty);
            self.emit(format!("  store {} {}, {}* {}", val_ty, val, val_ty, slot));
            let raw = self.tmp();
            self.emit(format!("  {} = bitcast {}* {} to i8*", raw, val_ty, slot));
            let tv = self.emit_task_load();
            self.emit(format!(
                "  call void @ty_chan_send(i8* {}, i8* {}, i8* {})",
                tv, chan_val, raw
            ));
        }
        "0".to_string()
    }

    fn emit_chan_recv(&mut self, call_expr: &Expression, chan_val: &str) -> String {
        let mut elem_from_chan_tys: Option<InferType> = None;
        if let ExpressionKind::Call { func, .. } = &call_expr.node {
            if let ExpressionKind::FieldAccess { base, .. } = &func.node {
                if let ExpressionKind::Identifier(id) = &base.node {
                    if let Some(elem_ty) = self.chan_elem_tys.get(&id.name) {
                        elem_from_chan_tys = Some(InferType::Con(elem_ty.clone()));
                    }
                }
            }
        }

        let elem_infer = if let Some(elem) = elem_from_chan_tys {
            elem
        } else if let Some(inferred) = self.inferred_expr_type(call_expr).cloned() {
            let opt_name = self.enum_name_for_variant("Some");
            match inferred {
                InferType::App(ref name, ref args) if name == &opt_name && args.len() == 1 => {
                    args[0].clone()
                }
                other => other,
            }
        } else {
            return "0".to_string();
        };

        let elem_ty = self.reg.lower_infer_type(&elem_infer);
        let opt_name = self.enum_name_for_variant("Some");
        let opt_infer = InferType::App(opt_name.clone(), vec![elem_infer]);
        self.reg.ensure_enum_layout_for_infer(&opt_infer);
        let opt_ty = TypeRegistry::mangle_app_struct_name(&opt_name, &[elem_ty.clone()]);
        let out_slot = self.tmp();
        self.emit_alloca(&out_slot, &opt_ty);
        let out_raw = self.tmp();
        self.emit(format!(
            "  {} = bitcast {}* {} to i8*",
            out_raw, opt_ty, out_slot
        ));
        let tv = self.emit_task_load();
        self.emit(format!(
            "  call void @ty_chan_recv(i8* {}, i8* {}, i8* {})",
            tv, chan_val, out_raw
        ));
        let loaded = self.tmp();
        self.emit(format!(
            "  {} = load {}, {}* {}",
            loaded, opt_ty, opt_ty, out_slot
        ));
        loaded
    }

    fn emit_chan_try_recv(&mut self, call_expr: &Expression, chan_val: &str) -> String {
        let elem_src = if let ExpressionKind::Call { func, .. } = &call_expr.node {
            if let ExpressionKind::FieldAccess { base, .. } = &func.node {
                if let ExpressionKind::Identifier(id) = &base.node {
                    self.chan_elem_tys.get(&id.name).cloned()
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
        .unwrap_or_else(|| "Int32".to_string());

        let opt_name = self.enum_name_for_variant("Some");
        let inner_infer = InferType::Con(elem_src);
        let ty = InferType::App(opt_name.clone(), vec![inner_infer.clone()]);
        let elem_ty = self.reg.lower_infer_type(&inner_infer);
        let opt_ty = TypeRegistry::mangle_app_struct_name(&opt_name, &[elem_ty.clone()]);
        self.reg.ensure_enum_layout_for_infer(&ty);

        let out_slot = self.tmp();
        self.emit_alloca(&out_slot, &elem_ty);
        let out_raw = self.tmp();
        self.emit(format!(
            "  {} = bitcast {}* {} to i8*",
            out_raw, elem_ty, out_slot
        ));

        let poll_lbl = self.label("try_recv_poll");
        let some_lbl = self.label("try_recv_some");
        let none_lbl = self.label("try_recv_none");
        let empty_lbl = self.label("try_recv_empty");
        let wait_lbl = self.label("try_recv_wait");
        let merge_lbl = self.label("try_recv_merge");

        self.emit(format!("  br label %{}", poll_lbl));
        self.emit(format!("{}:", poll_lbl));
        let success = self.tmp();
        let tv = self.emit_task_load();
        self.emit(format!(
            "  {} = call i32 @ty_chan_try_recv(i8* {}, i8* {}, i8* {})",
            success, tv, chan_val, out_raw
        ));
        let got_value = self.tmp();
        self.emit(format!("  {} = icmp eq i32 {}, 1", got_value, success));
        self.emit(format!(
            "  br i1 {}, label %{}, label %{}",
            got_value, some_lbl, empty_lbl
        ));

        self.emit(format!("{}:", some_lbl));
        let loaded = self.tmp();
        self.emit(format!(
            "  {} = load {}, {}* {}",
            loaded, elem_ty, elem_ty, out_slot
        ));
        let some_val = self.emit_enum_value(&opt_ty, "Some", Some((&elem_ty, &loaded)));
        self.emit(format!("  br label %{}", merge_lbl));

        self.emit(format!("{}:", empty_lbl));
        let is_closed = self.tmp();
        self.emit(format!("  {} = icmp slt i32 {}, 0", is_closed, success));
        self.emit(format!(
            "  br i1 {}, label %{}, label %{}",
            is_closed, none_lbl, wait_lbl
        ));

        self.emit(format!("{}:", wait_lbl));
        self.emit("  call void @ty_yield()".to_string());
        self.emit(format!("  br label %{}", poll_lbl));

        self.emit(format!("{}:", none_lbl));
        let none_val = self.emit_enum_value(&opt_ty, "None", None);
        self.emit(format!("  br label %{}", merge_lbl));

        self.emit(format!("{}:", merge_lbl));
        let phi = self.tmp();
        self.emit(format!(
            "  {} = phi {} [ {}, %{} ], [ {}, %{} ]",
            phi, opt_ty, some_val, some_lbl, none_val, none_lbl
        ));
        phi
    }

    fn emit_array_push(&mut self, base_val: &str, args: &[Expression]) -> String {
        if let Some(arg0) = args.first() {
            let val = self.emit_expr(arg0);
            let val_ty = self.expr_llvm_type(arg0);
            let slot = self.tmp();
            self.emit_alloca(&slot, &val_ty);
            self.emit(format!("  store {} {}, {}* {}", val_ty, val, val_ty, slot));
            let raw = self.tmp();
            self.emit(format!("  {} = bitcast {}* {} to i8*", raw, val_ty, slot));
            let tv = self.emit_task_load();
            self.emit(format!(
                "  call void @ty_array_push(i8* {}, %struct.TyArray* {}, i8* {})",
                tv, base_val, raw
            ));
        }
        "0".to_string()
    }

    fn emit_user_method_call(
        &mut self,
        call_expr: &Expression,
        method_sym: &str,
        base_val: &str,
        base_ty: &str,
        args: &[Expression],
    ) -> String {
        let runtime_name = link_symbol_name(method_sym);
        let (ret_ty, param_types) = self
            .reg
            .func_sigs
            .get(method_sym)
            .cloned()
            .unwrap_or_else(|| ("".to_string(), vec![]));
        let is_extern = self.reg.extern_fns.contains(method_sym);
        let self_ty = if is_extern {
            param_types.get(0)
        } else {
            param_types.get(1)
        }
        .cloned()
        .unwrap_or_else(|| base_ty.to_string());
        let mut arg_pairs = if is_extern {
            vec![format!("{} {}", self_ty, base_val)]
        } else {
            let tv = self.emit_task_load();
            vec![format!("i8* {}", tv), format!("{} {}", self_ty, base_val)]
        };
        let param_offset = if is_extern { 1 } else { 2 };
        for (i, a) in args.iter().enumerate() {
            let v = self.emit_expr(a);
            let actual_ty = self.expr_llvm_type(a);
            let t = param_types
                .get(i + param_offset)
                .cloned()
                .unwrap_or_else(|| actual_ty.clone());
            let v = self.emit_widen(&v, &actual_ty, &t);
            arg_pairs.push(format!("{} {}", t, v));
        }
        let tmp = self.tmp();
        if ret_ty == "void" {
            let has_out_param = param_types.len() > arg_pairs.len();
            let last_param = param_types.last().cloned().unwrap_or_default();
            if has_out_param {
                if let Some(desired_ty) = last_param
                    .strip_suffix('*')
                    .filter(|t| t.starts_with("%struct."))
                    .map(|t| t.to_string())
                {
                    let out_slot = self.tmp();
                    self.emit_alloca(&out_slot, &desired_ty);
                    arg_pairs.push(format!("{}* {}", desired_ty, out_slot));
                    self.emit(format!(
                        "  call void @{}({})",
                        runtime_name,
                        arg_pairs.join(", ")
                    ));
                    let loaded = self.tmp();
                    self.emit(format!(
                        "  {} = load {}, {}* {}",
                        loaded, desired_ty, desired_ty, out_slot
                    ));
                    return loaded;
                }
            }
            self.emit(format!(
                "  call void @{}({})",
                runtime_name,
                arg_pairs.join(", ")
            ));
            return "0".to_string();
        }
        let effective_ret = if ret_ty.is_empty() {
            self.inferred_expr_type(call_expr)
                .cloned()
                .map(|t| self.reg.lower_infer_type(&t))
                .filter(|t| !t.is_empty() && t != "void")
                .unwrap_or_else(|| "i32".to_string())
        } else {
            ret_ty.clone()
        };
        self.emit(format!(
            "  {} = call {} @{}({})",
            tmp,
            effective_ret,
            runtime_name,
            arg_pairs.join(", ")
        ));
        tmp
    }

    fn emit_free_call(
        &mut self,
        call_expr: &Expression,
        id: &Identifier,
        args: &[Expression],
    ) -> String {
        let is_variant = self
            .reg
            .enum_defs
            .values()
            .any(|def| def.variants.iter().any(|v| v.name == id.name));
        if matches!(id.name.as_str(), "Ok" | "Err" | "Some" | "None") || is_variant {
            return self.emit_adt_constructor(&id.name, call_expr, args);
        }
        if id.name == "chan" {
            let mut elem_llvm_ty = "i8".to_string();
            if let Some(infer) = self.inferred_expr_type(call_expr).cloned() {
                let inner = match infer {
                    InferType::App(n, mut a) if n == "Ref" && a.len() == 1 => a.remove(0),
                    other => other,
                };
                if let InferType::App(n, a) = inner {
                    if n == "Chan" && a.len() == 1 {
                        elem_llvm_ty = self.reg.lower_infer_type(&a[0]);
                    }
                }
            }
            let tmp = self.tmp();
            self.emit(format!(
                "  {} = call i8* @ty_chan_new(i64 {}, i64 64)",
                tmp,
                self.reg.llvm_const_sizeof(&elem_llvm_ty)
            ));
            return tmp;
        }

        let runtime_name =
            runtime_intrinsic_name(&id.name).unwrap_or_else(|| link_symbol_name(&id.name));
        let out_result = self.reg.out_result_funcs.contains(&id.name);
        let (ret_ty, param_types) = self
            .reg
            .func_sigs
            .get(&id.name)
            .cloned()
            .unwrap_or_else(|| ("i32".to_string(), vec![]));

        let tail = if self.current_fn_name.as_deref() == Some(id.name.as_str()) {
            "tail "
        } else {
            ""
        };
        let no_task = is_no_task_intrinsic(&runtime_name) || self.reg.extern_fns.contains(&id.name);
        let mut arg_pairs = Vec::new();
        if !no_task {
            let tv = self.emit_task_load();
            arg_pairs.push(format!("i8* {}", tv));
        }
        let param_offset = if no_task { 0 } else { 1 };
        for (i, arg) in args.iter().enumerate() {
            let v = self.emit_expr(arg);
            let actual_ty = self.expr_llvm_type(arg);
            let t = param_types
                .get(i + param_offset)
                .cloned()
                .unwrap_or_else(|| actual_ty.clone());
            let v = self.emit_widen(&v, &actual_ty, &t);
            arg_pairs.push(format!("{} {}", t, v));
        }
        if out_result {
            let result_ptr_ty = param_types
                .last()
                .cloned()
                .unwrap_or_else(|| "i8*".to_string());
            let result_ty = result_ptr_ty.trim_end_matches('*').to_string();
            {
                let unmangled = result_ty.strip_prefix("%struct.").unwrap_or(&result_ty);
                if let Some((enum_name, payload_llvm_types)) = parse_enum_from_mangled(unmangled) {
                    let args: Vec<InferType> = payload_llvm_types
                        .iter()
                        .map(|t| InferType::Con(TypeRegistry::llvm_ty_to_infer_name(t)))
                        .collect();
                    let result_infer = InferType::App(enum_name, args);
                    self.reg.ensure_enum_layout_for_infer(&result_infer);
                }
            }
            let result_slot = self.tmp();
            self.emit_alloca(&result_slot, &result_ty);
            arg_pairs.push(format!("{} {}", result_ptr_ty, result_slot));
            self.emit(format!(
                "  {}call void @{}({})",
                tail,
                runtime_name,
                arg_pairs.join(", ")
            ));
            let loaded = self.tmp();
            self.emit(format!(
                "  {} = load {}, {}* {}",
                loaded, result_ty, result_ty, result_slot
            ));
            return loaded;
        }
        if ret_ty == "void" {
            self.emit(format!(
                "  {}call void @{}({})",
                tail,
                runtime_name,
                arg_pairs.join(", ")
            ));
            return "0".to_string();
        }
        let tmp = self.tmp();
        self.emit(format!(
            "  {} = {}call {} @{}({})",
            tmp,
            tail,
            ret_ty,
            runtime_name,
            arg_pairs.join(", ")
        ));
        tmp
    }

    // ── Enum value construction ────────────────────────────────────────────────

    fn emit_enum_value(
        &mut self,
        enum_ty: &str,
        ctor: &str,
        payload: Option<(&str, &str)>,
    ) -> String {
        let layout = self
            .reg
            .enum_layouts
            .get(enum_ty)
            .cloned()
            .unwrap_or_else(|| panic!("missing enum layout for {enum_ty}"));
        let v = layout
            .variants
            .get(ctor)
            .cloned()
            .unwrap_or_else(|| panic!("unknown enum ctor {ctor} for {enum_ty}"));

        let t0 = self.tmp();
        self.emit(format!(
            "  {} = insertvalue {} undef, {} {}, 0",
            t0, enum_ty, layout.tag_ty, v.tag_value
        ));
        let mut cur = t0;
        if let (Some((payload_ty, payload_val)), Some(idx)) = (payload, v.payload_index) {
            let t1 = self.tmp();
            self.emit(format!(
                "  {} = insertvalue {} {}, {} {}, {}",
                t1, enum_ty, cur, payload_ty, payload_val, idx
            ));
            cur = t1;
        }
        cur
    }

    fn emit_some_none_from_i8_ptr(&mut self, opt_ty: &str, elem_ty: &str, ptr_i8: &str) -> String {
        let cond = self.tmp();
        self.emit(format!("  {} = icmp ne i8* {}, null", cond, ptr_i8));
        let some_lbl = self.label("opt_some");
        let none_lbl = self.label("opt_none");
        let merge_lbl = self.label("opt_merge");
        self.emit(format!(
            "  br i1 {}, label %{}, label %{}",
            cond, some_lbl, none_lbl
        ));

        self.emit(format!("{}:", some_lbl));
        let typed_ptr = self.tmp();
        self.emit(format!(
            "  {} = bitcast i8* {} to {}*",
            typed_ptr, ptr_i8, elem_ty
        ));
        let loaded = self.tmp();
        self.emit(format!(
            "  {} = load {}, {}* {}",
            loaded, elem_ty, elem_ty, typed_ptr
        ));
        let some_val = self.emit_enum_value(opt_ty, "Some", Some((elem_ty, &loaded)));
        self.emit(format!("  br label %{}", merge_lbl));

        self.emit(format!("{}:", none_lbl));
        let none_val = self.emit_enum_value(opt_ty, "None", None);
        self.emit(format!("  br label %{}", merge_lbl));

        self.emit(format!("{}:", merge_lbl));
        let phi = self.tmp();
        self.emit(format!(
            "  {} = phi {} [ {}, %{} ], [ {}, %{} ]",
            phi, opt_ty, some_val, some_lbl, none_val, none_lbl
        ));
        phi
    }

    fn emit_adt_constructor(
        &mut self,
        ctor: &str,
        call_expr: &Expression,
        args: &[Expression],
    ) -> String {
        let Some(types_ptr) = self.types else {
            return "0".to_string();
        };
        let types = unsafe { &*types_ptr };
        let Some(infer) = types.get(&call_expr.id).cloned() else {
            return "0".to_string();
        };

        let opt_name = self.enum_name_for_variant("Some");
        let res_name = self.enum_name_for_variant("Ok");
        let infer = match (&infer, ctor) {
            (_, "Some" | "None") if !matches!(&infer, InferType::App(n, _) if n == &opt_name) => {
                InferType::App(opt_name.clone(), vec![infer])
            }
            (_, "Ok") if !matches!(&infer, InferType::App(n, _) if n == &res_name) => {
                InferType::App(
                    res_name.clone(),
                    vec![infer, InferType::Con("Int32".to_string())],
                )
            }
            (_, "Err") if !matches!(&infer, InferType::App(n, _) if n == &res_name) => {
                InferType::App(
                    res_name.clone(),
                    vec![InferType::Con("Int32".to_string()), infer],
                )
            }
            _ => infer,
        };

        self.reg.ensure_enum_layout_for_infer(&infer);
        let ty = self.reg.lower_infer_type(&infer);

        let layout = self
            .reg
            .enum_layouts
            .get(&ty)
            .cloned()
            .unwrap_or_else(|| panic!("missing enum layout for {ty}"));
        let v = layout
            .variants
            .get(ctor)
            .cloned()
            .unwrap_or_else(|| panic!("unknown enum ctor {ctor} for {ty}"));

        let t0 = self.tmp();
        self.emit(format!(
            "  {} = insertvalue {} undef, {} {}, 0",
            t0, ty, layout.tag_ty, v.tag_value
        ));
        let mut cur = t0;
        if let (Some(idx), Some(payload_ty)) = (v.payload_index, v.payload_ty) {
            let payload = args
                .first()
                .map(|e| self.emit_expr(e))
                .unwrap_or_else(|| "0".to_string());
            let t1 = self.tmp();
            self.emit(format!(
                "  {} = insertvalue {} {}, {} {}, {}",
                t1, ty, cur, payload_ty, payload, idx
            ));
            cur = t1;
        }
        cur
    }
}
