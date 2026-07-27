//! Conc (coroutine) emission
//!
//! Handles codegen for `conc { ... }` blocks including closure capture,
//! slab allocation, trampoline emission, and spawning.

use crate::ast::*;
use crate::codegen::typeregistry::IrFunction;
use crate::codegen::ir_builder::IrBuilder;
use crate::codegen::get_size_class;

impl<'a> IrBuilder<'a> {
    // ── Conc (concurrent block) emission ──────────────────────────────────────

    pub fn emit_conc(&mut self, body: &Block) {
        let captured_names = self.collect_captured_vars(body);
        let captured: Vec<(String, String, String, bool)> = captured_names
            .iter()
            .filter(|n| *n != "task" && *n != "arg")
            .filter_map(|name| {
                if let Some(slot) = self.locals.get(name) {
                    let ty = self.locals_type.get(name).cloned().unwrap_or("i32".into());
                    let is_mutable = self.mutable_vars.contains(name);
                    Some((name.clone(), slot.clone(), ty, is_mutable))
                } else {
                    self.emit(format!(
                        "  ; BUG: capture '{}' not found in locals — missing let binding?",
                        name
                    ));
                    None
                }
            })
            .collect();

        let tramp_name = format!("__ty_conc_{}", self.label("tramp"));
        let tramp_ir = if captured.is_empty() {
            self.emit_conc_no_capture(body, &tramp_name)
        } else {
            self.emit_conc_closure(body, &tramp_name, &captured)
        };

        // Spawn
        let fn_cast = self.tmp();
        self.emit(format!(
            "  {} = bitcast void(i8*, i8*)* @{} to i8*",
            fn_cast, tramp_name
        ));
        let tv = self.emit_task_load();
        if let Some((closure_arg, closure_size)) = tramp_ir.1 {
            self.emit(format!(
                "  call i8* @ty_spawn_closure(i8* {}, i8* {}, i8* {}, i64 {})",
                tv, fn_cast, closure_arg, closure_size
            ));
        } else {
            self.emit(format!(
                "  call i8* @ty_spawn(i8* {}, i8* {}, i8* null)",
                tv, fn_cast
            ));
        }
        self.conc_functions.push(tramp_ir.0);
    }

    /// Returns (IrFunction, optional (closure_i8_ptr, closure_size)).
    fn emit_conc_no_capture(
        &mut self,
        body: &Block,
        tramp_name: &str,
    ) -> (IrFunction, Option<(String, i64)>) {
        let ctx = self.save_context();
        self.current_fn_ret_ty = "void".to_string();
        self.current_fn_name = Some(tramp_name.to_string());
        self.next_tmp = ctx.next_tmp;
        self.emit("entry:".to_string());
        self.emit_function_param("task".to_string(), "i8*".to_string());
        self.emit_function_param("arg".to_string(), "i8*".to_string());
        // !! Remove the parent-locals copy here entirely.
        // Free variables must be captured via emit_conc_closure, not by
        // referencing parent alloca slots that won't exist in this coroutine.
        self.emit_block_stmts(body, "void");
        // No closure to free — nothing was heap-allocated for this trampoline.
        self.emit("  ret void".to_string());
        let saved_tmp = self.next_tmp;
        let ir = IrFunction {
            name: tramp_name.to_string(),
            body: self.finish_function_ir(),
            ret_type: "void".to_string(),
            params: vec![
                ("task".to_string(), "i8*".to_string()),
                ("arg".to_string(), "i8*".to_string()),
            ],
            annotations: vec![],
        };
        self.restore_context(ctx);
        self.next_tmp = saved_tmp;
        (ir, None)
    }

    fn emit_conc_closure(
        &mut self,
        body: &Block,
        tramp_name: &str,
        captured: &[(String, String, String, bool)],
    ) -> (IrFunction, Option<(String, i64)>) {
        let closure_ty = format!("%closure.{}", tramp_name);
        let closure_field_tys: Vec<String> = captured
            .iter()
            .map(|(_, _, ty, is_mut)| {
                if *is_mut && !ty.ends_with('*') {
                    format!("{}*", ty)
                } else {
                    ty.clone()
                }
            })
            .collect();
        // Compute struct-like size with alignment/padding so it matches C sizeof(struct).
        let mut offset: i64 = 0;
        let mut max_align: i64 = 1;
        for ty in &closure_field_tys {
            let sz = self.reg.llvm_const_sizeof(ty);
            let al = self.reg.llvm_const_alignof(ty);
            if al > max_align {
                max_align = al;
            }
            let pad = if offset % al == 0 {
                0
            } else {
                al - (offset % al)
            };
            offset += pad;
            offset += sz;
        }
        let closure_size: i64 = if max_align > 1 {
            let rem = offset % max_align;
            if rem == 0 {
                offset
            } else {
                offset + (max_align - rem)
            }
        } else {
            offset
        };
        let class_id = get_size_class(closure_size);

        self.reg.type_decls.push(format!(
            "{} = type {{ {} }}",
            closure_ty,
            closure_field_tys.join(", ")
        ));

        // Heap-allocate the closure via slab_alloc so it survives past the
        // current stack frame.
        let task_slot = self.emit_task_load();
        let raw_ptr = self.tmp();
        self.emit(format!(
            "  {} = call i8* @slab_alloc(i8* {}, i32 {})",
            raw_ptr, task_slot, class_id
        ));
        // Bitcast the raw heap pointer to the typed closure struct pointer
        let closure_slot = self.tmp();
        self.emit(format!(
            "  {} = bitcast i8* {} to {}*",
            closure_slot, raw_ptr, closure_ty
        ));

        for (idx, (_, slot, ty, is_mut)) in captured.iter().enumerate() {
            let gep = self.tmp();
            self.emit(format!(
                "  {} = getelementptr inbounds {}, {}* {}, i32 0, i32 {}",
                gep, closure_ty, closure_ty, closure_slot, idx
            ));
            if *is_mut && !ty.ends_with('*') {
                self.emit(format!("  store {}* {}, {}** {}", ty, slot, ty, gep));
            } else if !ty.ends_with('*') {
                let loaded = self.tmp();
                self.emit(format!("  {} = load {}, {}* {}", loaded, ty, ty, slot));
                self.emit(format!("  store {} {}, {}* {}", ty, loaded, ty, gep));
            } else {
                let loaded = self.tmp();
                self.emit(format!("  {} = load {}, {}* {}", loaded, ty, ty, slot));
                self.emit(format!("  store {} {}, {}* {}", ty, loaded, ty, gep));
            }
        }

        // Pass the heap-allocated closure pointer (already i8* from slab_alloc)
        let closure_i8 = raw_ptr;

        // Emit trampoline in saved context
        let ctx = self.save_context();
        self.current_fn_ret_ty = "void".to_string();
        self.current_fn_name = Some(tramp_name.to_string());
        self.next_tmp = ctx.next_tmp;
        self.emit("entry:".to_string());
        self.emit_function_param("task".to_string(), "i8*".to_string());
        self.emit_function_param("arg".to_string(), "i8*".to_string());

        let arg_slot = self.locals["arg"].clone();
        let arg_i8 = self.tmp();
        self.emit(format!("  {} = load i8*, i8** {}", arg_i8, arg_slot));
        let cl = self.tmp();
        self.emit(format!(
            "  {} = bitcast i8* {} to {}*",
            cl, arg_i8, closure_ty
        ));

        for (idx, (name, _, ty, is_mut)) in captured.iter().enumerate() {
            let field_ty = if *is_mut && !ty.ends_with('*') {
                format!("{}*", ty)
            } else {
                ty.clone()
            };
            let gep = self.tmp();
            self.emit(format!(
                "  {} = getelementptr inbounds {}, {}* {}, i32 0, i32 {}",
                gep, closure_ty, closure_ty, cl, idx
            ));
            let loaded = self.tmp();
            self.emit(format!(
                "  {} = load {}, {}* {}",
                loaded, field_ty, field_ty, gep
            ));
            if *is_mut && !ty.ends_with('*') {
                self.locals.insert(name.clone(), loaded.clone());
                self.locals_type.insert(name.clone(), ty.clone());
            } else {
                let slot = self.tmp();
                self.emit_alloca(&slot, ty);
                self.emit(format!("  store {} {}, {}* {}", ty, loaded, ty, slot));
                self.locals.insert(name.clone(), slot);
                self.locals_type.insert(name.clone(), ty.clone());
            }
        }

        self.emit_block_stmts(body, "void");
        // Free the heap-allocated closure after the body completes.
        let task_in_tramp = self.emit_task_load();
        let arg_for_free = if let Some(slot) = self.locals.get("arg").cloned() {
            let t = self.tmp();
            self.emit(format!("  {} = load i8*, i8** {}", t, slot));
            t
        } else {
            "%arg".to_string()
        };
        self.emit(format!(
            "  call void @slab_free(i8* {}, i8* {}, i32 {})",
            task_in_tramp, arg_for_free, class_id
        ));
        self.emit("  ret void".to_string());
        let saved_tmp = self.next_tmp;
        let ir = IrFunction {
            name: tramp_name.to_string(),
            body: self.finish_function_ir(),
            ret_type: "void".to_string(),
            params: vec![
                ("task".to_string(), "i8*".to_string()),
                ("arg".to_string(), "i8*".to_string()),
            ],
            annotations: vec![],
        };
        self.restore_context(ctx);
        self.next_tmp = saved_tmp;
        (ir, Some((closure_i8, closure_size)))
    }

    // ── Drop helpers ──────────────────────────────────────────────────────────

    /// Emit a `slab_free` call for the named local, if it was slab-allocated.
    pub fn emit_slab_free(&mut self, name: &str) {
        let Some(typed_ptr) = self.locals.get(name).cloned() else {
            return;
        };
        let ty = self
            .locals_type
            .get(name)
            .cloned()
            .unwrap_or_else(|| "i32".to_string());

        // Only free if this was heap-allocated (pointer to struct / not a plain alloca slot)
        if !ty.ends_with('*') {
            return;
        }

        let size = self.reg.llvm_const_sizeof(&ty);
        let class_id = get_size_class(size);

        let raw = self.tmp();
        self.emit(format!(
            "  {} = bitcast {}* {} to i8*",
            raw,
            ty.trim_end_matches('*'),
            typed_ptr
        ));
        let tv = self.emit_task_load();
        self.emit(format!(
            "  call void @slab_free(i8* {}, i8* {}, i32 {})",
            tv, raw, class_id
        ));
    }
}