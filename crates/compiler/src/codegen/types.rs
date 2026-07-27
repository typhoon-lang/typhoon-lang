//! Type lowering and enum layout management
//!
//! Handles AST-to-LLVM type lowering, enum layout computation, and type utilities.

use crate::ast::*;
use crate::codegen::typeregistry::{TypeRegistry, EnumDef, EnumVariantDef};
use crate::codegen::ir_builder::IrBuilder;
use crate::codegen::{is_no_task_intrinsic, ty_type_name, annotation_ns_for_decl, annotation_ns_for_symbol};

impl<'a> IrBuilder<'a> {
    // ── Type collection ───────────────────────────────────────────────────────

    pub fn collect_types(&mut self, module: &Module) {
        self.reg = TypeRegistry::new();
        self.adt_structs.clear();

        self.reg.push_type_decl(
            "TyArray",
            "%struct.TyArray = type { i8*, i64, i64, i64, i64 }".to_string(),
        );

        self.reg
            .push_type_decl("Str", "%struct.Str = type { i8*, i32 }".to_string());

        self.collect_enum_defs(module);
        self.register_module_sigs(module);
        self.register_runtime_decls();

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
    }

    fn register_runtime_decls(&mut self) {
        let decls = [
            "declare void @ty_sched_init     ()",
            "declare void @ty_sched_run      ()",
            "declare void @ty_sched_shutdown ()",
            "declare i8*  @ty_spawn          (i8*, i8*, i8*)",
            "declare i8*  @ty_spawn_closure  (i8*, i8*, i8*, i64)",
            "declare void @ty_yield          ()",
            "declare void @ty_safepoint      ()",
            "declare void @ty_await          (i8*, i8*)",
            "declare i8*  @ty_chan_new       (i64, i64)",
            "declare void @ty_chan_send      (i8*, i8*, i8*)",
            "declare void @ty_chan_recv      (i8*, i8*, i8*)",
            "declare i32  @ty_chan_try_recv  (i8*, i8*, i8*)",
            "declare void @ty_chan_close     (i8*)",
            "declare %struct.TyArray* @ty_array_from_fixed (i8*, i8*, i64, i64, i64)",
            "declare void             @ty_array_push       (i8*, %struct.TyArray*, i8*)",
            "declare i8*              @ty_array_get_ptr    (%struct.TyArray*, i64)",
            "declare i8*  @slab_arena_new  ()",
            "declare i8*  @slab_alloc      (i8* %task, i32 %size_class)",
            "declare void @slab_free       (i8* %task, i8* %ptr, i32 %size_class)",
            "declare void @slab_arena_free (i8*)",
            "declare void @ty_io_subsystem_init     ()",
            "declare void @ty_io_subsystem_shutdown ()",
            "declare void @ty_net_init              ()",
            "declare void @ty_net_shutdown          ()",
            "declare i64  @ty_sys_write (i32 %fd, i8* %buf, i64 %len)",
            "declare i64  @ty_sys_read  (i32 %fd, i8* %buf, i64 %len)",
        ];
        for d in decls {
            let sym = d
                .split('@')
                .nth(1)
                .and_then(|s| s.split('(').next())
                .unwrap_or(d)
                .trim()
                .to_string();
            self.reg.push_declare(&sym, d.to_string());
        }

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
    }

    fn register_module_sigs(&mut self, module: &Module) {
        let module_ns = module.name.as_deref().unwrap_or("");
        self.reg
            .opaque_structs
            .extend(
                module
                    .declarations
                    .iter()
                    .filter_map(|decl| match &decl.node {
                        DeclarationKind::Struct { name, fields, .. } if fields.is_empty() => {
                            Some(name.name.clone())
                        }
                        _ => None,
                    }),
            );
        for decl in &module.declarations {
            let ns_comment = annotation_ns_for_decl(module_ns, decl, self.original_ns_by_symbol)
                .map(|ns| format!("; @ty_ns: {}", ns));
            match &decl.node {
                DeclarationKind::Struct { name, fields, .. } => {
                    let mut field_types = Vec::new();
                    let mut field_map = Vec::new();
                    for (id, ty) in fields {
                        let lt = self.reg.lower_type(ty, &self.reg.opaque_structs);
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
                    let annotated_line = if let Some(ref ns) = ns_comment {
                        format!("{}\n{}", ns, line)
                    } else {
                        line
                    };
                    self.reg.push_type_decl(&name.name, annotated_line);
                    self.reg.struct_fields.insert(name.name.clone(), field_map);
                }
                DeclarationKind::Enum { name, .. } => {
                    let line = format!("%enum.{} = type opaque", name.name);
                    let annotated = if let Some(ref ns) = ns_comment {
                        format!("{}\n{}", ns, line)
                    } else {
                        line
                    };
                    self.reg.push_type_decl(&name.name, annotated);
                }
                DeclarationKind::Newtype { name, type_alias } => {
                    let line = format!(
                        "%newtype.{} = type {}",
                        name.name,
                        self.reg.lower_type(type_alias, &self.reg.opaque_structs)
                    );
                    let annotated = if let Some(ref ns) = ns_comment {
                        format!("{}\n{}", ns, line)
                    } else {
                        line
                    };
                    self.reg.push_type_decl(&name.name, annotated);
                }
                DeclarationKind::Function {
                    name,
                    return_type,
                    params,
                    ..
                } => {
                    let ret_ty = return_type
                        .as_ref()
                        .map(|ty| self.reg.lower_type(ty, &self.reg.opaque_structs))
                        .unwrap_or_else(|| "void".to_string());
                    let mut param_types: Vec<String> = params
                        .iter()
                        .map(|p| self.reg.lower_type(&p.type_annotation, &self.reg.opaque_structs))
                        .collect();
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
                                out_result,
                                ..
                            } = node;
                            let out_result = *out_result || IrBuilder::needs_out_result_abi(return_type);
                            let out_result = &out_result;
                            let ty_sig_ret = return_type
                                .as_ref()
                                .map(|ty| self.reg.lower_type(ty, &self.reg.opaque_structs))
                                .unwrap_or_else(|| "void".to_string());
                            let mut param_types: Vec<String> = params
                                .iter()
                                .map(|p| self.reg.lower_type(&p.type_annotation, &self.reg.opaque_structs))
                                .collect();
                            let is_method_stub = name.name.starts_with("__ty_method__");
                            if params.is_empty()
                                && ty_sig_ret.starts_with("%struct.")
                                && ty_sig_ret.ends_with('*')
                            {
                                self.reg
                                    .default_factories
                                    .entry(ty_sig_ret.clone())
                                    .or_insert_with(|| name.name.clone());
                            }

                            let mut ann_lines: Vec<String> = Vec::new();
                            if let Some(ns) = annotation_ns_for_symbol(
                                module_ns,
                                &name.name,
                                self.original_ns_by_symbol,
                            ) {
                                ann_lines.push(format!("; @ty_ns: {}", ns));
                            }
                            let sig_params: Vec<String> = params
                                .iter()
                                .map(|p| {
                                    format!("{}: {}", p.name.name, ty_type_name(&p.type_annotation))
                                })
                                .collect();
                            let sig_ret = return_type
                                .as_ref()
                                .map(|ty| format!(" -> {}", ty_type_name(ty)))
                                .unwrap_or_default();
                            ann_lines.push(format!(
                                "; @ty_sig: fn {}({}){}",
                                name.name,
                                sig_params.join(", "),
                                sig_ret
                            ));
                            if *out_result {
                                ann_lines.push("; @ty_out_result".to_string());
                            }

                            if *out_result {
                                self.reg.out_result_funcs.insert(name.name.clone());
                                let mut c_param_types = vec!["i8*".to_string()];
                                c_param_types.extend(param_types.clone());
                                c_param_types.push(format!("{}*", ty_sig_ret));
                                let decl_line = format!(
                                    "declare void @{}({})",
                                    name.name,
                                    c_param_types.join(", ")
                                );
                                let annotated = format!(
                                    "{}\n{}",
                                    ann_lines.join("\n"),
                                    decl_line
                                );
                                self.reg.push_declare(&name.name, annotated);
                                self.reg
                                    .func_sigs
                                    .insert(name.name.clone(), ("void".to_string(), c_param_types));
                            } else {
                                if is_method_stub {
                                    param_types.insert(0, "i8*".to_string());
                                } else if !is_no_task_intrinsic(&name.name) {
                                    param_types.insert(0, "i8*".to_string());
                                }
                                let decl_line = format!(
                                    "declare {} @{}({})",
                                    ty_sig_ret,
                                    name.name,
                                    param_types.join(", ")
                                );
                                let annotated = format!(
                                    "{}\n{}",
                                    ann_lines.join("\n"),
                                    decl_line
                                );
                                self.reg.push_declare(&name.name, annotated);
                                self.reg
                                    .func_sigs
                                    .insert(name.name.clone(), (ty_sig_ret, param_types));
                                if !is_method_stub && is_no_task_intrinsic(&name.name) {
                                    self.reg.extern_fns.insert(name.name.clone());
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}