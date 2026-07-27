//! Match expression emission
//!
//! Handles codegen for `match` statements and expressions.

use crate::ast::*;
use crate::codegen::ir_builder::IrBuilder;

impl<'a> IrBuilder<'a> {
    // ── Match / if-let ────────────────────────────────────────────────────────

    pub fn emit_match_expression(&mut self, expr: &Expression, arms: &[MatchArm]) -> String {
        // The match expression's type is the arm-body type, not the scrutinee type.
        let mut result_ty = "void".to_string();
        'outer: for arm in arms {
            let candidates: Vec<&Expression> = match &arm.node.body.node {
                ExpressionKind::Block(b) => b
                    .trailing_expression
                    .as_ref()
                    .map(|e| vec![&arm.node.body, e.as_ref()])
                    .unwrap_or_else(|| vec![&arm.node.body]),
                _ => vec![&arm.node.body],
            };
            for c in candidates {
                if let Some(infer) = self.actual_inferred_type(c) {
                    let t = self.reg.lower_infer_type(&infer);
                    if t != "void" {
                        result_ty = t;
                        break 'outer;
                    }
                }
                let t = self.expr_llvm_type(c);
                if t != "void" {
                    result_ty = t;
                    break 'outer;
                }
            }
        }
        let result_slot = if result_ty != "void" {
            let slot = self.tmp();
            self.emit_alloca(&slot, &result_ty);
            Some((slot, result_ty))
        } else {
            None
        };

        let match_val = self.emit_expr(expr);
        let merge_lbl = self.label("match_merge");
        let fallback_lbl = self.label("match_fallback");
        let mut next_check = self.label("match_check");
        self.emit(format!("  br label %{}", next_check));

        for (idx, arm) in arms.iter().enumerate() {
            let body_lbl = self.label(&format!("match_body_{}", idx));
            let following = if idx + 1 == arms.len() {
                fallback_lbl.clone()
            } else {
                self.label(&format!("match_check_{}", idx))
            };
            self.emit(format!("{}:", next_check));
            let ok = self.emit_pattern_test(&arm.node.pattern, expr, &match_val);
            self.emit(format!(
                "  br i1 {}, label %{}, label %{}",
                ok, body_lbl, following
            ));
            self.emit(format!("{}:", body_lbl));

            let saved_locals = self.locals.clone();
            let saved_types = self.locals_type.clone();
            self.bind_pattern_value(&arm.node.pattern, expr, &match_val);
            if let Some(guard) = &arm.node.guard {
                let gv = self.emit_expr(guard);
                let guard_body = self.label("match_guard");
                self.emit(format!(
                    "  br i1 {}, label %{}, label %{}",
                    gv, guard_body, following
                ));
                self.emit(format!("{}:", guard_body));
            }
            let body_val = self.emit_expr(&arm.node.body);
            if let Some((slot, ty)) = &result_slot {
                let actual_ty = self.expr_llvm_type(&arm.node.body);
                let store_ty = if actual_ty == "void" {
                    ty.clone()
                } else {
                    actual_ty
                };
                self.emit(format!(
                    "  store {} {}, {}* {}",
                    store_ty, body_val, store_ty, slot
                ));
            }
            self.emit(format!("  br label %{}", merge_lbl));
            self.locals = saved_locals;
            self.locals_type = saved_types;
            next_check = following;
        }

        self.emit(format!("{}:", fallback_lbl));
        if let Some((slot, ty)) = &result_slot {
            self.emit(format!("  store {} undef, {}* {}", ty, ty, slot));
            self.emit(format!("  br label %{}", merge_lbl));
        } else {
            self.emit("  unreachable".to_string());
        }

        self.emit(format!("{}:", merge_lbl));
        if let Some((slot, ty)) = result_slot {
            let tmp = self.tmp();
            self.emit(format!("  {} = load {}, {}* {}", tmp, ty, ty, slot));
            return tmp;
        }
        "0".to_string()
    }

    pub fn emit_if_let(
        &mut self,
        call_expr: &Expression,
        pattern: &Pattern,
        matched: &Expression,
        then: &Block,
        else_branch: Option<&Expression>,
    ) -> String {
        let result_ty = self.expr_llvm_type(call_expr);
        let result_slot = if result_ty != "void" {
            let slot = self.tmp();
            self.emit_alloca(&slot, &result_ty);
            Some((slot, result_ty))
        } else {
            None
        };

        let match_val = self.emit_expr(matched);
        let then_lbl = self.label("iflet_then");
        let else_lbl = self.label("iflet_else");
        let merge_lbl = self.label("iflet_merge");
        let ok = self.emit_pattern_test(pattern, matched, &match_val);
        self.emit(format!(
            "  br i1 {}, label %{}, label %{}",
            ok, then_lbl, else_lbl
        ));

        // then branch
        self.emit(format!("{}:", then_lbl));
        let saved_locals = self.locals.clone();
        let saved_types = self.locals_type.clone();
        self.bind_pattern_value(pattern, matched, &match_val);
        let ret_ty = self.current_fn_ret_ty.clone();
        let then_term = self.emit_block_stmts(then, &ret_ty);
        if !then_term {
            if let Some(trail) = &then.trailing_expression {
                let v = self.emit_expr(trail);
                if let Some((slot, ty)) = &result_slot {
                    let actual_ty = self.expr_llvm_type(trail);
                    let store_ty = if actual_ty == "void" {
                        ty.clone()
                    } else {
                        actual_ty
                    };
                    self.emit(format!(
                        "  store {} {}, {}* {}",
                        store_ty, v, store_ty, slot
                    ));
                }
            }
            self.emit(format!("  br label %{}", merge_lbl));
        }
        self.locals = saved_locals;
        self.locals_type = saved_types;

        // else branch
        self.emit(format!("{}:", else_lbl));
        if let Some(else_expr) = else_branch {
            let v = self.emit_expr(else_expr);
            if let Some((slot, ty)) = &result_slot {
                let actual_ty = self.expr_llvm_type(else_expr);
                let store_ty = if actual_ty == "void" {
                    ty.clone()
                } else {
                    actual_ty
                };
                self.emit(format!(
                    "  store {} {}, {}* {}",
                    store_ty, v, store_ty, slot
                ));
            }
        } else if let Some((slot, ty)) = &result_slot {
            self.emit(format!("  store {} undef, {}* {}", ty, ty, slot));
        }
        self.emit(format!("  br label %{}", merge_lbl));

        self.emit(format!("{}:", merge_lbl));
        if let Some((slot, ty)) = result_slot {
            let tmp = self.tmp();
            self.emit(format!("  {} = load {}, {}* {}", tmp, ty, ty, slot));
            return tmp;
        }
        "0".to_string()
    }
}