//! Pattern matching helpers for codegen

use crate::ast::*;
use crate::type_inference::InferType;
use crate::codegen::ir_builder::IrBuilder;

impl<'a> IrBuilder<'a> {
    // ── Pattern helpers ───────────────────────────────────────────────────────

    pub fn emit_pattern_test(
        &mut self,
        pattern: &Pattern,
        scrutinee_expr: &Expression,
        scrutinee_val: &str,
    ) -> String {
        let actual_ty = self
            .actual_inferred_type(scrutinee_expr)
            .map(|t| {
                self.reg.ensure_enum_layout_for_infer(&t);
                self.reg.lower_infer_type(&t)
            })
            .unwrap_or_else(|| self.expr_llvm_type(scrutinee_expr));
        match &pattern.node {
            PatternKind::Wildcard => "1".to_string(),
            PatternKind::Identifier(id) => {
                if let Some(slot) = self.locals.get(&id.name).cloned() {
                    let pat_ty = self
                        .locals_type
                        .get(&id.name)
                        .cloned()
                        .unwrap_or_else(|| actual_ty.clone());
                    let loaded = self.tmp();
                    self.emit(format!(
                        "  {} = load {}, {}* {}",
                        loaded, pat_ty, pat_ty, slot
                    ));
                    let cmp = self.tmp();
                    self.emit(format!(
                        "  {} = icmp eq {} {}, {}",
                        cmp, actual_ty, scrutinee_val, loaded
                    ));
                    cmp
                } else {
                    "1".to_string()
                }
            }
            PatternKind::Literal(lit) => match &lit.kind {
                LiteralKind::Int(v, _) => {
                    let tmp = self.tmp();
                    self.emit(format!(
                        "  {} = icmp eq {} {}, {}",
                        tmp, actual_ty, scrutinee_val, v
                    ));
                    tmp
                }
                LiteralKind::Bool(v) => {
                    let tmp = self.tmp();
                    self.emit(format!(
                        "  {} = icmp eq i1 {}, {}",
                        tmp,
                        scrutinee_val,
                        if *v { "1" } else { "0" }
                    ));
                    tmp
                }
                _ => "1".to_string(),
            },
            PatternKind::EnumVariant { variant_name, .. } => self.emit_enum_tag_test(
                scrutinee_expr,
                &actual_ty,
                scrutinee_val,
                &variant_name.name,
            ),
            _ => "1".to_string(),
        }
    }

    pub fn emit_pattern_test_typed(
        &mut self,
        pattern: &Pattern,
        ty: &str,
        scrutinee_val: &str,
        scrutinee_expr: &Expression,
    ) -> String {
        let actual_ty = self
            .actual_inferred_type(scrutinee_expr)
            .map(|t| {
                self.reg.ensure_enum_layout_for_infer(&t);
                self.reg.lower_infer_type(&t)
            })
            .unwrap_or_else(|| ty.to_string());
        match &pattern.node {
            PatternKind::Wildcard => "1".to_string(),
            PatternKind::EnumVariant { variant_name, .. } => self.emit_enum_tag_test(
                scrutinee_expr,
                &actual_ty,
                scrutinee_val,
                &variant_name.name,
            ),
            _ => "1".to_string(),
        }
    }

    fn emit_enum_tag_test(
        &mut self,
        scrutinee_expr: &Expression,
        actual_llvm_ty: &str,
        scrutinee_val: &str,
        variant_name: &str,
    ) -> String {
        let Some(infer) = self.actual_inferred_type(scrutinee_expr) else {
            self.emit(format!(
                "  ; CODEGEN ERROR: could not resolve type for tag test variant {}",
                variant_name
            ));
            return "1".to_string();
        };
        let llvm_ty = self.reg.lower_infer_type(&infer);
        let Some(layout) = self.reg.enum_layouts.get(&llvm_ty).cloned() else {
            self.emit(format!(
                "  ; CODEGEN ERROR: could not resolve layout for tag test variant {} with type {}",
                variant_name, llvm_ty
            ));
            return "1".to_string();
        };
        let Some(v) = layout.variants.get(variant_name) else {
            self.emit(format!(
                "  ; CODEGEN ERROR: could not find variant {} in layout for {}",
                variant_name, llvm_ty
            ));
            return "1".to_string();
        };
        let loaded = self.tmp();
        let cmp = self.tmp();
        self.emit(format!(
            "  {} = extractvalue {} {}, 0",
            loaded, actual_llvm_ty, scrutinee_val
        ));
        self.emit(format!(
            "  {} = icmp eq {} {}, {}",
            cmp, layout.tag_ty, loaded, v.tag_value
        ));
        cmp
    }

    pub fn bind_pattern_value(
        &mut self,
        pattern: &Pattern,
        scrutinee_expr: &Expression,
        scrutinee_val: &str,
    ) {
        let ty = self.expr_llvm_type(scrutinee_expr);
        self.bind_pattern_typed(pattern, scrutinee_val, &ty, Some(scrutinee_expr));
    }

    pub fn bind_pattern_typed(
        &mut self,
        pattern: &Pattern,
        val: &str,
        ty: &str,
        scrutinee_expr: Option<&Expression>,
    ) {
        match &pattern.node {
            PatternKind::Wildcard | PatternKind::Literal(_) => {}
            PatternKind::Identifier(id) => {
                if self.locals.contains_key(&id.name) {
                    return;
                }
                let slot = self.tmp();
                self.emit_alloca(&slot, ty);
                self.emit(format!("  store {} {}, {}* {}", ty, val, ty, slot));
                self.locals.insert(id.name.clone(), slot);
                self.locals_type.insert(id.name.clone(), ty.to_string());
            }
            PatternKind::EnumVariant {
                variant_name,
                payload: Some(inner),
                ..
            } => {
                if let Some((idx, payload_ty)) =
                    self.enum_payload_info(scrutinee_expr, ty, &variant_name.name)
                {
                    let extracted = self.tmp();
                    self.emit(format!(
                        "  {} = extractvalue {} {}, {}",
                        extracted, ty, val, idx
                    ));
                    self.bind_pattern_typed(inner, &extracted, &payload_ty, None);
                } else {
                    self.emit(format!(
                        "  ; CODEGEN ERROR: could not extract payload for variant {:?} in type {}",
                        variant_name, ty
                    ));
                }
            }
            PatternKind::EnumVariant { payload: None, .. } => {}
            PatternKind::Struct { fields, .. } => {
                let struct_name = ty.trim_start_matches("%struct.").to_string();
                for (field_name, field_pat) in fields {
                    let (idx, fty) = self.reg.struct_field_info(&struct_name, &field_name.name);
                    let extracted = self.tmp();
                    self.emit(format!(
                        "  {} = extractvalue {} {}, {}",
                        extracted, ty, val, idx
                    ));
                    let ft = fty.clone();
                    self.bind_pattern_typed(field_pat, &extracted, &ft, None);
                }
            }
            PatternKind::Tuple(parts) | PatternKind::Array(parts) => {
                for (idx, part) in parts.iter().enumerate() {
                    let extracted = self.tmp();
                    self.emit(format!(
                        "  {} = extractvalue {} {}, {}",
                        extracted, ty, val, idx
                    ));
                    self.bind_pattern_typed(part, &extracted, "i32", None);
                }
            }
            PatternKind::Or(left, _) => self.bind_pattern_typed(left, val, ty, scrutinee_expr),
            PatternKind::Guard { pattern, .. } => {
                self.bind_pattern_typed(pattern, val, ty, scrutinee_expr)
            }
        }
    }

    fn enum_payload_info(
        &mut self,
        scrutinee_expr: Option<&Expression>,
        llvm_ty: &str,
        variant_name: &str,
    ) -> Option<(usize, String)> {
        if let Some(e) = scrutinee_expr {
            if let Some(inferred) = self.actual_inferred_type(e) {
                let llvm_ty_infer = self.reg.lower_infer_type(&inferred);
                let layout = self.reg.enum_layouts.get(&llvm_ty_infer)?;
                let v = layout.variants.get(variant_name)?;
                return Some((v.payload_index?, v.payload_ty.clone()?));
            }
        }
        let layout = self.reg.enum_layouts.get(llvm_ty)?;
        let v = layout.variants.get(variant_name)?;
        Some((v.payload_index?, v.payload_ty.clone()?))
    }

    // ── Type queries ─────────────────────────────────────────────────────────

    pub fn option_type_for_index(&mut self, expr: &Expression) -> Option<(String, String)> {
        let ty = self.inferred_expr_type(expr)?.clone();
        if let InferType::App(ref name, ref args) = ty {
            if name == "Option" && args.len() == 1 {
                let elem = self.reg.lower_infer_type(&args[0]);
                let opt = self.reg.lower_infer_type(&ty);
                return Some((opt, elem));
            }
        }
        None
    }
}