//! Type registry, enum layouts, and type lowering
//!
//! Handles type registration, preamble management, enum layout computation,
//! and AST-to-LLVM type lowering.

use crate::ast::*;
use crate::type_inference::InferType;
use crate::codegen::{mangle_llvm_type_name, int_suffix_to_llvm};
use std::collections::{HashMap, HashSet};

// ── Private types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct EnumDef {
    pub name: String,
    pub gen_params: Vec<String>,
    pub variants: Vec<EnumVariantDef>,
}

#[derive(Debug, Clone)]
pub struct EnumVariantDef {
    pub name: String,
    pub payload: Option<EnumVariantPayloadKind>,
}

#[derive(Debug, Clone)]
pub struct EnumLayout {
    #[allow(dead_code)]
    pub llvm_struct_ty: String,
    pub tag_ty: String,
    pub variants: HashMap<String, EnumVariantLayout>,
}

#[derive(Debug, Clone)]
pub struct EnumVariantLayout {
    pub tag_value: i64,
    pub payload_index: Option<usize>,
    pub payload_ty: Option<String>,
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
    pub annotations: Vec<String>,
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
            for ann in &func.annotations {
                out.push(ann.clone());
            }
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

// ── Type/preamble registry (split from emit state) ──────────────────────────────

/// Registry for type declarations, enum layouts, function signatures, and preamble.
/// Stable across function emission contexts.
pub struct TypeRegistry {
    pub type_decls: Vec<String>,
    pub extra_preamble: Vec<String>,
    pub struct_fields: HashMap<String, Vec<(String, String)>>,
    pub opaque_structs: HashSet<String>,
    pub default_factories: HashMap<String, String>,
    pub func_sigs: HashMap<String, (String, Vec<String>)>,
    pub extern_fns: HashSet<String>,
    pub out_result_funcs: HashSet<String>,
    pub string_pool: HashMap<String, (String, usize)>,
    pub enum_defs: HashMap<String, EnumDef>,
    pub enum_layouts: HashMap<String, EnumLayout>,
    pub declared_syms: HashSet<String>,
}

impl TypeRegistry {
    pub fn new() -> Self {
        Self {
            type_decls: Vec::new(),
            extra_preamble: Vec::new(),
            struct_fields: HashMap::new(),
            opaque_structs: HashSet::new(),
            default_factories: HashMap::new(),
            func_sigs: HashMap::new(),
            extern_fns: HashSet::new(),
            out_result_funcs: HashSet::new(),
            string_pool: HashMap::new(),
            enum_defs: HashMap::new(),
            enum_layouts: HashMap::new(),
            declared_syms: HashSet::new(),
        }
    }

    pub fn push_declare(&mut self, sym: &str, line: String) -> bool {
        if self.declared_syms.insert(sym.to_string()) {
            self.extra_preamble.push(line);
            true
        } else {
            false
        }
    }

    pub fn push_type_decl(&mut self, sym: &str, line: String) -> bool {
        let sym = sym
            .strip_prefix("%struct.")
            .or_else(|| sym.strip_prefix("%enum."))
            .or_else(|| sym.strip_prefix("%newtype."))
            .unwrap_or(sym);
        let fresh = !self.declared_syms.contains(sym);
        if fresh {
            self.declared_syms.insert(sym.to_string());
            self.type_decls.push(line);
            true
        } else {
            let is_opaque = line.contains("= type opaque");
            if !is_opaque {
                let stripped = sym;
                let prefix = if line.starts_with("%struct.") {
                    "%struct."
                } else if line.starts_with("%enum.") {
                    "%enum."
                } else if line.starts_with("%newtype.") {
                    "%newtype."
                } else {
                    ""
                };
                let old_opaque = format!("{}{} = type opaque", prefix, stripped);
                if let Some(pos) = self
                    .type_decls
                    .iter()
                    .position(|d| d.rsplit('\n').next() == Some(old_opaque.as_str()))
                {
                    self.type_decls[pos] = line;
                    return true;
                }
                self.type_decls.push(line);
                true
            } else {
                false
            }
        }
    }

    pub fn preamble(&self) -> Vec<String> {
        let mut p = self.type_decls.clone();
        p.extend(self.extra_preamble.iter().cloned());
        p
    }

    pub fn mangle_app_struct_name(name: &str, args: &[String]) -> String {
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

    pub fn struct_field_info(&self, struct_name: &str, field_name: &str) -> (usize, String) {
        if let Some(fields) = self.struct_fields.get(struct_name) {
            if let Some(idx) = fields.iter().position(|(n, _)| n == field_name) {
                return (idx, fields[idx].1.clone());
            }
        }
        (0, "i32".to_string())
    }

    pub fn method_symbol_for_call(&self, base_ty: &str, method: &str) -> Option<String> {
        base_ty
            .trim_end_matches('*')
            .strip_prefix("%struct.")
            .map(|name| format!("__ty_method__{}__{}", name, method))
    }

    /// Compute an enum layout from a definition + concrete type args, emit the
    /// type_decl, and store it. Does nothing if the layout already exists.
    pub fn ensure_enum_layout(
        &mut self,
        def: &EnumDef,
        llvm_args: &[String],
        _lower_type: &dyn Fn(&str, &str) -> String,
        lower_payload: &mut dyn FnMut(
            &EnumVariantPayloadKind,
            &HashMap<String, String>,
        ) -> Option<String>,
    ) -> String {
        let llvm_struct_ty = Self::mangle_app_struct_name(&def.name, llvm_args);
        if self.enum_layouts.contains_key(&llvm_struct_ty) {
            return llvm_struct_ty;
        }

        let mut subst = HashMap::<String, String>::new();
        for (p, a) in def.gen_params.iter().zip(llvm_args.iter()) {
            subst.insert(p.clone(), a.clone());
        }

        let tag_ty = "i32".to_string();
        let mut variants_layout = HashMap::new();
        let mut payload_fields: Vec<String> = Vec::new();

        for (tag_value, v) in def.variants.iter().enumerate() {
            let payload_ty = v.payload.as_ref().and_then(|p| lower_payload(p, &subst));
            let payload_index = payload_ty.as_ref().map(|ty| {
                payload_fields.push(ty.clone());
                payload_fields.len()
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
        self.push_type_decl(
            &llvm_struct_ty,
            format!("{} = type {}", llvm_struct_ty, body),
        );
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

    pub fn lower_type(&self, ty: &Type, opaque_structs: &HashSet<String>) -> String {
        Self::lower_type_with_opaque_structs(ty, opaque_structs)
    }

    /// Best-effort x86-64 size/alignment for a Typhoon AST type.
    pub fn scalar_type_size_align(ty: &Type) -> (usize, usize) {
        match ty.node.name.as_str() {
            "Int8" | "Bool" => (1, 1),
            "Int16" => (2, 2),
            "Int32" | "Float32" => (4, 4),
            "Int64" | "Float64" => (8, 8),
            _ => (8, 8),
        }
    }

    pub fn result_like_struct_size(ty: &Type) -> Option<usize> {
        if matches!(ty.node.name.as_str(), "Ref" | "ref" | "Chan" | "chan") {
            return None;
        }
        if ty.node.generic_args.is_empty() {
            return None;
        }
        let mut offset = 4usize;
        let mut max_align = 4usize;
        for arg in &ty.node.generic_args {
            let (sz, al) = Self::scalar_type_size_align(arg);
            offset = (offset + al - 1) / al * al;
            offset += sz;
            max_align = max_align.max(al);
        }
        Some((offset + max_align - 1) / max_align * max_align)
    }

    const MAX_REGISTER_RETURN_BYTES: usize = 8;

    pub fn needs_out_result_abi(return_type: &Option<Type>) -> bool {
        return_type
            .as_ref()
            .and_then(Self::result_like_struct_size)
            .map(|sz| sz > Self::MAX_REGISTER_RETURN_BYTES)
            .unwrap_or(false)
    }

    pub fn lower_type_with_opaque_structs(ty: &Type, opaque_structs: &HashSet<String>) -> String {
        let ty_name = ty.node.name.as_str();

        match ty_name {
            "Array" => return "%struct.TyArray*".to_string(),
            "Chan" => return "i8*".to_string(),
            "Ref" => return "i8*".to_string(),
            "Str" => return "%struct.Str*".to_string(),
            _ => {}
        }

        if !ty.node.generic_args.is_empty() {
            let args: Vec<_> = ty
                .node
                .generic_args
                .iter()
                .map(|a| Self::lower_type_with_opaque_structs(a, opaque_structs))
                .collect();
            return Self::mangle_app_struct_name(ty_name, &args);
        }

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
            "ref" | "Ref" => "i8*".to_string(),
            "Str" => "%struct.Str*".to_string(),
            name if opaque_structs.contains(name) => format!("%struct.{}*", name),
            name => format!("%struct.{}", name),
        }
    }

    pub fn lower_infer_type(&mut self, ty: &InferType) -> String {
        match ty {
            InferType::Con(name) => match name.as_str() {
                "Unit" => "void".to_string(),
                "Int8" => "i8".to_string(),
                "Int16" => "i16".to_string(),
                "Int32" => "i32".to_string(),
                "Int64" => "i64".to_string(),
                "Bool" => "i1".to_string(),
                "Str" => "%struct.Str*".to_string(),
                "Chan" | "chan" => "i8*".to_string(),
                n if self.opaque_structs.contains(n) => format!("%struct.{}*", n),
                n if n.starts_with("%") => n.to_string(),
                n => format!("%struct.{}", n),
            },
            InferType::App(name, args) if name == "Ref" && args.len() == 1 => "i8*".to_string(),
            InferType::App(name, _) if name == "Array" => "%struct.TyArray*".to_string(),
            InferType::App(name, _) if name == "Chan" => "i8*".to_string(),
            InferType::App(name, args) => {
                let llvm_args: Vec<String> =
                    args.iter().map(|a| self.lower_infer_type(a)).collect();
                self.ensure_enum_layout_for_infer(ty);
                Self::mangle_app_struct_name(name, &llvm_args)
            }
            InferType::FixedArray(elem, n) => format!("[{} x {}]", n, self.lower_infer_type(elem)),
            _ => "i32".to_string(),
        }
    }

    pub fn ensure_enum_layout_for_infer(&mut self, ty: &InferType) {
        let InferType::App(name, args) = ty else {
            return;
        };
        let Some(def) = self.enum_defs.get(name).cloned() else {
            return;
        };
        if def.gen_params.len() != args.len() {
            return;
        }

        let llvm_args: Vec<String> = args.iter().map(|a| self.lower_infer_type(a)).collect();
        let opaque_structs = self.opaque_structs.clone();
        let mut lower_payload =
            |payload: &EnumVariantPayloadKind, subst: &HashMap<String, String>| -> Option<String> {
                Self::lower_enum_payload(payload, subst, &opaque_structs)
            };

        self.ensure_enum_layout(&def, &llvm_args, &|_, _| String::new(), &mut lower_payload);
    }

    pub fn ensure_enum_layout_for_type(&mut self, ty: &Type) {
        let name = ty.node.name.as_str();
        let Some(def) = self.enum_defs.get(name).cloned() else {
            return;
        };
        if def.gen_params.len() != ty.node.generic_args.len() {
            return;
        }
        let llvm_args: Vec<String> = ty
            .node
            .generic_args
            .iter()
            .map(|a| Self::lower_type_with_opaque_structs(a, &self.opaque_structs))
            .collect();
        let opaque_structs = self.opaque_structs.clone();
        let mut lower_payload =
            |payload: &EnumVariantPayloadKind, subst: &HashMap<String, String>| -> Option<String> {
                Self::lower_enum_payload(payload, subst, &opaque_structs)
            };
        self.ensure_enum_layout(&def, &llvm_args, &|_, _| String::new(), &mut lower_payload);
    }

    pub fn lower_enum_payload(
        payload: &EnumVariantPayloadKind,
        subst: &HashMap<String, String>,
        opaque_structs: &HashSet<String>,
    ) -> Option<String> {
        match payload {
            EnumVariantPayloadKind::Unit(t) => {
                Some(Self::lower_type_with_subst(t, subst, opaque_structs))
            }
            EnumVariantPayloadKind::Tuple(ts) if ts.len() == 1 => {
                Some(Self::lower_type_with_subst(&ts[0], subst, opaque_structs))
            }
            _ => None,
        }
    }

    pub fn lower_type_with_subst(
        ty: &Type,
        subst: &HashMap<String, String>,
        opaque_structs: &HashSet<String>,
    ) -> String {
        if ty.node.generic_args.is_empty() {
            if let Some(v) = subst.get(&ty.node.name) {
                return v.clone();
            }
        }
        Self::lower_type_with_opaque_structs(ty, opaque_structs)
    }

    pub fn scan_decl_for_adts(&mut self, decl: &Declaration) {
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
            DeclarationKind::UnsafeOrExtern(uoe) => {
                if let UnsafeOrExternKind::Extern { declarations, .. } = &uoe.node {
                    for sig in declarations {
                        for p in &sig.node.params {
                            self.ensure_adt_for_type(&p.type_annotation);
                        }
                        if let Some(ret) = &sig.node.return_type {
                            self.ensure_adt_for_type(ret);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub fn ensure_adt_for_type(&mut self, ty: &Type) {
        for arg in &ty.node.generic_args {
            self.ensure_adt_for_type(arg);
        }
        if self.enum_defs.contains_key(ty.node.name.as_str()) {
            self.ensure_enum_layout_for_type(ty);
        }
    }

    pub fn ensure_adt_for_infertype(&mut self, ty: &InferType) {
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

    pub fn llvm_const_sizeof(&self, ty: &str) -> i64 {
        match ty {
            "i1" | "i8" => 1,
            "i16" => 2,
            "i32" | "float" => 4,
            "i64" | "double" => 8,
            _ => 8,
        }
    }

    pub fn llvm_ty_to_infer_name(llvm_ty: &str) -> String {
        match llvm_ty {
            "i8" => "Int8".to_string(),
            "i16" => "Int16".to_string(),
            "i32" => "Int32".to_string(),
            "i64" => "Int64".to_string(),
            "i1" => "Bool".to_string(),
            "i8*" => "Ref".to_string(),
            "%struct.Str" => "Str".to_string(),
            _ => {
                llvm_ty
                    .strip_prefix("%struct.")
                    .unwrap_or(llvm_ty)
                    .to_string()
            }
        }
    }

    pub fn llvm_const_alignof(&self, ty: &str) -> i64 {
        self.llvm_const_sizeof(ty)
    }

    pub fn zero_value(&self, ty: &str) -> String {
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

    pub fn fixed_array_len(&self, array_ty: &str) -> Option<usize> {
        let end = array_ty.find(']')?;
        let inner = &array_ty[1..end];
        inner[..inner.find(' ')?].trim().parse().ok()
    }

    pub fn infer_elem_ty(&self, elems: &[Expression]) -> String {
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
}

// Need access to emit methods - will be added when moving IrBuilder methods
// For now, this module defines the registry types

// Re-export for IrBuilder
pub use self::TypeRegistry as TypeRegistryInternal;