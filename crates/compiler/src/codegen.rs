use crate::ast::*;
use crate::liveness::DropInfo;
use crate::span::Span;
use crate::type_inference::InferType;
use std::collections::{HashMap, HashSet};

// ── Private types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct EnumDef {
    name: String,
    gen_params: Vec<String>,
    variants: Vec<EnumVariantDef>,
}

#[derive(Debug, Clone)]
struct EnumVariantDef {
    name: String,
    payload: Option<EnumVariantPayloadKind>,
}

#[derive(Debug, Clone)]
struct EnumLayout {
    llvm_struct_ty: String,
    tag_ty: String,
    variants: HashMap<String, EnumVariantLayout>,
}

#[derive(Debug, Clone)]
struct EnumVariantLayout {
    tag_value: i64,
    payload_index: Option<usize>,
    payload_ty: Option<String>,
}

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
        let mut out = self.preamble.clone();
        for func in &self.functions {
            let params = func
                .params
                .iter()
                .map(|(n, ty)| format!("{} %{}", ty, n))
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

// ── Saved function context (for nested emission, e.g. conc trampolines) ───────

struct FnContext {
    lines: Vec<String>,
    entry_allocas: Vec<String>,
    fn_name: Option<String>,
    ret_ty: String,
    locals: HashMap<String, String>,
    locals_type: HashMap<String, String>,
    mutable_vars: HashSet<String>,
    next_tmp: usize,
}

// ── Type/preamble registry (split from emit state) ────────────────────────────

struct TypeRegistry {
    type_decls: Vec<String>,
    extra_preamble: Vec<String>,
    struct_fields: HashMap<String, Vec<(String, String)>>,
    func_sigs: HashMap<String, (String, Vec<String>)>,
    extern_fns: HashSet<String>,
    string_pool: HashMap<String, (String, usize)>,
    enum_defs: HashMap<String, EnumDef>,
    enum_layouts: HashMap<String, EnumLayout>,
    /// Tracks every symbol name (type or function) already emitted into the
    /// preamble so that neither `register_runtime_decls` nor
    /// `register_module_sigs` ever emits a duplicate `declare` or `type` line.
    declared_syms: HashSet<String>,
}

impl TypeRegistry {
    fn new() -> Self {
        Self {
            type_decls: Vec::new(),
            extra_preamble: Vec::new(),
            struct_fields: HashMap::new(),
            func_sigs: HashMap::new(),
            extern_fns: HashSet::new(),
            string_pool: HashMap::new(),
            enum_defs: HashMap::new(),
            enum_layouts: HashMap::new(),
            declared_syms: HashSet::new(),
        }
    }

    /// Push a `declare …` line into `extra_preamble` only if `sym` has not
    /// been declared before.  Returns `true` when the line was actually added.
    fn push_declare(&mut self, sym: &str, line: String) -> bool {
        if self.declared_syms.insert(sym.to_string()) {
            self.extra_preamble.push(line);
            true
        } else {
            false
        }
    }

    /// Push a type-definition line into `type_decls` only if `sym` has not
    /// been declared before.  Returns `true` when the line was actually added.
    fn push_type_decl(&mut self, sym: &str, line: String) -> bool {
        let sym = sym
            .strip_prefix("%struct.")
            .or_else(|| sym.strip_prefix("%enum."))
            .or_else(|| sym.strip_prefix("%newtype."))
            .unwrap_or(sym);
        if self.declared_syms.insert(sym.to_string()) {
            self.type_decls.push(line);
            true
        } else {
            false
        }
    }

    fn preamble(&self) -> Vec<String> {
        let mut p = self.type_decls.clone();
        p.extend(self.extra_preamble.iter().cloned());
        p
    }

    fn find_std_enum_name(&self, simple: &str) -> Option<String> {
        if self.enum_defs.contains_key(simple) {
            return Some(simple.to_string());
        }
        let suffix = format!("__{}", simple);
        self.enum_defs
            .keys()
            .find(|k| k.ends_with(&suffix))
            .cloned()
    }

    fn option_enum_name(&self) -> String {
        self.find_std_enum_name("Option")
            .unwrap_or_else(|| "Option".to_string())
    }

    fn result_enum_name(&self) -> String {
        self.find_std_enum_name("Result")
            .unwrap_or_else(|| "Result".to_string())
    }

    fn mangle_app_struct_name(name: &str, args: &[String]) -> String {
        if args.is_empty() {
            format!("%struct.{}", name)
        } else {
            let mut out = format!("%struct.{}", name);
            for a in args {
                out.push_str("__");
                out.push_str(&mangle_llvm_type_name(a));
            }
            out
        }
    }

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
            .map(|name| {
                // Always use __ty_method__ — these are the value-returning wrappers
                // defined in typhoon-stdlib.ll. __ty_rt__ is the internal out-param
                // ABI used only by the method wrappers themselves, never by call sites.
                format!("__ty_method__{}__{}", name, method)
            })
    }

    /// Compute an enum layout from a definition + concrete type args, emit the
    /// type_decl, and store it. Does nothing if the layout already exists.
    fn ensure_enum_layout(
        &mut self,
        def: &EnumDef,
        llvm_args: &[String],
        lower_type: &dyn Fn(&str, &str) -> String, // (ty_name, subst_name) -> llvm_ty
        lower_payload: &mut dyn FnMut(
            &EnumVariantPayloadKind,
            &HashMap<String, String>,
        ) -> Option<String>,
    ) -> String {
        let llvm_struct_ty = TypeRegistry::mangle_app_struct_name(&def.name, llvm_args);
        if self.enum_layouts.contains_key(&llvm_struct_ty) {
            return llvm_struct_ty;
        }

        let mut subst = HashMap::<String, String>::new();
        for (p, a) in def.gen_params.iter().zip(llvm_args.iter()) {
            subst.insert(p.clone(), a.clone());
        }

        let tag_ty = "i8".to_string();
        let mut variants_layout = HashMap::new();
        let mut payload_fields: Vec<String> = Vec::new();

        for (tag_value, v) in def.variants.iter().enumerate() {
            let payload_ty = v.payload.as_ref().and_then(|p| lower_payload(p, &subst));
            let payload_index = payload_ty.as_ref().map(|ty| {
                payload_fields.push(ty.clone());
                payload_fields.len() // 1-based offset (index 0 is tag)
            });
            variants_layout.insert(
                v.name.clone(),
                EnumVariantLayout {
                    tag_value: tag_value as i64,
                    payload_index,
                    payload_ty,
                },
            );
        }

        let body = if payload_fields.is_empty() {
            format!("{{ {} }}", tag_ty)
        } else {
            format!("{{ {}, {} }}", tag_ty, payload_fields.join(", "))
        };
        self.type_decls
            .push(format!("{} = type {}", llvm_struct_ty, body));
        self.enum_layouts.insert(
            llvm_struct_ty.clone(),
            EnumLayout {
                llvm_struct_ty: llvm_struct_ty.clone(),
                tag_ty,
                variants: variants_layout,
            },
        );
        llvm_struct_ty
    }
}

// ── IR builder ────────────────────────────────────────────────────────────────

struct IrBuilder<'a> {
    // Emit state
    lines: Vec<String>,
    entry_allocas: Vec<String>,
    next_tmp: usize,
    next_label: usize,
    loop_labels: Vec<(String, String)>,
    current_fn_name: Option<String>,
    current_fn_ret_ty: String,

    // Variable tracking
    locals: HashMap<String, String>,
    locals_type: HashMap<String, String>,
    mutable_vars: HashSet<String>,
    chan_elem_tys: HashMap<String, String>,

    // Registries (stable across functions)
    reg: TypeRegistry,
    adt_structs: HashMap<String, String>,

    // External state
    types: Option<*const HashMap<NodeId, InferType>>,
    drop_map: &'a HashMap<NodeId, Vec<DropInfo>>,

    // Completed trampolines waiting to be appended
    conc_functions: Vec<IrFunction>,
}

impl<'a> IrBuilder<'a> {
    fn new(drop_map: &'a HashMap<NodeId, Vec<DropInfo>>) -> Self {
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
            conc_functions: Vec::new(),
            chan_elem_tys: HashMap::new(),
        }
    }

    // ── Context save/restore ──────────────────────────────────────────────────

    fn save_context(&mut self) -> FnContext {
        FnContext {
            lines: std::mem::take(&mut self.lines),
            entry_allocas: std::mem::take(&mut self.entry_allocas),
            fn_name: self.current_fn_name.clone(),
            ret_ty: self.current_fn_ret_ty.clone(),
            locals: std::mem::take(&mut self.locals),
            locals_type: std::mem::take(&mut self.locals_type),
            mutable_vars: std::mem::take(&mut self.mutable_vars),
            next_tmp: self.next_tmp,
        }
    }

    fn restore_context(&mut self, ctx: FnContext) {
        self.lines = ctx.lines;
        self.entry_allocas = ctx.entry_allocas;
        self.current_fn_name = ctx.fn_name;
        self.current_fn_ret_ty = ctx.ret_ty;
        self.locals = ctx.locals;
        self.locals_type = ctx.locals_type;
        self.mutable_vars = ctx.mutable_vars;
        self.next_tmp = ctx.next_tmp;
    }

    fn reset_for_function(&mut self, name: &str, ret_ty: &str) {
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
        // "void" is illegal in alloca/load/store — should never happen,
        // but guard here as a last resort to prevent invalid IR.
        let ty = if ty == "void" { "i32" } else { ty };
        self.entry_allocas
            .push(format!("  {} = alloca {}", tmp, ty));
    }

    /// Splice hoisted entry_allocas in right after the "entry:" label line.
    fn finish_function_ir(&mut self) -> String {
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

    // ── Delegation to TypeRegistry ────────────────────────────────────────────

    fn option_enum_name(&self) -> String {
        self.reg.option_enum_name()
    }
    fn result_enum_name(&self) -> String {
        self.reg.result_enum_name()
    }
    fn struct_field_info(&self, struct_name: &str, field_name: &str) -> (usize, String) {
        self.reg.struct_field_info(struct_name, field_name)
    }
    fn method_symbol_for_call(&self, base_ty: &str, method: &str) -> Option<String> {
        self.reg.method_symbol_for_call(base_ty, method)
    }

    // ── Type collection ───────────────────────────────────────────────────────

    fn collect_types(&mut self, module: &Module) {
        self.reg = TypeRegistry::new();
        self.adt_structs.clear();

        self.reg
            .push_type_decl("Buf", "%struct.Buf = type { i8*, i64, i64 }".to_string());

        self.reg.push_type_decl(
            "TyArray",
            "%struct.TyArray = type { i8*, i64, i64, i64, i64 }".to_string(),
        );

        self.collect_enum_defs(module);
        self.register_runtime_decls();
        self.register_module_sigs(module);

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

    fn collect_enum_defs(&mut self, module: &Module) {
        for decl in &module.declarations {
            if let DeclarationKind::Enum {
                name,
                generics,
                variants,
            } = &decl.node
            {
                let gen_params = generics.iter().map(|g| g.node.name.name.clone()).collect();
                let variants = variants
                    .iter()
                    .map(|v| EnumVariantDef {
                        name: v.node.name.name.clone(),
                        payload: v.node.payload.as_ref().map(|p| p.node.clone()),
                    })
                    .collect();
                self.reg.enum_defs.insert(
                    name.name.clone(),
                    EnumDef {
                        name: name.name.clone(),
                        gen_params,
                        variants,
                    },
                );
            }
        }

        // Pre-generate layouts for networking Result types used by runtime intrinsics.
        //
        // We cannot use ensure_enum_layout_for_infer here: Result/Option live in stdlib
        // modules that are compiled separately, so enum_defs never contains "Result" when
        // the user module is processed. That causes the lookup to return None and silently
        // skip registration. Later, the type-checker inference walk may register the same
        // mangled name with wrong i8 payload types (from an unresolved generic placeholder),
        // producing extractvalue type mismatches in LLVM.
        //
        // Fix: insert the layouts directly, bypassing the enum-def lookup entirely.
        // The runtime C ABI uses an `ok` byte, not source enum ordinal tags:
        // ok=1 means Ok(payload), ok=0 means Err(err).
        // ── Result<Listener, Int32> and Result<Socket, Int32> ────────────────
        let res_name = self.result_enum_name();
        for ok_payload in ["%struct.Listener*", "%struct.Socket*"] {
            let llvm_ty = TypeRegistry::mangle_app_struct_name(
                &res_name,
                &[mangle_llvm_type_name(ok_payload), "i32".to_string()],
            );
            if self.reg.enum_layouts.contains_key(&llvm_ty) {
                continue;
            }
            let body = format!("{{ i8, {}, i32 }}", ok_payload);
            self.reg
                .push_type_decl(&llvm_ty, format!("{} = type {}", llvm_ty, body));

            let mut variants = HashMap::new();
            variants.insert(
                "Ok".to_string(),
                EnumVariantLayout {
                    tag_value: 1,
                    payload_index: Some(1),
                    payload_ty: Some(ok_payload.to_string()),
                },
            );
            variants.insert(
                "Err".to_string(),
                EnumVariantLayout {
                    tag_value: 0,
                    payload_index: Some(2),
                    payload_ty: Some("i32".to_string()),
                },
            );
            self.reg.enum_layouts.insert(
                llvm_ty.clone(),
                EnumLayout {
                    llvm_struct_ty: llvm_ty,
                    tag_ty: "i8".to_string(),
                    variants,
                },
            );
        }

        // ── Option<Int8> and Option<Int32> ───────────────────────────────────
        //
        // Option lives in the stdlib module and is never in enum_defs when the
        // user module is compiled, so ensure_enum_layout_for_infer silently
        // returns without registering anything.  The mangled type name is still
        // emitted at use sites (alloca, load, extractvalue), so LLVM would see
        // an undefined type.
        //
        // Worse: if the first use of any Option<T> triggers ensure_enum_layout
        // via a path where lower_infer_type returns "i8" as a fallback (e.g.
        // because the channel element type wasn't resolved yet), the layout is
        // registered with an i8 payload and cached under the mangled name.
        // A later correct resolution of the same name is a no-op (the cache hit
        // guard at the top of ensure_enum_layout returns early), so the wrong
        // layout persists for the entire compilation and every Option<Int32>
        // alloca is only 2 bytes wide — causing a 4-byte ty_chan_recv to
        // overflow it on every popcount drain.
        //
        // Fix: pre-register the correct layouts here, before any use site can
        // race to register the wrong ones.  Extend this list as new element
        // types become common (Int64, Bool, Str, …).
        let opt_name = self.option_enum_name();
        // (llvm_elem_ty, payload_index_in_struct)
        let option_elem_tys: &[(&str, &str)] = &[
            ("i8", "i8"),
            ("i16", "i16"),
            ("i32", "i32"),
            ("i64", "i64"),
            ("i1", "i1"),
            ("i8*", "i8*"),
        ];
        for (elem_ty, _) in option_elem_tys {
            let llvm_ty =
                TypeRegistry::mangle_app_struct_name(&opt_name, &[mangle_llvm_type_name(elem_ty)]);
            if self.reg.enum_layouts.contains_key(&llvm_ty) {
                continue;
            }
            // Layout: { i8 tag, <elem_ty> value }
            // tag=0 → Some (payload at index 1), tag=1 → None (no payload)
            let body = format!("{{ i8, {} }}", elem_ty);
            // push_type_decl so declared_syms is updated and register_module_sigs
            // cannot later overwrite this with an opaque stub.
            self.reg
                .push_type_decl(&llvm_ty, format!("{} = type {}", llvm_ty, body));

            let mut variants = HashMap::new();
            variants.insert(
                "Some".to_string(),
                EnumVariantLayout {
                    tag_value: 0,
                    payload_index: Some(1),
                    payload_ty: Some(elem_ty.to_string()),
                },
            );
            variants.insert(
                "None".to_string(),
                EnumVariantLayout {
                    tag_value: 1,
                    payload_index: None,
                    payload_ty: None,
                },
            );
            self.reg.enum_layouts.insert(
                llvm_ty.clone(),
                EnumLayout {
                    llvm_struct_ty: llvm_ty,
                    tag_ty: "i8".to_string(),
                    variants,
                },
            );
        }
    }

    fn register_runtime_decls(&mut self) {
        let decls = [
            "declare void @ty_sched_init     ()",
            "declare void @ty_sched_run      ()",
            "declare void @ty_sched_shutdown ()",
            "declare i8*  @ty_spawn          (i8*, i8*, i8*)", // task, fn_ptr, arg
            "declare i8*  @ty_spawn_closure  (i8*, i8*, i8*, i64)", // task, fn_ptr, closure_ptr, size
            "declare void @ty_yield          ()",
            "declare void @ty_safepoint      ()",
            "declare void @ty_await          (i8*, i8*)", // task, coro_handle
            "declare i8*  @ty_chan_new       (i64, i64)", // elem_size, cap
            "declare void @ty_chan_send      (i8*, i8*, i8*)", // task, chan, elem_ptr
            "declare void @ty_chan_recv      (i8*, i8*, i8*)", // task, chan, out_ptr
            "declare i32  @ty_chan_try_recv  (i8*, i8*, i8*)", // task, chan, out_ptr -> i32 (0/1)
            "declare void @ty_chan_close     (i8*)",      // chan
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
            // Stdlib networking method stubs — implementations are in typhoon-stdlib.ll.
            // Declared here so the symbol is always visible in the user module IR
            // without relying on the all_decls → register_module_sigs pipeline, which
            // can be disrupted by desugar transforming the UnsafeOrExtern nodes.
            // Void methods (no return value):
            "declare void @__ty_method__Listener__close  (i8*, %struct.Listener*)",
            "declare void @__ty_method__Socket__consume  (i8*, %struct.Socket*, i8*)",
            "declare void @__ty_method__Socket__close    (i8*, %struct.Socket*)",
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
        ];
        for d in decls {
            // Extract the symbol name from "declare <ret> @name(...)" so we
            // can deduplicate against anything already pushed by
            // register_module_sigs or a previous call.
            let sym = d
                .split('@')
                .nth(1)
                .and_then(|s| s.split('(').next())
                .unwrap_or(d)
                .trim()
                .to_string();
            self.reg.push_declare(&sym, d.to_string());
        }

        // func_sigs for array/scheduler
        let sigs: &[(&str, &str, &[&str])] = &[
            ("ty_array_push", "void", &["%struct.TyArray*", "i8*"]),
            ("ty_spawn", "i8*", &["i8*", "i8*"]),
            ("ty_spawn_closure", "i8*", &["i8*", "i8*", "i64"]),
            ("ty_await", "void", &["i8*"]),
            ("ty_yield", "void", &[]),
            ("ty_safepoint", "void", &[]),
            ("ty_chan_new", "i8*", &["i64", "i64"]),
            ("ty_chan_send", "void", &["i8*", "i8*"]),
            ("ty_chan_recv", "void", &["i8*", "i8*"]),
            ("ty_chan_try_recv", "i32", &["i8*", "i8*"]),
            ("ty_chan_close", "void", &["i8*"]),
        ];
        for (name, ret, params) in sigs {
            self.reg.func_sigs.insert(
                name.to_string(),
                (
                    ret.to_string(),
                    params.iter().map(|s| s.to_string()).collect(),
                ),
            );
        }

        // Networking method stubs — value-returning wrappers defined in typhoon-stdlib.ll.
        // These use result_enum_name() so they can't go in the static array above.
        // Void stubs are already declared in the static array; only add func_sigs for them.
        {
            let res_listener = TypeRegistry::mangle_app_struct_name(
                &self.result_enum_name(),
                &["%struct.Listener*".to_string(), "i32".to_string()],
            );
            let res_socket = TypeRegistry::mangle_app_struct_name(
                &self.result_enum_name(),
                &["%struct.Socket*".to_string(), "i32".to_string()],
            );

            // Network::listen(self, addr: Str) -> Result<Listener, Int32>
            let listen_params = vec![
                "i8*".to_string(),
                "%struct.Network*".to_string(),
                "i8*".to_string(),
            ];
            let listen_decl = format!(
                "declare {} @__ty_method__Network__listen({})",
                res_listener,
                listen_params.join(", ")
            );
            self.reg
                .push_declare("__ty_method__Network__listen", listen_decl);
            self.reg.func_sigs.insert(
                "__ty_method__Network__listen".to_string(),
                (res_listener, listen_params),
            );

            // Listener::accept(self) -> Result<Socket, Int32>
            let accept_params = vec!["i8*".to_string(), "%struct.Listener*".to_string()];
            let accept_decl = format!(
                "declare {} @__ty_method__Listener__accept({})",
                res_socket,
                accept_params.join(", ")
            );
            self.reg
                .push_declare("__ty_method__Listener__accept", accept_decl);
            self.reg.func_sigs.insert(
                "__ty_method__Listener__accept".to_string(),
                (res_socket, accept_params),
            );

            // Void method func_sigs (declares already emitted in the static array)
            self.reg.func_sigs.insert(
                "__ty_method__Listener__close".to_string(),
                (
                    "void".to_string(),
                    vec!["i8*".to_string(), "%struct.Listener*".to_string()],
                ),
            );
            self.reg.func_sigs.insert(
                "__ty_method__Socket__consume".to_string(),
                (
                    "void".to_string(),
                    vec![
                        "i8*".to_string(),
                        "%struct.Socket*".to_string(),
                        "i8*".to_string(),
                    ],
                ),
            );
            self.reg.func_sigs.insert(
                "__ty_method__Socket__close".to_string(),
                (
                    "void".to_string(),
                    vec!["i8*".to_string(), "%struct.Socket*".to_string()],
                ),
            );
        }

        // stdio intrinsics keyed under both source and runtime names
        let stdio: &[(&str, &str, &str, &[&str])] = &[
            ("print", "ty_print", "void", &["i8*"]),
            ("println", "ty_println", "void", &["i8*"]),
            ("printf", "ty_printf", "void", &["i8*"]),
            ("fprint", "ty_fprint", "void", &["i32", "i8*"]),
            ("fprintln", "ty_fprintln", "void", &["i32", "i8*"]),
            ("fprintf", "ty_fprintf", "void", &["i32", "i8*"]),
            ("sprint", "ty_sprint", "void", &["%struct.Buf*", "i8*"]),
            ("sprintln", "ty_sprintln", "void", &["%struct.Buf*", "i8*"]),
            ("sprintf", "ty_sprintf", "void", &["%struct.Buf*", "i8*"]),
            ("scan", "ty_scan", "i8*", &[]),
            ("scanf", "ty_scanf", "i32", &["i8*"]),
            ("fscan", "ty_fscan", "i8*", &["i32"]),
            ("fscanf", "ty_fscanf", "i32", &["i32", "i8*"]),
            ("sscan", "ty_sscan", "i8*", &["i8*", "i8**"]),
            ("sscanf", "ty_sscanf", "i32", &["i8*", "i8*"]),
        ];
        for (src, rt, ret, params) in stdio {
            let sig = (
                ret.to_string(),
                params.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            );
            self.reg.func_sigs.insert(src.to_string(), sig.clone());
            self.reg.func_sigs.insert(rt.to_string(), sig);
        }
    }

    fn register_module_sigs(&mut self, module: &Module) {
        for decl in &module.declarations {
            match &decl.node {
                DeclarationKind::Struct { name, fields, .. } => {
                    let mut field_types = Vec::new();
                    let mut field_map = Vec::new();
                    for (id, ty) in fields {
                        let lt = Self::lower_type(
                            ty,
                            &self.option_enum_name(),
                            &self.result_enum_name(),
                        );
                        field_types.push(lt.clone());
                        field_map.push((id.name.clone(), lt));
                    }
                    let line = format!(
                        "%struct.{} = type {}",
                        name.name,
                        if field_types.is_empty() {
                            "opaque".to_string()
                        } else {
                            format!("{{ {} }}", field_types.join(", "))
                        }
                    );
                    // Only emit the type line if this struct hasn't already been
                    // declared (e.g. Buf / TyArray are pre-declared as concrete
                    // layout types in collect_types and must not be overwritten
                    // with an opaque stub from the source-level `struct Buf {}`).
                    self.reg.push_type_decl(&name.name, line);
                    self.reg.struct_fields.insert(name.name.clone(), field_map);
                }
                DeclarationKind::Enum { name, .. } => {
                    self.reg
                        .push_type_decl(&name.name, format!("%enum.{} = type opaque", name.name));
                }
                DeclarationKind::Newtype { name, type_alias } => {
                    let line = format!(
                        "%newtype.{} = type {}",
                        name.name,
                        Self::lower_type(
                            type_alias,
                            &self.option_enum_name(),
                            &self.result_enum_name()
                        )
                    );
                    self.reg.push_type_decl(&name.name, line);
                }
                DeclarationKind::Function {
                    name,
                    return_type,
                    params,
                    ..
                } => {
                    let ret_ty = return_type
                        .as_ref()
                        .map(|ty| {
                            Self::lower_type(ty, &self.option_enum_name(), &self.result_enum_name())
                        })
                        .unwrap_or_else(|| "void".to_string());
                    let mut param_types: Vec<String> = params
                        .iter()
                        .map(|p| {
                            Self::lower_type(
                                &p.type_annotation,
                                &self.option_enum_name(),
                                &self.result_enum_name(),
                            )
                        })
                        .collect();
                    // Every function (including main, now __ty_main_body) takes task.
                    param_types.insert(0, "i8*".to_string());
                    self.reg
                        .func_sigs
                        .insert(name.name.clone(), (ret_ty, param_types));
                }
                DeclarationKind::UnsafeOrExtern(uoe) => {
                    if let UnsafeOrExternKind::Extern { declarations, .. } = &uoe.node {
                        for Spanned { node, .. } in declarations {
                            let FunctionSignatureKind {
                                name,
                                params,
                                return_type,
                                ..
                            } = node;
                            let ret_ty = return_type
                                .as_ref()
                                .map(|ty| {
                                    Self::lower_type(
                                        ty,
                                        &self.option_enum_name(),
                                        &self.result_enum_name(),
                                    )
                                })
                                .unwrap_or_else(|| "void".to_string());
                            let mut param_types: Vec<String> = params
                                .iter()
                                .map(|p| {
                                    Self::lower_type(
                                        &p.type_annotation,
                                        &self.option_enum_name(),
                                        &self.result_enum_name(),
                                    )
                                })
                                .collect();
                            // LLVM-imported stdlib signatures can lose generic args on
                            // Result out-params (e.g. become `%struct.Result`).
                            // Canonicalize known net runtime externs to concrete Result
                            // layouts so declarations always reference defined types.
                            //
                            // Two corrections per function:
                            //   1. The Result type name is concretised to the mangled form.
                            //   2. A trailing `*` is appended — these are out-param pointers,
                            //      not by-value structs.  Without the `*` LLVM rejects the
                            //      call inside the stdlib.ll wrapper which does pass a pointer.
                            if name.name == "__ty_rt__Network__listen" && param_types.len() >= 4 {
                                param_types[3] = format!(
                                    "{}*",
                                    TypeRegistry::mangle_app_struct_name(
                                        &self.result_enum_name(),
                                        &["%struct.Listener*".to_string(), "i32".to_string()],
                                    )
                                );
                            } else if name.name == "__ty_rt__Listener__accept"
                                && param_types.len() >= 3
                            {
                                param_types[2] = format!(
                                    "{}*",
                                    TypeRegistry::mangle_app_struct_name(
                                        &self.result_enum_name(),
                                        &["%struct.Socket*".to_string(), "i32".to_string()],
                                    )
                                );
                            } else if name.name == "__ty_rt__Socket__consume" {
                                // consume gained a leading `task` parameter (i8*) in the
                                // runtime so it can call ty_spawn.  If the stdlib.ll being
                                // parsed pre-dates that change and only has (Socket*, chan),
                                // patch the signature here so the codegen-emitted declare
                                // and the actual call site both include task.
                                if param_types.first().map(|t| t.as_str()) != Some("i8*") {
                                    param_types.insert(0, "i8*".to_string());
                                }
                            }
                            // __ty_method__ stubs are Typhoon-ABI functions (take task as i8*)
                            // and must NOT be in extern_fns (which shifts param_offset).
                            // __ty_rt__ and other C externs use raw C ABI (no task prepend).
                            let is_method_stub = name.name.starts_with("__ty_method__");
                            if is_method_stub {
                                // Prepend task pointer — mirrors what the Function arm does.
                                param_types.insert(0, "i8*".to_string());
                            }
                            // Emit the LLVM declare
                            let decl_line = format!(
                                "declare {} @{}({})",
                                ret_ty,
                                name.name,
                                param_types.join(", ")
                            );
                            self.reg.push_declare(&name.name, decl_line);
                            self.reg
                                .func_sigs
                                .insert(name.name.clone(), (ret_ty, param_types));
                            if !is_method_stub {
                                self.reg.extern_fns.insert(name.name.clone());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // ── Function emission ─────────────────────────────────────────────────────

    pub fn lower_module(
        module: &Module,
        types: &HashMap<NodeId, InferType>,
        specializations: &HashMap<(String, Vec<InferType>), String>,
        drop_map: &HashMap<NodeId, Vec<DropInfo>>,
    ) -> IrModule {
        let mut b = IrBuilder::new(drop_map);
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
                    .map(|ty| Self::lower_type(ty, &b.option_enum_name(), &b.result_enum_name()))
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
                    });
                    Some(IrFunction {
                        name: "main".to_string(),
                        body: b.emit_bootstrap_main(),
                        ret_type: "i32".to_string(),
                        params: vec![],
                    })
                } else {
                    let body_ir = b.emit_function(name, params, &ret_ty, body);
                    let mut param_list: Vec<(String, String)> = params
                        .iter()
                        .map(|p| {
                            (
                                p.name.name.clone(),
                                Self::lower_type(
                                    &p.type_annotation,
                                    &b.option_enum_name(),
                                    &b.result_enum_name(),
                                ),
                            )
                        })
                        .collect();
                    param_list.insert(0, ("task".to_string(), "i8*".to_string()));
                    Some(IrFunction {
                        name: link_symbol_name(&name.name),
                        body: body_ir,
                        ret_type: ret_ty,
                        params: param_list,
                    })
                }
            })
            .collect();

        for ((func_name, concrete_types), spec_name) in specializations {
            if let Some(decl) = module.declarations.iter().find(|d| {
                matches!(&d.node, DeclarationKind::Function { name, .. } if &name.name == func_name)
            }) {
                if let DeclarationKind::Function { name, params, body, return_type, .. } = &decl.node {
                    let ret_ty = return_type.as_ref()
                        .map(|ty| Self::lower_type(ty,
                            &b.option_enum_name(),
                            &b.result_enum_name()))
                        .unwrap_or_else(|| "void".to_string());
                    let body_ir = b.emit_function(name, params, &ret_ty, body);
                    let mut param_list: Vec<(String, String)> = params.iter()
                        .map(|p| (p.name.name.clone(), Self::lower_type(&p.type_annotation,
                            &b.option_enum_name(),
                            &b.result_enum_name(),)))
                        .collect();
                    param_list.insert(0, ("task".to_string(), "i8*".to_string()));
                    all_functions.push(IrFunction {
                        name: spec_name.clone(), body: body_ir, ret_type: ret_ty, params: param_list,
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

    fn emit_function(
        &mut self,
        name: &Identifier,
        params: &[Parameter],
        ret_ty: &str,
        body: &Block,
    ) -> String {
        self.reset_for_function(&name.name, ret_ty);
        self.emit("entry:".to_string());
        self.emit("  call void @ty_safepoint()".to_string());
        // All functions (including former-main, now __ty_main_body) get a task param.
        self.emit_function_param("task".to_string(), "i8*".to_string());
        for p in params {
            let ty = Self::lower_type(
                &p.type_annotation,
                &self.option_enum_name(),
                &self.result_enum_name(),
            );
            self.emit_function_param(p.name.name.clone(), ty);
        }

        let terminated = self.emit_block_stmts(body, ret_ty);

        if !terminated {
            if let Some(expr) = &body.trailing_expression {
                let val = self.emit_expr(expr);
                let ty = self.expr_llvm_type(expr);
                self.emit(format!("  ret {} {}", ty, val));
            } else if !self
                .lines
                .iter()
                .any(|l| l.trim_start().starts_with("ret "))
            {
                if ret_ty == "void" {
                    self.emit("  ret void".to_string());
                } else {
                    let z = self.zero_value(ret_ty);
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
                let z = self.zero_value(ret_ty);
                self.emit(format!("  ret {} {}", ret_ty, z));
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
        self.reset_for_function("__ty_main_body", "void");
        self.emit("entry:".to_string());
        self.emit("  call void @ty_safepoint()".to_string());
        // Bind task and arg params (arg is unused but must be accepted).
        self.emit_function_param("task".to_string(), "i8*".to_string());
        self.emit_function_param("arg".to_string(), "i8*".to_string());

        // User params (main normally has none, but handle them anyway).
        for param in params {
            let ty = Self::lower_type(
                &param.type_annotation,
                &self.option_enum_name(),
                &self.result_enum_name(),
            );
            let slot = self.tmp();
            self.emit_alloca(&slot, &ty);
            if ty == "%struct.Network*" {
                let v = self.tmp();
                self.emit(format!("  {} = call %struct.Network* @ty_net_global()", v));
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
    fn emit_bootstrap_main(&mut self) -> String {
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

    fn emit_function_param(&mut self, name: String, ty: String) {
        let slot = self.tmp();
        self.emit_alloca(&slot, &ty);
        self.emit(format!("  store {} %{}, {}* {}", ty, name, ty, slot));
        self.locals.insert(name.clone(), slot);
        self.locals_type.insert(name, ty);
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
                let val = self.emit_expr(expr); // side-effects
                if self.current_fn_name.as_deref() == Some("__ty_main_body") {
                    if let Some((_, end_lbl)) = self.loop_labels.last().cloned() {
                        // In a coroutine, "return" inside a loop means "exit the loop".
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
                // Use the concrete emitted LLVM type of the scrutinee SSA value for IR ops
                // like `extractvalue`; inferred types can be equivalent but differently
                // mangled names, which LLVM still treats as distinct nominal types.
                // In LetBinding, prefer inferred type over value_llvm_type scraping:
                let match_ty = self
                    .actual_inferred_type(initializer)
                    .map(|t| self.lower_infer_type(&t))
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
                    // If we are inside a loop, branch back to the loop start
                    // rather than falling into merge. This implements the correct
                    // semantics of `let Ok(x) = expr else { ... }` inside a loop:
                    // when the pattern fails, run the else block then skip the
                    // rest of the loop body (continue). Without this, any conc{}
                    // or other statement after the let-else would execute even
                    // on the failure path — e.g. spawning handle_connection with
                    // a null/stale socket when accept() fails.
                    if let Some((loop_start, _)) = self.loop_labels.last().cloned() {
                        self.emit(format!("  br label %{}", loop_start));
                    } else {
                        self.emit(format!("  br label %{}", merge_lbl));
                    }
                }
                // Emit ok arm. Always br to merge — LLVM requires every
                // basic block to end with an explicit terminator.
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

    // ── Conc (concurrent block) emission ──────────────────────────────────────

    fn emit_conc(&mut self, body: &Block) {
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
                    // Surface this — a missing capture is always a codegen bug upstream
                    self.emit(format!(
                        "  ; BUG: capture '{}' not found in locals — missing let binding?",
                        name
                    ));
                    None // still skip, but now it's visible
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
            let sz = self.llvm_const_sizeof(ty);
            let al = self.llvm_const_alignof(ty);
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
        // current stack frame. Using alloca here would place the closure on
        // the caller's stack; when the accept loop iterates, the next
        // accept() overwrites the same stack slot before the spawned coro
        // has read it, corrupting every previously-spawned coro's captured
        // socket pointer. slab_alloc gives each conc{} its own stable copy.
        let task_slot = self.emit_task_load();
        // slab_alloc(arena: i8*, size: i32) -> i8*
        let raw_ptr = self.tmp();
        self.emit(format!(
            "  {} = call i8* @slab_alloc(i8* {}, i32 {})",
            raw_ptr, task_slot, closure_size
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
        // slab_free(arena: i8*, ptr: i8*, size_class: i32)
        // class_id is in scope here (computed above from closure_size).
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
        };
        self.restore_context(ctx);
        self.next_tmp = saved_tmp;
        (ir, Some((closure_i8, closure_size)))
    }

    // ── Drop helpers ──────────────────────────────────────────────────────────

    /// Emit a `slab_free` call for the named local, if it was slab-allocated.
    /// Looks up the typed pointer in `locals` and the LLVM type in `locals_type`
    /// to reconstruct the size class.
    fn emit_slab_free(&mut self, name: &str) {
        let Some(typed_ptr) = self.locals.get(name).cloned() else {
            return;
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
        let tv = self.emit_task_load();
        self.emit(format!(
            "  call void @slab_free(i8* {}, i8* {}, i32 {})",
            tv, raw, class_id
        ));
    }

    // ── Let binding ───────────────────────────────────────────────────────────

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
                let tv = self.emit_task_load();
                let out = self.tmp();
                self.emit(format!("  {} = call %struct.TyArray* @ty_array_from_fixed(i8* {}, i8* {}, i64 {}, i64 {}, i64 {})", out, tv, raw, elems.len(), sz, al));
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
                            .and_then(Self::chan_elem_type_from_annotation)
                            .map(|t| {
                                Self::lower_type(
                                    t,
                                    &self.option_enum_name(),
                                    &self.result_enum_name(),
                                )
                            })
                            .unwrap_or_else(|| "i8".to_string());
                        let elem_size = self.llvm_const_sizeof(&elem_ty);
                        let chan_ptr = self.tmp();
                        let chan_elem_src = type_annotation
                            .and_then(Self::chan_elem_type_from_annotation)
                            .map(|t| t.node.name.clone())
                            .unwrap_or_else(|| "Int8".to_string());
                        // Default to a small buffered channel to avoid deadlocks when producers run
                        // synchronously (e.g. when `conc` is lowered without spawning).
                        self.emit(format!(
                            "  {} = call i8* @ty_chan_new(i64 {}, i64 64)",
                            chan_ptr, elem_size
                        ));

                        let ty = type_annotation
                            .map(|t| {
                                Self::lower_type(
                                    t,
                                    &self.option_enum_name(),
                                    &self.result_enum_name(),
                                )
                            })
                            .unwrap_or_else(|| "i8*".to_string());
                        let slot = self.tmp();
                        self.emit_alloca(&slot, &ty);
                        self.emit(format!("  store {} {}, {}* {}", ty, chan_ptr, ty, slot));
                        self.locals.insert(name.name.clone(), slot);
                        self.locals_type.insert(name.name.clone(), ty);
                        // elem_ty for sizing stays as before (lowered LLVM type)
                        self.chan_elem_tys.insert(name.name.clone(), chan_elem_src);
                        return;
                    }
                }
            }
        }

        // General case
        let value = self.emit_expr(initializer);
        let init_ty = {
            let t = self.expr_llvm_type(initializer);
            if t == "void" {
                // expr_llvm_type returns "void" for match/block expressions whose
                // type checker entry is Unit due to NodeId collision or statement-
                // level match. Recover the real type from the last emitted load
                // (emit_match_expression always ends with `%tN = load <ty>, <ty>* <slot>`).
                self.lines
                    .iter()
                    .rev()
                    .find_map(|l| {
                        let t = l.trim_start();
                        if t.starts_with(&format!("{} = load ", value)) {
                            t.strip_prefix(&format!("{} = load ", value))
                                .and_then(|rest| rest.split(',').next())
                                .map(|ty| ty.trim().to_string())
                                .filter(|ty| ty != "void")
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| "i32".to_string())
            } else {
                t
            }
        };
        let ty = type_annotation
            .map(|t| Self::lower_type(t, &self.option_enum_name(), &self.result_enum_name()))
            .unwrap_or_else(|| init_ty.clone());
        let value = self.emit_widen(&value, &init_ty, &ty);

        if !ty.ends_with('*')
            && ty != "void"
            && mutable
            && ty.starts_with("%struct.")
            && !self.reg.enum_layouts.contains_key(&ty)
        {
            // Slab allocation for mutable user struct values only (not enum aggregates)
            let size = self.llvm_const_sizeof(&ty);
            let class_id = get_size_class(size);
            let tv = self.emit_task_load();
            let raw_ptr = self.tmp();
            self.emit(format!(
                "  {} = call i8* @slab_alloc(i8* {}, i32 {})",
                raw_ptr, tv, class_id
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
            self.emit_alloca(&slot, &ty);
            self.emit(format!("  store {} {}, {}* {}", ty, value, ty, slot));
            self.locals.insert(name.name.clone(), slot);
            self.locals_type.insert(name.name.clone(), ty);
        }
    }

    // ── Control flow ──────────────────────────────────────────────────────────

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
                let raw = self.tmp();
                self.emit(format!(
                    "  {} = bitcast {}* {} to i8*",
                    raw, array_ty, alloca
                ));

                // 2. Call the runtime to create a proper TyArray object
                let out = self.tmp();
                let elem_size = self.llvm_const_sizeof(&elem_ty);
                let align = self.llvm_const_alignof(&elem_ty);
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
                    tmp
                } else {
                    // For now, return 0 for any undefined identifier
                    // This includes captured variables and undefined references
                    if !id.name.is_empty() && id.name.chars().next().unwrap().is_alphabetic() {
                        self.emit(format!("  ; undefined identifier: {}", id.name));
                    }
                    "0".to_string()
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

    /// Build the LLVM instruction string for one arithmetic/comparison op.
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
                    // If the variable is not found, emit a comment and use a placeholder
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
        // Method call: base.method(args)
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

        // Channel methods
        if base_ty == "i8*" {
            match field.name.as_str() {
                "send" => return self.emit_chan_send(&base_val, args),
                "recv" => return self.emit_chan_recv(call_expr, &base_val),
                "try_recv" => return self.emit_chan_try_recv(call_expr, &base_val),
                _ => {}
            }
        }

        // Array push
        if base_ty == "%struct.TyArray*" && field.name == "push" {
            return self.emit_array_push(&base_val, args);
        }

        // User-defined method
        if let Some(method_sym) = self.method_symbol_for_call(&base_ty, &field.name) {
            return self.emit_user_method_call(call_expr, &method_sym, &base_val, &base_ty, args);
        }

        "0".to_string()
    }

    fn emit_chan_send(&mut self, chan_val: &str, args: &[Expression]) -> String {
        if let Some(arg0) = args.first() {
            let val = self.emit_expr(arg0);
            let val_ty = self
                .actual_inferred_type(arg0)
                .map(|t| self.lower_infer_type(&t))
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
        // Resolve to the element type T (not Option<T>)
        let elem_infer = if let Some(inferred) = self.inferred_expr_type(call_expr).cloned() {
            // inferred is Option<T> — unwrap to T
            let opt_name = self.option_enum_name();
            match inferred {
                InferType::App(ref name, ref args) if name == &opt_name && args.len() == 1 => {
                    args[0].clone()
                }
                other => other,
            }
        } else if let ExpressionKind::Call { func, .. } = &call_expr.node {
            if let ExpressionKind::FieldAccess { base, .. } = &func.node {
                if let ExpressionKind::Identifier(id) = &base.node {
                    if let Some(elem_ty) = self.chan_elem_tys.get(&id.name) {
                        InferType::Con(elem_ty.clone())
                    } else {
                        return "0".to_string();
                    }
                } else {
                    return "0".to_string();
                }
            } else {
                return "0".to_string();
            }
        } else {
            return "0".to_string();
        };

        let elem_ty = self.lower_infer_type(&elem_infer);
        let opt_infer = InferType::App(self.option_enum_name(), vec![elem_infer]);
        self.ensure_enum_layout_for_infer(&opt_infer);
        let opt_ty =
            TypeRegistry::mangle_app_struct_name(&self.option_enum_name(), &[elem_ty.clone()]);
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
        // elem_src is now e.g. "Int32"
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

        let opt_name = self.option_enum_name();
        let inner_infer = InferType::Con(elem_src);
        let ty = InferType::App(opt_name.clone(), vec![inner_infer.clone()]);
        let elem_ty = self.lower_infer_type(&inner_infer); // "i32"
        let opt_ty = TypeRegistry::mangle_app_struct_name(&opt_name, &[elem_ty.clone()]);
        self.ensure_enum_layout_for_infer(&ty);

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
        // Look up the func_sig registered by register_module_sigs.
        // For stdlib methods (@__ty_method__) this is populated from the @ty_sig annotation.
        // Fall back to empty rather than ("i32", []) so missing entries don't silently
        // produce wrong types — callers use actual_ty / inferred type instead.
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
            // Use the declared param type when available; fall back to the actual
            // emitted type rather than a hardcoded "i32" so missing func_sig entries
            // don't corrupt arg types (e.g. passing a string literal as i32).
            let t = param_types
                .get(i + param_offset)
                .cloned()
                .unwrap_or_else(|| actual_ty.clone());
            let v = self.emit_widen(&v, &actual_ty, &t);
            arg_pairs.push(format!("{} {}", t, v));
        }
        let tmp = self.tmp();
        if ret_ty == "void" {
            // Only treat last param as out-pointer if it is an extra param
            // beyond what arg_pairs already covers (task + self + explicit args).
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
            } // if has_out_param
            self.emit(format!(
                "  call void @{}({})",
                runtime_name,
                arg_pairs.join(", ")
            ));
            return "0".to_string();
        }
        // Determine the effective return type: prefer the func_sig declaration, but if
        // it was absent (empty string fallback) use the call-expression's inferred type.
        let effective_ret = if ret_ty.is_empty() {
            self.inferred_expr_type(call_expr)
                .cloned()
                .map(|t| self.lower_infer_type(&t))
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
        if matches!(id.name.as_str(), "Ok" | "Err" | "Some" | "None") {
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
                        elem_llvm_ty = self.lower_infer_type(&a[0]);
                    }
                }
            }
            let tmp = self.tmp();
            self.emit(format!(
                "  {} = call i8* @ty_chan_new(i64 {}, i64 64)",
                tmp,
                self.llvm_const_sizeof(&elem_llvm_ty)
            ));
            return tmp;
        }

        let runtime_name =
            runtime_intrinsic_name(&id.name).unwrap_or_else(|| link_symbol_name(&id.name));
        let (ret_ty, mut param_types) = self
            .reg
            .func_sigs
            .get(&id.name)
            .cloned()
            .unwrap_or_else(|| ("i32".to_string(), vec![]));
        if param_types.is_empty() && matches!(id.name.as_str(), "printf" | "fprintf" | "sprintf") {
            param_types = vec!["i8*".to_string()];
        }

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

    // ── Match / if-let ────────────────────────────────────────────────────────

    fn emit_match_expression(&mut self, expr: &Expression, arms: &[MatchArm]) -> String {
        // The match expression's type is the arm-body type, not the scrutinee type.
        // This function is also reused for `match` statements (all arms are `void`).
        let mut result_ty = "void".to_string();
        'outer: for arm in arms {
            // Check type-checker map on the body and any trailing expression inside it
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
                    let t = self.lower_infer_type(&infer);
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

    // ── Pattern helpers ───────────────────────────────────────────────────────

    fn emit_pattern_test(
        &mut self,
        pattern: &Pattern,
        scrutinee_expr: &Expression,
        scrutinee_val: &str,
    ) -> String {
        let actual_ty = self
            .actual_inferred_type(scrutinee_expr)
            .map(|t| {
                self.ensure_enum_layout_for_infer(&t);
                self.lower_infer_type(&t)
            })
            .unwrap_or_else(|| self.expr_llvm_type(scrutinee_expr));
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

    fn emit_pattern_test_typed(
        &mut self,
        pattern: &Pattern,
        ty: &str,
        scrutinee_val: &str,
        scrutinee_expr: &Expression,
    ) -> String {
        let actual_ty = self
            .actual_inferred_type(scrutinee_expr)
            .map(|t| {
                self.ensure_enum_layout_for_infer(&t);
                self.lower_infer_type(&t)
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
            eprintln!(
                "DEBUG: emit_enum_tag_test - actual_inferred_type returned None for variant {}",
                variant_name
            );
            self.emit(format!(
                "  ; CODEGEN ERROR: could not resolve type for tag test variant {}",
                variant_name
            ));
            return "1".to_string();
        };
        let llvm_ty = self.lower_infer_type(&infer);
        eprintln!(
            "DEBUG: emit_enum_tag_test - variant={}, llvm_ty={}",
            variant_name, llvm_ty
        );
        let Some(layout) = self.reg.enum_layouts.get(&llvm_ty).cloned() else {
            eprintln!(
                "DEBUG: emit_enum_tag_test - layout not found for type={}",
                llvm_ty
            );
            self.emit(format!(
                "  ; CODEGEN ERROR: could not resolve layout for tag test variant {} with type {}",
                variant_name, llvm_ty
            ));
            return "1".to_string();
        };
        let Some(v) = layout.variants.get(variant_name) else {
            eprintln!(
                "DEBUG: emit_enum_tag_test - variant={} not in layout variants={:?}",
                variant_name,
                layout.variants.keys()
            );
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

    fn enum_payload_info(
        &mut self,
        scrutinee_expr: Option<&Expression>,
        llvm_ty: &str,
        variant_name: &str,
    ) -> Option<(usize, String)> {
        // Prefer inference-driven lookup (needed for generic enums like Option<T> / Result<T,E>).
        if let Some(e) = scrutinee_expr {
            if let Some(inferred) = self.actual_inferred_type(e) {
                let llvm_ty_infer = self.lower_infer_type(&inferred);
                let layout = self.reg.enum_layouts.get(&llvm_ty_infer)?;
                let v = layout.variants.get(variant_name)?;
                return Some((v.payload_index?, v.payload_ty.clone()?));
            }
        }
        let layout = self.reg.enum_layouts.get(llvm_ty)?;
        let v = layout.variants.get(variant_name)?;
        Some((v.payload_index?, v.payload_ty.clone()?))
    }

    // (no legacy ADT field parsing; enums must be defined/imported and laid out explicitly)

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
        let ty = self.lower_infer_type(&infer);

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

    // ── Type lowering ─────────────────────────────────────────────────────────

    fn lower_type(ty: &Type, option_name: &str, result_name: &str) -> String {
        // mirrors lower_type but without &self
        let ty_name = ty.node.name.as_str();
        let is_option = ty_name == "Option"
            || ty_name == option_name
            || ty_name.ends_with("::Option")
            || ty_name.ends_with("__Option");
        let is_result = ty_name == "Result"
            || ty_name == result_name
            || ty_name.ends_with("::Result")
            || ty_name.ends_with("__Result");
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
            _ if is_option => {
                let args: Vec<_> = ty
                    .node
                    .generic_args
                    .iter()
                    .map(|a| Self::lower_type(a, option_name, result_name))
                    .collect();
                TypeRegistry::mangle_app_struct_name(option_name, &args)
            }
            _ if is_result => {
                let args: Vec<_> = ty
                    .node
                    .generic_args
                    .iter()
                    .map(|a| Self::lower_type(a, option_name, result_name))
                    .collect();
                TypeRegistry::mangle_app_struct_name(result_name, &args)
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
                n if n.starts_with("%") => n.to_string(), // already an LLVM type, pass through
                n => format!("%struct.{}", n),
            },
            InferType::App(name, args) if name == "Ref" && args.len() == 1 => "i8*".to_string(),
            InferType::App(name, args)
                if (name == "Option"
                    || name == &self.option_enum_name()
                    || name.ends_with("::Option")
                    || name.ends_with("__Option"))
                    && args.len() == 1 =>
            {
                let inner = self.lower_infer_type(&args[0]);
                self.ensure_enum_layout_for_infer(ty);
                TypeRegistry::mangle_app_struct_name(&self.option_enum_name(), &[inner])
            }
            InferType::App(name, args)
                if (name == "Result"
                    || name == &self.result_enum_name()
                    || name.ends_with("::Result")
                    || name.ends_with("__Result"))
                    && args.len() == 2 =>
            {
                let ok = self.lower_infer_type(&args[0]);
                let err = self.lower_infer_type(&args[1]);
                self.ensure_enum_layout_for_infer(ty);
                TypeRegistry::mangle_app_struct_name(&self.result_enum_name(), &[ok, err])
            }
            InferType::App(name, _) if name == "Array" => "%struct.TyArray*".to_string(),
            InferType::App(name, _) if name == "Chan" => "i8*".to_string(),
            InferType::FixedArray(elem, n) => format!("[{} x {}]", n, self.lower_infer_type(elem)),
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
        // Literals have unambiguous LLVM types — check these BEFORE the type
        // checker map, which can have NodeId collisions across merged modules.
        match &expr.node {
            ExpressionKind::Literal(Literal {
                kind: LiteralKind::Str(_),
                ..
            }) => return "i8*".to_string(),
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
        // Type checker inference (can have NodeId collisions — only use for non-literals)
        if let Some(ty) = self.actual_inferred_type(expr) {
            return self.lower_infer_type(&ty);
        }
        match &expr.node {
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
                    return self
                        .reg
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

    fn actual_inferred_type(&mut self, expr: &Expression) -> Option<InferType> {
        if let Some(t) = self.inferred_expr_type(expr) {
            return Some(t.clone());
        }

        if let ExpressionKind::Call { func, .. } = &expr.node {
            // Handle method calls: base.method(...)
            if let ExpressionKind::FieldAccess { base, field, .. } = &func.node {
                let base_ty = self.expr_llvm_type(base);
                let method_sym = self.method_symbol_for_call(&base_ty, &field.name);
                eprintln!("DEBUG: actual_inferred_type - Checking field access: base_ty={}, field={}, sym={:?}", base_ty, field.name, method_sym);

                if let Some(method_sym) = method_sym {
                    if let Some((ret_ty_str, _)) = self.reg.func_sigs.get(&method_sym).cloned() {
                        if self.reg.enum_layouts.contains_key(&ret_ty_str) {
                            // The return type is a known enum layout. Reconstruct InferType args
                            // from the layout's variants, converting LLVM types back to source names.
                            let layout = self.reg.enum_layouts.get(&ret_ty_str).cloned().unwrap();
                            // Reconstruct Result<T,E> args in canonical (Ok, Err) order.
                            let ok_payload = layout.variants.get("Ok").and_then(|v| v.payload_ty.clone());
                            let err_payload = layout.variants.get("Err").and_then(|v| v.payload_ty.clone());
                            if let (Some(ok_ty), Some(err_ty)) = (ok_payload, err_payload) {
                                let args: Vec<InferType> = vec![
                                    InferType::Con(Self::llvm_ty_to_infer_name(&ok_ty)),
                                    InferType::Con(Self::llvm_ty_to_infer_name(&err_ty)),
                                ];
                                let res = InferType::App(self.reg.result_enum_name(), args);
                                self.ensure_enum_layout_for_infer(&res);
                                return Some(res);
                            } else {
                                // Fallback: preserve old behaviour (tag-ordered) if Ok/Err names not present.
                                let mut sorted: Vec<_> = layout.variants.values().collect();
                                sorted.sort_by_key(|v| v.tag_value);
                                let args: Vec<InferType> = sorted
                                    .iter()
                                    .filter_map(|v| v.payload_ty.as_ref())
                                    .map(|llvm_ty| InferType::Con(Self::llvm_ty_to_infer_name(llvm_ty)))
                                    .collect();
                                if !args.is_empty() {
                                    let res = InferType::App(self.reg.result_enum_name(), args);
                                    self.ensure_enum_layout_for_infer(&res);
                                    return Some(res);
                                }
                            }
                        }
                    }
                }
                if matches!(field.name.as_str(), "recv" | "try_recv") {
                    if let ExpressionKind::Identifier(id) = &base.node {
                        if let Some(elem_ty) = self.chan_elem_tys.get(&id.name) {
                            let inner = InferType::Con(elem_ty.clone());
                            return Some(InferType::App(self.reg.option_enum_name(), vec![inner]));
                        }
                    }
                }
            }
        }
        None
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

    // ── Enum layout management ────────────────────────────────────────────────

    fn ensure_enum_layout_for_infer(&mut self, ty: &InferType) {
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
        let option_name = self.reg.option_enum_name();
        let result_name = self.reg.result_enum_name();
        let mut lower_payload =
            |payload: &EnumVariantPayloadKind, subst: &HashMap<String, String>| -> Option<String> {
                Self::lower_enum_payload(payload, subst, &option_name, &result_name)
            };
        self.reg
            .ensure_enum_layout(&def, &llvm_args, &|_, _| String::new(), &mut lower_payload);
    }

    fn ensure_enum_layout_for_type(&mut self, ty: &Type) {
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
            .map(|a| Self::lower_type(a, &self.option_enum_name(), &self.result_enum_name()))
            .collect();
        let option_name = self.reg.option_enum_name();
        let result_name = self.reg.result_enum_name();
        let mut lower_payload =
            |payload: &EnumVariantPayloadKind, subst: &HashMap<String, String>| -> Option<String> {
                Self::lower_enum_payload(payload, subst, &option_name, &result_name)
            };
        self.reg
            .ensure_enum_layout(&def, &llvm_args, &|_, _| String::new(), &mut lower_payload);
    }

    fn lower_enum_payload(
        payload: &EnumVariantPayloadKind,
        subst: &HashMap<String, String>,
        option_name: &str,
        result_name: &str,
    ) -> Option<String> {
        // For now, support the subset Typhoon uses for Option/Result-like enums:
        // - unit payload `Some(T)` encoded as `Unit(T)`
        // - 1-tuple payload `Some(T)` encoded as `Tuple([T])`
        // Anything more complex should be lowered via a dedicated struct type later.
        match payload {
            EnumVariantPayloadKind::Unit(t) => Some(Self::lower_type_with_subst(
                t,
                subst,
                option_name,
                result_name,
            )),
            EnumVariantPayloadKind::Tuple(ts) if ts.len() == 1 => Some(
                Self::lower_type_with_subst(&ts[0], subst, option_name, result_name),
            ),
            _ => None,
        }
    }

    fn lower_type_with_subst(
        ty: &Type,
        subst: &HashMap<String, String>,
        option_name: &str,
        result_name: &str,
    ) -> String {
        // If the type refers directly to a generic parameter, substitute the concrete LLVM type.
        if ty.node.generic_args.is_empty() {
            if let Some(v) = subst.get(&ty.node.name) {
                return v.clone();
            }
        }
        // Otherwise lower normally; nested generic args in enum payloads aren't supported yet.
        Self::lower_type(ty, option_name, result_name)
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
        // Ensure concrete enum layouts for any enum types mentioned in signatures/fields.
        if self.reg.enum_defs.contains_key(ty.node.name.as_str()) {
            self.ensure_enum_layout_for_type(ty);
        }
    }

    fn ensure_adt_for_infertype(&mut self, ty: &InferType) {
        match ty {
            InferType::App(_, args) => {
                for a in args {
                    self.ensure_adt_for_infertype(a);
                }
                self.ensure_enum_layout_for_infer(ty);
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

    /// Convert an LLVM type string back to a source-level type name
    /// suitable for use in InferType::Con, so lower_infer_type can
    /// map it correctly.
    fn llvm_ty_to_infer_name(llvm_ty: &str) -> String {
        match llvm_ty {
            "i8" => "Int8".to_string(),
            "i16" => "Int16".to_string(),
            "i32" => "Int32".to_string(),
            "i64" => "Int64".to_string(),
            "i1" => "Bool".to_string(),
            "i8*" => "Str".to_string(),
            _ => {
                // "%struct.Listener" -> "Listener"
                llvm_ty
                    .strip_prefix("%struct.")
                    .unwrap_or(llvm_ty)
                    .to_string()
            }
        }
    }

    fn llvm_const_alignof(&self, ty: &str) -> i64 {
        self.llvm_const_sizeof(ty)
    }

    fn emit_widen(&mut self, val: &str, actual_ty: &str, expected_ty: &str) -> String {
        if actual_ty == expected_ty {
            return val.to_string();
        }
        // LLVM pointer-typed arguments must use `null`, not integer `0`.
        // Some fallback expression paths produce an integer zero literal;
        // coerce that case when the callee expects a pointer.
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

    fn value_llvm_type(&self, value: &str) -> Option<String> {
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

    fn fixed_array_len(&self, array_ty: &str) -> Option<usize> {
        let end = array_ty.find(']')?;
        let inner = &array_ty[1..end];
        inner[..inner.find(' ')?].trim().parse().ok()
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
                llvm_escape(bytes)
            ));
            let pair = (global.clone(), n);
            self.reg.string_pool.insert(s.to_string(), pair.clone());
            pair
        };
        let tmp = self.tmp();
        self.emit(format!(
            "  {} = getelementptr inbounds [{} x i8], [{} x i8]* {}, i32 0, i32 0",
            tmp, n, n, global
        ));
        tmp
    }

    // ── Captured variable analysis ────────────────────────────────────────────

    fn collect_captured_vars(&self, block: &Block) -> Vec<String> {
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

    fn visit_stmt_identifiers(&self, stmt: &Statement, f: &mut dyn FnMut(&str)) {
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
                if let Some(eb) = else_branch {
                    match &eb.node {
                        ElseBranchKind::Block(b) => {
                            for s in &b.statements {
                                self.visit_stmt_identifiers(s, f);
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
            }
            _ => {}
        }
    }

    fn visit_pattern_identifiers(&self, pattern: &Pattern, f: &mut dyn FnMut(&str)) {
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

    fn visit_expr_identifiers(&self, expr: &Expression, f: &mut dyn FnMut(&str)) {
        if let ExpressionKind::Identifier(id) = &expr.node {
            f(&id.name);
        }
        match &expr.node {
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

    fn chan_elem_type_from_annotation(ty: &Type) -> Option<&Type> {
        match ty.node.name.as_str() {
            "Ref" if ty.node.generic_args.len() == 1 => {
                Self::chan_elem_type_from_annotation(&ty.node.generic_args[0])
            }
            "Chan" if ty.node.generic_args.len() == 1 => Some(&ty.node.generic_args[0]),
            _ => None,
        }
    }
}

// ── Entry point (public API) ──────────────────────────────────────────────────

pub struct Codegen;

impl Codegen {
    pub fn lower_module(
        module: &Module,
        types: &HashMap<NodeId, InferType>,
        specializations: &HashMap<(String, Vec<InferType>), String>,
        drop_map: &HashMap<NodeId, Vec<DropInfo>>,
    ) -> IrModule {
        IrBuilder::lower_module(module, types, specializations, drop_map)
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
        .replace("%struct.", "struct_")
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
        checker
            .check_module(&module, &std::collections::HashMap::new())
            .unwrap();
        let mut liveness = crate::liveness::LiveAnalyzer::new();
        let drop_map = liveness
            .analyze_module(&module)
            .unwrap_or(&std::collections::HashMap::new())
            .clone();
        Codegen::lower_module(
            &module,
            checker.types(),
            &checker.specializations,
            &drop_map,
        )
        .to_llvm_ir()
    }

    #[test]
    fn lowers_function_declarations() {
        assert!(compile("fn id(a: Int32) -> Int32 { return a; }")
            .contains("define i32 @id(i8* %task, i32 %a)"));
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
        assert!(
            text.contains("alloca i32"),
            "expected alloca for immutable scalar let"
        );
        assert!(text.contains("store i32 3"));
        assert!(text.contains("load i32"));
    }

    #[test]
    fn try_recv_yields_until_value_or_close() {
        let text = compile("enum Option<T> { Some(T) None } fn main() -> Int32 { let ch: ref chan<Int32> = chan<Int32>(); match ch.try_recv() { Some(v) => { return v; } None => { return 0; } } }");
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
        let text = compile("enum Option<T> { Some(T) None } fn main() -> Int32 { let mut xs: Array<Int32> = [1,2]; xs.push(3); let v: Option<Int32> = xs[0]; return 0; }");
        assert!(text.contains("@ty_array_push"));
        assert!(text.contains("@ty_array_get_ptr"));
    }

    #[test]
    fn lowers_result_constructors_to_aggregate_values() {
        let text = compile(
            "enum Result<T, E> { Ok(T) Err(E) } fn main() -> Result<Int32, Str> { return Ok(1); }",
        );
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
        let text = compile("enum Result<T, E> { Ok(T) Err(E) } fn main(x: Result<Int32, Str>) -> Int32 { if let Ok(v) = x { return v; } else { return 0; } }");
        assert!(text.contains("iflet_then"));
        assert!(text.contains("extractvalue"));
        assert!(text.contains("iflet_merge"));
    }
}
