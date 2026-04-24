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

                    if is_main(&name.name) {
                        // ── Emit user's main body as a coroutine ──────────────
                        // The body runs inside a spawned coroutine so that
                        // ty_chan_recv/send can park properly (me != NULL).
                        // We give it the internal name __ty_main_body and treat
                        // it like any other non-main function (task param, no
                        // sched_init, no sched_shutdown inside).
                        let body_ir = b.emit_main_body(params, body);
                        let body_fn = IrFunction {
                            name: "__ty_main_body".to_string(),
                            body: body_ir,
                            ret_type: "void".to_string(),
                            params: vec![
                                ("task".to_string(), "i8*".to_string()),
                                ("arg".to_string(), "i8*".to_string()),
                            ],
                        };

                        // ── Emit thin bootstrap main() ────────────────────────
                        // Initialises the scheduler, spawns __ty_main_body, then
                        // drives it to completion via ty_sched_shutdown.
                        let bootstrap = IrFunction {
                            name: "main".to_string(),
                            body: b.emit_bootstrap_main(),
                            ret_type: "i32".to_string(),
                            params: vec![],
                        };

                        // We return both; collect() flattens via extend below.
                        // Use a small trick: push body_fn into conc_functions so
                        // we can return only one Option here, then push bootstrap.
                        b.conc_functions.push(body_fn);
                        Some(bootstrap)
                    } else {
                        let body_ir = b.emit_function(name, params, &ret_ty, body);
                        let mut param_list: Vec<(String, String)> = params
                            .iter()
                            .map(|p| (p.name.name.clone(), b.lower_type(&p.type_annotation)))
                            .collect();
                        param_list.insert(0, ("task".to_string(), "i8*".to_string()));
                        Some(IrFunction {
                            name: link_symbol_name(&name.name), // ← apply the same mangling
                            body: body_ir,
                            ret_type: ret_ty,
                            params: param_list,
                        })
                    }
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
    entry_allocas: Vec<String>,
    next_tmp: usize,
    next_label: usize,
    current_fn_name: Option<String>,
    current_fn_ret_ty: String,
    locals: HashMap<String, String>,
    locals_type: HashMap<String, String>,
    parent_locals: HashMap<String, String>, // captured variables from parent scope
    parent_types: HashMap<String, String>,  // types of captured variables
    mutable_vars: std::collections::HashSet<String>, // track mutable variables for capture
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
            entry_allocas: Vec::new(),
            next_tmp: 0,
            next_label: 0,
            current_fn_name: None,
            current_fn_ret_ty: "void".to_string(),
            locals: HashMap::new(),
            locals_type: HashMap::new(),
            parent_locals: HashMap::new(),
            parent_types: HashMap::new(),
            mutable_vars: std::collections::HashSet::new(),
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

    /// Emit an alloca into the entry block regardless of the current basic block.
    /// LLVM only lowers entry-block allocas to static frame slots; allocas anywhere
    /// else trigger the broken __chkstk + subq %rax,%rsp sequence on Windows x64.
    fn emit_alloca(&mut self, tmp: &str, ty: &str) {
        self.entry_allocas
            .push(format!("  {} = alloca {}", tmp, ty));
    }

    /// Splice hoisted entry_allocas in right after the "entry:" label line.
    fn finish_function_ir(&mut self) -> String {
        let mut all = Vec::with_capacity(self.lines.len() + self.entry_allocas.len());
        if !self.lines.is_empty() {
            all.push(self.lines[0].clone());
        }
        all.extend(self.entry_allocas.drain(..));
        if self.lines.len() > 1 {
            all.extend(self.lines[1..].iter().cloned());
        }
        all.join("\n")
    }

    /// Load task from its alloca slot — never use the raw %task SSA param in
    /// call arguments because %rcx gets clobbered by intervening loads.
    fn emit_task_load(&mut self) -> String {
        if let Some(slot) = self.locals.get("task").cloned() {
            let t = self.tmp();
            self.emit(format!("  {} = load i8*, i8** {}", t, slot));
            t
        } else {
            "%task".to_string()
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
            // Opaque runtime handles (passed as pointers only)
            "%struct.Network = type { i8 }",
            "%struct.Listener = type { i8 }",
            "%struct.Socket = type { i8 }",
        ] {
            self.type_decls.push(decl.to_string());
        }

        // Networking Result types referenced by runtime intrinsics below.
        // Ensure their ADT layouts exist before we emit `declare` lines.
        self.ensure_result("%struct.Listener*", "i32");
        self.ensure_result("%struct.Socket*", "i32");
        let res_listener_i32 = format!(
            "%struct.Result__{}__{}",
            mangle_llvm_type_name("%struct.Listener*"),
            mangle_llvm_type_name("i32")
        );
        let res_socket_i32 = format!(
            "%struct.Result__{}__{}",
            mangle_llvm_type_name("%struct.Socket*"),
            mangle_llvm_type_name("i32")
        );

        for decl in [
            // ── scheduler ──
            "declare void @ty_sched_init     ()",
            "declare void @ty_sched_run      ()",
            "declare void @ty_sched_shutdown ()",
            "declare i8*  @ty_spawn          (i8*, i8*, i8*)", // task, fn_ptr, arg
            "declare void @ty_yield          ()",
            "declare void @ty_await          (i8*, i8*)", // task, coro_handle
            "declare i8*  @ty_chan_new       (i64, i64)", // elem_size, cap
            "declare void @ty_chan_send      (i8*, i8*, i8*)", // task, chan, elem_ptr
            "declare void @ty_chan_recv      (i8*, i8*, i8*)", // task, chan, out_ptr
            "declare i32  @ty_chan_try_recv  (i8*, i8*, i8*)", // task, chan, out_ptr -> i32 (0/1)
            "declare void @ty_chan_close     (i8*)",      // chan
            // ── Buf (all now take task first) ──
            "declare %struct.Buf* @ty_buf_new      (i8* %task)",
            "declare void         @ty_buf_push_str (i8*, %struct.Buf*, i8*)",
            "declare i8*          @ty_buf_into_str (i8*, %struct.Buf*)",
            // ── TyArray (all now take task first) ──
            "declare %struct.TyArray* @ty_array_from_fixed (i8*, i8*, i64, i64, i64)",
            "declare void             @ty_array_push       (i8*, %struct.TyArray*, i8*)",
            "declare i8*              @ty_array_get_ptr    (%struct.TyArray*, i64)",
            // ── arena / slab ──
            "declare i8*  @slab_arena_new  ()",
            "declare i8*  @slab_alloc      (i8* %task, i32 %size_class)",
            "declare void @slab_free       (i8* %task, i8* %ptr, i32 %size_class)",
            "declare void @slab_arena_free (i8*)",
            // ── I/O driver (file ops) ──────────────────────────────────────────────────
            "declare void @ty_io_subsystem_init     ()",
            "declare void @ty_io_subsystem_shutdown ()",
            "declare i32  @ty_io_open               (i8* %driver, i8* %path, i32 %flags, i32 %mode)",
            "declare void @ty_io_close              (i8* %driver, i32 %fd)",
            // ── networking ───────────────────────────────────────────────────────────
            "declare void @ty_net_init              ()",
            "declare void @ty_net_shutdown          ()",
            "declare %struct.Network* @ty_net_global()",
            // ── print family ──────────────────────────────────────────────────────────
            "declare void @ty_print    (i8* %task, i8* %s)",
            "declare void @ty_println  (i8* %task, i8* %s)",
            // ty_printf is varargs — LLVM declares varargs with "..."
            "declare void @ty_printf   (i8* %task, i8* %fmt, ...)",
            "declare void @ty_fprint   (i8* %task, i32 %fd, i8* %s)",
            "declare void @ty_fprintln (i8* %task, i32 %fd, i8* %s)",
            "declare void @ty_fprintf  (i8* %task, i32 %fd, i8* %fmt, ...)",
            "declare void @ty_sprint   (i8* %task, %struct.Buf* %out, i8* %s)",
            "declare void @ty_sprintln (i8* %task, %struct.Buf* %out, i8* %s)",
            "declare void @ty_sprintf  (i8* %task, %struct.Buf* %out, i8* %fmt, ...)",
            // ── scan family ───────────────────────────────────────────────────────────
            "declare i8*  @ty_scan     (i8* %task)",
            "declare i32  @ty_scanf    (i8* %task, i8* %fmt, ...)",
            "declare i8*  @ty_fscan    (i8* %task, i32 %fd)",
            "declare i32  @ty_fscanf   (i8* %task, i32 %fd, i8* %fmt, ...)",
            "declare i8*  @ty_sscan    (i8* %task, i8* %src, i8** %rest_out)",
            "declare i32  @ty_sscanf   (i8* %task, i8* %src, i8* %fmt, ...)",
            // ── Network / Listener / Socket methods (runtime-provided) ──────────────
            // Result<Listener, Int32>
            // Result<Socket, Int32>
        ] {
            self.extra_preamble.push(decl.to_string());
        }

        // These declarations depend on the computed Result struct names above.
        self.extra_preamble.push(format!(
            "declare void @__ty_method__Network__listen(i8* %task, %struct.Network* %self, i8* %addr, {}* %out)",
            res_listener_i32
        ));
        self.extra_preamble.push(format!(
            "declare void @__ty_method__Listener__accept(i8* %task, %struct.Listener* %self, {}* %out)",
            res_socket_i32
        ));
        self.extra_preamble.push(
            "declare void @__ty_method__Socket__consume(i8* %task, %struct.Socket* %self, i8* %chan)"
                .to_string(),
        );
        self.extra_preamble.push(
            "declare void @__ty_method__Socket__close(i8* %task, %struct.Socket* %self)"
                .to_string(),
        );

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

        // Runtime-provided networking methods.
        self.func_sigs.insert(
            "__ty_method__Network__listen".into(),
            (
                "void".into(),
                vec![
                    "i8*".into(),
                    "%struct.Network*".into(),
                    "i8*".into(),
                    format!("{}*", res_listener_i32),
                ],
            ),
        );
        self.func_sigs.insert(
            "__ty_method__Listener__accept".into(),
            (
                "void".into(),
                vec![
                    "i8*".into(),
                    "%struct.Listener*".into(),
                    format!("{}*", res_socket_i32),
                ],
            ),
        );
        self.func_sigs.insert(
            "__ty_method__Socket__consume".into(),
            (
                "void".into(),
                vec!["i8*".into(), "%struct.Socket*".into(), "i8*".into()],
            ),
        );
        self.func_sigs.insert(
            "__ty_method__Socket__close".into(),
            ("void".into(), vec!["i8*".into(), "%struct.Socket*".into()]),
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
        self.func_sigs.insert(
            "ty_chan_try_recv".into(),
            ("i32".into(), vec!["i8*".into(), "i8*".into()]),
        ); // chan, out_ptr -> i32
        self.func_sigs
            .insert("ty_chan_close".into(), ("void".into(), vec!["i8*".into()]));

        // ── stdio intrinsics ──────────────────────────────────────────────────
        // Keyed under both source names (what the call-site lookup uses) and
        // runtime names. Param lists exclude the implicit leading `task` arg.
        for (src, rt, ret, params) in [
            ("print", "ty_print", "void", vec!["i8*"]),
            ("println", "ty_println", "void", vec!["i8*"]),
            ("printf", "ty_printf", "void", vec!["i8*"]),
            ("fprint", "ty_fprint", "void", vec!["i32", "i8*"]),
            ("fprintln", "ty_fprintln", "void", vec!["i32", "i8*"]),
            ("fprintf", "ty_fprintf", "void", vec!["i32", "i8*"]),
            ("sprint", "ty_sprint", "void", vec!["%struct.Buf*", "i8*"]),
            (
                "sprintln",
                "ty_sprintln",
                "void",
                vec!["%struct.Buf*", "i8*"],
            ),
            ("sprintf", "ty_sprintf", "void", vec!["%struct.Buf*", "i8*"]),
            ("scan", "ty_scan", "i8*", vec![]),
            ("scanf", "ty_scanf", "i32", vec!["i8*"]),
            ("fscan", "ty_fscan", "i8*", vec!["i32"]),
            ("fscanf", "ty_fscanf", "i32", vec!["i32", "i8*"]),
            ("sscan", "ty_sscan", "i8*", vec!["i8*", "i8**"]),
            ("sscanf", "ty_sscanf", "i32", vec!["i8*", "i8*"]),
        ] {
            let sig = (
                ret.to_string(),
                params.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            );
            self.func_sigs.insert(src.to_string(), sig.clone());
            self.func_sigs.insert(rt.to_string(), sig);
        }

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
                    // Every function (including main, now __ty_main_body) takes task.
                    param_types.insert(0, "i8*".to_string());
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
        self.entry_allocas.clear();
        self.locals.clear();
        self.locals_type.clear();
        self.next_tmp = 0; // Reset temporary counter for each function
        self.current_fn_ret_ty = ret_ty.to_string();
        self.current_fn_name = Some(name.name.clone());
        self.emit("entry:".to_string());
        // All functions (including former-main, now __ty_main_body) get a task param.
        self.emit_function_param("task".to_string(), "i8*".to_string());
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

        self.finish_function_ir()
    }

    // ── Bootstrap helpers ─────────────────────────────────────────────────────

    /// Emit the user's `main` body as a void coroutine named `__ty_main_body`.
    /// It receives `(task: i8*, arg: i8*)` like every other spawned trampoline,
    /// ignores `arg`, and returns void.  Scheduler init/shutdown are NOT emitted
    /// here — the thin bootstrap `main()` owns those.
    fn emit_main_body(&mut self, params: &[Parameter], body: &Block) -> String {
        self.lines.clear();
        self.entry_allocas.clear();
        self.locals.clear();
        self.locals_type.clear();
        self.next_tmp = 0;
        self.current_fn_ret_ty = "void".to_string();
        self.current_fn_name = Some("__ty_main_body".to_string());

        self.emit("entry:".to_string());
        // Bind task and arg params (arg is unused but must be accepted).
        self.emit_function_param("task".to_string(), "i8*".to_string());
        self.emit_function_param("arg".to_string(), "i8*".to_string());

        // User params (main normally has none, but handle them anyway).
        for param in params {
            let ty = self.lower_type(&param.type_annotation);
            let slot = self.tmp();
            self.emit_alloca(&slot, &ty);
            if ty == "%struct.Network*" {
                let net_val = self.tmp();
                self.emit(format!(
                    "  {} = call %struct.Network* @ty_net_global()",
                    net_val
                ));
                self.emit(format!("  store {} {}, {}* {}", ty, net_val, ty, slot));
            } else {
                let z = self.zero_value(&ty);
                self.emit(format!("  store {} {}, {}* {}", ty, z, ty, slot));
            }
            self.locals.insert(param.name.name.clone(), slot);
            self.locals_type.insert(param.name.name.clone(), ty);
        }

        let terminated = self.emit_block_stmts(body, "void");
        if !terminated {
            self.emit("  ret void".to_string());
        }

        self.finish_function_ir()
    }

    /// Emit the thin C-style `main()` that:
    ///   1. initialises the arena, scheduler, and I/O subsystem
    ///   2. spawns `__ty_main_body` as a coroutine (Go-style: main IS a goroutine)
    ///   3. runs the scheduler to completion
    ///   4. tears down I/O and returns 0
    fn emit_bootstrap_main(&mut self) -> String {
        let mut lines: Vec<String> = Vec::new();
        lines.push("entry:".to_string());

        // Arena + scheduler + I/O + Net init
        lines.push("  %arena = call i8* @slab_arena_new()".to_string());
        lines.push("  call void @ty_sched_init()".to_string());
        lines.push("  call void @ty_io_subsystem_init()".to_string());
        lines.push("  call void @ty_net_init()".to_string());

        // Cast __ty_main_body to i8* function pointer and spawn it
        lines.push("  %main_fn = bitcast void(i8*, i8*)* @__ty_main_body to i8*".to_string());
        lines.push("  call i8* @ty_spawn(i8* %arena, i8* %main_fn, i8* null)".to_string());

        // Run scheduler until all coroutines finish
        lines.push("  call void @ty_sched_run()".to_string());
        lines.push("  call void @ty_sched_shutdown()".to_string());
        lines.push("  call void @ty_net_shutdown()".to_string());
        lines.push("  call void @ty_io_subsystem_shutdown()".to_string());
        lines.push("  ret i32 0".to_string());

        lines.join("\n")
    }

    fn emit_function_param(&mut self, name: String, lower_type: String) {
        let slot = self.tmp();
        self.emit_alloca(&slot, &lower_type);
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
                // __ty_main_body is a void coroutine — drop the return value
                // (side-effects are already computed) and just return void.
                // The bootstrap main() owns shutdown.
                if self.current_fn_name.as_deref() == Some("__ty_main_body") {
                    self.emit("  ret void".to_string());
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
                // ── Collect captured variables ─────────────────────────────
                // Filter out `task` and `arg` (hidden params, not user vars).
                // Collect (name, slot, llvm_type) while locals maps are still
                // fully populated — before any state save that would empty them.
                let captured_names = self.collect_captured_vars(body);
                let captured: Vec<(String, String, String, bool)> = captured_names
                    .iter()
                    .filter(|n| *n != "task" && *n != "arg")
                    .filter_map(|name| {
                        let slot = self.locals.get(name)?.clone();
                        let ty = self
                            .locals_type
                            .get(name)
                            .cloned()
                            .unwrap_or_else(|| "i32".to_string());
                        let is_mutable = self.mutable_vars.contains(name);
                        Some((name.clone(), slot, ty, is_mutable))
                    })
                    .collect();

                let tramp_name = format!("__ty_conc_{}", self.label("tramp"));

                if captured.is_empty() {
                    // ── No-capture path ────────────────────────────────────
                    let saved_lines = std::mem::take(&mut self.lines);
                    let saved_entry_allocas = std::mem::take(&mut self.entry_allocas);
                    let saved_fn_name = self.current_fn_name.clone();
                    let saved_ret_ty = self.current_fn_ret_ty.clone();
                    let saved_locals = std::mem::take(&mut self.locals);
                    let saved_types = std::mem::take(&mut self.locals_type);
                    let saved_mutable_vars = std::mem::take(&mut self.mutable_vars);

                    self.current_fn_ret_ty = "void".to_string();
                    self.current_fn_name = Some(tramp_name.clone());
                    self.emit("entry:".to_string());
                    self.emit_function_param("task".to_string(), "i8*".to_string());
                    self.emit_function_param("arg".to_string(), "i8*".to_string());

                    // Even in the "no-capture" path the body may reference
                    // variables from the enclosing scope by name (e.g. a
                    // socket pointer passed into a spawned block).  Without
                    // restoring the parent locals those identifiers fall
                    // through to the "undefined identifier → 0" branch,
                    // producing a null pointer that is immediately
                    // dereferenced → segfault.
                    //
                    // We copy the parent locals into the trampoline scope,
                    // excluding the hidden `task` and `arg` params that the
                    // trampoline re-binds via emit_function_param above.
                    for (name, slot) in &saved_locals {
                        if name != "task" && name != "arg" {
                            self.locals.insert(name.clone(), slot.clone());
                        }
                    }
                    for (name, ty) in &saved_types {
                        if name != "task" && name != "arg" {
                            self.locals_type.insert(name.clone(), ty.clone());
                        }
                    }

                    self.emit_block_stmts(body, "void");
                    self.emit("  ret void".to_string());

                    let tramp_ir = IrFunction {
                        name: tramp_name.clone(),
                        body: self.finish_function_ir(),
                        ret_type: "void".to_string(),
                        params: vec![
                            ("task".to_string(), "i8*".to_string()),
                            ("arg".to_string(), "i8*".to_string()),
                        ],
                    };

                    self.lines = saved_lines;
                    self.entry_allocas = saved_entry_allocas;
                    self.locals = saved_locals;
                    self.locals_type = saved_types;
                    self.mutable_vars = saved_mutable_vars;
                    self.current_fn_name = saved_fn_name;
                    self.current_fn_ret_ty = saved_ret_ty;

                    // Spawn task for this concurrent block.
                    let fn_cast = self.tmp();
                    self.emit(format!(
                        "  {} = bitcast void(i8*, i8*)* @{} to i8*",
                        fn_cast, tramp_name
                    ));
                    {
                        let _tv = self.emit_task_load();
                        self.emit(format!(
                            "  call i8* @ty_spawn(i8* {}, i8* {}, i8* null)",
                            _tv, fn_cast
                        ));
                    }

                    self.conc_functions.push(tramp_ir);
                } else {
                    // ── Closure path ───────────────────────────────────────
                    //
                    // Memory model (Option B):
                    //   1. Parent allocates closure struct from its own arena
                    //      via slab_alloc(task, class).
                    //   2. Parent passes the raw i8* pointer as the `arg`
                    //      parameter to ty_spawn.
                    //   3. Trampoline bitcasts arg → closure*, unpacks fields
                    //      into local alloca slots, runs the body, then calls
                    //      slab_free(task, arg, class) using its OWN `%task`
                    //      (the spawned coroutine's arena).
                    //   4. Because the spawned coroutine's arena is released
                    //      in bulk when it exits (slab_arena_free), the
                    //      slab_free is belt-and-suspenders — it recycles the
                    //      slot back into the per-class free list immediately
                    //      so it can be reused within the same coroutine's
                    //      lifetime, rather than waiting for bulk teardown.
                    //
                    // The closure is allocated from the PARENT's arena.  The
                    // trampoline receives a raw pointer to it and frees it
                    // through its own arena.  This is safe because the spawned
                    // coroutine's arena was created from the same underlying
                    // virtual memory region — slab_free does nothing more than
                    // push the pointer onto a free list; it never munmaps.
                    // The actual release happens at arena_free time.

                    // ── 1. Build closure struct type ───────────────────────
                    // Done here, while locals_type is still fully populated.
                    let closure_ty = format!("%closure.{}", tramp_name);

                    // Build closure field types: mutable non-heap vars → pointers
                    let closure_field_tys: Vec<String> = captured
                        .iter()
                        .map(|(_name, _, ty, is_mutable)| {
                            if *is_mutable && !ty.ends_with('*') {
                                format!("{}*", ty)
                            } else {
                                ty.clone()
                            }
                        })
                        .collect();

                    // Compute exact packed size for the size-class selection.
                    let closure_size: i64 = closure_field_tys
                        .iter()
                        .map(|ty| self.llvm_const_sizeof(ty))
                        .sum();
                    let class_id = get_size_class(closure_size);

                    // Emit struct type declaration into preamble.
                    self.type_decls.push(format!(
                        "{} = type {{ {} }}",
                        closure_ty,
                        closure_field_tys.join(", ")
                    ));

                    // ── 2. Allocate & populate closure in parent ───────────
                    let closure_raw = self.tmp();
                    {
                        let _tv = self.emit_task_load();
                        self.emit(format!(
                            "  {} = call i8* @slab_alloc(i8* {}, i32 {})",
                            closure_raw, _tv, class_id
                        ));
                    }
                    let closure_typed = self.tmp();
                    self.emit(format!(
                        "  {} = bitcast i8* {} to {}*",
                        closure_typed, closure_raw, closure_ty
                    ));

                    // In the parent — populate closure fields
                    for (idx, (_name, slot, ty, is_mutable_var)) in captured.iter().enumerate() {
                        let gep = self.tmp();

                        let _field_ty = if *is_mutable_var && !ty.ends_with('*') {
                            format!("{}*", ty)
                        } else {
                            ty.clone()
                        };

                        self.emit(format!(
                            "  {} = getelementptr inbounds {}, {}* {}, i32 0, i32 {}",
                            gep, closure_ty, closure_ty, closure_typed, idx
                        ));

                        if *is_mutable_var && !ty.ends_with('*') {
                            // Stack-allocated mutable (non-pointer): capture pointer to enable updates.
                            // For heap-allocated mutables, `slot` already is a typed pointer, and this
                            // still does the right thing (capture pointer by value).
                            self.emit(format!("  store {}* {}, {}** {}", ty, slot, ty, gep));
                        } else {
                            // Capture by value (including pointer-typed values like `i8*`):
                            // locals store addresses (alloca slots) for non-mutable values.
                            let loaded = self.tmp();
                            self.emit(format!("  {} = load {}, {}* {}", loaded, ty, ty, slot));
                            self.emit(format!("  store {} {}, {}* {}", ty, loaded, ty, gep));
                        }
                    }

                    // ── 3. Emit trampoline ─────────────────────────────────
                    let saved_lines = std::mem::take(&mut self.lines);
                    let saved_entry_allocas = std::mem::take(&mut self.entry_allocas);
                    let saved_fn_name = self.current_fn_name.clone();
                    let saved_ret_ty = self.current_fn_ret_ty.clone();
                    let saved_locals = std::mem::take(&mut self.locals);
                    let saved_types = std::mem::take(&mut self.locals_type);
                    let saved_mutable_vars = std::mem::take(&mut self.mutable_vars);

                    self.current_fn_ret_ty = "void".to_string();
                    self.current_fn_name = Some(tramp_name.clone());
                    self.emit("entry:".to_string());
                    self.emit_function_param("task".to_string(), "i8*".to_string());
                    self.emit_function_param("arg".to_string(), "i8*".to_string());

                    // Load arg from its alloca slot (emit_function_param stored it there)
                    let arg_slot = self.locals.get("arg").cloned().unwrap();
                    let arg_i8 = self.tmp();
                    self.emit(format!("  {} = load i8*, i8** {}", arg_i8, arg_slot));
                    // Bitcast to typed closure pointer
                    let cl = self.tmp();
                    self.emit(format!(
                        "  {} = bitcast i8* {} to {}*",
                        cl, arg_i8, closure_ty
                    ));

                    // Unpack each field into closure field
                    for (idx, (name, _, ty, is_mutable_var)) in captured.iter().enumerate() {
                        let gep = self.tmp();

                        let field_ty = if *is_mutable_var && !ty.ends_with('*') {
                            format!("{}*", ty)
                        } else {
                            ty.clone()
                        };

                        self.emit(format!(
                            "  {} = getelementptr inbounds {}, {}* {}, i32 0, i32 {}",
                            gep, closure_ty, closure_ty, cl, idx
                        ));
                        let loaded = self.tmp();
                        self.emit(format!(
                            "  {} = load {}, {}* {}",
                            loaded, field_ty, field_ty, gep
                        ));

                        if *is_mutable_var && !ty.ends_with('*') {
                            // Pointer to stack var: register directly as the pointer to the actual location
                            self.locals.insert(name.clone(), loaded.clone());
                            self.locals_type.insert(name.clone(), ty.clone());
                        } else {
                            // Value (including pointer values): copy into a fresh alloca
                            let slot = self.tmp();
                            self.emit_alloca(&slot, &ty);
                            self.emit(format!("  store {} {}, {}* {}", ty, loaded, ty, slot));
                            self.locals.insert(name.clone(), slot);
                            self.locals_type.insert(name.clone(), ty.clone());
                        }
                    }

                    // Emit body
                    self.emit_block_stmts(body, "void");

                    // NOTE: do not free closure here. `slab_alloc` uses per-arena free lists,
                    // and the spawned coroutine's arena may differ from the parent's arena that
                    // allocated this closure. Freeing into wrong arena can corrupt allocator state.

                    self.emit("  ret void".to_string());

                    let tramp_ir = IrFunction {
                        name: tramp_name.clone(),
                        body: self.finish_function_ir(),
                        ret_type: "void".to_string(),
                        params: vec![
                            ("task".to_string(), "i8*".to_string()),
                            ("arg".to_string(), "i8*".to_string()),
                        ],
                    };

                    self.lines = saved_lines;
                    self.entry_allocas = saved_entry_allocas;
                    self.locals = saved_locals;
                    self.locals_type = saved_types;
                    self.mutable_vars = saved_mutable_vars;
                    self.current_fn_name = saved_fn_name;
                    self.current_fn_ret_ty = saved_ret_ty;

                    // ── 4. Spawn task with closure ─────────────────────────
                    let fn_cast = self.tmp();
                    self.emit(format!(
                        "  {} = bitcast void(i8*, i8*)* @{} to i8*",
                        fn_cast, tramp_name
                    ));
                    {
                        let _tv = self.emit_task_load();
                        self.emit(format!(
                            "  call i8* @ty_spawn(i8* {}, i8* {}, i8* {})",
                            _tv, fn_cast, closure_raw
                        ));
                    }

                    self.conc_functions.push(tramp_ir);
                }
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
        {
            let _tv = self.emit_task_load();
            self.emit(format!(
                "  call void @slab_free(i8* {}, i8* {}, i32 {})",
                _tv, raw, class_id
            ));
        }
    }

    fn emit_let(
        &mut self,
        name: &Identifier,
        initializer: &Expression,
        type_annotation: Option<&Type>,
        mutable: bool,
    ) {
        // Track mutable variables for closure capture purposes
        if mutable {
            self.mutable_vars.insert(name.name.clone());
        }

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
                {
                    let _tv = self.emit_task_load();
                    self.emit(format!("  {} = call %struct.TyArray* @ty_array_from_fixed(i8* {}, i8* {}, i64 {}, i64 {}, i64 {})", out, _tv, raw, elems.len(), sz, al));
                }

                let slot = self.tmp();
                self.emit_alloca(&slot, "%struct.TyArray*");
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
        // Special-case chan constructor so we can use the annotated element type to size the channel.
        if let ExpressionKind::Call { func, args } = &initializer.node {
            if args.is_empty() {
                if let ExpressionKind::Identifier(id) = &func.node {
                    if id.name == "chan" {
                        let elem_ty = type_annotation
                            .and_then(|t| Self::chan_elem_type_from_annotation(t))
                            .map(|t| self.lower_type(t))
                            .unwrap_or_else(|| "i8".to_string());
                        let elem_size = self.llvm_const_sizeof(&elem_ty);
                        let chan_ptr = self.tmp();
                        // Default to a small buffered channel to avoid deadlocks when producers run
                        // synchronously (e.g. when `conc` is lowered without spawning).
                        self.emit(format!(
                            "  {} = call i8* @ty_chan_new(i64 {}, i64 64)",
                            chan_ptr, elem_size
                        ));

                        let ty = type_annotation
                            .map(|t| self.lower_type(t))
                            .unwrap_or_else(|| "i8*".to_string());
                        let slot = self.tmp();
                        self.emit_alloca(&slot, &ty);
                        self.emit(format!("  store {} {}, {}* {}", ty, chan_ptr, ty, slot));
                        self.locals.insert(name.name.clone(), slot);
                        self.locals_type.insert(name.name.clone(), ty);
                        return;
                    }
                }
            }
        }

        let value = self.emit_expr(initializer);
        let init_ty = self.expr_llvm_type(initializer);
        let ty = type_annotation
            .map(|t| self.lower_type(t))
            .unwrap_or_else(|| init_ty.clone());
        let value = self.emit_widen(&value, &init_ty, &ty);

        let is_heap_allocated = !ty.ends_with('*') && ty != "void";

        if is_heap_allocated {
            // Implement Slab Allocation Logic
            let size = self.llvm_const_sizeof(&ty);
            let class_id = get_size_class(size);

            let raw_ptr = self.tmp();
            // %task is passed as a hidden first argument to the function
            {
                let _tv = self.emit_task_load();
                self.emit(format!(
                    "  {} = call i8* @slab_alloc(i8* {}, i32 {})",
                    raw_ptr, _tv, class_id
                ));
            }

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
            self.emit_alloca(&slot, &ty);
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
                self.emit_alloca(&idx_slot, "i64");
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
                {
                    let _tv = self.emit_task_load();
                    self.emit(format!("  {} = call %struct.TyArray* @ty_array_from_fixed(i8* {}, i8* {}, i64 {}, i64 {}, i64 {})", ty_array_ptr, _tv, raw_ptr_i8, elems.len(), elem_size, align));
                }

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
                } else {
                    // For now, return 0 for any undefined identifier
                    // This includes captured variables and undefined references
                    if !id.name.is_empty() && id.name.chars().next().unwrap().is_alphabetic() {
                        self.emit(format!("  ; undefined identifier: {}", id.name));
                        "0".to_string()
                    } else {
                        id.name.clone()
                    }
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
            ExpressionKind::UnaryOp { op, expr: inner } => {
                let v = self.emit_expr(inner);
                let ty = self.expr_llvm_type(inner);
                match op {
                    Operator::Not => {
                        let tmp = self.tmp();
                        if ty == "i1" {
                            self.emit(format!("  {} = xor i1 {}, 1", tmp, v));
                        } else {
                            // Fallback: treat as int-like; compare to 0.
                            self.emit(format!("  {} = icmp eq {} {}, 0", tmp, ty, v));
                        }
                        tmp
                    }
                    Operator::Sub => {
                        let tmp = self.tmp();
                        if matches!(ty.as_str(), "half" | "float" | "double") {
                            self.emit(format!("  {} = fsub {} 0.0, {}", tmp, ty, v));
                        } else {
                            self.emit(format!("  {} = sub {} 0, {}", tmp, ty, v));
                        }
                        tmp
                    }
                    _ => "0".to_string(),
                }
            }
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
                let slot = self.locals.get(&id.name).cloned().unwrap_or_else(|| {
                    // If the variable is not found, emit a comment and use a placeholder
                    self.emit(format!("  ; undefined lvalue: {}", id.name));
                    format!("null ; UNDEFINED")
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
                    let _tv = self.emit_task_load();
                    arg_pairs.push(format!("i8* {}", _tv));
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

            // Channel methods (lowered to runtime calls). Channels currently lower to `i8*`.
            if base_ty == "i8*" {
                match field.name.as_str() {
                    "send" => {
                        if let Some(arg0) = args.first() {
                            let val = self.emit_expr(arg0);
                            let val_ty = self.expr_llvm_type(arg0);
                            let slot = self.tmp();
                            self.emit_alloca(&slot, &val_ty);
                            self.emit(format!("  store {} {}, {}* {}", val_ty, val, val_ty, slot));
                            let raw = self.tmp();
                            self.emit(format!("  {} = bitcast {}* {} to i8*", raw, val_ty, slot));
                            {
                                let _tv = self.emit_task_load();
                                self.emit(format!(
                                    "  call void @ty_chan_send(i8* {}, i8* {}, i8* {})",
                                    _tv, base_val, raw
                                ));
                            }
                        }
                        return "0".to_string();
                    }
                    "recv" => {
                        if let Some(ty) = self.inferred_expr_type(call_expr).cloned() {
                            let elem_ty = self.lower_infer_type(&ty);
                            self.ensure_option(&elem_ty);

                            let out_slot = self.tmp();
                            self.emit_alloca(&out_slot, &elem_ty);
                            let out_raw = self.tmp();
                            self.emit(format!(
                                "  {} = bitcast {}* {} to i8*",
                                out_raw, elem_ty, out_slot
                            ));
                            {
                                let _tv = self.emit_task_load();
                                self.emit(format!(
                                    "  call void @ty_chan_recv(i8* {}, i8* {}, i8* {})",
                                    _tv, base_val, out_raw
                                ));
                            }
                            let loaded = self.tmp();
                            self.emit(format!(
                                "  {} = load {}, {}* {}",
                                loaded, elem_ty, elem_ty, out_slot
                            ));
                            return loaded;
                        }
                        return "0".to_string();
                    }
                    "try_recv" => {
                        if let Some(ty) = self.inferred_expr_type(call_expr).cloned() {
                            if let InferType::App(ref name, ref ty_args) = ty {
                                if name == "Option" && ty_args.len() == 1 {
                                    let elem_ty = self.lower_infer_type(&ty_args[0]);
                                    let opt_ty = self.lower_infer_type(&ty);
                                    self.ensure_option(&elem_ty);

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
                                    {
                                        let _tv = self.emit_task_load();
                                        self.emit(format!("  {} = call i32 @ty_chan_try_recv(i8* {}, i8* {}, i8* {})", success, _tv, base_val, out_raw));
                                    }

                                    let got_value = self.tmp();
                                    self.emit(format!(
                                        "  {} = icmp eq i32 {}, 1",
                                        got_value, success
                                    ));
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
                                    let some_val =
                                        self.emit_option_some(&opt_ty, &elem_ty, &loaded);
                                    self.emit(format!("  br label %{}", merge_lbl));

                                    self.emit(format!("{}:", empty_lbl));
                                    let is_closed = self.tmp();
                                    self.emit(format!(
                                        "  {} = icmp slt i32 {}, 0",
                                        is_closed, success
                                    ));
                                    self.emit(format!(
                                        "  br i1 {}, label %{}, label %{}",
                                        is_closed, none_lbl, wait_lbl
                                    ));

                                    self.emit(format!("{}:", wait_lbl));
                                    self.emit("  call void @ty_yield()".to_string());
                                    self.emit(format!("  br label %{}", poll_lbl));

                                    self.emit(format!("{}:", none_lbl));
                                    let none_val = self.emit_option_none(&opt_ty, &elem_ty);
                                    self.emit(format!("  br label %{}", merge_lbl));

                                    self.emit(format!("{}:", merge_lbl));
                                    let phi = self.tmp();
                                    self.emit(format!(
                                        "  {} = phi {} [ {}, %{} ], [ {}, %{} ]",
                                        phi, opt_ty, some_val, some_lbl, none_val, none_lbl
                                    ));
                                    return phi;
                                }
                            }
                        }
                        return "0".to_string();
                    }
                    _ => {}
                }
            }

            // Array push
            if base_ty == "%struct.TyArray*" && field.name == "push" {
                if let Some(arg0) = args.first() {
                    let val = self.emit_expr(arg0);
                    let val_ty = self.expr_llvm_type(arg0);
                    let slot = self.tmp();
                    self.emit_alloca(&slot, &val_ty);
                    self.emit(format!("  store {} {}, {}* {}", val_ty, val, val_ty, slot));
                    let raw = self.tmp();
                    self.emit(format!("  {} = bitcast {}* {} to i8*", raw, val_ty, slot));
                    {
                        let _tv = self.emit_task_load();
                        self.emit(format!(
                            "  call void @ty_array_push(i8* {}, %struct.TyArray* {}, i8* {})",
                            _tv, base_val, raw
                        ));
                    }
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

                let _tv = self.emit_task_load();
                let mut arg_pairs =
                    vec![format!("i8* {}", _tv), format!("{} {}", self_ty, base_val)];
                for (i, a) in args.iter().enumerate() {
                    let v = self.emit_expr(a);
                    let actual_ty = self.expr_llvm_type(a);
                    let t = param_types
                        .get(i + 2) // ← was i + 1, now offset by 2 (skip task + self)
                        .cloned()
                        .unwrap_or_else(|| "i32".to_string());
                    let v = self.emit_widen(&v, &actual_ty, &t);
                    arg_pairs.push(format!("{} {}", t, v));
                }
                let tmp = self.tmp();
                if ret_ty == "void" {
                    // Check whether the last declared parameter is a Result/struct out-pointer.
                    // If so, allocate it on the stack, pass the pointer, load and return the struct.
                    // This covers __ty_method__Network__listen, __ty_method__Listener__accept, and
                    // any future runtime methods that use the out-pointer ABI.
                    let last_param = param_types.last().cloned().unwrap_or_default();
                    let out_struct_ty = last_param
                        .strip_suffix('*')
                        .filter(|t| t.starts_with("%struct."))
                        .map(|t| t.to_string());

                    if let Some(desired_ty) = out_struct_ty {
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
            // Channel construction: chan<T>()
            if id.name == "chan" {
                // Derive element type from inference: chan() returns Ref<Chan<T>>.
                let mut elem_llvm_ty = "i8".to_string();
                if let Some(infer) = self.inferred_expr_type(call_expr).cloned() {
                    let inner = match infer {
                        InferType::App(n, mut a) if n == "Ref" && a.len() == 1 => a.remove(0),
                        other => other,
                    };
                    if let InferType::App(n, a) = inner {
                        if n == "Chan" && a.len() == 1 {
                            elem_llvm_ty = self.lower_infer_type(&a[0]);
                        }
                    }
                }
                let elem_size = self.llvm_const_sizeof(&elem_llvm_ty);
                let tmp = self.tmp();
                // Default to a small buffered channel to avoid deadlocks when producers run
                // synchronously (e.g. when `conc` is lowered without spawning).
                self.emit(format!(
                    "  {} = call i8* @ty_chan_new(i64 {}, i64 64)",
                    tmp, elem_size
                ));
                return tmp;
            }
            let runtime_name =
                runtime_intrinsic_name(&id.name).unwrap_or_else(|| link_symbol_name(&id.name));
            let (ret_ty, mut param_types) = self
                .func_sigs
                .get(&id.name)
                .cloned()
                .unwrap_or_else(|| ("i32".to_string(), vec![]));

            // Builtin printf/fprintf/sprintf funcs: first arg is format string (i8*)
            if param_types.is_empty()
                && matches!(id.name.as_str(), "printf" | "fprintf" | "sprintf")
            {
                param_types = vec!["i8*".to_string()];
            }

            let tail = if self.current_fn_name.as_deref() == Some(id.name.as_str()) {
                "tail "
            } else {
                ""
            };

            let mut arg_pairs = Vec::new();
            let task_prepended = !is_no_task_intrinsic(&runtime_name);
            if task_prepended {
                let _tv = self.emit_task_load();
                arg_pairs.push(format!("i8* {}", _tv));
            }
            // param_types[0] is the task slot (i8*); user args start at index 1
            // when task was prepended, so offset accordingly.
            let param_offset = if task_prepended { 1 } else { 0 };
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
        // The match expression's type is the arm-body type, not the scrutinee type.
        // This function is also reused for `match` statements (all arms are `void`).
        let mut result_ty = "void".to_string();
        for arm in arms {
            let ty = self.expr_llvm_type(&arm.node.body);
            if ty != "void" {
                result_ty = ty;
                break;
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
            PatternKind::Wildcard => "1".to_string(),
            PatternKind::Identifier(id) => {
                // If identifier already bound in scope, treat as value pattern (equality test).
                // Else treat as binder pattern (always matches).
                if let Some(slot) = self.locals.get(&id.name).cloned() {
                    let pat_ty = self
                        .locals_type
                        .get(&id.name)
                        .cloned()
                        .unwrap_or_else(|| ty.clone());
                    let loaded = self.tmp();
                    self.emit(format!(
                        "  {} = load {}, {}* {}",
                        loaded, pat_ty, pat_ty, slot
                    ));
                    let cmp = self.tmp();
                    self.emit(format!(
                        "  {} = icmp eq {} {}, {}",
                        cmp, ty, scrutinee_val, loaded
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
                // Value pattern: already bound, do not rebind/shadow.
                if self.locals.contains_key(&id.name) {
                    return;
                }
                let slot = self.tmp();
                self.emit_alloca(&slot, &ty);
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
            // `ref T` / `&T` parse to the canonical "Ref" type in the AST.
            // We currently lower Ref as an opaque runtime pointer.
            "Str" | "ref" | "Ref" => "i8*".to_string(),
            "Buf" => "%struct.Buf*".to_string(),
            "Array" => "%struct.TyArray*".to_string(),
            "Chan" => "i8*".to_string(),
            "Network" => "%struct.Network*".to_string(),
            "Listener" => "%struct.Listener*".to_string(),
            "Socket" => "%struct.Socket*".to_string(),
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
                "Chan" => "i8*".to_string(),
                "Network" => "%struct.Network*".to_string(),
                "Listener" => "%struct.Listener*".to_string(),
                "Socket" => "%struct.Socket*".to_string(),
                n => format!("%struct.{}", n),
            },
            InferType::App(name, args) if name == "Ref" && args.len() == 1 => "i8*".to_string(),
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
            InferType::App(name, _) if name == "Chan" => "i8*".to_string(),
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
                    // Handle builtin chan constructor
                    if id.name == "chan" {
                        return "i8*".to_string();
                    }
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

    fn chan_elem_type_from_annotation(ty: &Type) -> Option<&Type> {
        match ty.node.name.as_str() {
            "Ref" if ty.node.generic_args.len() == 1 => {
                Self::chan_elem_type_from_annotation(&ty.node.generic_args[0])
            }
            "Chan" if ty.node.generic_args.len() == 1 => Some(&ty.node.generic_args[0]),
            _ => None,
        }
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

    fn emit_widen(&mut self, val: &str, actual_ty: &str, expected_ty: &str) -> String {
        if actual_ty == expected_ty {
            return val.to_string();
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
            "  {} = getelementptr inbounds [{} x i8], [{} x i8]* {}, i32 0, i32 0",
            tmp, n, n, global
        ));
        tmp
    }

    /// Scan a block and collect all identifiers that are referenced but not defined locally.
    /// These are potential captured variables.
    fn collect_captured_vars(&self, block: &Block) -> Vec<String> {
        let mut captured = Vec::new();
        let mut defined = std::collections::HashSet::new();

        // Visit each statement in order, adding let-bound names to `defined`
        // AFTER visiting the initializer — so `let x = x + 1` correctly
        // captures the outer `x` from the RHS before shadowing it.
        for stmt in &block.statements {
            {
                let captured_ref = &mut captured;
                let defined_ref = &defined;
                self.visit_statement_for_identifiers(stmt, &mut |expr: &Expression| {
                    if let ExpressionKind::Identifier(id) = &expr.node {
                        if !defined_ref.contains(&id.name) && !captured_ref.contains(&id.name) {
                            captured_ref.push(id.name.clone());
                        }
                    }
                });
            }
            // Mark name as locally defined only after visiting initializer
            if let StatementKind::LetBinding { name, .. } = &stmt.node {
                defined.insert(name.name.clone());
            }
        }

        // Visit trailing expression with all let bindings now in scope
        if let Some(expr) = &block.trailing_expression {
            let captured_ref = &mut captured;
            let defined_ref = &defined;
            self.visit_expr_for_identifiers(expr, &mut |expr: &Expression| {
                if let ExpressionKind::Identifier(id) = &expr.node {
                    if !defined_ref.contains(&id.name) && !captured_ref.contains(&id.name) {
                        captured_ref.push(id.name.clone());
                    }
                }
            });
        }

        captured
    }

    fn visit_statement_for_identifiers(
        &self,
        stmt: &Statement,
        visitor: &mut dyn FnMut(&Expression),
    ) {
        match &stmt.node {
            StatementKind::LetBinding { initializer, .. } => {
                self.visit_expr_for_identifiers(initializer, visitor);
            }
            StatementKind::Expression(expr) => {
                self.visit_expr_for_identifiers(expr, visitor);
            }
            StatementKind::Return(Some(expr)) => {
                self.visit_expr_for_identifiers(expr, visitor);
            }
            StatementKind::Match { expr, arms } => {
                self.visit_expr_for_identifiers(expr, visitor);
                for arm in arms {
                    if let Some(g) = &arm.node.guard {
                        self.visit_expr_for_identifiers(g, visitor);
                    }
                    self.visit_expr_for_identifiers(&arm.node.body, visitor);
                }
            }
            StatementKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.visit_expr_for_identifiers(condition, visitor);
                for s in &then_branch.statements {
                    self.visit_statement_for_identifiers(s, visitor);
                }
                if let Some(eb) = else_branch {
                    match &eb.node {
                        ElseBranchKind::Block(b) => {
                            for s in &b.statements {
                                self.visit_statement_for_identifiers(s, visitor);
                            }
                        }
                        ElseBranchKind::If(stmt) => {
                            self.visit_statement_for_identifiers(stmt, visitor);
                        }
                    }
                }
            }
            StatementKind::Loop { body, .. } => {
                for s in &body.statements {
                    self.visit_statement_for_identifiers(s, visitor);
                }
            }
            StatementKind::Conc { body } => {
                for s in &body.statements {
                    self.visit_statement_for_identifiers(s, visitor);
                }
            }
            _ => {}
        }
    }

    fn visit_expr_for_identifiers(&self, expr: &Expression, visitor: &mut dyn FnMut(&Expression)) {
        visitor(expr);

        match &expr.node {
            ExpressionKind::BinaryOp { left, right, .. } => {
                self.visit_expr_for_identifiers(left, visitor);
                self.visit_expr_for_identifiers(right, visitor);
            }
            ExpressionKind::UnaryOp { expr, .. } => {
                self.visit_expr_for_identifiers(expr, visitor);
            }
            ExpressionKind::Call { func, args } => {
                self.visit_expr_for_identifiers(func, visitor);
                for arg in args {
                    self.visit_expr_for_identifiers(arg, visitor);
                }
            }
            ExpressionKind::FieldAccess { base, .. } => {
                self.visit_expr_for_identifiers(base, visitor);
            }
            ExpressionKind::IndexAccess { base, index } => {
                self.visit_expr_for_identifiers(base, visitor);
                self.visit_expr_for_identifiers(index, visitor);
            }
            ExpressionKind::StructInit { fields, .. } => {
                for (_, e) in fields {
                    self.visit_expr_for_identifiers(e, visitor);
                }
            }
            ExpressionKind::MergeExpression { base, fields } => {
                if let Some(b) = base {
                    self.visit_expr_for_identifiers(b, visitor);
                }
                for (_, e) in fields {
                    self.visit_expr_for_identifiers(e, visitor);
                }
            }
            ExpressionKind::Match { expr, arms } => {
                self.visit_expr_for_identifiers(expr, visitor);
                for arm in arms {
                    if let Some(g) = &arm.node.guard {
                        self.visit_expr_for_identifiers(g, visitor);
                    }
                    self.visit_expr_for_identifiers(&arm.node.body, visitor);
                }
            }
            ExpressionKind::IfLet {
                expr,
                then,
                else_branch,
                ..
            } => {
                self.visit_expr_for_identifiers(expr, visitor);
                for s in &then.statements {
                    self.visit_statement_for_identifiers(s, visitor);
                }
                if let Some(t) = &then.trailing_expression {
                    self.visit_expr_for_identifiers(t, visitor);
                }
                if let Some(e) = else_branch {
                    self.visit_expr_for_identifiers(e, visitor);
                }
            }
            ExpressionKind::TryOperator { expr } => {
                self.visit_expr_for_identifiers(expr, visitor);
            }
            ExpressionKind::Pipe { left, right } => {
                self.visit_expr_for_identifiers(left, visitor);
                self.visit_expr_for_identifiers(right, visitor);
            }
            ExpressionKind::Block(b) => {
                for s in &b.statements {
                    self.visit_statement_for_identifiers(s, visitor);
                }
                if let Some(e) = &b.trailing_expression {
                    self.visit_expr_for_identifiers(e, visitor);
                }
            }
            _ => {}
        }
    }
}

// ── Free functions ────────────────────────────────────────────────────────────

fn is_main(name: &str) -> bool {
    // The user's entry point is usually named `main`.
    // In a namespace, it becomes `ns__main`.
    name == "main" || name.ends_with("__main")
}

fn int_suffix_to_llvm(suffix: &str) -> &'static str {
    match suffix {
        "i8" | "u8" => "i8",
        "i16" => "i16",
        "i64" => "i64",
        _ => "i32",
    }
}

fn estimate_type_size(ty: &str) -> i64 {
    if ty.starts_with("%struct.") {
        // For structs, use a conservative estimate (8 bytes per field + overhead)
        // This is not exact but good enough for size class estimation
        64
    } else if ty.ends_with('*') {
        8 // pointer
    } else {
        match ty {
            "i1" => 1,
            "i8" => 1,
            "i16" => 2,
            "i32" => 4,
            "i64" => 8,
            "float" => 4,
            "double" => 8,
            _ => 8, // default
        }
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

        // stdio
        "print" => Some("ty_print".to_string()),
        "println" => Some("ty_println".to_string()),
        "printf" => Some("ty_printf".to_string()),
        "fprint" => Some("ty_fprint".to_string()),
        "fprintln" => Some("ty_fprintln".to_string()),
        "fprintf" => Some("ty_fprintf".to_string()),
        "sprint" => Some("ty_sprint".to_string()),
        "sprintln" => Some("ty_sprintln".to_string()),
        "sprintf" => Some("ty_sprintf".to_string()),
        "scan" => Some("ty_scan".to_string()),
        "scanf" => Some("ty_scanf".to_string()),
        "fscan" => Some("ty_fscan".to_string()),
        "fscanf" => Some("ty_fscanf".to_string()),
        "sscan" => Some("ty_sscan".to_string()),
        "sscanf" => Some("ty_sscanf".to_string()),

        _ => None,
    }
}

fn link_symbol_name(name: &str) -> String {
    // Keep a stable OS entrypoint symbol (`main`) and route the user entry
    // function through a distinct symbol to avoid collisions (notably in unit
    // tests that parse a single file without namespace mangling).
    if name == "main" || name == "main__main" {
        "__ty_user_main".to_string()
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
        let text = compile("fn id(a: Int32) -> Int32 { return a; }");
        assert!(text.contains("define i32 @id(i8* %task, i32 %a)"));
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
        assert!(text.contains("call i8* @slab_alloc"));
        assert!(text.contains("store i32 3"));
        assert!(text.contains("load i32"));
    }

    #[test]
    fn try_recv_yields_until_value_or_close() {
        let text = compile(
            "fn main() -> Int32 { let ch: ref chan<Int32> = chan<Int32>(); match ch.try_recv() { Some(v) => { return v; } None => { return 0; } } }",
        );
        assert!(text.contains("try_recv_poll"));
        assert!(text.contains("call void @ty_yield()"));
        assert!(text.contains("icmp slt i32"));
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
    fn lowers_match_block_to_control_flow() {
        let text = compile("namespace main\nfn main(x: Int32) -> Int32 { match x { 0 => { return 1; }, _ => { return 2; } } }");
        assert!(text.contains("br label %match_check"));
        assert!(text.contains("icmp eq i32"));
        assert!(text.contains("match_merge"));
    }

    #[test]
    fn lowers_match_exp_to_control_flow() {
        let text = compile(
            "namespace main\nfn main(x: Int32) -> Int32 { return match x { 0 => 1, _ => 2, } }",
        );
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
