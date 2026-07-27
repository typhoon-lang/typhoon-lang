//! Statement and expression emission
//!
//! Handles emission of all statement kinds (let, return, if, match, loop, conc)
//! and expression kinds (literals, calls, binary ops, field access, etc.)

use crate::ast::*;
use crate::codegen::ir_builder::{IrBuilder};

// ── Statement emission ────────────────────────────────────────────────────────

impl<'a> IrBuilder<'a> {
    /// Emit all statements in `block`. Returns true if a terminator was emitted.
    pub fn emit_block_stmts(&mut self, block: &Block, ret_ty: &str) -> bool {
        self.annotate_span(&block.span);
        for stmt in &block.statements {
            if self.emit_stmt(stmt, ret_ty) {
                return true;
            }
        }
        // Before exiting the block, emit slab_free for everything dying here.
        if let Some(drops) = self.drop_map.get(&block.block_id).cloned() {
            for drop in &drops {
                if drop.is_heap {
                    self.emit_slab_free(&drop.name);
                }
            }
        }
        false
    }

    /// Emit one statement. Returns true if it is a terminator.
    pub fn emit_stmt(&mut self, stmt: &Statement, ret_ty: &str) -> bool {
        match &stmt.node {
            StatementKind::Return(Some(expr)) => {
                let val = self.emit_expr(expr);
                if self.current_fn_name.as_deref() == Some("__ty_main_body") {
                    if let Some((_, end_lbl)) = self.loop_labels.last().cloned() {
                        self.emit(
                            "  ; CODEGEN WARNING: return inside loop treated as break in coroutine"
                                .to_string(),
                        );
                        self.emit(format!("  br label %{}", end_lbl));
                    } else {
                        self.emit("  ret void".to_string());
                    }
                } else {
                    let ty = self.expr_llvm_type(expr);
                    self.emit(format!("  ret {} {}", ty, val));
                }
                true
            }
            StatementKind::Return(None) => {
                self.emit("  ret void".to_string());
                true
            }
            StatementKind::Break => {
                let (_, end) = self.loop_labels.last().unwrap().clone();
                self.emit(format!("  br label %{}", end));
                true
            }
            StatementKind::Continue => {
                let (start, _) = self.loop_labels.last().unwrap().clone();
                self.emit(format!("  br label %{}", start));
                true
            }
            StatementKind::LetBinding {
                pattern,
                initializer,
                type_annotation,
                mutable,
                else_block,
            } => {
                if let Some(name) = pattern.get_identifier() {
                    self.emit_let(name, initializer, type_annotation.as_ref(), *mutable);
                    return false;
                }
                // Destructuring let with optional else block
                let match_val = self.emit_expr(initializer);
                let match_ty = self
                    .actual_inferred_type(initializer)
                    .map(|t| self.reg.lower_infer_type(&t))
                    .or_else(|| self.value_llvm_type(&match_val))
                    .unwrap_or_else(|| self.expr_llvm_type(initializer));
                let then_lbl = self.label("letelse_ok");
                let else_lbl = self.label("letelse_fail");
                let merge_lbl = self.label("letelse_merge");
                let ok = self.emit_pattern_test_typed(pattern, &match_ty, &match_val, initializer);
                self.emit(format!(
                    "  br i1 {}, label %{}, label %{}",
                    ok, then_lbl, else_lbl
                ));
                // Emit else arm first to determine if it terminates.
                self.emit(format!("{}:", else_lbl));
                let else_term = else_block
                    .as_ref()
                    .map(|b| self.emit_block_stmts(b, ret_ty))
                    .unwrap_or(false);
                if !else_term {
                    if let Some((loop_start, _)) = self.loop_labels.last().cloned() {
                        self.emit(format!("  br label %{}", loop_start));
                    } else {
                        self.emit(format!("  br label %{}", merge_lbl));
                    }
                }
                // Emit ok arm. Always br to merge.
                self.emit(format!("{}:", then_lbl));
                self.bind_pattern_typed(pattern, &match_val, &match_ty, Some(initializer));
                self.emit(format!("  br label %{}", merge_lbl));
                self.emit(format!("{}:", merge_lbl));
                false
            }
            StatementKind::Expression(expr) => {
                self.emit_expr(expr);
                false
            }
            StatementKind::If {
                condition,
                then_branch,
                else_branch,
            } => self.emit_if(condition, then_branch, else_branch.as_ref(), ret_ty),
            StatementKind::Match { expr, arms } => {
                self.emit_match_expression(expr, arms);
                false
            }
            StatementKind::Loop { kind, body } => {
                self.emit_loop(kind, body, ret_ty);
                false
            }
            StatementKind::Conc { body } => {
                self.emit_conc(body);
                false
            }
            _ => false,
        }
    }

    /// Emit an if statement. Returns true if it terminates.
    fn emit_if(
        &mut self,
        condition: &Expression,
        then_branch: &Block,
        else_branch: Option<&ElseBranch>,
        ret_ty: &str,
    ) -> bool {
        let cond = self.emit_expr(condition);
        let then_lbl = self.label("then");
        let else_lbl = self.label("else");
        let merge_lbl = self.label("if_merge");
        let cont_lbl = self.label("if_cont");

        self.emit(format!(
            "  br i1 {}, label %{}, label %{}",
            cond, then_lbl, else_lbl
        ));

        self.emit(format!("{}:", then_lbl));
        let then_term = self.emit_block_stmts(then_branch, ret_ty);
        if !then_term {
            self.emit(format!("  br label %{}", merge_lbl));
        }

        self.emit(format!("{}:", else_lbl));
        let else_term = match else_branch {
            None => {
                self.emit(format!("  br label %{}", merge_lbl));
                false
            }
            Some(eb) => match &eb.node {
                ElseBranchKind::Block(b) => {
                    let t = self.emit_block_stmts(b, ret_ty);
                    if !t {
                        self.emit(format!("  br label %{}", merge_lbl));
                    }
                    t
                }
                ElseBranchKind::If(stmt) => self.emit_stmt(stmt, ret_ty),
            },
        };

        if then_term && else_term {
            self.emit(format!("{}:", merge_lbl));
            self.emit("  unreachable".to_string());
            return true;
        }
        self.emit(format!("{}:", merge_lbl));
        self.emit(format!("  br label %{}", cont_lbl));
        self.emit(format!("{}:", cont_lbl));
        false
    }

    fn emit_loop(&mut self, kind: &Spanned<LoopKindKind>, body: &Block, ret_ty: &str) {
        match &kind.node {
            LoopKindKind::While { condition, .. } => {
                let start = self.label("while_start");
                let body_lbl = self.label("while_body");
                let end = self.label("while_end");
                self.loop_labels.push((start.clone(), end.clone()));
                self.emit(format!("  br label %{}", start));
                self.emit(format!("{}:", start));
                let cond = self.emit_expr(condition);
                self.emit(format!(
                    "  br i1 {}, label %{}, label %{}",
                    cond, body_lbl, end
                ));
                self.emit(format!("{}:", body_lbl));
                if !self.emit_block_stmts(body, ret_ty) {
                    self.emit(format!("  br label %{}", start));
                }
                self.emit(format!("{}:", end));
                self.loop_labels.pop();
            }
            LoopKindKind::For {
                pattern, iterator, ..
            } => {
                let iter_val = self.emit_expr(iterator);
                let elem_ty = self
                    .inferred_expr_type(iterator)
                    .cloned()
                    .and_then(|t| self.array_elem_type_from_infertype(&t))
                    .unwrap_or_else(|| "i32".to_string());

                let idx_slot = self.tmp();
                self.emit_alloca(&idx_slot, "i64");
                self.emit(format!("  store i64 0, i64* {}", idx_slot));

                let len_ptr = self.tmp();
                self.emit(format!("  {} = getelementptr inbounds %struct.TyArray, %struct.TyArray* {}, i32 0, i32 1", len_ptr, iter_val));
                let len = self.tmp();
                self.emit(format!("  {} = load i64, i64* {}", len, len_ptr));

                let start = self.label("for_start");
                let body_lbl = self.label("for_body");
                let end = self.label("for_end");
                self.loop_labels.push((start.clone(), end.clone()));
                self.emit(format!("  br label %{}", start));
                self.emit(format!("{}:", start));

                let idx = self.tmp();
                self.emit(format!("  {} = load i64, i64* {}", idx, idx_slot));
                let cmp = self.tmp();
                self.emit(format!("  {} = icmp slt i64 {}, {}", cmp, idx, len));
                self.emit(format!(
                    "  br i1 {}, label %{}, label %{}",
                    cmp, body_lbl, end
                ));

                self.emit(format!("{}:", body_lbl));
                if let PatternKind::Identifier(id) = &pattern.node {
                    let elem_ptr_i8 = self.tmp();
                    self.emit(format!(
                        "  {} = call i8* @ty_array_get_ptr(%struct.TyArray* {}, i64 {})",
                        elem_ptr_i8, iter_val, idx
                    ));
                    let elem_ptr = self.tmp();
                    self.emit(format!(
                        "  {} = bitcast i8* {} to {}*",
                        elem_ptr, elem_ptr_i8, elem_ty
                    ));
                    let elem_val = self.tmp();
                    self.emit(format!(
                        "  {} = load {}, {}* {}",
                        elem_val, elem_ty, elem_ty, elem_ptr
                    ));
                    let pat_slot = self.tmp();
                    self.emit_alloca(&pat_slot, &elem_ty);
                    self.emit(format!(
                        "  store {} {}, {}* {}",
                        elem_ty, elem_val, elem_ty, pat_slot
                    ));
                    self.locals.insert(id.name.clone(), pat_slot);
                    self.locals_type.insert(id.name.clone(), elem_ty.clone());
                }

                let body_term = self.emit_block_stmts(body, ret_ty);
                if !body_term {
                    let idx2 = self.tmp();
                    self.emit(format!("  {} = load i64, i64* {}", idx2, idx_slot));
                    let next = self.tmp();
                    self.emit(format!("  {} = add i64 {}, 1", next, idx2));
                    self.emit(format!("  store i64 {}, i64* {}", next, idx_slot));
                    self.emit(format!("  br label %{}", start));
                }
                self.emit(format!("{}:", end));
                self.loop_labels.pop();
            }
            LoopKindKind::Block(b) => {
                let start = self.label("loop_start");
                let end = self.label("loop_end");
                self.loop_labels.push((start.clone(), end.clone()));
                self.emit(format!("  br label %{}", start));
                self.emit(format!("{}:", start));
                if !self.emit_block_stmts(b, ret_ty) {
                    self.emit("  call void @ty_safepoint()".to_string());
                    self.emit(format!("  br label %{}", start));
                }
                self.emit(format!("{}:", end));
                self.loop_labels.pop();
            }
        }
    }
}