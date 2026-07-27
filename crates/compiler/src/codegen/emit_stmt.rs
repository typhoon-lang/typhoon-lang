//! Statement emission helpers (Let, If, Loop, Match, Conc, etc.)

use crate::ast::*;
use crate::codegen::ir_builder::IrBuilder;

impl<'a> IrBuilder<'a> {
    // ── Function emission ──────────────────────────────────────────────────────

    /// Emit a user function body.
    pub fn emit_function(
        &mut self,
        name: &Identifier,
        params: &[Parameter],
        ret_ty: &str,
        body: &Block,
    ) -> String {
        self.reset_for_function(&name.name, ret_ty);
        self.emit("entry:".to_string());
        self.emit("  call void @ty_safepoint()".to_string());
        self.emit_function_param("task".to_string(), "i8*".to_string());
        for p in params {
            let ty = self
                .reg
                .lower_type(&p.type_annotation, &self.reg.opaque_structs);
            self.emit_function_param(p.name.name.clone(), ty);
        }

        let terminated = self.emit_block_stmts(body, ret_ty);

        if !terminated {
            if let Some(expr) = &body.trailing_expression {
                let val = self.emit_expr(expr);
                let ty = self.expr_llvm_type(expr);
                self.emit(format!("  ret {} {}", ty, val));
            } else if self
                .lines
                .iter()
                .filter(|l| !l.trim_start().is_empty() && !l.trim_start().starts_with(';'))
                .last()
                .map_or(false, |l| {
                    !l.trim_end().ends_with(':')
                        && !l.starts_with("ret ")
                        && !l.starts_with("unreachable")
                })
            {
                if ret_ty == "void" {
                    self.emit("  ret void".to_string());
                } else {
                    let z = self.reg.zero_value(ret_ty);
                    self.emit(format!("  ret {} {}", ret_ty, z));
                }
            }
        }

        // Guard against a dangling empty label
        if self
            .lines
            .last()
            .map_or(false, |l| l.trim_end().ends_with(':'))
        {
            if ret_ty == "void" {
                self.emit("  ret void".to_string());
            } else {
                let z = self.reg.zero_value(ret_ty);
                self.emit(format!("  ret {} {}", ret_ty, z));
            }
        }

        self.finish_function_ir()
    }

    pub fn emit_function_param(&mut self, name: String, ty: String) {
        let slot = self.tmp();
        self.emit_alloca(&slot, &ty);
        self.emit(format!("  store {} %{}, {}* {}", ty, name, ty, slot));
        self.locals.insert(name.clone(), slot);
        self.locals_type.insert(name, ty);
    }

    // ── Conc emission is handled by the pattern module

    // ── Let binding ───────────────────────────────────────────────────────────

    pub fn emit_let(
        &mut self,
        name: &Identifier,
        initializer: &Expression,
        type_annotation: Option<&Type>,
        mutable: bool,
    ) {
        if mutable {
            self.mutable_vars.insert(name.name.clone());
        }

        // Array literal: build fixed or growable array
        if let ExpressionKind::Literal(Literal {
            kind: LiteralKind::Array(elems),
            ..
        }) = &initializer.node
        {
            let _wants_growable =
                mutable || type_annotation.map_or(false, |ty| ty.node.name == "Array");
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
            self.locals.insert(name.name.clone(), out);
            self.locals_type
                .insert(name.name.clone(), "%struct.TyArray*".to_string());
            return;
        }

        // Regular let
        let val = self.emit_expr(initializer);
        let ty = self
            .actual_inferred_type(initializer)
            .map(|t| self.reg.lower_infer_type(&t))
            .or_else(|| self.value_llvm_type(&val))
            .unwrap_or_else(|| self.expr_llvm_type(initializer));
        let slot = self.tmp();
        self.emit_alloca(&slot, &ty);
        self.emit(format!("  store {} {}, {}* {}", ty, val, ty, slot));
        self.locals.insert(name.name.clone(), slot);
        self.locals_type.insert(name.name.clone(), ty);
    }
}
