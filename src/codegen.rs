use crate::ast::*;
use crate::liveness::DropInfo;
use crate::span::Span;
use crate::type_inference::InferType;
use std::collections::HashMap;

// ── Public types ──────────────────────────────────────────────────────────────

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

impl IrModule {
    pub fn to_llvm_ir(&self) -> String {
        let mut out = Vec::new();
        out.extend(self.preamble.iter().cloned());
        for func in &self.functions {
            let params = func
                .params
                .iter()
                .map(|(name, ty)| format!("{} %{}", ty, name))
                .collect::<Vec<_>>()
                .join(", ");
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

// ── Entry point ───────────────────────────────────────────────────────────────

pub struct Codegen;

impl Codegen {
    pub fn lower_module(
        module: &Module,
        types: &HashMap<NodeId, InferType>,
        drop_map: &HashMap<NodeId, Vec<DropInfo>>,
    ) -> IrModule {
        let mut b = IrBuilder::new(drop_map);
        b.types = Some(types as *const _);
        b.collect_types(module);
        let mut all_functions: Vec<IrFunction> = module
            .declarations
            .iter()
            .filter_map(|decl| {
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
                        .map(|ty| b.lower_type(ty))
                        .unwrap_or_else(|| "void".to_string());
                    let body_ir = b.emit_function(name, params, &ret_ty, body);
                    let mut param_list: Vec<(String, String)> = params
                        .iter()
                        .map(|p| (p.name.name.clone(), b.lower_type(&p.type_annotation)))
                        .collect();
                    if !is_main(&name.name) {
                        param_list.insert(0, ("task".to_string(), "i8*".to_string()));
                    }
                    Some(IrFunction {
                        name: link_symbol_name(&name.name),
                        body: body_ir,
                        ret_type: ret_ty,
                        params: param_list,
                    })
                } else {
                    None
                }
            })
            .collect();
        all_functions.extend(b.conc_functions.drain(..));
        IrModule {
            functions: all_functions,
            preamble: b.preamble(),
        }
    }
}

// ── IR builder ────────────────────────────────────────────────────────────────

struct IrBuilder<'a> {
    lines: Vec<String>,
    next_tmp: usize,
    next_label: usize,
    current_fn_name: Option<String>,
    current_fn_ret_ty: String,
    locals: HashMap<String, String>,
    locals_type: HashMap<String, String>,
    parent_locals: HashMap<String, String>,  // captured variables from parent scope
    parent_types: HashMap<String, String>,   // types of captured variables
    type_decls: Vec<String>,
    struct_fields: HashMap<String, Vec<(String, String)>>,
    func_sigs: HashMap<String, (String, Vec<String>)>,
    string_pool: HashMap<String, (String, usize)>,
    extra_preamble: Vec<String>,
    adt_structs: HashMap<String, String>,
    types: Option<*const HashMap<NodeId, InferType>>,
    drop_map: &'a HashMap<NodeId, Vec<DropInfo>>,
    conc_functions: Vec<IrFunction>,
}

impl<'a> IrBuilder<'a> {
    fn new(drop_map: &'a HashMap<NodeId, Vec<DropInfo>>) -> Self {
        Self {
            lines: Vec::new(),
            next_tmp: 0,
            next_label: 0,
            current_fn_name: None,
            current_fn_ret_ty: "void".to_string(),
            locals: HashMap::new(),
            locals_type: HashMap::new(),
            parent_locals: HashMap::new(),
            parent_types: HashMap::new(),
            type_decls: Vec::new(),
            struct_fields: HashMap::new(),
            func_sigs: HashMap::new(),
            string_pool: HashMap::new(),
            extra_preamble: Vec::new(),
            adt_structs: HashMap::new(),
            types: None,
            drop_map: drop_map,
            conc_functions: Vec::new(),
        }
    }

    fn preamble(&self) -> Vec<String> {
        let mut p = self.type_decls.clone();
        p.extend(self.extra_preamble.iter().cloned());
        p
    }

    // ── Low-level emitters ────────────────────────────────────────────────────

    fn emit(&mut self, line: String) {
        self.lines.push(line);
    }

    fn tmp(&mut self) -> String {
        let n = self.next_tmp;
        self.next_tmp += 1;
        format!("%t{}", n)
    }

    fn label(&mut self, prefix: &str) -> String {
        let n = self.next_label;
        self.next_label += 1;
        format!("{}_{}", prefix, n)
    }

    fn annotate_span(&mut self, span: &Span) {
        if *span != Span::default() {
            self.emit(format!(
                "  ; span {}..{} @ {}:{}",
                span.start, span.end, span.line, span.col
            ));
        }
    }

    // ── Type collection ───────────────────────────────────────────────────────

    fn collect_types(&mut self, module: &Module) {
        self.type_decls.clear();
        self.struct_fields.clear();
        self.func_sigs.clear();
        self.string_pool.clear();
        self.extra_preamble.clear();
        self.adt_structs.clear();

        for decl in [
            "%struct.Buf = type { i8*, i64, i64 }",
            "%struct.TyArray = type { i8*, i64, i64, i64, i64 }",
        ] {
            self.type_decls.push(decl.to_string());
        }

        for decl in [
            // ── scheduler ──
            "declare void @ty_sched_init()",
            "declare void @ty_sched_shutdown()",
            "declare i8* @ty_spawn(i8*, i8*, i8*)", // task, fn_ptr, arg
            "declare void @ty_yield()",
            "declare void @ty_await(i8*, i8*)", // task, coro_handle
            "declare i8* @ty_chan_new(i64, i64)", // elem_size, cap
            "declare void @ty_chan_send(i8*, i8*, i8*)", // task, chan, elem_ptr
            "declare void @ty_chan_recv(i8*, i8*, i8*)", // task, chan, out_ptr
            "declare void @ty_chan_close(i8*)", // chan
            // ── Buf (all now take task first) ──
            "declare %struct.Buf* @ty_buf_new(i8* %task)",
            "declare void @ty_buf_push_str(i8*, %struct.Buf*, i8*)",
            "declare i8* @ty_buf_into_str(i8*, %struct.Buf*)",
            // ── TyArray (all now take task first) ──
            "declare %struct.TyArray* @ty_array_from_fixed(i8*, i8*, i64, i64, i64)",
            "declare void @ty_array_push(i8*, %struct.TyArray*, i8*)",
            "declare i8* @ty_array_get_ptr(%struct.TyArray*, i64)",
            // ── arena / slab ──
            "declare i8* @slab_arena_new()",
            "declare i8* @slab_alloc(i8* %task, i32 %size_class)",
            "declare void @slab_free(i8* %task, i8* %ptr, i32 %size_class)",
            "declare void @slab_arena_free(i8*)",
        ] {
            self.extra_preamble.push(decl.to_string());
        }

        self.func_sigs
            .insert("__ty_buf_new".into(), ("%struct.Buf*".into(), vec![]));
        self.func_sigs.insert(
            "__ty_buf_push_str".into(),
            ("void".into(), vec!["%struct.Buf*".into(), "i8*".into()]),
        );
        self.func_sigs.insert(
            "__ty_buf_into_str".into(),
            ("i8*".into(), vec!["%struct.Buf*".into()]),
        );
        self.func_sigs.insert(
            "ty_array_push".into(),
            ("void".into(), vec!["%struct.TyArray*".into(), "i8*".into()]),
        );
        // ty_array_get_ptr has no task — handled in no_task_intrinsics()
        self.func_sigs.insert(
            "ty_spawn".into(),
            ("i8*".into(), vec!["i8*".into(), "i8*".into()]),
        ); // fn_ptr, arg
        self.func_sigs
            .insert("ty_await".into(), ("void".into(), vec!["i8*".into()])); // coro_handle
        self.func_sigs
            .insert("ty_yield".into(), ("void".into(), vec![]));
        self.func_sigs.insert(
            "ty_chan_new".into(),
            ("i8*".into(), vec!["i64".into(), "i64".into()]),
        );
        self.func_sigs.insert(
            "ty_chan_send".into(),
            ("void".into(), vec!["i8*".into(), "i8*".into()]),
        ); // chan, elem_ptr
        self.func_sigs.insert(
            "ty_chan_recv".into(),
            ("void".into(), vec!["i8*".into(), "i8*".into()]),
        ); // chan, out_ptr
        self.func_sigs
            .insert("ty_chan_close".into(), ("void".into(), vec!["i8*".into()]));

        for decl in &module.declarations {
            match &decl.node {
                DeclarationKind::Struct { name, fields, .. } => {
                    let mut field_types = Vec::new();
                    let mut field_map = Vec::new();
                    for (id, ty) in fields {
                        let lt = self.lower_type(ty);
                        field_types.push(lt.clone());
                        field_map.push((id.name.clone(), lt));
                    }
                    self.type_decls.push(format!(
                        "%struct.{} = type {{ {} }}",
                        name.name,
                        field_types.join(", ")
                    ));
                    self.struct_fields.insert(name.name.clone(), field_map);
                }
                DeclarationKind::Enum { name, .. } => {
                    self.type_decls
                        .push(format!("%enum.{} = type opaque", name.name));
                }
                DeclarationKind::Newtype { name, type_alias } => {
                    self.type_decls.push(format!(
                        "%newtype.{} = type {}",
                        name.name,
                        self.lower_type(type_alias)
                    ));
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
                    let mut param_types: Vec<String> = params
                        .iter()
                        .map(|p| self.lower_type(&p.type_annotation))
                        .collect();
                    if !is_main(&name.name) {
                        param_types.insert(0, "i8*".to_string());
                    }
                    self.func_sigs
                        .insert(name.name.clone(), (ret_ty, param_types));
                }
                _ => {}
            }
        }

        for decl in &module.declarations {
            self.scan_decl_for_adts(decl);
        }
        if let Some(ptr) = self.types {
            let types = unsafe { &*ptr };
            for ty in types.values() {
                self.ensure_adt_for_infertype(ty);
            }
        }
    }

    // ── Function emission ─────────────────────────────────────────────────────

    fn emit_function(
        &mut self,
        name: &Identifier,
        params: &[Parameter],
        ret_ty: &str,
        body: &Block,
    ) -> String {
        self.lines.clear();
        self.locals.clear();
        self.locals_type.clear();
        self.current_fn_ret_ty = ret_ty.to_string();
        self.current_fn_name = Some(name.name.clone());
        self.emit("entry:".to_string());
        if is_main(&name.name) {
            self.emit("  %t0 = alloca i8*".to_string());
            self.emit("  %task_init = call i8* @slab_arena_new()".to_string());
            self.emit("  store i8* %task_init, i8** %t0".to_string());
            self.emit("  call void @ty_sched_init()".to_string());
            self.emit("  %task = load i8*, i8** %t0".to_string());
        } else {
            self.emit_function_param("task".to_string(), "i8*".to_string());
        }
        for param in params {
            let ty = self.lower_type(&param.type_annotation);
            self.emit_function_param(param.name.name.clone(), ty);
        }

        let terminated = self.emit_block_stmts(body, ret_ty);

        if !terminated {
            if let Some(expr) = &body.trailing_expression {
                let val = self.emit_expr(expr);
                let ty = self.expr_llvm_type(expr);
                self.emit(format!("  ret {} {}", ty, val));
            } else {
                let has_ret = self
                    .lines
                    .iter()
                    .any(|l| l.trim_start().starts_with("ret "));
                if !has_ret {
                    if ret_ty == "void" {
                        self.emit("  ret void".to_string());
                    } else {
                        self.emit(format!("  ret {} 0", ret_ty));
                    }
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
                self.emit(format!("  ret {} 0", ret_ty));
            }
        }

        self.lines.join("\n")
    }

    fn emit_function_param(&mut self, name: String, lower_type: String) {
        let slot = self.tmp();
        self.emit(format!("  {} = alloca {}", slot, lower_type));
        self.emit(format!(
            "  store {} %{}, {}* {}",
            lower_type, name, lower_type, slot
        ));
        self.locals.insert(name.clone(), slot);
        self.locals_type.insert(name.clone(), lower_type);
    }

    // ── Statement emission ────────────────────────────────────────────────────

    /// Emit all statements in `block`. Returns true if a terminator was emitted.
    fn emit_block_stmts(&mut self, block: &Block, ret_ty: &str) -> bool {
        self.annotate_span(&block.span);
        for stmt in &block.statements {
            if self.emit_stmt(stmt, ret_ty) {
                return true;
            }
        }
        // Before exiting the block, emit slab_free for everything dying here.
        // block.id comes from the Spanned wrapper, mirroring how liveness keys drops.
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
    fn emit_stmt(&mut self, stmt: &Statement, ret_ty: &str) -> bool {
        match &stmt.node {
            StatementKind::Return(Some(expr)) => {
                let val = self.emit_expr(expr);
                let ty = self.expr_llvm_type(expr);
                if self.current_fn_name.as_deref().map_or(false, is_main) {
                    self.emit("  call void @ty_sched_shutdown()".to_string());
                }
                self.emit(format!("  ret {} {}", ty, val));
                true
            }
            StatementKind::Return(None) => {
                if self.current_fn_name.as_deref().map_or(false, is_main) {
                    self.emit("  call void @ty_sched_shutdown()".to_string());
                }
                self.emit("  ret void".to_string());
                true
            }
            StatementKind::LetBinding {
                name,
                initializer,
                type_annotation,
                mutable,
                ..
            } => {
                self.emit_let(name, initializer, type_annotation.as_ref(), *mutable);
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
                // Emit a trampoline function that can access captured variables from parent scope.
                let tramp_name = format!(
                    "__ty_conc_{}",
                    self.label("tramp")
                );
                
                let saved_lines = std::mem::take(&mut self.lines);
                let saved_fn_name = self.current_fn_name.clone();
                let saved_fn_ret_ty = self.current_fn_ret_ty.clone();
                let saved_locals = std::mem::take(&mut self.locals);
                let saved_types = std::mem::take(&mut self.locals_type);
                let saved_parent_locals = std::mem::take(&mut self.parent_locals);
                let saved_parent_types = std::mem::take(&mut self.parent_types);

                // Capture parent scope for the trampoline
                self.parent_locals = saved_locals.clone();
                self.parent_types = saved_types.clone();

                self.lines.clear();
                self.locals.clear();
                self.locals_type.clear();
                self.current_fn_ret_ty = "void".to_string();
                self.current_fn_name = Some(tramp_name.clone());
                self.emit("entry:".to_string());
                self.emit_function_param("task".to_string(), "i8*".to_string());
                self.emit_function_param("arg".to_string(), "i8*".to_string());
                self.emit_block_stmts(body, "void");
                self.emit("  ret void".to_string());

                let tramp_ir = IrFunction {
                    name: tramp_name.clone(),
                    body: self.lines.join("\n"),
                    ret_type: "void".to_string(),
                    params: vec![
                        ("task".to_string(), "i8*".to_string()),
                        ("arg".to_string(), "i8*".to_string()),
                    ],
                };
                
                // Restore
                self.lines = saved_lines;
                self.locals = saved_locals;
                self.locals_type = saved_types;
                self.current_fn_name = saved_fn_name;
                self.current_fn_ret_ty = saved_fn_ret_ty;
                self.parent_locals = saved_parent_locals;
                self.parent_types = saved_parent_types;

                // 2. Emit the spawn call at the current site
                let fn_ptr = self.tmp();
                self.emit(format!(
                    "  {} = bitcast void (i8*, i8*)* @{} to i8*",
                    fn_ptr, tramp_name
                ));
                let null_arg = self.tmp();
                self.emit(format!("  {} = bitcast i8* null to i8*", null_arg));
                self.emit(format!(
                    "  call i8* @ty_spawn(i8* %task, i8* {}, i8* {})",
                    fn_ptr, null_arg
                ));

                self.conc_functions.push(tramp_ir);
                false
            }
            _ => false,
        }
    }

    fn emit_block_end(&mut self, block_id: NodeId) {
        if let Some(drops) = self.drop_map.get(&block_id).cloned() {
            for drop in &drops {
                if drop.is_heap {
                    self.emit_slab_free(&drop.name);
                }
            }
        }
    }

    /// Emit a `slab_free` call for the named local, if it was slab-allocated.
    /// Looks up the typed pointer in `locals` and the LLVM type in `locals_type`
    /// to reconstruct the size class.
    fn emit_slab_free(&mut self, name: &str) {
        let typed_ptr = match self.locals.get(name).cloned() {
            Some(p) => p,
            None => return, // not a local we know about
        };
        let ty = self
            .locals_type
            .get(name)
            .cloned()
            .unwrap_or_else(|| "i32".to_string());

        // Only free if this was heap-allocated (pointer to struct / not a plain alloca slot)
        // Convention: slab-allocated locals store the typed pointer directly (ends with '*')
        // while stack allocas store the slot address. We use the same heuristic as emit_let.
        if !ty.ends_with('*') {
            return;
        }

        let size = self.llvm_const_sizeof(&ty);
        let class_id = get_size_class(size);

        let raw = self.tmp();
        self.emit(format!(
            "  {} = bitcast {}* {} to i8*",
            raw,
            ty.trim_end_matches('*'),
            typed_ptr
        ));
        self.emit(format!(
            "  call void @slab_free(i8* %task, i8* {}, i32 {})",
            raw, class_id
        ));
    }

    fn emit_let(
        &mut self,
        name: &Identifier,
        initializer: &Expression,
        type_annotation: Option<&Type>,
        mutable: bool,
    ) {
        // Array literal: build fixed or growable array
        if let ExpressionKind::Literal(Literal {
            kind: LiteralKind::Array(elems),
            ..
        }) = &initializer.node
        {
            let wants_growable =
                mutable || type_annotation.map_or(false, |ty| ty.node.name == "Array");
            let elem_ty = self.infer_elem_ty(elems);
            let array_ty = format!("[{} x {}]", elems.len(), elem_ty);
            let alloca = self.tmp();
            self.emit(format!("  {} = alloca {}", alloca, array_ty));
            for (i, elem) in elems.iter().enumerate() {
                let val = self.emit_expr(elem);
                let gep = self.tmp();
                self.emit(format!(
                    "  {} = getelementptr inbounds {}, {}* {}, i32 0, i32 {}",
                    gep, array_ty, array_ty, alloca, i
                ));
                self.emit(format!("  store {} {}, {}* {}", elem_ty, val, elem_ty, gep));
            }
            if wants_growable {
                // 1. Bitcast the raw stack array to i8* for the runtime call
                let raw = self.tmp();
                self.emit(format!(
                    "  {} = bitcast {}* {} to i8*",
                    raw, array_ty, alloca
                ));

                // 2. Call the runtime to create a proper TyArray object
                let sz = self.llvm_const_sizeof(&elem_ty);
                let al = self.llvm_const_alignof(&elem_ty);
                let out = self.tmp();
                self.emit(format!(
                    "  {} = call %struct.TyArray* @ty_array_from_fixed(i8* %task, i8* {}, i64 {}, i64 {}, i64 {})",
                    out, raw, elems.len(), sz, al
                ));

                let slot = self.tmp();
                self.emit(format!("  {} = alloca %struct.TyArray*", slot));
                self.emit(format!(
                    "  store %struct.TyArray* {}, %struct.TyArray** {}",
                    out, slot
                ));
                self.locals.insert(name.name.clone(), slot);
                self.locals_type
                    .insert(name.name.clone(), "%struct.TyArray*".into());
            } else {
                self.locals.insert(name.name.clone(), alloca.clone());
                self.locals_type.insert(name.name.clone(), array_ty);
            }
            return;
        }

        // General case
        let value = self.emit_expr(initializer);
        let ty = type_annotation
            .map(|t| self.lower_type(t))
            .unwrap_or_else(|| self.expr_llvm_type(initializer));

        let is_heap_allocated = mutable && !ty.ends_with('*') && ty != "void";

        if is_heap_allocated {
            // Implement Slab Allocation Logic
            let size = self.llvm_const_sizeof(&ty);
            let class_id = get_size_class(size);

            let raw_ptr = self.tmp();
            // %task is passed as a hidden first argument to the function
            self.emit(format!(
                "  {} = call i8* @slab_alloc(i8* %task, i32 {})",
                raw_ptr, class_id
            ));

            let typed_ptr = self.tmp();
            self.emit(format!(
                "  {} = bitcast i8* {} to {}*",
                typed_ptr, raw_ptr, ty
            ));

            self.emit(format!("  store {} {}, {}* {}", ty, value, ty, typed_ptr));
            self.locals.insert(name.name.clone(), typed_ptr);
            self.locals_type.insert(name.name.clone(), ty);
        } else {
            // Default stack allocation (alloca)
            let slot = self.tmp();
            self.emit(format!("  {} = alloca {}", slot, ty));
            self.emit(format!("  store {} {}, {}* {}", ty, value, ty, slot));
            self.locals.insert(name.name.clone(), slot);
            self.locals_type.insert(name.name.clone(), ty);
        }
    }

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
                ElseBranchKind::Block(block) => {
                    let t = self.emit_block_stmts(block, ret_ty);
                    if !t {
                        self.emit(format!("  br label %{}", merge_lbl));
                    }
                    t
                }
                ElseBranchKind::If(stmt) => self.emit_stmt(stmt, ret_ty),
            },
        };

        self.emit(format!("{}:", merge_lbl));
        if then_term && else_term {
            self.emit("  unreachable".to_string());
            return true;
        }
        false
    }

    fn emit_loop(&mut self, kind: &Spanned<LoopKindKind>, body: &Block, ret_ty: &str) {
        match &kind.node {
            LoopKindKind::While { condition, .. } => {
                let start = self.label("while_start");
                let body_lbl = self.label("while_body");
                let end = self.label("while_end");
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
                self.emit(format!("  {} = alloca i64", idx_slot));
                self.emit(format!("  store i64 0, i64* {}", idx_slot));

                let len_ptr = self.tmp();
                self.emit(format!(
                    "  {} = getelementptr inbounds %struct.TyArray, %struct.TyArray* {}, i32 0, i32 1",
                    len_ptr, iter_val
                ));
                let len = self.tmp();
                self.emit(format!("  {} = load i64, i64* {}", len, len_ptr));

                let start = self.label("for_start");
                let body_lbl = self.label("for_body");
                let end = self.label("for_end");
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
                    self.emit(format!("  {} = alloca {}", pat_slot, elem_ty));
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
            }
            LoopKindKind::Block(b) => {
                let start = self.label("loop_start");
                self.emit(format!("  br label %{}", start));
                self.emit(format!("{}:", start));
                if !self.emit_block_stmts(b, ret_ty) {
                    self.emit(format!("  br label %{}", start));
                }
            }
        }
    }

    // ── Expression emission ───────────────────────────────────────────────────

    fn emit_expr(&mut self, expr: &Expression) -> String {
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
                let elem_ty = self.infer_elem_ty(elems);
                let array_ty = format!("[{} x {}]", elems.len(), elem_ty);
                let alloca = self.tmp();
                self.emit(format!("  {} = alloca {}", alloca, array_ty));
                for (i, elem) in elems.iter().enumerate() {
                    let val = self.emit_expr(elem);
                    let gep = self.tmp();
                    self.emit(format!(
                        "  {} = getelementptr inbounds {}, {}* {}, i32 0, i32 {}",
                        gep, array_ty, array_ty, alloca, i
                    ));
                    self.emit(format!("  store {} {}, {}* {}", elem_ty, val, elem_ty, gep));
                }

                // 1. Bitcast the raw stack array to i8* for the runtime call
                let raw_ptr_i8 = self.tmp();
                self.emit(format!(
                    "  {} = bitcast {}* {} to i8*",
                    raw_ptr_i8, array_ty, alloca
                ));

                // 2. Call the runtime to create a proper TyArray object
                let ty_array_ptr = self.tmp();
                let elem_size = self.llvm_const_sizeof(&elem_ty);
                let align = self.llvm_const_alignof(&elem_ty);
                self.emit(format!(
                    "  {} = call %struct.TyArray* @ty_array_from_fixed(i8* %task, i8* {}, i64 {}, i64 {}, i64 {})",
                    ty_array_ptr, raw_ptr_i8, elems.len(), elem_size, align
                ));

                ty_array_ptr // Return the pointer to the ACTUAL TyArray struct
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
                    tmp
                } else if self.parent_locals.get(&id.name).is_some() {
                    // Variable from parent scope (captured in conc block)
                    // For now, generate a stub that will fail at runtime
                    // A proper implementation would unpack from arg parameter
                    format!("0 ; FIXME: captured var {}", id.name)
                } else {
                    id.name.clone()
                }
            }
            ExpressionKind::Block(block) => {
                let ret_ty = self.current_fn_ret_ty.clone();
                let saved_locals = self.locals.clone();
                let saved_types = self.locals_type.clone();
                self.emit_block_stmts(block, &ret_ty);
                let result = block
                    .trailing_expression
                    .as_ref()
                    .map(|e| self.emit_expr(e))
                    .unwrap_or_else(|| "0".to_string());
                self.locals = saved_locals;
                self.locals_type = saved_types;
                result
            }
            ExpressionKind::BinaryOp { op, left, right } => self.emit_binop(op, left, right),
            ExpressionKind::StructInit { name, fields } => {
                let struct_ty = format!("%struct.{}", name.name);
                let mut cur = "undef".to_string();
                for (field_name, field_expr) in fields {
                    let val = self.emit_expr(field_expr);
                    let (idx, fty) = self.struct_field_info(&name.name, &field_name.name);
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
                    let (idx, fty) = self.struct_field_info(&struct_name, &field_name.name);
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
                let (idx, _) = self.struct_field_info(&struct_name, &field.name);
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
            ExpressionKind::IfLet {
                pattern,
                expr: matched,
                then,
                else_branch,
            } => self.emit_if_let(expr, pattern, matched, then, else_branch.as_deref()),
            ExpressionKind::Placeholder(_) => "0".to_string(),
            _ => "0".to_string(),
        }
    }

    // ── Binary operations ─────────────────────────────────────────────────────

    fn emit_binop(&mut self, op: &Operator, left: &Expression, right: &Expression) -> String {
        // Simple assignment
        if *op == Operator::Assign {
            let (slot, lval_ty) = self.resolve_lvalue(left);
            let rhs_val = self.emit_expr(right);
            self.emit(format!(
                "  store {} {}, {}* {}",
                lval_ty, rhs_val, lval_ty, slot
            ));
            return rhs_val;
        }

        // Compound assignment
        if matches!(
            op,
            Operator::AddAssign | Operator::SubAssign | Operator::MulAssign | Operator::DivAssign
        ) {
            return self.emit_assign_op(op, left, right);
        }

        // Pipe
        if *op == Operator::Pipe {
            return self.emit_pipe(left, right);
        }

        let ty = self.expr_llvm_type(left);
        let lhs = self.emit_expr(left);
        let rhs = self.emit_expr(right);
        let dst = self.tmp();
        let instr = self.arith_instr(op, &ty, &lhs, &rhs, &dst);
        self.emit(instr);
        dst
    }

    /// Build the LLVM instruction string for one arithmetic/comparison op.
    fn arith_instr(&self, op: &Operator, ty: &str, lhs: &str, rhs: &str, dst: &str) -> String {
        let is_float = matches!(ty, "float" | "double" | "half");
        let is_bool = ty == "i1";
        if is_float {
            let opc = match op {
                Operator::Add | Operator::AddAssign => "fadd",
                Operator::Sub | Operator::SubAssign => "fsub",
                Operator::Mul | Operator::MulAssign => "fmul",
                Operator::Div | Operator::DivAssign => "fdiv",
                Operator::Mod => "frem",
                Operator::Eq => return format!("  {} = fcmp oeq {} {}, {}", dst, ty, lhs, rhs),
                Operator::Ne => return format!("  {} = fcmp one {} {}, {}", dst, ty, lhs, rhs),
                Operator::Lt => return format!("  {} = fcmp olt {} {}, {}", dst, ty, lhs, rhs),
                Operator::Gt => return format!("  {} = fcmp ogt {} {}, {}", dst, ty, lhs, rhs),
                Operator::Le => return format!("  {} = fcmp ole {} {}, {}", dst, ty, lhs, rhs),
                Operator::Ge => return format!("  {} = fcmp oge {} {}, {}", dst, ty, lhs, rhs),
                _ => "fadd",
            };
            format!("  {} = {} {} {}, {}", dst, opc, ty, lhs, rhs)
        } else if is_bool {
            match op {
                Operator::And | Operator::BitAnd => {
                    format!("  {} = and i1 {}, {}", dst, lhs, rhs)
                }
                Operator::Or | Operator::BitOr => {
                    format!("  {} = or i1 {}, {}", dst, lhs, rhs)
                }
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
        let lhs_val = self.tmp();
        self.emit(format!(
            "  {} = load {}, {}* {}",
            lhs_val, lval_ty, lval_ty, slot
        ));
        let rhs_val = self.emit_expr(right);
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
                let slot = self
                    .locals
                    .get(&id.name)
                    .cloned()
                    .or_else(|| self.parent_locals.get(&id.name).cloned())
                    .unwrap_or(id.name.clone());
                let ty = self
                    .locals_type
                    .get(&id.name)
                    .cloned()
                    .or_else(|| self.parent_types.get(&id.name).cloned())
                    .unwrap_or_else(|| "i32".to_string());
                (slot, ty)
            }
            ExpressionKind::IndexAccess { base, index } => {
                let (base_ptr, array_ty) = match &base.node {
                    ExpressionKind::Identifier(id) => (
                        self.locals
                            .get(&id.name)
                            .cloned()
                            .or_else(|| self.parent_locals.get(&id.name).cloned())
                            .unwrap_or(id.name.clone()),
                        self.locals_type
                            .get(&id.name)
                            .cloned()
                            .or_else(|| self.parent_types.get(&id.name).cloned())
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
                            .or_else(|| self.parent_locals.get(&id.name).cloned())
                            .unwrap_or(id.name.clone()),
                        self.locals_type
                            .get(&id.name)
                            .cloned()
                            .or_else(|| self.parent_types.get(&id.name).cloned())
                            .unwrap_or_else(|| "%struct.?".to_string()),
                    ),
                    _ => (self.emit_expr(base), "%struct.?".to_string()),
                };
                let struct_name = base_ty.trim_start_matches("%struct.").to_string();
                let (idx, fty) = self.struct_field_info(&struct_name, &field.name);
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
        if let ExpressionKind::Call { func, args } = &right.node {
            if let ExpressionKind::Identifier(id) = &func.node {
                let lhs = self.emit_expr(left);
                let lhs_ty = self.expr_llvm_type(left);
                let (ret_ty, param_types) = self
                    .func_sigs
                    .get(&id.name)
                    .cloned()
                    .unwrap_or_else(|| ("i32".to_string(), vec![]));

                // param_types[0] = i8* (task) for user fns, or first real param
                // for intrinsics. Treat the same as free-function branch:
                // first slot is always task unless no_task intrinsic.
                let mut arg_pairs = Vec::new();
                if !is_no_task_intrinsic(&id.name) {
                    arg_pairs.push("i8* %task".to_string());
                }
                // The piped value is the first *user-visible* argument.
                let first_user_ty = if is_no_task_intrinsic(&id.name) {
                    param_types.get(0)
                } else {
                    param_types.get(1) // skip the task slot
                }
                .cloned()
                .unwrap_or(lhs_ty);
                arg_pairs.push(format!("{} {}", first_user_ty, lhs));

                let offset = if is_no_task_intrinsic(&id.name) { 1 } else { 2 };
                for (i, a) in args.iter().enumerate() {
                    let v = self.emit_expr(a);
                    let t = param_types
                        .get(i + offset)
                        .cloned()
                        .unwrap_or_else(|| "i32".to_string());
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
                return tmp;
            }
        }
        self.emit_expr(left);
        self.emit_expr(right);
        "0".to_string()
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
            return self.emit_option_from_i8_ptr(&opt_ty, &elem_ty, &raw_ptr);
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

        let len = self.fixed_array_len(&array_ty).unwrap_or(0);
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
        let some_val = self.emit_option_some(&opt_ty, &elem_ty, &loaded);
        self.emit(format!("  br label %{}", merge_lbl));

        self.emit(format!("{}:", none_lbl));
        let none_val = self.emit_option_none(&opt_ty, &elem_ty);
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
        // Method call: base.method(args)
        if let ExpressionKind::FieldAccess { base, field } = &func.node {
            let base_val = self.emit_expr(base);
            let base_ty = self.expr_llvm_type(base);

            // Array push
            if base_ty == "%struct.TyArray*" && field.name == "push" {
                if let Some(arg0) = args.first() {
                    let val = self.emit_expr(arg0);
                    let val_ty = self.expr_llvm_type(arg0);
                    let slot = self.tmp();
                    self.emit(format!("  {} = alloca {}", slot, val_ty));
                    self.emit(format!("  store {} {}, {}* {}", val_ty, val, val_ty, slot));
                    let raw = self.tmp();
                    self.emit(format!("  {} = bitcast {}* {} to i8*", raw, val_ty, slot));
                    self.emit(format!(
                        "  call void @ty_array_push(i8* %task, %struct.TyArray* {}, i8* {})",
                        base_val, raw
                    ));
                }
                return "0".to_string();
            }

            // User-defined method
            if let Some(method_sym) = self.method_symbol_for_call(&base_ty, &field.name) {
                let runtime_name = link_symbol_name(&method_sym);
                let (ret_ty, param_types) = self
                    .func_sigs
                    .get(&method_sym)
                    .cloned()
                    .unwrap_or_else(|| ("i32".to_string(), vec![]));

                // param_types is [i8* (task), self_ty, arg1_ty, ...]
                // Index 0 = task (injected separately)
                // Index 1 = self
                // Index 2+ = explicit args
                let self_ty = param_types
                    .get(1) // ← was .first() i.e. index 0
                    .cloned()
                    .unwrap_or_else(|| base_ty.clone());

                let mut arg_pairs = vec![
                    "i8* %task".to_string(), // ← task first, bare reference
                    format!("{} {}", self_ty, base_val),
                ];
                for (i, a) in args.iter().enumerate() {
                    let v = self.emit_expr(a);
                    let t = param_types
                        .get(i + 2) // ← was i + 1, now offset by 2 (skip task + self)
                        .cloned()
                        .unwrap_or_else(|| "i32".to_string());
                    arg_pairs.push(format!("{} {}", t, v));
                }
                let tmp = self.tmp();
                if ret_ty == "void" {
                    self.emit(format!(
                        "  call void @{}({})",
                        runtime_name,
                        arg_pairs.join(", ")
                    ));
                    return "0".to_string();
                }
                self.emit(format!(
                    "  {} = call {} @{}({})",
                    tmp,
                    ret_ty,
                    runtime_name,
                    arg_pairs.join(", ")
                ));
                return tmp;
            }
        }

        // ADT constructors / free functions
        if let ExpressionKind::Identifier(id) = &func.node {
            if matches!(id.name.as_str(), "Ok" | "Err" | "Some" | "None") {
                return self.emit_adt_constructor(&id.name, call_expr, args);
            }
            let runtime_name =
                runtime_intrinsic_name(&id.name).unwrap_or_else(|| link_symbol_name(&id.name));
            let (ret_ty, param_types) = self
                .func_sigs
                .get(&id.name)
                .cloned()
                .unwrap_or_else(|| ("i32".to_string(), vec![]));
            let tail = if self.current_fn_name.as_deref() == Some(id.name.as_str()) {
                "tail "
            } else {
                ""
            };

            let mut arg_pairs = Vec::new();
            if !is_no_task_intrinsic(&runtime_name) {
                arg_pairs.push("i8* %task".to_string());
            }
            for (i, arg) in args.iter().enumerate() {
                let v = self.emit_expr(arg);
                let t = param_types
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| "i32".to_string());
                arg_pairs.push(format!("{} {}", t, v));
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
            return tmp;
        }

        "0".to_string()
    }

    // ── Match / if-let ────────────────────────────────────────────────────────

    fn emit_match_expression(&mut self, expr: &Expression, arms: &[MatchArm]) -> String {
        let result_ty = self.expr_llvm_type(expr);
        let result_slot = if result_ty != "void" {
            let slot = self.tmp();
            self.emit(format!("  {} = alloca {}", slot, result_ty));
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
                let store_ty = if actual_ty == "i32" || actual_ty.is_empty() {
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

    fn emit_if_let(
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
            self.emit(format!("  {} = alloca {}", slot, result_ty));
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
                    let store_ty = if actual_ty == "i32" {
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
                let store_ty = if actual_ty == "i32" {
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

    // ── Pattern helpers ───────────────────────────────────────────────────────

    fn emit_pattern_test(
        &mut self,
        pattern: &Pattern,
        scrutinee_expr: &Expression,
        scrutinee_val: &str,
    ) -> String {
        let ty = self.expr_llvm_type(scrutinee_expr);
        match &pattern.node {
            PatternKind::Wildcard | PatternKind::Identifier(_) => "1".to_string(),
            PatternKind::Literal(lit) => match &lit.kind {
                LiteralKind::Int(v, _) => {
                    let tmp = self.tmp();
                    self.emit(format!(
                        "  {} = icmp eq {} {}, {}",
                        tmp, ty, scrutinee_val, v
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
            PatternKind::EnumVariant { variant_name, .. } => {
                if let Some(tag) = enum_variant_tag(&variant_name.name) {
                    let loaded = self.tmp();
                    let cmp = self.tmp();
                    self.emit(format!(
                        "  {} = extractvalue {} {}, 0",
                        loaded, ty, scrutinee_val
                    ));
                    self.emit(format!("  {} = icmp eq i1 {}, {}", cmp, loaded, tag));
                    cmp
                } else {
                    "1".to_string()
                }
            }
            _ => "1".to_string(),
        }
    }

    fn bind_pattern_value(
        &mut self,
        pattern: &Pattern,
        scrutinee_expr: &Expression,
        scrutinee_val: &str,
    ) {
        let ty = self.expr_llvm_type(scrutinee_expr);
        self.bind_pattern_typed(pattern, scrutinee_val, &ty, Some(scrutinee_expr));
    }

    fn bind_pattern_typed(
        &mut self,
        pattern: &Pattern,
        val: &str,
        ty: &str,
        scrutinee_expr: Option<&Expression>,
    ) {
        match &pattern.node {
            PatternKind::Wildcard | PatternKind::Literal(_) => {}
            PatternKind::Identifier(id) => {
                let slot = self.tmp();
                self.emit(format!("  {} = alloca {}", slot, ty));
                self.emit(format!("  store {} {}, {}* {}", ty, val, ty, slot));
                self.locals.insert(id.name.clone(), slot);
                self.locals_type.insert(id.name.clone(), ty.to_string());
            }
            PatternKind::EnumVariant {
                variant_name,
                payload: Some(inner),
                ..
            } => {
                if let Some(idx) = enum_variant_payload_index(&variant_name.name) {
                    let payload_ty = scrutinee_expr
                        .and_then(|e| {
                            let inferred = self.inferred_expr_type(e)?.clone();
                            Some(self.payload_type_from_infer(&inferred, &variant_name.name, idx))
                        })
                        .unwrap_or_else(|| "i32".to_string());
                    let extracted = self.tmp();
                    self.emit(format!(
                        "  {} = extractvalue {} {}, {}",
                        extracted, ty, val, idx
                    ));
                    let pt = payload_ty.clone();
                    self.bind_pattern_typed(inner, &extracted, &pt, None);
                }
            }
            PatternKind::EnumVariant { payload: None, .. } => {}
            PatternKind::Struct { fields, .. } => {
                let struct_name = ty.trim_start_matches("%struct.").to_string();
                for (field_name, field_pat) in fields {
                    let (idx, fty) = self.struct_field_info(&struct_name, &field_name.name);
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

    fn payload_type_from_infer(
        &mut self,
        infer: &InferType,
        variant_name: &str,
        payload_index: usize,
    ) -> String {
        match infer {
            InferType::App(name, args) if name == "Option" && args.len() == 1 => {
                if variant_name == "Some" && payload_index == 1 {
                    return self.lower_infer_type(&args[0]);
                }
            }
            InferType::App(name, args) if name == "Result" && args.len() == 2 => {
                match payload_index {
                    1 if variant_name == "Ok" => return self.lower_infer_type(&args[0]),
                    2 if variant_name == "Err" => return self.lower_infer_type(&args[1]),
                    _ => {}
                }
            }
            _ => {}
        }
        "i32".to_string()
    }

    // ── Option / Result helpers ───────────────────────────────────────────────

    fn emit_option_some(&mut self, opt_ty: &str, elem_ty: &str, value: &str) -> String {
        let t1 = self.tmp();
        self.emit(format!("  {} = insertvalue {} undef, i1 1, 0", t1, opt_ty));
        let t2 = self.tmp();
        self.emit(format!(
            "  {} = insertvalue {} {}, {} {}, 1",
            t2, opt_ty, t1, elem_ty, value
        ));
        t2
    }

    fn emit_option_none(&mut self, opt_ty: &str, elem_ty: &str) -> String {
        let t1 = self.tmp();
        self.emit(format!("  {} = insertvalue {} undef, i1 0, 0", t1, opt_ty));
        let z = self.zero_value(elem_ty);
        let t2 = self.tmp();
        self.emit(format!(
            "  {} = insertvalue {} {}, {} {}, 1",
            t2, opt_ty, t1, elem_ty, z
        ));
        t2
    }

    fn emit_option_from_i8_ptr(&mut self, opt_ty: &str, elem_ty: &str, ptr_i8: &str) -> String {
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
        let some_val = self.emit_option_some(opt_ty, elem_ty, &loaded);
        self.emit(format!("  br label %{}", merge_lbl));

        self.emit(format!("{}:", none_lbl));
        let none_val = self.emit_option_none(opt_ty, elem_ty);
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
        let ty = self.lower_infer_type(&infer);
        let tag = matches!(ctor, "Ok" | "Some");
        let t0 = self.tmp();
        self.emit(format!(
            "  {} = insertvalue {} undef, i1 {}, 0",
            t0,
            ty,
            if tag { "1" } else { "0" }
        ));
        let mut cur = t0;

        let payload_info: Option<(String, usize)> = match (&infer, ctor) {
            (InferType::App(n, inner), "Ok") if n == "Result" && inner.len() == 2 => {
                Some((self.lower_infer_type(&inner[0]), 1))
            }
            (InferType::App(n, inner), "Err") if n == "Result" && inner.len() == 2 => {
                Some((self.lower_infer_type(&inner[1]), 2))
            }
            (InferType::App(n, inner), "Some") if n == "Option" && inner.len() == 1 => {
                Some((self.lower_infer_type(&inner[0]), 1))
            }
            _ => None,
        };
        if let Some((payload_ty, idx)) = payload_info {
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

    // ── Type helpers ──────────────────────────────────────────────────────────

    fn lower_type(&self, ty: &Type) -> String {
        match ty.node.name.as_str() {
            "Unit" => "void".to_string(),
            "Int8" | "Char" | "Byte" => "i8".to_string(),
            "Int16" => "i16".to_string(),
            "Int32" => "i32".to_string(),
            "Int64" => "i64".to_string(),
            "Float16" => "half".to_string(),
            "Float32" => "float".to_string(),
            "Float64" => "double".to_string(),
            "Bool" => "i1".to_string(),
            "Str" | "ref" => "i8*".to_string(),
            "Buf" => "%struct.Buf*".to_string(),
            "Array" => "%struct.TyArray*".to_string(),
            "Option" => ty
                .node
                .generic_args
                .first()
                .map(|a| {
                    format!(
                        "%struct.Option__{}",
                        mangle_llvm_type_name(&self.lower_type(a))
                    )
                })
                .unwrap_or_else(|| "%struct.Option__i32".to_string()),
            "Result" => {
                let a = ty
                    .node
                    .generic_args
                    .get(0)
                    .map(|t| self.lower_type(t))
                    .unwrap_or_else(|| "i32".to_string());
                let b = ty
                    .node
                    .generic_args
                    .get(1)
                    .map(|t| self.lower_type(t))
                    .unwrap_or_else(|| "i32".to_string());
                format!(
                    "%struct.Result__{}__{}",
                    mangle_llvm_type_name(&a),
                    mangle_llvm_type_name(&b)
                )
            }
            name => format!("%struct.{}", name),
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
                n => format!("%struct.{}", n),
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
            InferType::App(name, _) if name == "Array" => "%struct.TyArray*".to_string(),
            InferType::FixedArray(elem, n) => {
                format!("[{} x {}]", n, self.lower_infer_type(elem))
            }
            _ => "i32".to_string(),
        }
    }

    fn expr_llvm_type(&mut self, expr: &Expression) -> String {
        // Locals (most specific)
        if let ExpressionKind::Identifier(id) = &expr.node {
            if let Some(ty) = self.locals_type.get(&id.name) {
                return ty.clone();
            }
        }
        // Type checker inference
        if let Some(ty) = self.inferred_expr_type(expr).cloned() {
            return self.lower_infer_type(&ty);
        }
        // Syntactic fallback
        match &expr.node {
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Int(_, suffix),
                ..
            }) => int_suffix_to_llvm(suffix.as_deref().unwrap_or("")).to_string(),
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Float(_, suffix),
                ..
            }) => if suffix.as_deref() == Some("f64") {
                "double"
            } else {
                "float"
            }
            .to_string(),
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
                .map(|b| self.expr_llvm_type(b))
                .unwrap_or_else(|| "%struct.?".to_string()),
            ExpressionKind::FieldAccess { base, field } => {
                let base_ty = self.expr_llvm_type(base);
                let struct_name = base_ty.trim_start_matches("%struct.").to_string();
                let (_, fty) = self.struct_field_info(&struct_name, &field.name);
                fty
            }
            ExpressionKind::Call { func, .. } => {
                if let ExpressionKind::FieldAccess { base, field } = &func.node {
                    let base_ty = self.expr_llvm_type(base);
                    if base_ty == "%struct.TyArray*" && field.name == "push" {
                        return "void".to_string();
                    }
                    if let Some(sym) = self.method_symbol_for_call(&base_ty, &field.name) {
                        return self
                            .func_sigs
                            .get(&sym)
                            .map(|(r, _)| r.clone())
                            .unwrap_or_else(|| "i32".to_string());
                    }
                } else if let ExpressionKind::Identifier(id) = &func.node {
                    return self
                        .func_sigs
                        .get(&id.name)
                        .map(|(r, _)| r.clone())
                        .unwrap_or_else(|| "i32".to_string());
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

    fn inferred_expr_type(&self, expr: &'a Expression) -> Option<&'a InferType> {
        // SAFETY: types map lives longer than this builder call.
        let types = unsafe { &*self.types? };
        types.get(&expr.id)
    }

    fn array_elem_type_from_infertype(&mut self, ty: &InferType) -> Option<String> {
        if let InferType::App(name, args) = ty {
            if name == "Array" && args.len() == 1 {
                return Some(self.lower_infer_type(&args[0]));
            }
        }
        None
    }

    fn option_type_for_index(&mut self, expr: &Expression) -> Option<(String, String)> {
        let ty = self.inferred_expr_type(expr)?.clone();
        if let InferType::App(ref name, ref args) = ty {
            if name == "Option" && args.len() == 1 {
                let elem = self.lower_infer_type(&args[0]);
                let opt = self.lower_infer_type(&ty);
                return Some((opt, elem));
            }
        }
        None
    }

    // ── Struct field lookup ───────────────────────────────────────────────────

    /// Returns (field_index, field_llvm_type) for a struct field.
    fn struct_field_info(&self, struct_name: &str, field_name: &str) -> (usize, String) {
        if let Some(fields) = self.struct_fields.get(struct_name) {
            if let Some(idx) = fields.iter().position(|(n, _)| n == field_name) {
                return (idx, fields[idx].1.clone());
            }
        }
        (0, "i32".to_string())
    }

    fn method_symbol_for_call(&self, base_ty: &str, method: &str) -> Option<String> {
        base_ty
            .trim_end_matches('*')
            .strip_prefix("%struct.")
            .map(|name| format!("__ty_method__{}__{}", name, method))
    }

    // ── ADT struct tracking ───────────────────────────────────────────────────

    fn ensure_option(&mut self, inner: &str) {
        let name = format!("%struct.Option__{}", mangle_llvm_type_name(inner));
        if !self.adt_structs.contains_key(&name) {
            self.type_decls
                .push(format!("{} = type {{ i1, {} }}", name, inner));
            self.adt_structs.insert(name, "option".to_string());
        }
    }

    fn ensure_result(&mut self, ok: &str, err: &str) {
        let name = format!(
            "%struct.Result__{}__{}",
            mangle_llvm_type_name(ok),
            mangle_llvm_type_name(err)
        );
        if !self.adt_structs.contains_key(&name) {
            self.type_decls
                .push(format!("{} = type {{ i1, {}, {} }}", name, ok, err));
            self.adt_structs.insert(name, "result".to_string());
        }
    }

    fn scan_decl_for_adts(&mut self, decl: &Declaration) {
        match &decl.node {
            DeclarationKind::Function {
                params,
                return_type,
                ..
            } => {
                for p in params {
                    self.ensure_adt_for_type(&p.type_annotation);
                }
                if let Some(ret) = return_type {
                    self.ensure_adt_for_type(ret);
                }
            }
            DeclarationKind::Struct { fields, .. } => {
                for (_, ty) in fields {
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
                if let Some(a) = ty.node.generic_args.first() {
                    let inner = self.lower_type(a);
                    self.ensure_option(&inner);
                }
            }
            "Result" => {
                if let (Some(a), Some(b)) =
                    (ty.node.generic_args.get(0), ty.node.generic_args.get(1))
                {
                    let (ok, err) = (self.lower_type(a), self.lower_type(b));
                    self.ensure_result(&ok, &err);
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

    // ── Size / alignment ──────────────────────────────────────────────────────

    fn llvm_const_sizeof(&self, ty: &str) -> i64 {
        match ty {
            "i1" | "i8" => 1,
            "i16" => 2,
            "i32" | "float" => 4,
            "i64" | "double" => 8,
            _ => 8,
        }
    }

    fn llvm_const_alignof(&self, ty: &str) -> i64 {
        self.llvm_const_sizeof(ty)
    }

    fn llvm_size_align_of(&mut self, ty: &str) -> (String, String) {
        match ty {
            "i1" | "i8" => ("1".to_string(), "1".to_string()),
            "i16" => ("2".to_string(), "2".to_string()),
            "i32" | "float" => ("4".to_string(), "4".to_string()),
            "i64" | "double" => ("8".to_string(), "8".to_string()),
            t if t.ends_with('*') => ("8".to_string(), "8".to_string()),
            _ => {
                let gep = self.tmp();
                self.emit(format!(
                    "  {} = getelementptr {}, {}* null, i32 1",
                    gep, ty, ty
                ));
                let sz = self.tmp();
                self.emit(format!("  {} = ptrtoint {}* {} to i64", sz, ty, gep));
                (sz, "8".to_string())
            }
        }
    }

    fn zero_value(&self, ty: &str) -> String {
        if ty.ends_with('*') {
            "null".to_string()
        } else if matches!(ty, "float" | "double") {
            "0.0".to_string()
        } else if ty.starts_with('i') {
            "0".to_string()
        } else {
            "zeroinitializer".to_string()
        }
    }

    fn fixed_array_len(&self, array_ty: &str) -> Option<usize> {
        let end = array_ty.find(']')?;
        let inner = &array_ty[1..end];
        let space = inner.find(' ')?;
        inner[..space].trim().parse().ok()
    }

    /// Infer the LLVM element type from the first element of an array literal.
    fn infer_elem_ty(&self, elems: &[Expression]) -> String {
        elems
            .first()
            .map(|e| match &e.node {
                ExpressionKind::Literal(Literal {
                    kind: LiteralKind::Int(_, suffix),
                    ..
                }) => int_suffix_to_llvm(suffix.as_deref().unwrap_or("")).to_string(),
                ExpressionKind::Literal(Literal {
                    kind: LiteralKind::Float(_, suffix),
                    ..
                }) => if suffix.as_deref() == Some("f64") {
                    "double"
                } else {
                    "float"
                }
                .to_string(),
                ExpressionKind::Literal(Literal {
                    kind: LiteralKind::Bool(_),
                    ..
                }) => "i1".to_string(),
                _ => "i32".to_string(),
            })
            .unwrap_or_else(|| "i32".to_string())
    }

    // ── String literals ───────────────────────────────────────────────────────

    fn emit_string(&mut self, s: &str) -> String {
        let (global, n) = if let Some(v) = self.string_pool.get(s).cloned() {
            v
        } else {
            let id = self.string_pool.len();
            let global = format!("@.str.{}", id);
            let bytes = s.as_bytes();
            let n = bytes.len() + 1;
            self.extra_preamble.push(format!(
                "{} = private unnamed_addr constant [{} x i8] c\"{}\\00\"",
                global,
                n,
                llvm_escape(bytes)
            ));
            let pair = (global.clone(), n);
            self.string_pool.insert(s.to_string(), pair.clone());
            pair
        };
        let tmp = self.tmp();
        self.emit(format!(
            "  {} = getelementptr inbounds ([{} x i8], [{} x i8]* {}, i32 0, i32 0)",
            tmp, n, n, global
        ));
        tmp
    }
}

// ── Free functions ────────────────────────────────────────────────────────────

fn is_main(name: &str) -> bool {
    return name == "main" || name.ends_with("__main");
}

fn int_suffix_to_llvm(suffix: &str) -> &'static str {
    match suffix {
        "i8" | "u8" => "i8",
        "i16" => "i16",
        "i64" => "i64",
        _ => "i32",
    }
}

fn get_size_class(size: i64) -> u32 {
    match size {
        0..=8 => 0,
        9..=16 => 1,
        17..=32 => 2,
        33..=64 => 3,
        65..=128 => 4,
        _ => 5, // Fallback/Large
    }
}

fn array_elem_type_from_str(array_ty: &str) -> String {
    if let (Some(x), Some(end)) = (array_ty.find(" x "), array_ty.find(']')) {
        array_ty[x + 3..end].to_string()
    } else {
        "i32".to_string()
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

fn is_no_task_intrinsic(name: &str) -> bool {
    matches!(
        name,
        "ty_array_get_ptr" | "ty_yield" | "ty_chan_new" | "ty_chan_close" | "slab_arena_new"
    )
}

fn runtime_intrinsic_name(name: &str) -> Option<String> {
    match name {
        "__ty_buf_new" => Some("ty_buf_new".to_string()),
        "__ty_buf_push_str" => Some("ty_buf_push_str".to_string()),
        "__ty_buf_into_str" => Some("ty_buf_into_str".to_string()),

        // Scheduler builtins — surface name matches C symbol directly
        "spawn" => Some("ty_spawn".to_string()),
        "yield" => Some("ty_yield".to_string()),
        "await" => Some("ty_await".to_string()),

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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn compile(source: &str) -> String {
        let mut src = source.trim().to_string();
        if !src.starts_with("namespace main") {
            src = format!("namespace main\n{}", src);
        }
        if !src.contains("fn main") {
            src.push_str("\nfn main() -> Int32 { return 0; }");
        }
        let module = Parser::new(Lexer::new(src).tokenize())
            .parse_module()
            .unwrap();
        let mut checker = crate::type_inference::TypeChecker::new();
        checker.check_module(&module).unwrap();
        let mut liveness = crate::liveness::LiveAnalyzer::new();
        let drop_map = liveness
            .analyze_module(&module)
            .unwrap_or(&std::collections::HashMap::new())
            .clone();
        let ir = Codegen::lower_module(&module, checker.types(), &drop_map);
        ir.to_llvm_ir()
    }

    #[test]
    fn lowers_function_declarations() {
        let source = "fn main(a: Int32) -> Int32 { return a; }";
        let src = format!("namespace main\n{}", source);
        let module = Parser::new(Lexer::new(src).tokenize())
            .parse_module()
            .unwrap();
        let mut checker = crate::type_inference::TypeChecker::new();
        checker.check_module(&module).unwrap();
        let mut liveness = crate::liveness::LiveAnalyzer::new();
        let drop_map = liveness
            .analyze_module(&module)
            .unwrap_or(&std::collections::HashMap::new())
            .clone();
        let ir = Codegen::lower_module(&module, checker.types(), &drop_map);
        assert_eq!(ir.functions.len(), 1);
        assert_eq!(ir.functions[0].name, "main");
        assert_eq!(ir.functions[0].ret_type, "i32");
        assert_eq!(ir.functions[0].params.len(), 1);
    }

    #[test]
    fn emits_basic_llvm_ir() {
        let text = compile("fn main() -> Int32 { return 0; }");
        assert!(text.contains("define i32 @main()"));
        assert!(text.contains("ret i32 0"));
    }

    #[test]
    fn lowers_let_bindings() {
        let text = compile("fn main() -> Int32 { let x: Int32 = 3; return x; }");
        assert!(text.contains("alloca i32"));
        assert!(text.contains("store i32 3"));
        assert!(text.contains("load i32"));
    }

    #[test]
    fn emits_if_branches() {
        let text =
            compile("fn main(flag: Bool) -> Int32 { if flag { return 1; } else { return 2; } }");
        assert!(text.contains("br i1"));
        assert!(text.contains("if_merge"));
    }

    #[test]
    fn lowers_struct_init_and_merge() {
        let text = compile("struct User { id: Int32, age: Int32 } fn main() -> Int32 { let user: User = User { id: 1, age: 2 }; let updated: User = { ...user, age: 3 }; return 0; }");
        assert!(text.contains("%struct.User = type"));
        assert!(text.contains("insertvalue %struct.User"));
    }

    #[test]
    fn heap_allocates_mutable_struct_lets() {
        let text = compile("struct Point { x: Int32, y: Int32 } fn main() -> Int32 { let mut p: Point = Point { x: 1, y: 2 }; return 0; }");
        assert!(text.contains("call i8* @slab_alloc"));
        assert!(text.contains("bitcast i8*"));
        assert!(text.contains("%struct.Point*"));
    }

    #[test]
    fn widens_mutable_array_literals_to_tyarray() {
        let text = compile("fn main() -> Int32 { let mut xs: Array<Int32> = [1,2,3]; return 0; }");
        assert!(text.contains("%struct.TyArray = type"));
        assert!(text.contains("@ty_array_from_fixed"));
    }

    #[test]
    fn lowers_struct_method_calls_as_function_calls() {
        let text = compile("struct User { id: Int32 } fn __ty_method__User__get_id(self: User) -> Int32 { return self.id; } fn main() -> Int32 { let u: User = User { id: 1 }; return u.get_id(); }");
        assert!(text.contains("call i32 @__ty_method__User__get_id"));
    }

    #[test]
    fn lowers_array_push_and_index_to_runtime_calls() {
        let text = compile("fn main() -> Int32 { let mut xs: Array<Int32> = [1,2]; xs.push(3); let v: Option<Int32> = xs[0]; return 0; }");
        assert!(text.contains("@ty_array_push"));
        assert!(text.contains("@ty_array_get_ptr"));
    }

    #[test]
    fn lowers_result_constructors_to_aggregate_values() {
        let text = compile("fn main() -> Result<Int32, Str> { return Ok(1); }");
        assert!(text.contains("%struct.Result__"));
        assert!(text.contains("insertvalue %struct.Result__"));
    }

    #[test]
    fn lowers_match_to_control_flow() {
        let text = compile("fn main(x: Int32) -> Int32 { match x { 0 => 1, _ => 2, } }");
        assert!(text.contains("br label %match_check"));
        assert!(text.contains("icmp eq i32"));
        assert!(text.contains("match_merge"));
    }

    #[test]
    fn lowers_if_let_to_control_flow() {
        let text = compile("fn main(x: Result<Int32, Str>) -> Int32 { if let Ok(v) = x { return v; } else { return 0; } }");
        assert!(text.contains("iflet_then"));
        assert!(text.contains("extractvalue"));
        assert!(text.contains("iflet_merge"));
    }
}
