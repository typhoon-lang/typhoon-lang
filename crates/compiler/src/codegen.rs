//! LLVM IR Code Generation for Typhoon
//!
//! This module provides the main codegen pipeline: lowering a typed AST
//! (with liveness drop map) into an LLVM IR module string.

mod typeregistry;
mod ir_builder;
mod conc;
mod types;
mod emit_stmt;
mod emit_expr;
mod emit_match;
mod pattern;
mod emit;

pub use typeregistry::*;
pub use ir_builder::*;
// Internal modules - not re-exported to avoid unused import warnings
// pub use conc::*;
// pub use types::*;
// pub use emit_stmt::*;
// pub use emit_expr::*;
// pub use emit_match::*;
// pub use pattern::*;
// pub use emit::*;

// ── Entry point (public API) ──────────────────────────────────────────────────

pub struct Codegen;

impl Codegen {
    pub fn lower_module(
        module: &Module,
        types: &HashMap<NodeId, InferType>,
        specializations: &HashMap<(String, Vec<InferType>), String>,
        drop_map: &HashMap<NodeId, Vec<DropInfo>>,
        original_ns_by_symbol: &HashMap<String, String>,
        enum_variants: &HashMap<String, (String, Vec<TypeVarId>, Option<InferType>)>,
    ) -> IrModule {
        crate::codegen::ir_builder::IrBuilder::lower_module(
            module,
            types,
            specializations,
            drop_map,
            original_ns_by_symbol,
            enum_variants,
        )
    }
}

// Re-export TypeVarId for external use
pub use crate::type_inference::TypeVarId;

// ── Free functions ────────────────────────────────────────────────────────────

pub fn is_main(name: &str) -> bool {
    name == "main" || name.ends_with("__main")
}

pub fn int_suffix_to_llvm(suffix: &str) -> &'static str {
    match suffix {
        "i8" | "u8" => "i8",
        "i16" => "i16",
        "i64" => "i64",
        _ => "i32",
    }
}

pub fn get_size_class(size: i64) -> u32 {
    match size {
        0..=8 => 0,
        9..=16 => 1,
        17..=32 => 2,
        33..=64 => 3,
        65..=128 => 4,
        _ => 5,
    }
}

pub fn array_elem_type_from_str(array_ty: &str) -> String {
    if let (Some(x), Some(end)) = (array_ty.find(" x "), array_ty.find(']')) {
        array_ty[x + 3..end].to_string()
    } else {
        "i32".to_string()
    }
}

pub fn is_no_task_intrinsic(name: &str) -> bool {
    matches!(
        name,
        "ty_array_get_ptr"
            | "ty_yield"
            | "ty_chan_new"
            | "ty_chan_close"
            | "slab_arena_new"
            | "ty_sys_write"
            | "ty_sys_read"
            | "ty_str_len"
            | "ty_str_byte"
            | "ty_net_global"
    )
}

pub fn runtime_intrinsic_name(name: &str) -> Option<String> {
    match name {
        "__ty_buf_new" => Some("ty_buf_new".to_string()),
        "__ty_buf_push_str" => Some("ty_buf_push_str".to_string()),
        "__ty_buf_into_str" => Some("ty_buf_into_str".to_string()),
        "spawn" => Some("ty_spawn".to_string()),
        "yield" => Some("ty_yield".to_string()),
        "await" => Some("ty_await".to_string()),
        _ => None,
    }
}

// ── IR annotation helpers ────────────────────────────────────────────────────

/// Reconstruct a Typhoon-level type name from an AST Type node.
pub fn ty_type_name(ty: &Type) -> String {
    let base = ty.node.name.as_str();
    if ty.node.generic_args.is_empty() {
        base.to_string()
    } else {
        let args: Vec<String> = ty.node.generic_args.iter().map(ty_type_name).collect();
        format!("{}<{}>", base, args.join(", "))
    }
}

/// Build the `; @ty_sig: fn name(p: T, ...) -> R` string for a function.
pub fn ty_sig_line(fn_name: &str, params: &[Parameter], return_type: Option<&Type>) -> String {
    let param_strs: Vec<String> = params
        .iter()
        .map(|p| format!("{}: {}", p.name.name, ty_type_name(&p.type_annotation)))
        .collect();
    let ret_part = return_type
        .map(|ty| format!(" -> {}", ty_type_name(ty)))
        .unwrap_or_default();
    format!(
        "; @ty_sig: fn {}({}){}",
        fn_name,
        param_strs.join(", "),
        ret_part
    )
}

pub fn annotation_ns_for_decl<'a>(
    module_ns: &'a str,
    decl: &Declaration,
    original_ns_by_symbol: &'a HashMap<String, String>,
) -> Option<&'a str> {
    match &decl.node {
        DeclarationKind::Struct { name, .. }
        | DeclarationKind::Enum { name, .. }
        | DeclarationKind::Newtype { name, .. }
        | DeclarationKind::Function { name, .. } => {
            annotation_ns_for_symbol(module_ns, &name.name, original_ns_by_symbol)
        }
        DeclarationKind::UnsafeOrExtern(Spanned {
            node: UnsafeOrExternKind::Extern { declarations, .. },
            ..
        }) => declarations.first().and_then(|sig| {
            annotation_ns_for_symbol(module_ns, &sig.node.name.name, original_ns_by_symbol)
        }),
        _ => {
            if module_ns.is_empty() {
                None
            } else {
                Some(module_ns)
            }
        }
    }
}

pub fn annotation_ns_for_symbol<'a>(
    module_ns: &'a str,
    symbol: &str,
    original_ns_by_symbol: &'a HashMap<String, String>,
) -> Option<&'a str> {
    original_ns_by_symbol
        .get(symbol)
        .map(String::as_str)
        .or_else(|| (!module_ns.is_empty()).then_some(module_ns))
}

/// Build the annotation vec for a user-defined function.
pub fn build_fn_annotations(
    decl_ns: Option<&str>,
    fn_name: &str,
    params: &[Parameter],
    return_type: Option<&Type>,
) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(ns) = decl_ns {
        if !ns.is_empty() {
            out.push(format!("; @ty_ns: {}", ns));
        }
    }
    out.push(ty_sig_line(fn_name, params, return_type));
    out
}

pub fn link_symbol_name(name: &str) -> String {
    if name == "main" || name == "main__main" {
        "__ty_user_main".to_string()
    } else {
        name.to_string()
    }
}

pub fn llvm_escape(bytes: &[u8]) -> String {
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

pub fn mangle_llvm_type_name(llvm_ty: &str) -> String {
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

/// Inverse of mangle_llvm_type_name.
pub fn parse_enum_from_mangled(mangled: &str) -> Option<(String, Vec<String>)> {
    let known_prefixes = ["Result", "Option"];
    for prefix in &known_prefixes {
        if let Some(suffix) = mangled.strip_prefix(prefix) {
            if suffix.is_empty() {
                return Some((prefix.to_string(), vec![]));
            }
            let payload_str = suffix.strip_prefix("__")?;
            let llvm_types: Vec<String> = payload_str
                .split("__")
                .map(|seg| unmangle_llvm_type_segment(seg))
                .filter(|s| !s.is_empty())
                .collect();
            return Some((prefix.to_string(), llvm_types));
        }
    }
    None
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

pub fn unmangle_llvm_type_segment(name: &str) -> String {
    let mut s = name.to_string();
    if s.ends_with("ptr") {
        s = format!("{}*", s.strip_suffix("ptr").unwrap());
    }
    if let Some(inner) = s.strip_prefix("struct_") {
        s = format!("%struct.{}", inner);
    }
    s
}

// Re-export required types from parent modules
pub use crate::ast::{NodeId, Spanned, Declaration, DeclarationKind, Parameter, Type, TypeKind, Expression, ExpressionKind, Literal, LiteralKind, Block, Statement, StatementKind, Pattern, PatternKind, Operator, MatchArm, EnumVariantKind, EnumVariantPayloadKind, UnsafeOrExternKind, FunctionSignatureKind, UsePathKind, ElseBranchKind, LoopKindKind, Module, EnumVariant};
pub use crate::codegen::typeregistry::{EnumDef, EnumVariantDef, EnumVariantLayout, IrModule};
pub use crate::span::Span;
pub use crate::type_inference::InferType;
pub use crate::liveness::DropInfo;
use std::collections::HashMap;