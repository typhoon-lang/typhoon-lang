use crate::ast::*;
use crate::span::Span;
use std::collections::HashMap;
use crate::type_inference::InferType;

#[derive(Debug, Clone, PartialEq)]
pub struct IrModule {
    pub functions: Vec<IrFunction>,
    pub preamble: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IrFunction {
    pub name: String,
    pub body: String,
    pub ret_type: String,
    pub params: Vec<(String, String)>,
}

pub struct Codegen;

impl Codegen {
    pub fn lower_module(module: &Module, types: &HashMap<NodeId, InferType>) -> IrModule {
        let mut functions = Vec::new();
        let mut builder = IrBuilder::new();
        builder.types = Some(types as *const _);
        builder.collect_types(module);
        for decl in &module.declarations {
            if let DeclarationKind::Function {
                name,
                return_type,
                body,
                params,
                ..
            } = &decl.node
            {
                let ret_ty = return_type
                    .as_ref()
                    .map(|ty| builder.lower_type(ty))
                    .unwrap_or_else(|| "void".to_string());
                let body_ir = builder.emit_function(name, params, &ret_ty, body);
                let param_list = params
                    .iter()
                    .map(|p| (p.name.name.clone(), builder.lower_type(&p.type_annotation)))
                    .collect();
                functions.push(IrFunction {
                    name: link_symbol_name(&name.name),
                    body: body_ir,
                    ret_type: ret_ty,
                    params: param_list,
                });
            }
        }
        let preamble = builder.preamble();
        IrModule {
            functions,
            preamble,
        }
    }
}

struct IrBuilder {
    lines: Vec<String>,
    next_tmp: usize,
    last_value: Option<String>,
    current_fn_name: Option<String>,
    current_fn_ret_ty: String,
    locals: HashMap<String, String>,
    locals_type: HashMap<String, String>,
    type_decls: Vec<String>,
    next_label: usize,
    struct_fields: HashMap<String, Vec<(String, String)>>,
    func_sigs: HashMap<String, (String, Vec<String>)>,
    string_pool: HashMap<String, (String, usize)>,
    extra_preamble: Vec<String>,
    types: Option<*const HashMap<NodeId, InferType>>,
    adt_structs: HashMap<String, String>,
}

impl IrBuilder {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            next_tmp: 0,
            last_value: None,
            current_fn_name: None,
            current_fn_ret_ty: "void".to_string(),
            locals: HashMap::new(),
            locals_type: HashMap::new(),
            type_decls: Vec::new(),
            next_label: 0,
            struct_fields: HashMap::new(),
            func_sigs: HashMap::new(),
            string_pool: HashMap::new(),
            extra_preamble: Vec::new(),
            types: None,
            adt_structs: HashMap::new(),
        }
    }

    fn collect_types(&mut self, module: &Module) {
        self.type_decls.clear();
        self.struct_fields.clear();
        self.func_sigs.clear();
        self.string_pool.clear();
        self.extra_preamble.clear();
        self.adt_structs.clear();

        // Builtin/runtime types + decls used by desugaring.
        self.type_decls.push("%struct.Buf = type { i8*, i64, i64 }".to_string());
        self.type_decls
            .push("%struct.TyArray = type { i8*, i64, i64, i64, i64 }".to_string());

        self.extra_preamble
            .push("declare i8* @ty_alloc(i64, i64)".to_string());
        self.extra_preamble
            .push("declare i8* @ty_realloc(i8*, i64, i64)".to_string());
        self.extra_preamble
            .push("declare void @ty_free(i8*)".to_string());

        self.extra_preamble.push("declare %struct.Buf* @ty_buf_new()".to_string());
        self.extra_preamble
            .push("declare void @ty_buf_push_str(%struct.Buf*, i8*)".to_string());
        self.extra_preamble
            .push("declare i8* @ty_buf_into_str(%struct.Buf*)".to_string());
        self.extra_preamble.push(
            "declare %struct.TyArray* @ty_array_from_fixed(i8*, i64, i64, i64)".to_string(),
        );
        self.extra_preamble
            .push("declare void @ty_array_push(%struct.TyArray*, i8*)".to_string());
        self.extra_preamble
            .push("declare i8* @ty_array_get_ptr(%struct.TyArray*, i64)".to_string());

        self.func_sigs.insert(
            "__ty_buf_new".to_string(),
            ("%struct.Buf*".to_string(), Vec::new()),
        );
        self.func_sigs.insert(
            "__ty_buf_push_str".to_string(),
            ("void".to_string(), vec!["%struct.Buf*".to_string(), "i8*".to_string()]),
        );
        self.func_sigs.insert(
            "__ty_buf_into_str".to_string(),
            ("i8*".to_string(), vec!["%struct.Buf*".to_string()]),
        );
        for decl in &module.declarations {
            match &decl.node {
                DeclarationKind::Struct { name, fields, .. } => {
                    let mut field_types = Vec::new();
                    let mut field_map = Vec::new();
                    for (field, ty) in fields {
                        let lowered = self.lower_type(ty);
                        field_types.push(lowered.clone());
                        field_map.push((field.name.clone(), lowered));
                    }
                    let body = field_types.join(", ");
                    self.type_decls
                        .push(format!("%struct.{} = type {{ {} }}", name.name, body));
                    self.struct_fields.insert(name.name.clone(), field_map);
                }
                DeclarationKind::Enum { name, .. } => {
                    self.type_decls
                        .push(format!("%enum.{} = type opaque", name.name));
                }
                DeclarationKind::Newtype { name, type_alias } => {
                    let alias = self.lower_type(type_alias);
                    self.type_decls
                        .push(format!("%newtype.{} = type {}", name.name, alias));
                }
                DeclarationKind::Function {
                    name,
                    return_type,
                    params,
                    ..
                } => {
                    let ret_ty = return_type
                        .as_ref()
                        .map(|ty| self.lower_type(ty))
                        .unwrap_or_else(|| "void".to_string());
                    let param_types = params
                        .iter()
                        .map(|p| self.lower_type(&p.type_annotation))
                        .collect();
                    self.func_sigs
                        .insert(name.name.clone(), (ret_ty, param_types));
                }
                _ => {}
            }
        }

        // Scan AST types for Result/Option instantiations in annotations.
        for decl in &module.declarations {
            self.scan_decl_for_adts(decl);
        }

        // Scan inferred types if provided.
        if let Some(types_ptr) = self.types {
            // SAFETY: types map lives longer than this builder call.
            let types = unsafe { &*types_ptr };
            for ty in types.values() {
                self.ensure_adt_for_infertype(ty);
            }
        }
    }

    fn preamble(&self) -> Vec<String> {
        let mut preamble = Vec::new();
        preamble.extend(self.type_decls.clone());
        preamble.extend(self.extra_preamble.clone());
        preamble
    }

    fn emit_function(
        &mut self,
        _name: &Identifier,
        params: &[Parameter],
        ret_ty: &str,
        body: &Block,
    ) -> String {
        self.lines.clear();
        self.locals.clear();
        self.last_value = None;
        self.current_fn_ret_ty = ret_ty.to_string();
        self.current_fn_name = Some(_name.name.clone());
        self.lines.push("entry:".to_string());

        for param in params {
            let param_ty = self.lower_type(&param.type_annotation);
            let slot = self.next_register();
            self.lines.push(format!("  {} = alloca {}", slot, param_ty));
            self.lines.push(format!(
                "  store {} %{}, {}* {}",
                param_ty, param.name.name, param_ty, slot
            ));
            self.locals.insert(param.name.name.clone(), slot);
            self.locals_type.insert(param.name.name.clone(), param_ty);
        }

        self.emit_block(body, ret_ty);
        self.finish(ret_ty)
    }

    fn finish(&mut self, return_type: &str) -> String {
        let has_ret = self
            .lines
            .iter()
            .any(|l| l.trim_start().starts_with("ret ") || l.trim_start().starts_with("ret\t"));

        if let Some(value) = self.last_value.take() {
            if return_type != "void" {
                if !has_ret {
                    self.lines.push(format!("  ret {} {}", return_type, value));
                }
            } else if !has_ret {
                self.lines.push("  ret void".to_string());
            }
        } else if !has_ret {
            self.lines.push("  ret void".to_string());
        }

        if let Some(last) = self.lines.last() {
            if last.trim_end().ends_with(':') {
                if return_type != "void" {
                    self.lines.push(format!("  ret {} 0", return_type));
                } else {
                    self.lines.push("  ret void".to_string());
                }
            }
        }

        self.lines.join("\n")
    }

    fn emit_block(&mut self, block: &Block, current_fn_ret_ty: &str) {
        self.annotate_span(&block.span);
        for stmt in &block.statements {
            match &stmt.node {
                StatementKind::Return(Some(expr)) => {
                    let value = self.emit_expr(expr);
                    self.last_value = Some(value);
                    return;
                }
                StatementKind::Return(None) => {
                    self.last_value = None;
                    return;
                }
                StatementKind::LetBinding {
                    name,
                    initializer,
                    type_annotation,
                    mutable,
                    ..
                } => {
                    if let ExpressionKind::Literal(Literal {
                        kind: LiteralKind::Array(elems),
                        ..
                    }) = &initializer.node
                    {
                        let wants_growable = *mutable
                            || type_annotation
                                .as_ref()
                                .is_some_and(|ty| ty.node.name == "Array");

                        let len = elems.len();
                        let elem_ty = if let Some(first) = elems.get(0) {
                            match &first.node {
                                ExpressionKind::Literal(Literal {
                                    kind: LiteralKind::Int(_, suffix),
                                    ..
                                }) => {
                                    if let Some(s) = suffix {
                                        match s.as_str() {
                                            "i8" => "i8".to_string(),
                                            "i16" => "i16".to_string(),
                                            "i32" => "i32".to_string(),
                                            "i64" => "i64".to_string(),
                                            "u8" => "i8".to_string(),
                                            _ => "i32".to_string(),
                                        }
                                    } else {
                                        "i32".to_string()
                                    }
                                }
                                ExpressionKind::Literal(Literal {
                                    kind: LiteralKind::Float(_, suffix),
                                    ..
                                }) => {
                                    if let Some(s) = suffix {
                                        match s.as_str() {
                                            "f32" => "float".to_string(),
                                            "f64" => "double".to_string(),
                                            _ => "float".to_string(),
                                        }
                                    } else {
                                        "float".to_string()
                                    }
                                }
                                ExpressionKind::Literal(Literal {
                                    kind: LiteralKind::Bool(_),
                                    ..
                                }) => "i1".to_string(),
                                _ => "i32".to_string(),
                            }
                        } else {
                            "i32".to_string()
                        };
                        let array_ty = format!("[{} x {}]", len, elem_ty);
                        let alloca = self.next_register();
                        self.lines
                            .push(format!("  {} = alloca {}", alloca, array_ty));
                        for (i, elem_expr) in elems.iter().enumerate() {
                            let val = match &elem_expr.node {
                                ExpressionKind::Literal(Literal {
                                    kind: LiteralKind::Int(v, _),
                                    ..
                                }) => v.to_string(),
                                ExpressionKind::Literal(Literal {
                                    kind: LiteralKind::Float(f, _),
                                    ..
                                }) => f.to_string(),
                                ExpressionKind::Literal(Literal {
                                    kind: LiteralKind::Bool(b),
                                    ..
                                }) => {
                                    if *b {
                                        "1".to_string()
                                    } else {
                                        "0".to_string()
                                    }
                                }
                                _ => self.emit_expr(elem_expr),
                            };
                            let gep = self.next_register();
                            self.lines.push(format!(
                                "  {} = getelementptr inbounds {}, {}* {}, i32 0, i32 {}",
                                gep, array_ty, array_ty, alloca, i
                            ));
                            self.lines
                                .push(format!("  store {} {}, {}* {}", elem_ty, val, elem_ty, gep));
                        }
                        if wants_growable {
                            let raw_ptr = self.next_register();
                            self.lines.push(format!(
                                "  {} = bitcast {}* {} to i8*",
                                raw_ptr, array_ty, alloca
                            ));

                            let elem_size = self.llvm_const_sizeof(&elem_ty);
                            let elem_align = self.llvm_const_alignof(&elem_ty);
                            let out = self.next_register();
                            self.lines.push(format!(
                                "  {} = call %struct.TyArray* @ty_array_from_fixed(i8* {}, i64 {}, i64 {}, i64 {})",
                                out, raw_ptr, len, elem_size, elem_align
                            ));

                            let slot = self.next_register();
                            self.lines
                                .push(format!("  {} = alloca %struct.TyArray*", slot));
                            self.lines.push(format!(
                                "  store %struct.TyArray* {}, %struct.TyArray** {}",
                                out, slot
                            ));
                            self.locals.insert(name.name.clone(), slot);
                            self.locals_type
                                .insert(name.name.clone(), "%struct.TyArray*".to_string());
                        } else {
                            self.locals.insert(name.name.clone(), alloca.clone());
                            self.locals_type.insert(name.name.clone(), array_ty);
                        }
                    } else {
                        let value = self.emit_expr(initializer);
                        let ty = type_annotation
                            .as_ref()
                            .map(|ty| self.lower_type(ty))
                            .unwrap_or_else(|| "i32".to_string());
                        if *mutable && !ty.ends_with('*') && ty != "void" {
                            let (size, align) = self.llvm_size_align_of(&ty);
                            let raw = self.next_register();
                            self.lines.push(format!(
                                "  {} = call i8* @ty_alloc(i64 {}, i64 {})",
                                raw, size, align
                            ));
                            let ptr = self.next_register();
                            self.lines.push(format!(
                                "  {} = bitcast i8* {} to {}*",
                                ptr, raw, ty
                            ));
                            self.lines
                                .push(format!("  store {} {}, {}* {}", ty, value, ty, ptr));
                            self.locals.insert(name.name.clone(), ptr);
                            self.locals_type.insert(name.name.clone(), ty);
                        } else {
                            let alloca = self.next_register();
                            self.lines.push(format!("  {} = alloca {}", alloca, ty));
                            self.lines
                                .push(format!("  store {} {}, {}* {}", ty, value, ty, alloca));
                            self.locals.insert(name.name.clone(), alloca);
                            self.locals_type.insert(name.name.clone(), ty);
                        }
                    }
                }
                StatementKind::Expression(expr) => {
                    let _ = self.emit_expr(expr);
                }
                StatementKind::If {
                    condition,
                    then_branch,
                    else_branch,
                } => {
                    let cond_val = self.emit_expr(condition);
                    let then_label = self.next_block("then");
                    let else_label = self.next_block("else");
                    let merge_label = self.next_block("if_merge");

                    self.lines.push(format!(
                        "  br i1 {}, label %{}, label %{}",
                        cond_val, then_label, else_label
                    ));

                    self.lines.push(format!("{}:", then_label));
                    let then_terminated =
                        self.emit_block_terminated(then_branch, current_fn_ret_ty);
                    if !then_terminated {
                        self.lines.push(format!("  br label %{}", merge_label));
                    }

                    self.lines.push(format!("{}:", else_label));
                    let else_terminated = match else_branch {
                        None => {
                            self.lines.push(format!("  br label %{}", merge_label));
                            true
                        }
                        Some(eb) => match &eb.node {
                            ElseBranchKind::Block(block) => {
                                let t = self.emit_block_terminated(block, current_fn_ret_ty);
                                if !t {
                                    self.lines.push(format!("  br label %{}", merge_label));
                                }
                                t
                            }
                            ElseBranchKind::If(stmt) => {
                                self.emit_statement_terminated(stmt, current_fn_ret_ty)
                            }
                        },
                    };

                    self.lines.push(format!("{}:", merge_label));
                    if then_terminated && else_terminated {
                        self.lines.push("  unreachable".to_string());
                    }
                }
                StatementKind::Match { expr, arms } => {
                    self.emit_match_statement(expr, arms, current_fn_ret_ty);
                }
                StatementKind::Loop { kind, body } => {
                    let loop_label = self.next_block("loop");
                    let loop_body = self.next_block("loop_body");
                    let loop_end = self.next_block("loop_end");
                    match &kind.node {
                        LoopKindKind::While { condition, .. } => {
                            self.lines.push(format!("  br label %{}", loop_label));
                            self.lines.push(format!("{}:", loop_label));
                            let cond_val = self.emit_expr(condition);
                            self.lines.push(format!(
                                "  br i1 {}, label %{}, label %{}",
                                cond_val, loop_body, loop_end
                            ));
                            self.lines.push(format!("{}:", loop_body));
                            self.emit_block(body, current_fn_ret_ty);
                            self.lines.push(format!("  br label %{}", loop_label));
                            self.lines.push(format!("{}:", loop_end));
                        }
                        LoopKindKind::For {
                            pattern, iterator, ..
                        } => {
                            if let ExpressionKind::Identifier(iter_id) = &iterator.node {
                                if let Some(iter_ty) = self.locals_type.get(&iter_id.name).cloned()
                                {
                                    if iter_ty.starts_with('[') && iter_ty.contains(" x i32") {
                                        if let Some(end_bracket) = iter_ty.find(']') {
                                            let inside = &iter_ty[1..end_bracket];
                                            if let Some(space_idx) = inside.find(' ') {
                                                let len_str = &inside[..space_idx];
                                                if let Ok(len) = len_str.parse::<usize>() {
                                                    if let PatternKind::Identifier(ident) =
                                                        &pattern.node
                                                    {
                                                        let pat_alloc = self.next_register();
                                                        self.lines.push(format!(
                                                            "  {} = alloca i32",
                                                            pat_alloc
                                                        ));
                                                        self.locals.insert(
                                                            ident.name.clone(),
                                                            pat_alloc.clone(),
                                                        );
                                                        self.locals_type.insert(
                                                            ident.name.clone(),
                                                            "i32".to_string(),
                                                        );
                                                        let idx_slot = self.next_register();
                                                        self.lines.push(format!(
                                                            "  {} = alloca i32",
                                                            idx_slot
                                                        ));
                                                        self.lines.push(format!(
                                                            "  store i32 0, i32* {}",
                                                            idx_slot
                                                        ));
                                                        let iter_alloc = self
                                                            .locals
                                                            .get(&iter_id.name)
                                                            .cloned()
                                                            .unwrap_or(iter_id.name.clone());
                                                        self.lines.push(format!(
                                                            "  br label %{}",
                                                            loop_label
                                                        ));
                                                        self.lines.push(format!("{}:", loop_label));
                                                        let idx_val = self.next_register();
                                                        self.lines.push(format!(
                                                            "  {} = load i32, i32* {}",
                                                            idx_val, idx_slot
                                                        ));
                                                        let cmp = self.next_register();
                                                        self.lines.push(format!(
                                                            "  {} = icmp slt i32 {}, {}",
                                                            cmp, idx_val, len
                                                        ));
                                                        self.lines.push(format!(
                                                            "  br i1 {}, label %{}, label %{}",
                                                            cmp, loop_body, loop_end
                                                        ));
                                                        self.lines.push(format!("{}:", loop_body));
                                                        let idx_val2 = self.next_register();
                                                        self.lines.push(format!(
                                                            "  {} = load i32, i32* {}",
                                                            idx_val2, idx_slot
                                                        ));
                                                        let gep = self.next_register();
                                                        self.lines.push(format!("  {} = getelementptr inbounds {}, {}* {}, i32 0, i32 {}", gep, iter_ty, iter_ty, iter_alloc, idx_val2));
                                                        let elem = self.next_register();
                                                        self.lines.push(format!(
                                                            "  {} = load i32, i32* {}",
                                                            elem, gep
                                                        ));
                                                        self.lines.push(format!(
                                                            "  store i32 {}, i32* {}",
                                                            elem, pat_alloc
                                                        ));
                                                        let body_terminated = self
                                                            .emit_block_terminated(
                                                                body,
                                                                current_fn_ret_ty,
                                                            );
                                                        if !body_terminated {
                                                            let next_idx = self.next_register();
                                                            self.lines.push(format!(
                                                                "  {} = add i32 {}, 1",
                                                                next_idx, idx_val2
                                                            ));
                                                            self.lines.push(format!(
                                                                "  store i32 {}, i32* {}",
                                                                next_idx, idx_slot
                                                            ));
                                                            self.lines.push(format!(
                                                                "  br label %{}",
                                                                loop_label
                                                            ));
                                                        }
                                                        self.lines.push(format!("{}:", loop_end));
                                                        continue;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            self.lines.push(format!("  br label %{}", loop_label));
                            self.lines.push(format!("{}:", loop_body));
                            self.emit_block(body, current_fn_ret_ty);
                            self.lines.push(format!("  br label %{}", loop_label));
                            self.lines.push(format!("{}:", loop_end));
                        }
                        _ => {
                            self.lines.push(format!("  br label %{}", loop_label));
                            self.lines.push(format!("{}:", loop_body));
                            self.emit_block(body, current_fn_ret_ty);
                            self.lines.push(format!("  br label %{}", loop_label));
                            self.lines.push(format!("{}:", loop_end));
                        }
                    }
                }
                StatementKind::Conc { body } => {
                    self.emit_block(body, current_fn_ret_ty);
                }
                _ => {}
            }
        }
        if let Some(expr) = &block.trailing_expression {
            let value = self.emit_expr(expr);
            self.last_value = Some(value);
        }
    }

    fn emit_block_scoped(&mut self, block: &Block, current_fn_ret_ty: &str) -> Option<String> {
        let saved_locals = self.locals.clone();
        let saved_types = self.locals_type.clone();
        let saved_last_value = self.last_value.clone();
        self.last_value = None;
        self.emit_block(block, current_fn_ret_ty);
        let value = self.last_value.clone();
        self.locals = saved_locals;
        self.locals_type = saved_types;
        self.last_value = saved_last_value;
        value
    }

    fn emit_block_terminated(&mut self, block: &Block, ret_ty: &str) -> bool {
        for stmt in &block.statements {
            let terminated = self.emit_statement_terminated(stmt, ret_ty);
            if terminated {
                return true;
            }
        }
        false
    }

    fn emit_statement_terminated(&mut self, stmt: &Statement, ret_ty: &str) -> bool {
        match &stmt.node {
            StatementKind::Return(Some(expr)) => {
                let value = self.emit_expr(expr);
                self.lines.push(format!("  ret {} {}", ret_ty, value));
                true
            }
            StatementKind::Return(None) => {
                self.lines.push("  ret void".to_string());
                true
            }
            StatementKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond_val = self.emit_expr(condition);
                let then_label = self.next_block("then");
                let else_label = self.next_block("else");
                let merge_label = self.next_block("if_merge");

                self.lines.push(format!(
                    "  br i1 {}, label %{}, label %{}",
                    cond_val, then_label, else_label
                ));

                self.lines.push(format!("{}:", then_label));
                let then_terminated = self.emit_block_terminated(then_branch, ret_ty);
                if !then_terminated {
                    self.lines.push(format!("  br label %{}", merge_label));
                }

                self.lines.push(format!("{}:", else_label));
                let else_terminated = match else_branch {
                    None => {
                        self.lines.push(format!("  br label %{}", merge_label));
                        true
                    }
                    Some(eb) => match &eb.node {
                        ElseBranchKind::Block(block) => {
                            let t = self.emit_block_terminated(block, ret_ty);
                            if !t {
                                self.lines.push(format!("  br label %{}", merge_label));
                            }
                            t
                        }
                        ElseBranchKind::If(stmt) => self.emit_statement_terminated(stmt, ret_ty),
                    },
                };

                self.lines.push(format!("{}:", merge_label));
                if then_terminated && else_terminated {
                    self.lines.push("  unreachable".to_string());
                    return true;
                }
                false
            }
            StatementKind::Match { expr: _, arms } => {
                let merge_label = self.next_block("match_merge");
                for (idx, arm) in arms.iter().enumerate() {
                    let arm_label = self.next_block(&format!("match_arm_{}", idx));
                    self.lines.push(format!("  br label %{}", arm_label));
                    self.lines.push(format!("{}:", arm_label));
                    let _ = self.emit_expr(&arm.node.body);
                    self.lines.push(format!("  br label %{}", merge_label));
                }
                self.lines.push(format!("{}:", merge_label));
                false
            }
            StatementKind::Loop { kind, body } => {
                match &kind.node {
                    LoopKindKind::While { condition, .. } => {
                        let loop_label = self.next_block("loop");
                        let loop_body = self.next_block("loop_body");
                        let loop_end = self.next_block("loop_end");
                        self.lines.push(format!("  br label %{}", loop_label));
                        self.lines.push(format!("{}:", loop_label));
                        let cond_val = self.emit_expr(condition);
                        self.lines.push(format!(
                            "  br i1 {}, label %{}, label %{}",
                            cond_val, loop_body, loop_end
                        ));
                        self.lines.push(format!("{}:", loop_body));
                        self.emit_block(body, ret_ty);
                        self.lines.push(format!("  br label %{}", loop_label));
                        self.lines.push(format!("{}:", loop_end));
                    }
                    _ => {
                        self.emit_statement(stmt, ret_ty);
                    }
                }
                false
            }
            _other => {
                self.emit_statement(stmt, ret_ty);
                false
            }
        }
    }

    fn emit_statement(&mut self, stmt: &Statement, current_fn_ret_ty: &str) {
        match &stmt.node {
            StatementKind::Return(Some(expr)) => {
                let value = self.emit_expr(expr);
                self.last_value = Some(value);
            }
            StatementKind::Return(None) => {
                self.last_value = None;
            }
            StatementKind::Expression(expr) => {
                let _ = self.emit_expr(expr);
            }
            StatementKind::If { .. } | StatementKind::Match { .. } | StatementKind::Loop { .. } => {
                self.emit_block(
                    &Block {
                        statements: vec![stmt.clone()],
                        trailing_expression: None,
                        span: stmt.span,
                    },
                    current_fn_ret_ty,
                );
            }
            _ => {}
        }
    }

    fn annotate_span(&mut self, span: &Span) {
        if *span == Span::default() {
            return;
        }
        self.lines.push(format!(
            "  ; span {}..{} @ {}:{}",
            span.start, span.end, span.line, span.col
        ));
    }

    fn emit_expr(&mut self, expr: &Expression) -> String {
        match &expr.node {
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Int(value, _),
                ..
            }) => value.to_string(),
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Bool(value),
                ..
            }) => {
                if *value {
                    "1".to_string()
                } else {
                    "0".to_string()
                }
            }
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Str(value),
                ..
            }) => self.emit_string(value),
            ExpressionKind::Block(block) => {
                let current_fn_ret_ty = self.current_fn_ret_ty.clone();
                self.emit_block_scoped(block, &current_fn_ret_ty)
                    .unwrap_or_else(|| "0".to_string())
            }
            ExpressionKind::Identifier(id) => {
                if let Some(slot) = self.locals.get(&id.name).cloned() {
                    let ty = self
                        .locals_type
                        .get(&id.name)
                        .cloned()
                        .unwrap_or_else(|| "i32".to_string());
                    let tmp = self.next_register();
                    self.lines
                        .push(format!("  {} = load {}, {}* {}", tmp, ty, ty, slot));
                    tmp
                } else {
                    id.name.clone()
                }
            }
            ExpressionKind::BinaryOp { op, left, right } => {
                fn is_float_ty(s: &str) -> bool {
                    matches!(s, "float" | "double" | "half")
                }
                fn is_bool_ty(s: &str) -> bool {
                    s == "i1"
                }

                let ty = match &left.node {
                    ExpressionKind::Identifier(id) => self
                        .locals_type
                        .get(&id.name)
                        .cloned()
                        .unwrap_or_else(|| "i32".to_string()),
                    ExpressionKind::Literal(Literal {
                        kind: LiteralKind::Int(_, suffix),
                        ..
                    }) => {
                        if let Some(s) = suffix {
                            match s.as_str() {
                                "i8" => "i8".to_string(),
                                "i16" => "i16".to_string(),
                                "i32" => "i32".to_string(),
                                "i64" => "i64".to_string(),
                                "u8" => "i8".to_string(),
                                _ => "i32".to_string(),
                            }
                        } else {
                            "i32".to_string()
                        }
                    }
                    ExpressionKind::Literal(Literal {
                        kind: LiteralKind::Float(_, suffix),
                        ..
                    }) => {
                        if let Some(s) = suffix {
                            match s.as_str() {
                                "f32" => "float".to_string(),
                                "f64" => "double".to_string(),
                                _ => "float".to_string(),
                            }
                        } else {
                            "float".to_string()
                        }
                    }
                    ExpressionKind::Literal(Literal {
                        kind: LiteralKind::Bool(_),
                        ..
                    }) => "i1".to_string(),
                    ExpressionKind::Call { func, .. } => {
                        if let ExpressionKind::Identifier(id) = &func.node {
                            self.func_sigs
                                .get(&id.name)
                                .map(|(r, _)| r.clone())
                                .unwrap_or_else(|| "i32".to_string())
                        } else {
                            "i32".to_string()
                        }
                    }
                    _ => "i32".to_string(),
                };

                match op {
                    Operator::AddAssign
                    | Operator::SubAssign
                    | Operator::MulAssign
                    | Operator::DivAssign => {
                        if let ExpressionKind::Identifier(id) = &left.node {
                            let slot = self
                                .locals
                                .get(&id.name)
                                .cloned()
                                .unwrap_or(id.name.clone());
                            let lhs_val = self.next_register();
                            self.lines
                                .push(format!("  {} = load {}, {}* {}", lhs_val, ty, ty, slot));
                            let rhs_val = self.emit_expr(right);
                            let res = self.next_register();
                            let instr = if is_float_ty(&ty) {
                                match op {
                                    Operator::AddAssign => {
                                        format!("  {} = fadd {} {}, {}", res, ty, lhs_val, rhs_val)
                                    }
                                    Operator::SubAssign => {
                                        format!("  {} = fsub {} {}, {}", res, ty, lhs_val, rhs_val)
                                    }
                                    Operator::MulAssign => {
                                        format!("  {} = fmul {} {}, {}", res, ty, lhs_val, rhs_val)
                                    }
                                    Operator::DivAssign => {
                                        format!("  {} = fdiv {} {}, {}", res, ty, lhs_val, rhs_val)
                                    }
                                    _ => {
                                        format!("  {} = fadd {} {}, {}", res, ty, lhs_val, rhs_val)
                                    }
                                }
                            } else {
                                match op {
                                    Operator::AddAssign => {
                                        format!("  {} = add {} {}, {}", res, ty, lhs_val, rhs_val)
                                    }
                                    Operator::SubAssign => {
                                        format!("  {} = sub {} {}, {}", res, ty, lhs_val, rhs_val)
                                    }
                                    Operator::MulAssign => {
                                        format!("  {} = mul {} {}, {}", res, ty, lhs_val, rhs_val)
                                    }
                                    Operator::DivAssign => {
                                        format!("  {} = sdiv {} {}, {}", res, ty, lhs_val, rhs_val)
                                    }
                                    _ => format!("  {} = add {} {}, {}", res, ty, lhs_val, rhs_val),
                                }
                            };
                            self.lines.push(instr);
                            self.lines
                                .push(format!("  store {} {}, {}* {}", ty, res, ty, slot));
                            return res;
                        }
                        if let ExpressionKind::IndexAccess { base, index } = &left.node {
                            let (base_ptr, array_ty) =
                                if let ExpressionKind::Identifier(bid) = &base.node {
                                    let ptr = self
                                        .locals
                                        .get(&bid.name)
                                        .cloned()
                                        .unwrap_or(bid.name.clone());
                                    let aty = self
                                        .locals_type
                                        .get(&bid.name)
                                        .cloned()
                                        .unwrap_or_else(|| "[0 x i32]".to_string());
                                    (ptr, aty)
                                } else {
                                    (self.emit_expr(base), "[0 x i32]".to_string())
                                };
                            let elem_ty = if let Some(xpos) = array_ty.find(" x ") {
                                let after = &array_ty[xpos + 3..];
                                let end = after.find(']').unwrap_or(after.len());
                                after[..end].to_string()
                            } else {
                                "i32".to_string()
                            };
                            let idx_val = self.emit_expr(index);
                            let gep = self.next_register();
                            self.lines.push(format!(
                                "  {} = getelementptr inbounds {}, {}* {}, i32 0, i32 {}",
                                gep, array_ty, array_ty, base_ptr, idx_val
                            ));
                            let lhs_val = self.next_register();
                            self.lines.push(format!(
                                "  {} = load {}, {}* {}",
                                lhs_val, elem_ty, elem_ty, gep
                            ));
                            let rhs_val = self.emit_expr(right);
                            let res = self.next_register();
                            let instr = if is_float_ty(&elem_ty) {
                                match op {
                                    Operator::AddAssign => format!(
                                        "  {} = fadd {} {}, {}",
                                        res, elem_ty, lhs_val, rhs_val
                                    ),
                                    Operator::SubAssign => format!(
                                        "  {} = fsub {} {}, {}",
                                        res, elem_ty, lhs_val, rhs_val
                                    ),
                                    Operator::MulAssign => format!(
                                        "  {} = fmul {} {}, {}",
                                        res, elem_ty, lhs_val, rhs_val
                                    ),
                                    Operator::DivAssign => format!(
                                        "  {} = fdiv {} {}, {}",
                                        res, elem_ty, lhs_val, rhs_val
                                    ),
                                    _ => format!(
                                        "  {} = fadd {} {}, {}",
                                        res, elem_ty, lhs_val, rhs_val
                                    ),
                                }
                            } else {
                                match op {
                                    Operator::AddAssign => format!(
                                        "  {} = add {} {}, {}",
                                        res, elem_ty, lhs_val, rhs_val
                                    ),
                                    Operator::SubAssign => format!(
                                        "  {} = sub {} {}, {}",
                                        res, elem_ty, lhs_val, rhs_val
                                    ),
                                    Operator::MulAssign => format!(
                                        "  {} = mul {} {}, {}",
                                        res, elem_ty, lhs_val, rhs_val
                                    ),
                                    Operator::DivAssign => format!(
                                        "  {} = sdiv {} {}, {}",
                                        res, elem_ty, lhs_val, rhs_val
                                    ),
                                    _ => format!(
                                        "  {} = add {} {}, {}",
                                        res, elem_ty, lhs_val, rhs_val
                                    ),
                                }
                            };
                            self.lines.push(instr);
                            self.lines
                                .push(format!("  store {} {}, {}* {}", elem_ty, res, elem_ty, gep));
                            return res;
                        }
                        if let ExpressionKind::FieldAccess { base, field } = &left.node {
                            let (base_ptr, base_ty) =
                                if let ExpressionKind::Identifier(bid) = &base.node {
                                    let ptr = self
                                        .locals
                                        .get(&bid.name)
                                        .cloned()
                                        .unwrap_or(bid.name.clone());
                                    let bty = self
                                        .locals_type
                                        .get(&bid.name)
                                        .cloned()
                                        .unwrap_or_else(|| "%struct.?".to_string());
                                    (ptr, bty)
                                } else {
                                    (self.emit_expr(base), "%struct.?".to_string())
                                };
                            let struct_name = base_ty.trim_start_matches("%struct.").to_string();
                            let field_index = self
                                .struct_fields
                                .get(&struct_name)
                                .and_then(|fields| fields.iter().position(|f| f.0 == field.name))
                                .unwrap_or(0);
                            let field_ty = self
                                .struct_fields
                                .get(&struct_name)
                                .and_then(|fields| fields.get(field_index))
                                .map(|(_, ty)| ty.clone())
                                .unwrap_or_else(|| "i32".to_string());
                            let gep = self.next_register();
                            self.lines.push(format!(
                                "  {} = getelementptr inbounds {}, {}* {}, i32 0, i32 {}",
                                gep, base_ty, base_ty, base_ptr, field_index
                            ));
                            let lhs_val = self.next_register();
                            self.lines.push(format!(
                                "  {} = load {}, {}* {}",
                                lhs_val, field_ty, field_ty, gep
                            ));
                            let rhs_val = self.emit_expr(right);
                            let res = self.next_register();
                            let instr = if is_float_ty(&field_ty) {
                                match op {
                                    Operator::AddAssign => format!(
                                        "  {} = fadd {} {}, {}",
                                        res, field_ty, lhs_val, rhs_val
                                    ),
                                    Operator::SubAssign => format!(
                                        "  {} = fsub {} {}, {}",
                                        res, field_ty, lhs_val, rhs_val
                                    ),
                                    Operator::MulAssign => format!(
                                        "  {} = fmul {} {}, {}",
                                        res, field_ty, lhs_val, rhs_val
                                    ),
                                    Operator::DivAssign => format!(
                                        "  {} = fdiv {} {}, {}",
                                        res, field_ty, lhs_val, rhs_val
                                    ),
                                    _ => format!(
                                        "  {} = fadd {} {}, {}",
                                        res, field_ty, lhs_val, rhs_val
                                    ),
                                }
                            } else {
                                match op {
                                    Operator::AddAssign => format!(
                                        "  {} = add {} {}, {}",
                                        res, field_ty, lhs_val, rhs_val
                                    ),
                                    Operator::SubAssign => format!(
                                        "  {} = sub {} {}, {}",
                                        res, field_ty, lhs_val, rhs_val
                                    ),
                                    Operator::MulAssign => format!(
                                        "  {} = mul {} {}, {}",
                                        res, field_ty, lhs_val, rhs_val
                                    ),
                                    Operator::DivAssign => format!(
                                        "  {} = sdiv {} {}, {}",
                                        res, field_ty, lhs_val, rhs_val
                                    ),
                                    _ => format!(
                                        "  {} = add {} {}, {}",
                                        res, field_ty, lhs_val, rhs_val
                                    ),
                                }
                            };
                            self.lines.push(instr);
                            self.lines.push(format!(
                                "  store {} {}, {}* {}",
                                field_ty, res, field_ty, gep
                            ));
                            return res;
                        }
                    }
                    Operator::Pipe => {
                        if let ExpressionKind::Call { func, args } = &right.node {
                            if let ExpressionKind::Identifier(id) = &func.node {
                                let mut arg_pairs = Vec::new();
                                let lhs = self.emit_expr(left);
                                let param_types = self
                                    .func_sigs
                                    .get(&id.name)
                                    .map(|(_, p)| p.clone())
                                    .unwrap_or_else(|| vec![]);
                                let first_ty = param_types
                                    .get(0)
                                    .cloned()
                                    .unwrap_or_else(|| "i32".to_string());
                                arg_pairs.push(format!("{} {}", first_ty, lhs));
                                for (idx, a) in args.iter().enumerate() {
                                    let val = self.emit_expr(a);
                                    let ty_a = param_types
                                        .get(idx + 1)
                                        .cloned()
                                        .unwrap_or_else(|| "i32".to_string());
                                    arg_pairs.push(format!("{} {}", ty_a, val));
                                }
                                let ret_ty = self
                                    .func_sigs
                                    .get(&id.name)
                                    .map(|(r, _)| r.clone())
                                    .unwrap_or_else(|| "i32".to_string());
                                let tmp = self.next_register();
                                self.lines.push(format!(
                                    "  {} = call {} @{}({})",
                                    tmp,
                                    ret_ty,
                                    id.name,
                                    arg_pairs.join(", ")
                                ));
                                return tmp;
                            }
                        } else if let ExpressionKind::Identifier(id) = &right.node {
                            let lhs = self.emit_expr(left);
                            let param_types = self
                                .func_sigs
                                .get(&id.name)
                                .map(|(_, p)| p.clone())
                                .unwrap_or_else(|| vec![]);
                            let ty0 = param_types
                                .get(0)
                                .cloned()
                                .unwrap_or_else(|| "i32".to_string());
                            let ret_ty = self
                                .func_sigs
                                .get(&id.name)
                                .map(|(r, _)| r.clone())
                                .unwrap_or_else(|| "i32".to_string());
                            let tmp = self.next_register();
                            self.lines.push(format!(
                                "  {} = call {} @{}({} {})",
                                tmp, ret_ty, id.name, ty0, lhs
                            ));
                            return tmp;
                        }
                        let _ = self.emit_expr(left);
                        let _ = self.emit_expr(right);
                        return "0".to_string();
                    }
                    _ => {}
                }

                let lhs = self.emit_expr(left);
                let rhs = self.emit_expr(right);
                let tmp = self.next_register();
                let instr = if is_float_ty(&ty) {
                    match op {
                        Operator::Add => format!("  {} = fadd {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Sub => format!("  {} = fsub {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Mul => format!("  {} = fmul {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Div => format!("  {} = fdiv {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Mod => format!("  {} = frem {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Eq => format!("  {} = fcmp oeq {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Ne => format!("  {} = fcmp one {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Lt => format!("  {} = fcmp olt {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Gt => format!("  {} = fcmp ogt {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Le => format!("  {} = fcmp ole {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Ge => format!("  {} = fcmp oge {} {}, {}", tmp, ty, lhs, rhs),
                        _ => format!("  {} = fadd {} {}, {}", tmp, ty, lhs, rhs),
                    }
                } else if is_bool_ty(&ty) {
                    match op {
                        Operator::And => format!("  {} = and i1 {}, {}", tmp, lhs, rhs),
                        Operator::Or => format!("  {} = or i1 {}, {}", tmp, lhs, rhs),
                        Operator::Eq => format!("  {} = icmp eq i1 {}, {}", tmp, lhs, rhs),
                        Operator::Ne => format!("  {} = icmp ne i1 {}, {}", tmp, lhs, rhs),
                        _ => format!("  {} = or i1 {}, {}", tmp, lhs, rhs),
                    }
                } else {
                    match op {
                        Operator::Add => format!("  {} = add {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Sub => format!("  {} = sub {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Mul => format!("  {} = mul {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Div => format!("  {} = sdiv {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Mod => format!("  {} = srem {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Eq => format!("  {} = icmp eq {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Ne => format!("  {} = icmp ne {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Lt => format!("  {} = icmp slt {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Gt => format!("  {} = icmp sgt {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Le => format!("  {} = icmp sle {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Ge => format!("  {} = icmp sge {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::And => format!("  {} = and {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Or => format!("  {} = or {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::BitAnd => format!("  {} = and {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::BitOr => format!("  {} = or {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::BitXor => format!("  {} = xor {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Shl => format!("  {} = shl {} {}, {}", tmp, ty, lhs, rhs),
                        Operator::Shr => format!("  {} = lshr {} {}, {}", tmp, ty, lhs, rhs),
                        _ => format!("  {} = add {} {}, {}", tmp, ty, lhs, rhs),
                    }
                };
                self.lines.push(instr);
                tmp
            }
            ExpressionKind::StructInit { name, fields } => {
                let struct_ty = format!("%struct.{}", name.name);
                let mut cur = "undef".to_string();
                for (field_name, field_expr) in fields {
                    let field_value = self.emit_expr(field_expr);
                    let field_index = self
                        .struct_fields
                        .get(&name.name)
                        .and_then(|fields| fields.iter().position(|f| f.0 == field_name.name))
                        .unwrap_or(0);
                    let field_type = self
                        .struct_fields
                        .get(&name.name)
                        .and_then(|fields| fields.get(field_index))
                        .map(|(_, ty)| ty.clone())
                        .unwrap_or_else(|| "i32".to_string());
                    let next = self.next_register();
                    self.lines.push(format!(
                        "  {} = insertvalue {} {}, {} {}, {}",
                        next, struct_ty, cur, field_type, field_value, field_index
                    ));
                    cur = next;
                }
                cur
            }
            ExpressionKind::MergeExpression { base, fields } => {
                let (mut cur, base_ty) = if let Some(base_expr) = base {
                    match &base_expr.node {
                        ExpressionKind::Identifier(id) => {
                            let ty = self
                                .locals_type
                                .get(&id.name)
                                .cloned()
                                .unwrap_or_else(|| "%struct.?".to_string());
                            (self.emit_expr(base_expr), ty)
                        }
                        _ => (self.emit_expr(base_expr), self.expr_llvm_type(base_expr)),
                    }
                } else {
                    ("undef".to_string(), "%struct.?".to_string())
                };

                let struct_name = base_ty.trim_start_matches("%struct.").to_string();
                for (field_name, field_expr) in fields {
                    let value = self.emit_expr(field_expr);
                    let field_index = self
                        .struct_fields
                        .get(&struct_name)
                        .and_then(|fields| fields.iter().position(|f| f.0 == field_name.name))
                        .unwrap_or(0);
                    let field_type = self
                        .struct_fields
                        .get(&struct_name)
                        .and_then(|fields| fields.get(field_index))
                        .map(|(_, ty)| ty.clone())
                        .unwrap_or_else(|| "i32".to_string());
                    let next = self.next_register();
                    self.lines.push(format!(
                        "  {} = insertvalue {} {}, {} {}, {}",
                        next, base_ty, cur, field_type, value, field_index
                    ));
                    cur = next;
                }
                cur
            }
            ExpressionKind::FieldAccess { base, field } => {
                let (base_val, base_ty) = match &base.node {
                    ExpressionKind::Identifier(id) => {
                        let ty = self
                            .locals_type
                            .get(&id.name)
                            .cloned()
                            .unwrap_or_else(|| "%struct.?".to_string());
                        (self.emit_expr(base), ty)
                    }
                    _ => (self.emit_expr(base), "%struct.?".to_string()),
                };
                let struct_name = base_ty.trim_start_matches("%struct.").to_string();
                let field_index = self
                    .struct_fields
                    .get(&struct_name)
                    .and_then(|fields| fields.iter().position(|f| f.0 == field.name))
                    .unwrap_or(0);
                let field_ty = self
                    .struct_fields
                    .get(&struct_name)
                    .and_then(|fields| fields.get(field_index))
                    .map(|(_, ty)| ty.clone())
                    .unwrap_or_else(|| "i32".to_string());
                let gep = self.next_register();
                self.lines.push(format!(
                    "  {} = extractvalue {} {}, {}",
                    gep, base_ty, base_val, field_index
                ));
                let _ = field_ty;
                gep
            }
            ExpressionKind::IndexAccess { base, index } => {
                let base_val = self.emit_expr(base);
                let base_ty = self.expr_llvm_type(base);
                let idx_val = self.emit_expr(index);

                let Some((opt_ty, elem_ty)) = self.option_type_for_index(expr) else {
                    return "0".to_string();
                };

                if base_ty == "%struct.TyArray*" {
                    let idx64 = self.next_register();
                    self.lines.push(format!("  {} = sext i32 {} to i64", idx64, idx_val));
                    let raw_ptr = self.next_register();
                    self.lines.push(format!(
                        "  {} = call i8* @ty_array_get_ptr(%struct.TyArray* {}, i64 {})",
                        raw_ptr, base_val, idx64
                    ));
                    return self.emit_option_from_i8_ptr(&opt_ty, &elem_ty, &raw_ptr);
                }

                // Fixed array fallback: base_val is an alloca pointer when base is an identifier.
                // Use locals to find the actual alloca.
                let (base_ptr, array_ty) = match &base.node {
                    ExpressionKind::Identifier(id) => {
                        let ptr = self
                            .locals
                            .get(&id.name)
                            .cloned()
                            .unwrap_or(id.name.clone());
                        let aty = self
                            .locals_type
                            .get(&id.name)
                            .cloned()
                            .unwrap_or_else(|| "[0 x i32]".to_string());
                        (ptr, aty)
                    }
                    _ => (base_val, base_ty),
                };

                if !array_ty.starts_with('[') {
                    return "0".to_string();
                }
                let len = self.fixed_array_len(&array_ty).unwrap_or(0);
                let in_bounds = self.next_register();
                self.lines.push(format!(
                    "  {} = icmp ult i32 {}, {}",
                    in_bounds, idx_val, len
                ));
                let some_label = self.next_block("idx_some");
                let none_label = self.next_block("idx_none");
                let merge_label = self.next_block("idx_merge");
                self.lines.push(format!(
                    "  br i1 {}, label %{}, label %{}",
                    in_bounds, some_label, none_label
                ));

                self.lines.push(format!("{}:", some_label));
                let gep = self.next_register();
                self.lines.push(format!(
                    "  {} = getelementptr inbounds {}, {}* {}, i32 0, i32 {}",
                    gep, array_ty, array_ty, base_ptr, idx_val
                ));
                let loaded = self.next_register();
                self.lines.push(format!(
                    "  {} = load {}, {}* {}",
                    loaded, elem_ty, elem_ty, gep
                ));
                let some_val = self.emit_option_some(&opt_ty, &elem_ty, &loaded);
                self.lines.push(format!("  br label %{}", merge_label));

                self.lines.push(format!("{}:", none_label));
                let none_val = self.emit_option_none(&opt_ty, &elem_ty);
                self.lines.push(format!("  br label %{}", merge_label));

                self.lines.push(format!("{}:", merge_label));
                let phi = self.next_register();
                self.lines.push(format!(
                    "  {} = phi {} [ {}, %{} ], [ {}, %{} ]",
                    phi, opt_ty, some_val, some_label, none_val, none_label
                ));
                phi
            }
            ExpressionKind::Call { func, args } => {
                if let ExpressionKind::FieldAccess { base, field } = &func.node {
                    let base_val = self.emit_expr(base);
                    let base_ty = self.expr_llvm_type(base);
                    if base_ty == "%struct.TyArray*" && field.name == "push" {
                        if let Some(arg0) = args.get(0) {
                            let val = self.emit_expr(arg0);
                            let val_ty = self.expr_llvm_type(arg0);
                            let slot = self.next_register();
                            self.lines.push(format!("  {} = alloca {}", slot, val_ty));
                            self.lines
                                .push(format!("  store {} {}, {}* {}", val_ty, val, val_ty, slot));
                            let raw = self.next_register();
                            self.lines.push(format!(
                                "  {} = bitcast {}* {} to i8*",
                                raw, val_ty, slot
                            ));
                            self.lines.push(format!(
                                "  call void @ty_array_push(%struct.TyArray* {}, i8* {})",
                                base_val, raw
                            ));
                        }
                        return "0".to_string();
                    }

                    if let Some(method_sym) = self.method_symbol_for_call(&base_ty, &field.name) {
                        let runtime_name = link_symbol_name(&method_sym);
                        let (ret_ty, param_types) = match self.func_sigs.get(&method_sym) {
                            Some(sig) => sig.clone(),
                            None => ("i32".to_string(), vec![]),
                        };
                        let mut full_args = Vec::new();
                        full_args.push((param_types.get(0).cloned().unwrap_or(base_ty), base_val));
                        for (idx, arg) in args.iter().enumerate() {
                            let val = self.emit_expr(arg);
                            let ty = param_types
                                .get(idx + 1)
                                .cloned()
                                .unwrap_or_else(|| "i32".to_string());
                            full_args.push((ty, val));
                        }
                        if ret_ty == "void" {
                            self.lines.push(format!(
                                "  call void @{}({})",
                                runtime_name,
                                full_args
                                    .iter()
                                    .map(|(t, v)| format!("{} {}", t, v))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ));
                            return "0".to_string();
                        } else {
                            let tmp = self.next_register();
                            self.lines.push(format!(
                                "  {} = call {} @{}({})",
                                tmp,
                                ret_ty,
                                runtime_name,
                                full_args
                                    .iter()
                                    .map(|(t, v)| format!("{} {}", t, v))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ));
                            return tmp;
                        }
                    }
                }

                if let ExpressionKind::Identifier(id) = &func.node {
                    if matches!(id.name.as_str(), "Ok" | "Err" | "Some" | "None") {
                        return self.emit_adt_constructor(&id.name, expr, args);
                    }
                    let runtime_name = runtime_intrinsic_name(&id.name)
                        .unwrap_or_else(|| link_symbol_name(&id.name));
                    let (ret_ty, param_types) = match self.func_sigs.get(&id.name) {
                        Some(sig) => sig.clone(),
                        None => ("i32".to_string(), vec![]),
                    };
                    let tail_prefix = if self.current_fn_name.as_deref() == Some(id.name.as_str()) {
                        "tail "
                    } else {
                        ""
                    };
                    let mut arg_pairs = Vec::new();
                    for (idx, arg) in args.iter().enumerate() {
                        let val = self.emit_expr(arg);
                        let ty = param_types
                            .get(idx)
                            .cloned()
                            .unwrap_or_else(|| "i32".to_string());
                        arg_pairs.push(format!("{} {}", ty, val));
                    }
                    if ret_ty == "void" {
                        self.lines.push(format!(
                            "  {}call void @{}({})",
                            tail_prefix,
                            runtime_name,
                            arg_pairs.join(", ")
                        ));
                        return "0".to_string();
                    } else {
                        let tmp = self.next_register();
                        self.lines.push(format!(
                            "  {} = {}call {} @{}({})",
                            tmp,
                            tail_prefix,
                            ret_ty,
                            runtime_name,
                            arg_pairs.join(", ")
                        ));
                        return tmp;
                    }
                }
                "0".to_string()
            }
            ExpressionKind::TryOperator { expr } => self.emit_expr(expr),
            ExpressionKind::Match { expr, arms } => {
                self.emit_match_expression(expr, arms)
            }
            ExpressionKind::IfLet {
                pattern,
                expr: matched,
                then,
                else_branch,
            } => self.emit_if_let_expression(expr, pattern, matched, then, else_branch.as_deref()),
            ExpressionKind::Placeholder(_) => "0".to_string(),
            _ => "0".to_string(),
        }
    }

    fn next_register(&mut self) -> String {
        let name = format!("%t{}", self.next_tmp);
        self.next_tmp += 1;
        name
    }

    fn method_symbol_for_call(&self, base_ty: &str, method: &str) -> Option<String> {
        let ty = base_ty.trim_end_matches('*');
        if let Some(rest) = ty.strip_prefix("%struct.") {
            return Some(format!("__ty_method__{}__{}", rest, method));
        }
        None
    }

    fn option_type_for_index(&mut self, expr: &Expression) -> Option<(String, String)> {
        let ty = self.inferred_expr_type(expr)?;
        if let InferType::App(name, args) = ty {
            if name == "Option" && args.len() == 1 {
                let elem = self.lower_infer_type(&args[0]);
                let opt = self.lower_infer_type(ty);
                return Some((opt, elem));
            }
        }
        None
    }

    fn fixed_array_len(&self, array_ty: &str) -> Option<usize> {
        let end = array_ty.find(']')?;
        let inner = &array_ty[1..end];
        let space = inner.find(' ')?;
        inner[..space].trim().parse::<usize>().ok()
    }

    fn zero_value(&self, ty: &str) -> String {
        if ty.ends_with('*') {
            "null".to_string()
        } else if ty == "float" {
            "0.0".to_string()
        } else if ty == "double" {
            "0.0".to_string()
        } else if ty == "i1" || ty.starts_with('i') {
            "0".to_string()
        } else {
            "zeroinitializer".to_string()
        }
    }

    fn emit_option_some(&mut self, opt_ty: &str, elem_ty: &str, value: &str) -> String {
        let tmp1 = self.next_register();
        self.lines
            .push(format!("  {} = insertvalue {} undef, i1 1, 0", tmp1, opt_ty));
        let tmp2 = self.next_register();
        self.lines.push(format!(
            "  {} = insertvalue {} {}, {} {}, 1",
            tmp2, opt_ty, tmp1, elem_ty, value
        ));
        tmp2
    }

    fn emit_option_none(&mut self, opt_ty: &str, elem_ty: &str) -> String {
        let tmp1 = self.next_register();
        self.lines
            .push(format!("  {} = insertvalue {} undef, i1 0, 0", tmp1, opt_ty));
        let z = self.zero_value(elem_ty);
        let tmp2 = self.next_register();
        self.lines.push(format!(
            "  {} = insertvalue {} {}, {} {}, 1",
            tmp2, opt_ty, tmp1, elem_ty, z
        ));
        tmp2
    }

    fn emit_option_from_i8_ptr(&mut self, opt_ty: &str, elem_ty: &str, ptr_i8: &str) -> String {
        let cond = self.next_register();
        self.lines
            .push(format!("  {} = icmp ne i8* {}, null", cond, ptr_i8));
        let some_label = self.next_block("opt_some");
        let none_label = self.next_block("opt_none");
        let merge_label = self.next_block("opt_merge");
        self.lines.push(format!(
            "  br i1 {}, label %{}, label %{}",
            cond, some_label, none_label
        ));

        self.lines.push(format!("{}:", some_label));
        let typed_ptr = self.next_register();
        self.lines.push(format!(
            "  {} = bitcast i8* {} to {}*",
            typed_ptr, ptr_i8, elem_ty
        ));
        let loaded = self.next_register();
        self.lines.push(format!(
            "  {} = load {}, {}* {}",
            loaded, elem_ty, elem_ty, typed_ptr
        ));
        let some_val = self.emit_option_some(opt_ty, elem_ty, &loaded);
        self.lines.push(format!("  br label %{}", merge_label));

        self.lines.push(format!("{}:", none_label));
        let none_val = self.emit_option_none(opt_ty, elem_ty);
        self.lines.push(format!("  br label %{}", merge_label));

        self.lines.push(format!("{}:", merge_label));
        let phi = self.next_register();
        self.lines.push(format!(
            "  {} = phi {} [ {}, %{} ], [ {}, %{} ]",
            phi, opt_ty, some_val, some_label, none_val, none_label
        ));
        phi
    }

    fn llvm_const_sizeof(&self, ty: &str) -> i64 {
        match ty {
            "i1" | "i8" => 1,
            "i16" => 2,
            "i32" | "float" => 4,
            "i64" | "double" => 8,
            t if t.ends_with('*') => 8,
            _ => 8,
        }
    }

    fn llvm_const_alignof(&self, ty: &str) -> i64 {
        match ty {
            "i1" | "i8" => 1,
            "i16" => 2,
            "i32" | "float" => 4,
            "i64" | "double" => 8,
            t if t.ends_with('*') => 8,
            _ => 8,
        }
    }

    fn llvm_size_align_of(&mut self, ty: &str) -> (String, String) {
        match ty {
            "i1" | "i8" => ("1".to_string(), "1".to_string()),
            "i16" => ("2".to_string(), "2".to_string()),
            "i32" | "float" => ("4".to_string(), "4".to_string()),
            "i64" | "double" => ("8".to_string(), "8".to_string()),
            t if t.ends_with('*') => ("8".to_string(), "8".to_string()),
            _ => {
                let gep = self.next_register();
                self.lines
                    .push(format!("  {} = getelementptr {}, {}* null, i32 1", gep, ty, ty));
                let sz = self.next_register();
                self.lines
                    .push(format!("  {} = ptrtoint {}* {} to i64", sz, ty, gep));
                (sz, "8".to_string())
            }
        }
    }

    fn expr_llvm_type(&mut self, expr: &Expression) -> String {
        if let ExpressionKind::Identifier(id) = &expr.node {
            if let Some(ty) = self.locals_type.get(&id.name) {
                return ty.clone();
            }
        }

        if let Some(ty) = self.inferred_expr_type(expr).cloned() {
            return self.lower_infer_type(&ty);
        }

        match &expr.node {
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Int(_, suffix),
                ..
            }) => suffix
                .as_deref()
                .map(|s| match s {
                    "i8" => "i8".to_string(),
                    "i16" => "i16".to_string(),
                    "i32" => "i32".to_string(),
                    "i64" => "i64".to_string(),
                    "u8" => "i8".to_string(),
                    _ => "i32".to_string(),
                })
                .unwrap_or_else(|| "i32".to_string()),
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Float(_, suffix),
                ..
            }) => suffix
                .as_deref()
                .map(|s| match s {
                    "f32" => "float".to_string(),
                    "f64" => "double".to_string(),
                    _ => "float".to_string(),
                })
                .unwrap_or_else(|| "float".to_string()),
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Bool(_),
                ..
            }) => "i1".to_string(),
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Str(_),
                ..
            }) => "i8*".to_string(),
            ExpressionKind::StructInit { name, .. } => format!("%struct.{}", name.name),
            ExpressionKind::MergeExpression { base, .. } => base
                .as_ref()
                .map(|expr| self.expr_llvm_type(expr))
                .unwrap_or_else(|| "%struct.?".to_string()),
            ExpressionKind::FieldAccess { base, field } => {
                let base_ty = self.expr_llvm_type(base);
                let struct_name = base_ty.trim_start_matches("%struct.").to_string();
                self.struct_fields
                    .get(&struct_name)
                    .and_then(|fields| fields.iter().find(|(name, _)| name == &field.name))
                    .map(|(_, ty)| ty.clone())
                    .unwrap_or_else(|| "i32".to_string())
            }
            ExpressionKind::Call { func, .. } => {
                if let ExpressionKind::FieldAccess { base, field } = &func.node {
                    let base_ty = self.expr_llvm_type(base);
                    if base_ty == "%struct.TyArray*" && field.name == "push" {
                        return "void".to_string();
                    }
                    if let Some(method_sym) = self.method_symbol_for_call(&base_ty, &field.name) {
                        return self
                            .func_sigs
                            .get(&method_sym)
                            .map(|(ret, _)| ret.clone())
                            .unwrap_or_else(|| "i32".to_string());
                    }
                    "i32".to_string()
                } else if let ExpressionKind::Identifier(id) = &func.node {
                    self.func_sigs
                        .get(&id.name)
                        .map(|(ret, _)| ret.clone())
                        .unwrap_or_else(|| "i32".to_string())
                } else {
                    "i32".to_string()
                }
            }
            ExpressionKind::Block(block) => block
                .trailing_expression
                .as_ref()
                .map(|expr| self.expr_llvm_type(expr))
                .unwrap_or_else(|| "void".to_string()),
            ExpressionKind::Match { .. } | ExpressionKind::IfLet { .. } => self
                .inferred_expr_type(expr)
                .cloned()
                .map(|ty| self.lower_infer_type(&ty))
                .unwrap_or_else(|| "i32".to_string()),
            ExpressionKind::TryOperator { expr } => self.expr_llvm_type(expr),
            _ => "i32".to_string(),
        }
    }

    fn inferred_expr_type<'a>(&self, expr: &'a Expression) -> Option<&'a InferType> {
        let types = self.types?;
        // SAFETY: the pointer is set to the checker-owned map for this lowering pass.
        let types = unsafe { &*types };
        types.get(&expr.id)
    }

    fn emit_if_let_expression(
        &mut self,
        expr: &Expression,
        pattern: &Pattern,
        matched: &Expression,
        then: &Block,
        else_branch: Option<&Expression>,
    ) -> String {
        let result_ty = self.expr_llvm_type(expr);
        let result_slot = if result_ty == "void" {
            None
        } else {
            let slot = self.next_register();
            self.lines.push(format!("  {} = alloca {}", slot, result_ty));
            Some((slot, result_ty))
        };

        let match_value = self.emit_expr(matched);
        let then_label = self.next_block("iflet_then");
        let else_label = self.next_block("iflet_else");
        let merge_label = self.next_block("iflet_merge");
        let success = self.emit_pattern_test(pattern, matched, &match_value);
        self.lines.push(format!(
            "  br i1 {}, label %{}, label %{}",
            success, then_label, else_label
        ));

        self.lines.push(format!("{}:", then_label));
        let saved_locals = self.locals.clone();
        let saved_types = self.locals_type.clone();
        let _ = self.bind_pattern_value(pattern, matched, &match_value);
        let current_fn_ret_ty = self.current_fn_ret_ty.clone();
        let then_value = self
            .emit_block_scoped(then, &current_fn_ret_ty)
            .unwrap_or_else(|| "0".to_string());
        if let Some((slot, ty)) = &result_slot {
            self.lines
                .push(format!("  store {} {}, {}* {}", ty, then_value, ty, slot));
        }
        self.lines.push(format!("  br label %{}", merge_label));
        self.locals = saved_locals;
        self.locals_type = saved_types;

        self.lines.push(format!("{}:", else_label));
        if let Some(else_expr) = else_branch {
            let else_value = self.emit_expr(else_expr);
            if let Some((slot, ty)) = &result_slot {
                self.lines
                    .push(format!("  store {} {}, {}* {}", ty, else_value, ty, slot));
            }
        } else if let Some((slot, ty)) = &result_slot {
            self.lines
                .push(format!("  store {} undef, {}* {}", ty, ty, slot));
        }
        self.lines.push(format!("  br label %{}", merge_label));

        self.lines.push(format!("{}:", merge_label));
        if let Some((slot, ty)) = result_slot {
            let tmp = self.next_register();
            self.lines
                .push(format!("  {} = load {}, {}* {}", tmp, ty, ty, slot));
            return tmp;
        }
        "0".to_string()
    }

    fn emit_match_expression(&mut self, expr: &Expression, arms: &[MatchArm]) -> String {
        let result_ty = self.expr_llvm_type(expr);
        let result_slot = if result_ty == "void" {
            None
        } else {
            let slot = self.next_register();
            self.lines.push(format!("  {} = alloca {}", slot, result_ty));
            Some((slot, result_ty))
        };

        let match_value = self.emit_expr(expr);
        let merge_label = self.next_block("match_merge");
        let fallback_label = self.next_block("match_fallback");
        let mut next_check_label = self.next_block("match_check");
        self.lines.push(format!("  br label %{}", next_check_label));

        for (idx, arm) in arms.iter().enumerate() {
            let body_label = self.next_block(&format!("match_body_{}", idx));
            let following_label = if idx + 1 == arms.len() {
                fallback_label.clone()
            } else {
                self.next_block(&format!("match_check_{}", idx))
            };
            self.lines.push(format!("{}:", next_check_label));
            let success = self.emit_pattern_test(&arm.node.pattern, expr, &match_value);
            self.lines.push(format!(
                "  br i1 {}, label %{}, label %{}",
                success, body_label, following_label
            ));
            self.lines.push(format!("{}:", body_label));
            let saved_locals = self.locals.clone();
            let saved_types = self.locals_type.clone();
            let _ = self.bind_pattern_value(&arm.node.pattern, expr, &match_value);
            if let Some(guard) = &arm.node.guard {
                let guard_value = self.emit_expr(guard);
                let guard_body = self.next_block("match_guard");
                self.lines.push(format!(
                    "  br i1 {}, label %{}, label %{}",
                    guard_value, guard_body, following_label
                ));
                self.lines.push(format!("{}:", guard_body));
            }
            let body_value = self.emit_expr(&arm.node.body);
            if let Some((slot, ty)) = &result_slot {
                self.lines
                    .push(format!("  store {} {}, {}* {}", ty, body_value, ty, slot));
            }
            self.lines.push(format!("  br label %{}", merge_label));
            self.locals = saved_locals;
            self.locals_type = saved_types;
            next_check_label = following_label;
        }

        self.lines.push(format!("{}:", fallback_label));
        if let Some((slot, ty)) = &result_slot {
            self.lines
                .push(format!("  store {} undef, {}* {}", ty, ty, slot));
            self.lines.push(format!("  br label %{}", merge_label));
        } else {
            self.lines.push("  unreachable".to_string());
        }

        self.lines.push(format!("{}:", merge_label));
        if let Some((slot, ty)) = result_slot {
            let tmp = self.next_register();
            self.lines
                .push(format!("  {} = load {}, {}* {}", tmp, ty, ty, slot));
            return tmp;
        }
        "0".to_string()
    }

    fn emit_pattern_test(
        &mut self,
        pattern: &Pattern,
        scrutinee_expr: &Expression,
        scrutinee_value: &str,
    ) -> String {
        let scrutinee_ty = self.expr_llvm_type(scrutinee_expr);
        match &pattern.node {
            PatternKind::Wildcard | PatternKind::Identifier(_) => "1".to_string(),
            PatternKind::Literal(lit) => match &lit.kind {
                LiteralKind::Int(v, _) => {
                    let tmp = self.next_register();
                    self.lines.push(format!(
                        "  {} = icmp eq {} {}, {}",
                        tmp, scrutinee_ty, scrutinee_value, v
                    ));
                    tmp
                }
                LiteralKind::Bool(v) => {
                    let tmp = self.next_register();
                    self.lines.push(format!(
                        "  {} = icmp eq i1 {}, {}",
                        tmp,
                        scrutinee_value,
                        if *v { "1" } else { "0" }
                    ));
                    tmp
                }
                _ => "1".to_string(),
            },
            PatternKind::EnumVariant { variant_name, .. } => {
                if let Some(tag) = enum_variant_tag(&variant_name.name) {
                    let loaded_tag = self.next_register();
                    let cmp = self.next_register();
                    self.lines.push(format!(
                        "  {} = extractvalue {} {}, 0",
                        loaded_tag, scrutinee_ty, scrutinee_value
                    ));
                    self.lines.push(format!("  {} = icmp eq i1 {}, {}", cmp, loaded_tag, tag));
                    cmp
                } else {
                    "1".to_string()
                }
            }
            PatternKind::Struct { .. }
            | PatternKind::Tuple(_)
            | PatternKind::Array(_)
            | PatternKind::Or(_, _)
            | PatternKind::Guard { .. } => "1".to_string(),
        }
    }

    fn bind_pattern_value(
        &mut self,
        pattern: &Pattern,
        scrutinee_expr: &Expression,
        scrutinee_value: &str,
    ) -> Option<()> {
        let scrutinee_ty = self.expr_llvm_type(scrutinee_expr);
        match &pattern.node {
            PatternKind::EnumVariant {
                variant_name,
                payload: Some(inner),
                ..
            } => {
                let payload_index = enum_variant_payload_index(&variant_name.name)?;
                let payload_ty = self
                    .enum_variant_payload_type(scrutinee_expr, &variant_name.name, payload_index)
                    .unwrap_or_else(|| "i32".to_string());
                let extracted = self.next_register();
                self.lines.push(format!(
                    "  {} = extractvalue {} {}, {}",
                    extracted, scrutinee_ty, scrutinee_value, payload_index
                ));
                self.bind_pattern_value_typed(inner, &extracted, &payload_ty)
            }
            _ => self.bind_pattern_value_typed(pattern, scrutinee_value, &scrutinee_ty),
        }
    }

    fn bind_pattern_value_typed(
        &mut self,
        pattern: &Pattern,
        scrutinee_value: &str,
        scrutinee_ty: &str,
    ) -> Option<()> {
        match &pattern.node {
            PatternKind::Wildcard | PatternKind::Literal(_) => Some(()),
            PatternKind::Identifier(id) => {
                let slot = self.next_register();
                self.lines.push(format!("  {} = alloca {}", slot, scrutinee_ty));
                self.lines.push(format!(
                    "  store {} {}, {}* {}",
                    scrutinee_ty, scrutinee_value, scrutinee_ty, slot
                ));
                self.locals.insert(id.name.clone(), slot);
                self.locals_type.insert(id.name.clone(), scrutinee_ty.to_string());
                Some(())
            }
            PatternKind::EnumVariant {
                variant_name,
                payload: Some(inner),
                ..
            } => {
                let payload_index = enum_variant_payload_index(&variant_name.name)?;
                let payload_ty = enum_variant_payload_type(&variant_name.name, scrutinee_ty, payload_index)
                    .unwrap_or_else(|| "i32".to_string());
                let extracted = self.next_register();
                self.lines.push(format!(
                    "  {} = extractvalue {} {}, {}",
                    extracted, scrutinee_ty, scrutinee_value, payload_index
                ));
                self.bind_pattern_value_typed(inner, &extracted, &payload_ty)
            }
            PatternKind::EnumVariant { payload: None, .. } => Some(()),
            PatternKind::Struct { fields, .. } => {
                let struct_name = scrutinee_ty.trim_start_matches("%struct.").to_string();
                for (field_name, field_pattern) in fields {
                    let field_index = self
                        .struct_fields
                        .get(&struct_name)
                        .and_then(|fields| fields.iter().position(|(name, _)| name == &field_name.name))
                        .unwrap_or(0);
                    let field_ty = self
                        .struct_fields
                        .get(&struct_name)
                        .and_then(|fields| fields.get(field_index))
                        .map(|(_, ty)| ty.clone())
                        .unwrap_or_else(|| "i32".to_string());
                    let extracted = self.next_register();
                    self.lines.push(format!(
                        "  {} = extractvalue {} {}, {}",
                        extracted, scrutinee_ty, scrutinee_value, field_index
                    ));
                    let _ = self.bind_pattern_value_typed(field_pattern, &extracted, &field_ty);
                }
                Some(())
            }
            PatternKind::Tuple(parts) | PatternKind::Array(parts) => {
                for (idx, part) in parts.iter().enumerate() {
                    let extracted = self.next_register();
                    self.lines.push(format!(
                        "  {} = extractvalue {} {}, {}",
                        extracted, scrutinee_ty, scrutinee_value, idx
                    ));
                    let _ = self.bind_pattern_value_typed(part, &extracted, "i32");
                }
                Some(())
            }
            PatternKind::Or(left, _) => self.bind_pattern_value_typed(left, scrutinee_value, scrutinee_ty),
            PatternKind::Guard { pattern, .. } => {
                self.bind_pattern_value_typed(pattern, scrutinee_value, scrutinee_ty)
            }
        }
    }

    fn emit_match_statement(&mut self, expr: &Expression, arms: &[MatchArm], current_fn_ret_ty: &str) {
        let _ = current_fn_ret_ty;
        let _ = self.emit_match_expression(expr, arms);
    }

    fn enum_variant_payload_type(
        &mut self,
        scrutinee_expr: &Expression,
        variant_name: &str,
        payload_index: usize,
    ) -> Option<String> {
        let inferred = self.inferred_expr_type(scrutinee_expr)?.clone();
        match inferred {
            InferType::App(name, args) if name == "Option" && args.len() == 1 => {
                (variant_name == "Some" && payload_index == 1)
                    .then(|| self.lower_infer_type(&args[0]))
            }
            InferType::App(name, args) if name == "Result" && args.len() == 2 => match payload_index {
                1 => (variant_name == "Ok").then(|| self.lower_infer_type(&args[0])),
                2 => (variant_name == "Err").then(|| self.lower_infer_type(&args[1])),
                _ => None,
            },
            _ => None,
        }
    }

    fn lower_type(&self, ty: &Type) -> String {
        match ty.node.name.as_str() {
            "Unit" => "void".to_string(),
            "Int8" => "i8".to_string(),
            "Int16" => "i16".to_string(),
            "Int32" => "i32".to_string(),
            "Int64" => "i64".to_string(),
            "Float16" => "half".to_string(),
            "Float32" => "float".to_string(),
            "Float64" => "double".to_string(),
            "Bool" => "i1".to_string(),
            "Str" => "i8*".to_string(),
            "Buf" => "%struct.Buf*".to_string(),
            "Array" => "%struct.TyArray*".to_string(),
            "ref" => "i8*".to_string(),
            "Char" => "i8".to_string(),
            "Byte" => "i8".to_string(),
            "Option" => {
                if let Some(arg0) = ty.node.generic_args.get(0) {
                    let inner = self.lower_type(arg0);
                    format!("%struct.Option__{}", mangle_llvm_type_name(&inner))
                } else {
                    "%struct.Option__i32".to_string()
                }
            }
            "Result" => {
                let a = ty.node.generic_args.get(0).map(|t| self.lower_type(t)).unwrap_or_else(|| "i32".to_string());
                let b = ty.node.generic_args.get(1).map(|t| self.lower_type(t)).unwrap_or_else(|| "i32".to_string());
                format!(
                    "%struct.Result__{}__{}",
                    mangle_llvm_type_name(&a),
                    mangle_llvm_type_name(&b)
                )
            }
            name => format!("%struct.{}", name),
        }
    }

    fn emit_adt_constructor(&mut self, ctor: &str, call_expr: &Expression, args: &[Expression]) -> String {
        let Some(types_ptr) = self.types else {
            return "0".to_string();
        };
        // SAFETY: types map outlives this builder.
        let types = unsafe { &*types_ptr };
        let Some(infer) = types.get(&call_expr.id) else {
            return "0".to_string();
        };
        let ty = self.lower_infer_type(infer);
        let mut cur = "undef".to_string();
        // insert tag at index 0
        let tag = match ctor {
            "Ok" | "Some" => "1",
            "Err" | "None" => "0",
            _ => "0",
        };
        let t0 = self.next_register();
        self.lines.push(format!(
            "  {} = insertvalue {} {}, i1 {}, 0",
            t0, ty, cur, tag
        ));
        cur = t0;

        match ctor {
            "Ok" => {
                let payload = args.get(0).map(|e| self.emit_expr(e)).unwrap_or_else(|| "0".to_string());
                let ok_ty = match infer {
                    InferType::App(name, inner) if name == "Result" && inner.len() == 2 => self.lower_infer_type(&inner[0]),
                    _ => "i32".to_string(),
                };
                let t1 = self.next_register();
                self.lines.push(format!(
                    "  {} = insertvalue {} {}, {} {}, 1",
                    t1, ty, cur, ok_ty, payload
                ));
                cur = t1;
            }
            "Err" => {
                let payload = args.get(0).map(|e| self.emit_expr(e)).unwrap_or_else(|| "0".to_string());
                let err_ty = match infer {
                    InferType::App(name, inner) if name == "Result" && inner.len() == 2 => self.lower_infer_type(&inner[1]),
                    _ => "i32".to_string(),
                };
                let t1 = self.next_register();
                self.lines.push(format!(
                    "  {} = insertvalue {} {}, {} {}, 2",
                    t1, ty, cur, err_ty, payload
                ));
                cur = t1;
            }
            "Some" => {
                let payload = args.get(0).map(|e| self.emit_expr(e)).unwrap_or_else(|| "0".to_string());
                let inner_ty = match infer {
                    InferType::App(name, inner) if name == "Option" && inner.len() == 1 => self.lower_infer_type(&inner[0]),
                    _ => "i32".to_string(),
                };
                let t1 = self.next_register();
                self.lines.push(format!(
                    "  {} = insertvalue {} {}, {} {}, 1",
                    t1, ty, cur, inner_ty, payload
                ));
                cur = t1;
            }
            "None" => {}
            _ => {}
        }

        cur
    }

    fn scan_decl_for_adts(&mut self, decl: &Declaration) {
        match &decl.node {
            DeclarationKind::Function { params, return_type, .. } => {
                for p in params {
                    self.ensure_adt_for_type(&p.type_annotation);
                }
                if let Some(ret) = return_type {
                    self.ensure_adt_for_type(ret);
                }
            }
            DeclarationKind::Struct { fields, .. } => {
                for (_id, ty) in fields {
                    self.ensure_adt_for_type(ty);
                }
            }
            DeclarationKind::Newtype { type_alias, .. } => self.ensure_adt_for_type(type_alias),
            _ => {}
        }
    }

    fn ensure_adt_for_type(&mut self, ty: &Type) {
        for arg in &ty.node.generic_args {
            self.ensure_adt_for_type(arg);
        }
        match ty.node.name.as_str() {
            "Option" => {
                if let Some(arg0) = ty.node.generic_args.get(0) {
                    let inner = self.lower_type(arg0);
                    self.ensure_option(&inner);
                }
            }
            "Result" => {
                let a = ty.node.generic_args.get(0).map(|t| self.lower_type(t));
                let b = ty.node.generic_args.get(1).map(|t| self.lower_type(t));
                if let (Some(a), Some(b)) = (a, b) {
                    self.ensure_result(&a, &b);
                }
            }
            _ => {}
        }
    }

    fn ensure_adt_for_infertype(&mut self, ty: &InferType) {
        match ty {
            InferType::App(name, args) if name == "Option" && args.len() == 1 => {
                let inner = self.lower_infer_type(&args[0]);
                self.ensure_option(&inner);
            }
            InferType::App(name, args) if name == "Result" && args.len() == 2 => {
                let ok = self.lower_infer_type(&args[0]);
                let err = self.lower_infer_type(&args[1]);
                self.ensure_result(&ok, &err);
            }
            InferType::App(_, args) => {
                for a in args {
                    self.ensure_adt_for_infertype(a);
                }
            }
            InferType::Fn(args, ret) => {
                for a in args {
                    self.ensure_adt_for_infertype(a);
                }
                self.ensure_adt_for_infertype(ret);
            }
            _ => {}
        }
    }

    fn lower_infer_type(&mut self, ty: &InferType) -> String {
        match ty {
            InferType::Con(name) => match name.as_str() {
                "Unit" => "void".to_string(),
                "Int8" => "i8".to_string(),
                "Int16" => "i16".to_string(),
                "Int32" => "i32".to_string(),
                "Int64" => "i64".to_string(),
                "Bool" => "i1".to_string(),
                "Str" => "i8*".to_string(),
                "Buf" => "%struct.Buf*".to_string(),
                _ => format!("%struct.{}", name),
            },
            InferType::App(name, args) if name == "Option" && args.len() == 1 => {
                let inner = self.lower_infer_type(&args[0]);
                self.ensure_option(&inner);
                format!("%struct.Option__{}", mangle_llvm_type_name(&inner))
            }
            InferType::App(name, args) if name == "Result" && args.len() == 2 => {
                let ok = self.lower_infer_type(&args[0]);
                let err = self.lower_infer_type(&args[1]);
                self.ensure_result(&ok, &err);
                format!(
                    "%struct.Result__{}__{}",
                    mangle_llvm_type_name(&ok),
                    mangle_llvm_type_name(&err)
                )
            }
            InferType::App(name, args) if name == "Array" && args.len() == 1 => {
                let _ = &args[0];
                "%struct.TyArray*".to_string()
            }
            InferType::FixedArray(elem, n) => {
                let inner = self.lower_infer_type(elem);
                format!("[{} x {}]", n, inner)
            }
            _ => "i32".to_string(),
        }
    }

    fn ensure_option(&mut self, inner: &str) {
        let name = format!("%struct.Option__{}", mangle_llvm_type_name(inner));
        if self.adt_structs.contains_key(&name) {
            return;
        }
        let def = format!("{} = type {{ i1, {} }}", name, inner);
        self.type_decls.push(def);
        self.adt_structs.insert(name, "option".to_string());
    }

    fn ensure_result(&mut self, ok: &str, err: &str) {
        let name = format!(
            "%struct.Result__{}__{}",
            mangle_llvm_type_name(ok),
            mangle_llvm_type_name(err)
        );
        if self.adt_structs.contains_key(&name) {
            return;
        }
        let def = format!("{} = type {{ i1, {}, {} }}", name, ok, err);
        self.type_decls.push(def);
        self.adt_structs.insert(name, "result".to_string());
    }

    fn emit_string(&mut self, s: &str) -> String {
        let (global, n) = if let Some(v) = self.string_pool.get(s).cloned() {
            v
        } else {
            let id = self.string_pool.len();
            let global = format!("@.str.{}", id);
            let bytes = s.as_bytes();
            let n = bytes.len() + 1;
            let escaped = llvm_escape(bytes);
            let decl = format!(
                "{} = private unnamed_addr constant [{} x i8] c\"{}\\00\"",
                global, n, escaped
            );
            self.extra_preamble.push(decl);
            let pair = (global.clone(), n);
            self.string_pool.insert(s.to_string(), pair.clone());
            pair
        };
        let tmp = self.next_register();
        self.lines.push(format!(
            "  {} = getelementptr inbounds ([{} x i8], [{} x i8]* {}, i32 0, i32 0)",
            tmp, n, n, global
        ));
        tmp
    }

    fn next_block(&mut self, prefix: &str) -> String {
        let label = format!("{}_{}", prefix, self.next_label);
        self.next_label += 1;
        label
    }
}

fn enum_variant_tag(variant_name: &str) -> Option<&'static str> {
    match variant_name {
        "Ok" | "Some" => Some("1"),
        "Err" | "None" => Some("0"),
        _ => None,
    }
}

fn enum_variant_payload_index(variant_name: &str) -> Option<usize> {
    match variant_name {
        "Ok" | "Some" => Some(1),
        "Err" => Some(2),
        _ => None,
    }
}

fn enum_variant_payload_type(
    variant_name: &str,
    scrutinee_ty: &str,
    payload_index: usize,
) -> Option<String> {
    if scrutinee_ty.starts_with("%struct.Option__") {
        return (variant_name == "Some" && payload_index == 1).then_some("i32".to_string());
    }
    if scrutinee_ty.starts_with("%struct.Result__") {
        return match payload_index {
            1 | 2 => Some("i32".to_string()),
            _ => None,
        };
    }
    None
}

fn runtime_intrinsic_name(name: &str) -> Option<String> {
    match name {
        "__ty_buf_new" => Some("ty_buf_new".to_string()),
        "__ty_buf_push_str" => Some("ty_buf_push_str".to_string()),
        "__ty_buf_into_str" => Some("ty_buf_into_str".to_string()),
        _ => None,
    }
}

fn link_symbol_name(name: &str) -> String {
    if name == "main__main" {
        "main".to_string()
    } else {
        name.to_string()
    }
}

fn llvm_escape(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\5C"),
            b'"' => out.push_str("\\22"),
            b'\n' => out.push_str("\\0A"),
            b'\r' => out.push_str("\\0D"),
            b'\t' => out.push_str("\\09"),
            0x20..=0x7E => out.push(b as char),
            _ => out.push_str(&format!("\\{:02X}", b)),
        }
    }
    out
}

fn mangle_llvm_type_name(llvm_ty: &str) -> String {
    llvm_ty
        .replace('%', "")
        .replace('*', "ptr")
        .replace(' ', "")
        .replace('.', "_")
        .replace('[', "arr")
        .replace(']', "")
        .replace('{', "")
        .replace('}', "")
        .replace(',', "_")
        .replace('<', "")
        .replace('>', "")
}

impl IrModule {
    pub fn to_llvm_ir(&self) -> String {
        let mut out = Vec::new();
        out.extend(self.preamble.iter().cloned());
        for func in &self.functions {
            let params = if func.params.is_empty() {
                "".to_string()
            } else {
                func.params
                    .iter()
                    .map(|(name, ty)| format!("{} %{}", ty, name))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            out.push(format!(
                "define {} @{}({}) {{",
                func.ret_type, func.name, params
            ));
            out.push(func.body.clone());
            out.push("}".to_string());
        }
        out.join("\n")
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

    fn parse_module(source: &str) -> Module {
        Parser::new(Lexer::new(normalize_source(source)).tokenize())
            .parse_module()
            .unwrap()
    }

    #[test]
    fn lowers_function_declarations() {
        let source = "fn main(a: Int32) -> Int32 { return a; }";
        let module = parse_module(source);
        let mut checker = crate::type_inference::TypeChecker::new();
        checker.check_module(&module).unwrap();
        let ir = Codegen::lower_module(&module, checker.types());
        assert_eq!(ir.functions.len(), 1);
        assert_eq!(ir.functions[0].name, "main");
        assert_eq!(ir.functions[0].ret_type, "i32");
        assert_eq!(ir.functions[0].params.len(), 1);
    }

    #[test]
    fn emits_basic_llvm_ir() {
        let source = "fn main() -> Int32 { return 0; }";
        let module = parse_module(source);
        let mut checker = crate::type_inference::TypeChecker::new();
        checker.check_module(&module).unwrap();
        let ir = Codegen::lower_module(&module, checker.types());
        let text = ir.to_llvm_ir();
        assert!(text.contains("define i32 @main()"));
        assert!(text.contains("ret i32 0"));
    }

    #[test]
    fn lowers_let_bindings() {
        let source = "fn main() -> Int32 { let x: Int32 = 3; return x; }";
        let module = parse_module(source);
        let mut checker = crate::type_inference::TypeChecker::new();
        checker.check_module(&module).unwrap();
        let ir = Codegen::lower_module(&module, checker.types());
        let text = ir.to_llvm_ir();
        assert!(text.contains("alloca i32"));
        assert!(text.contains("store i32 3"));
        assert!(text.contains("load i32"));
    }

    #[test]
    fn emits_if_branches() {
        let source = "fn main(flag: Bool) -> Int32 { if flag { return 1; } else { return 2; } }";
        let module = parse_module(source);
        let mut checker = crate::type_inference::TypeChecker::new();
        checker.check_module(&module).unwrap();
        let ir = Codegen::lower_module(&module, checker.types());
        let text = ir.to_llvm_ir();
        assert!(text.contains("br i1"));
        assert!(text.contains("if_merge"));
    }

    #[test]
    fn lowers_struct_init_and_merge() {
        let source = "struct User { id: Int32, age: Int32 } fn main() -> Int32 { let user: User = User { id: 1, age: 2 }; let updated: User = { ...user, age: 3 }; return 0; }";
        let module = parse_module(source);
        let mut checker = crate::type_inference::TypeChecker::new();
        checker.check_module(&module).unwrap();
        let ir = Codegen::lower_module(&module, checker.types());
        let text = ir.to_llvm_ir();
        assert!(text.contains("%struct.User = type"));
        assert!(text.contains("insertvalue %struct.User"));
    }

    #[test]
    fn heap_allocates_mutable_struct_lets() {
        let source =
            "struct Point { x: Int32, y: Int32 } fn main() -> Int32 { let mut p: Point = Point { x: 1, y: 2 }; return 0; }";
        let module = parse_module(source);
        let mut checker = crate::type_inference::TypeChecker::new();
        checker.check_module(&module).unwrap();
        let ir = Codegen::lower_module(&module, checker.types());
        let text = ir.to_llvm_ir();
        assert!(text.contains("call i8* @ty_alloc"));
        assert!(text.contains("bitcast i8*"));
        assert!(text.contains("%struct.Point*"));
    }

    #[test]
    fn widens_mutable_array_literals_to_tyarray() {
        let source = "fn main() -> Int32 { let mut xs: Array<Int32> = [1,2,3]; return 0; }";
        let module = parse_module(source);
        let mut checker = crate::type_inference::TypeChecker::new();
        checker.check_module(&module).unwrap();
        let ir = Codegen::lower_module(&module, checker.types());
        let text = ir.to_llvm_ir();
        assert!(text.contains("%struct.TyArray = type"));
        assert!(text.contains("@ty_array_from_fixed"));
    }

    #[test]
    fn lowers_struct_method_calls_as_function_calls() {
        let source = "struct User { id: Int32 } fn __ty_method__User__get_id(self: User) -> Int32 { return self.id; } fn main() -> Int32 { let u: User = User { id: 1 }; return u.get_id(); }";
        let module = parse_module(source);
        let mut checker = crate::type_inference::TypeChecker::new();
        checker.check_module(&module).unwrap();
        let ir = Codegen::lower_module(&module, checker.types());
        let text = ir.to_llvm_ir();
        assert!(text.contains("call i32 @__ty_method__User__get_id"));
    }

    #[test]
    fn lowers_array_push_and_index_to_runtime_calls() {
        let source = "fn main() -> Int32 { let mut xs: Array<Int32> = [1,2]; xs.push(3); let v: Option<Int32> = xs[0]; return 0; }";
        let module = parse_module(source);
        let mut checker = crate::type_inference::TypeChecker::new();
        checker.check_module(&module).unwrap();
        let ir = Codegen::lower_module(&module, checker.types());
        let text = ir.to_llvm_ir();
        assert!(text.contains("@ty_array_push"));
        assert!(text.contains("@ty_array_get_ptr"));
    }

    #[test]
    fn lowers_result_constructors_to_aggregate_values() {
        let source = "fn main() -> Result<Int32, Str> { return Ok(1); }";
        let module = parse_module(source);
        let mut checker = crate::type_inference::TypeChecker::new();
        checker.check_module(&module).unwrap();
        let ir = Codegen::lower_module(&module, checker.types());
        let text = ir.to_llvm_ir();
        assert!(text.contains("%struct.Result__"));
        assert!(text.contains("insertvalue %struct.Result__"));
    }

    #[test]
    fn lowers_match_to_control_flow() {
        let source = "fn main(x: Int32) -> Int32 { match x { 0 => 1, _ => 2, } }";
        let module = parse_module(source);
        let mut checker = crate::type_inference::TypeChecker::new();
        checker.check_module(&module).unwrap();
        let ir = Codegen::lower_module(&module, checker.types());
        let text = ir.to_llvm_ir();
        assert!(text.contains("br label %match_check"));
        assert!(text.contains("icmp eq i32"));
        assert!(text.contains("match_merge"));
    }

    #[test]
    fn lowers_if_let_to_control_flow() {
        let source = "fn main(x: Result<Int32, Str>) -> Int32 { if let Ok(v) = x { return v; } else { return 0; } }";
        let module = parse_module(source);
        let mut checker = crate::type_inference::TypeChecker::new();
        checker.check_module(&module).unwrap();
        let ir = Codegen::lower_module(&module, checker.types());
        let text = ir.to_llvm_ir();
        assert!(text.contains("iflet_then"));
        assert!(text.contains("extractvalue"));
        assert!(text.contains("iflet_merge"));
    }
}
