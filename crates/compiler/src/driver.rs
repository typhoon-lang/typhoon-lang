use crate::ast::NodeId;
use crate::ast::*;
use crate::desugar::Desugar;
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::span::Span;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
struct NamespaceUnit {
    declarations: Vec<Declaration>,
    uses: Vec<UsePath>,
}

pub fn collect_ty_files(path: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let mut stack = Vec::new();

    if path.is_dir() {
        stack.push(path.to_path_buf());
    } else {
        out.push(path.to_path_buf());
    }

    while let Some(dir) = stack.pop() {
        let entries =
            fs::read_dir(&dir).map_err(|e| format!("read_dir {}: {}", dir.display(), e))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("read_dir entry {}: {}", dir.display(), e))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) == Some("ty") {
                out.push(path);
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn parse_file(path: &Path) -> Result<Module, String> {
    let source = fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let tokens = Lexer::new(source).tokenize();
    Parser::new(tokens).parse_module()
}

fn mangle(ns: &str, name: &str) -> String {
    if ns.starts_with("std") {
        return name.to_string();
    }
    let ns = ns.replace("::", "__");
    format!("{}__{}", ns, name)
}

// ── LLVM IR Namespace Import ─────────────────────────────────────────────────

/// Parses LLVM IR directly into a NamespaceUnit for stdlib integration.
/// Replaces the round-trip: .ll → .ty source → parse_module()
fn parse_llvm_to_namespace_unit(
    ll_source: &str,
) -> Result<HashMap<String, NamespaceUnit>, Vec<String>> {
    let mut units: HashMap<String, NamespaceUnit> = HashMap::new();

    // Track method signatures by receiver type for impl block generation
    let mut method_sigs: std::collections::HashMap<String, Vec<FunctionSignatureKind>> =
        std::collections::HashMap::new();
    // Track pending @ty_sig: annotation
    let mut pending_ty_sig: Option<String> = None;
    let mut pending_ty_ns: Option<String> = None;

    let mut push_decl = |ns: &str, decl: Declaration| {
        let entry = units
            .entry(ns.to_string())
            .or_insert_with(|| NamespaceUnit {
                declarations: vec![],
                uses: vec![],
            });
        entry.declarations.push(decl);
    };

    for line in ll_source.lines() {
        let line = line.trim();

        if line.is_empty() {
            pending_ty_sig = None;
            pending_ty_ns = None;
            continue;
        }

        if let Some(ns) = line.strip_prefix("; @ty_ns:") {
            pending_ty_ns = Some(ns.trim().to_string());
            continue;
        }

        // ; @ty_sig: fn consume(self, ch: ref chan<Int8>)
        if let Some(sig) = line.strip_prefix("; @ty_sig:") {
            pending_ty_sig = Some(sig.trim().to_string());
            continue;
        }
        if line.starts_with(';') {
            continue;
        }

        // %struct.Foo = type { ... } or %struct.Foo = type opaque
        if let Some(rest) = line.strip_prefix("%struct.") {
            if let Some(name) = rest.split('=').next().map(|s| s.trim()) {
                // Strip pointer suffix for type name
                let name = name.trim_end_matches('*');
                if !name.is_empty() {
                    let ty_name = if name == "TyArray" { "Array" } else { name };
                    let ns = pending_ty_ns.clone().unwrap_or_else(|| "std".to_string());
                    push_decl(&ns, make_struct_decl(ty_name, &ns));
                }
            }
            pending_ty_ns = None;
            continue;
        }

        // %enum.Option<T> = type { Some(T), None } or %enum.Result<T, E> = type { Ok(T), Err(E) }
        if let Some(rest) = line.strip_prefix("%enum.") {
            if let Some(lhs) = rest.split('=').next().map(|s| s.trim()) {
                if let Some(body) = rest.split('{').nth(1).and_then(|s| s.split('}').next()) {
                    let (enum_name, generics) = if let Some(i) = lhs.find('<') {
                        (&lhs[..i], Some(&lhs[i..]))
                    } else {
                        (lhs, None)
                    };
                    let ns = pending_ty_ns.clone().unwrap_or_else(|| "std".to_string());
                    push_decl(&ns, make_enum_decl(enum_name, generics, body));
                }
            }
            pending_ty_ns = None;
            continue;
        }

        // declare or define
        let method_rest = if let Some(r) = line.strip_prefix("declare ") {
            Some(("declare", r))
        } else if let Some(r) = line.strip_prefix("define ") {
            Some(("define", r))
        } else {
            None
        };

        if let Some((kind, rest)) = method_rest {
            // Skip type definitions (already handled above)
            if rest.contains("= type") || rest.contains("= type opaque") {
                continue;
            }

            if let Some((_, after_at)) = rest.split_once('@') {
                let name = after_at
                    .split('(')
                    .next()
                    .map(str::trim)
                    .unwrap_or(after_at);

                // Method binding: @__ty_method__Type__method
                if name.starts_with("__ty_method__") {
                    let name = &name["__ty_method__".len()..]; // strip "__ty_method__"
                    if let Some((type_name, method_name)) = name.split_once("__") {
                        let ns = pending_ty_ns.clone().unwrap_or_else(|| "std".to_string());
                        // __ty_method__ signatures must come from explicit @ty_sig metadata.
                        let explicit_sig = pending_ty_sig.take().ok_or_else(|| {
                            vec![format!(
                                "Missing @ty_sig annotation for method {}::{} ({})",
                                type_name, method_name, rest
                            )]
                        })?;
                        let mut sig = parse_ty_signature(&explicit_sig).ok_or_else(|| {
                            vec![format!(
                                "Invalid @ty_sig annotation for method {}::{}: {}",
                                type_name, method_name, explicit_sig
                            )]
                        })?;
                        let parsed_name = sig.name.name.clone();
                        if !parsed_name.is_empty() && parsed_name != method_name {
                            return Err(vec![format!(
                                "@ty_sig method name mismatch for {}::{} (got '{}')",
                                type_name, method_name, parsed_name
                            )]);
                        }
                        sig.name.name = method_name.to_string();
                        method_sigs
                            .entry(format!("{}::{}", ns, type_name))
                            .or_default()
                            .push(sig);
                    }
                } else {
                    // Free function - only emit from 'declare', not 'define'
                    if kind == "declare" {
                        let ns = pending_ty_ns.clone().unwrap_or_else(|| "std".to_string());
                        if let Some(sig) = parse_ll_declare_signature(rest) {
                            push_decl(&ns, make_extern_fn_decl(name.to_string(), sig, &ns));
                        }
                    }
                }
            }
            pending_ty_sig = None;
            pending_ty_ns = None;
        }
    }

    // Generate impl blocks for methods grouped by type
    for (qualified_type, methods) in &method_sigs {
        let Some((ns, type_name)) = qualified_type.rsplit_once("::") else {
            continue;
        };
        let impl_methods: Vec<Declaration> = methods
            .iter()
            .map(|sig| Declaration {
                node: DeclarationKind::Function {
                    name: sig.name.clone(),
                    generics: sig.generics.clone(),
                    params: sig.params.clone(),
                    return_type: sig.return_type.clone(),
                    body: Block {
                        statements: vec![],
                        trailing_expression: None,
                        span: Span::default(),
                        block_id: NodeId(0),
                    },
                },
                span: Span::default(),
                id: NodeId(0),
            })
            .collect();

        push_decl(
            ns,
            Declaration {
                node: DeclarationKind::Impl {
                    trait_name: TypeKind {
                        name: type_name.to_string(),
                        generic_args: vec![],
                    }
                    .to_spanned(),
                    type_name: TypeKind {
                        name: type_name.to_string(),
                        generic_args: vec![],
                    }
                    .to_spanned(),
                    generics: vec![],
                    methods: impl_methods,
                },
                span: Span::default(),
                id: NodeId(0),
            },
        );
    }

    Ok(units)
}

fn make_struct_decl(name: &str, ns: &str) -> Declaration {
    Declaration {
        node: DeclarationKind::Struct {
            name: Identifier {
                name: name.to_string(),
                span: Span::default(),
            },
            generics: vec![],
            fields: vec![],
        },
        span: Span::default(),
        id: NodeId(0),
    }
}

fn make_enum_decl(name: &str, generics: Option<&str>, body: &str) -> Declaration {
    let variants: Vec<EnumVariant> = body
        .split(',')
        .map(|v| {
            let v = v.trim();
            if v.is_empty() {
                return EnumVariant::new_dummy(
                    EnumVariantKind {
                        name: Identifier {
                            name: String::new(),
                            span: Span::default(),
                        },
                        payload: None,
                    },
                    Span::default(),
                );
            }

            // Parse "Name" or "Name(Type)" or "Name(Type1, Type2)"
            let (vname, payload) = if let Some(idx) = v.find('(') {
                let name = v[..idx].trim();
                let inner = &v[idx + 1..v.len() - 1].trim();
                if inner.is_empty() {
                    (name, None)
                } else if inner.contains(',') {
                    let types: Vec<Type> = inner
                        .split(',')
                        .map(|t| {
                            TypeKind {
                                name: ll_type_to_ty(t.trim()).to_string(),
                                generic_args: vec![],
                            }
                            .to_spanned()
                        })
                        .collect();
                    (
                        name,
                        Some(EnumVariantPayloadKind::Tuple(types).to_spanned()),
                    )
                } else {
                    // Single type
                    let ty = ll_type_to_ty(inner);
                    if ty == "Str" {
                        (
                            name,
                            Some(
                                EnumVariantPayloadKind::Unit(
                                    TypeKind {
                                        name: "Int8".to_string(),
                                        generic_args: vec![],
                                    }
                                    .to_spanned(),
                                )
                                .to_spanned(),
                            ),
                        )
                    } else {
                        (
                            name,
                            Some(
                                EnumVariantPayloadKind::Unit(
                                    TypeKind {
                                        name: ty.to_string(),
                                        generic_args: vec![],
                                    }
                                    .to_spanned(),
                                )
                                .to_spanned(),
                            ),
                        )
                    }
                }
            } else {
                (v, None)
            };

            EnumVariant::new_dummy(
                EnumVariantKind {
                    name: Identifier {
                        name: vname.to_string(),
                        span: Span::default(),
                    },
                    payload,
                },
                Span::default(),
            )
        })
        .collect();

    let generics: Vec<GenericParam> = generics
        .map(|g| {
            g.trim_matches(|c| c == '<' || c == '>')
                .split(',')
                .map(|name| {
                    GenericParam::new_dummy(
                        GenericParamKind {
                            name: Identifier {
                                name: name.trim().to_string(),
                                span: Span::default(),
                            },
                            bounds: vec![],
                        },
                        Span::default(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    Declaration {
        node: DeclarationKind::Enum {
            name: Identifier {
                name: name.to_string(),
                span: Span::default(),
            },
            generics,
            variants,
        },
        span: Span::default(),
        id: NodeId(0),
    }
}

fn make_extern_fn_decl(name: String, sig: (Vec<Parameter>, Option<Type>), ns: &str) -> Declaration {
    Declaration {
        node: DeclarationKind::UnsafeOrExtern(
            UnsafeOrExternKind::Extern {
                abi: "C".to_string(),
                declarations: vec![Spanned::new_dummy(
                    FunctionSignatureKind {
                        name: Identifier {
                            name,
                            span: Span::default(),
                        },
                        generics: vec![],
                        params: sig.0,
                        return_type: sig.1,
                    },
                    Span::default(),
                )],
            }
            .to_spanned(),
        ),
        span: Span::default(),
        id: NodeId(0),
    }
}

fn parse_ll_method_signature(
    rest: &str,
    type_name: &str,
    method_name: &str,
) -> Option<FunctionSignatureKind> {
    // rest = "i8* @__ty_rt__Socket__consume(self: %struct.Socket*, i8* %chan)"
    let (ret_ll, _after_at) = rest.split_once('@')?;
    let ret_ty = parse_ll_type_node(ret_ll.trim());
    let ret = if ret_ty == "Unit" {
        None
    } else {
        Some(
            TypeKind {
                name: ret_ty,
                generic_args: vec![],
            }
            .to_spanned(),
        )
    };

    let args_part = rest.split('(').nth(1)?.trim_end_matches(')').trim();

    if args_part.is_empty() {
        return Some(FunctionSignatureKind {
            name: Identifier {
                name: method_name.to_string(),
                span: Span::default(),
            },
            generics: vec![],
            params: vec![],
            return_type: ret,
        });
    }

    let mut params = Vec::new();
    let mut param_idx = 0usize;

    for arg in args_part.split(',') {
        let arg = arg.trim();
        if arg == "..." || arg.is_empty() {
            continue;
        }

        // Extract type (e.g., "i8* %task" or "%struct.Network*")
        let ll_ty = arg.split_whitespace().next()?;

        // Skip self pointer (it's implicit in the impl context)
        let normalized_ty = ll_ty.trim_end_matches('*');
        let is_self_ptr = normalized_ty == "%struct.Socket"
            || normalized_ty == "%struct.Listener"
            || normalized_ty == "%struct.Network"
            || normalized_ty == format!("%struct.{}", type_name);

        if is_self_ptr {
            continue;
        }

        param_idx += 1;
        let ty = parse_ll_type_node(ll_ty);
        params.push(Parameter {
            name: Identifier {
                name: format!("arg{}", param_idx),
                span: Span::default(),
            },
            type_annotation: TypeKind {
                name: ty,
                generic_args: vec![],
            }
            .to_spanned(),
            span: Span::default(),
        });
    }

    Some(FunctionSignatureKind {
        name: Identifier {
            name: method_name.to_string(),
            span: Span::default(),
        },
        generics: vec![],
        params,
        return_type: ret,
    })
}

fn parse_ll_declare_signature(rest: &str) -> Option<(Vec<Parameter>, Option<Type>)> {
    let parts: Vec<&str> = rest.splitn(2, '@').collect();
    if parts.len() != 2 {
        return None;
    }
    let ret_ll = parts[0].trim();

    let args_part = parts[1].split('(').nth(1)?.trim_end_matches(')').trim();

    let ret = {
        let ty = parse_ll_type_node(ret_ll);
        if ty == "Unit" {
            None
        } else {
            Some(
                TypeKind {
                    name: ty,
                    generic_args: vec![],
                }
                .to_spanned(),
            )
        }
    };

    let mut params = Vec::new();
    let mut param_idx = 0usize;
    let mut variadic = false;

    if !args_part.is_empty() {
        for arg in args_part.split(',') {
            let arg = arg.trim();
            if arg == "..." {
                variadic = true;
                continue;
            }
            if arg.is_empty() {
                continue;
            }

            // "i8* %task" or "%struct.Buf* %out" or just "i8*"
            let ll_ty = arg.split_whitespace().next()?;
            let ty = parse_ll_type_node(ll_ty);
            param_idx += 1;
            params.push(Parameter {
                name: Identifier {
                    name: format!("arg{}", param_idx),
                    span: Span::default(),
                },
                type_annotation: TypeKind {
                    name: ty,
                    generic_args: vec![],
                }
                .to_spanned(),
                span: Span::default(),
            });
        }
    }

    if variadic {
        params.push(Parameter {
            name: Identifier {
                name: "...".to_string(),
                span: Span::default(),
            },
            type_annotation: TypeKind {
                name: "...".to_string(),
                generic_args: vec![],
            }
            .to_spanned(),
            span: Span::default(),
        });
    }

    Some((params, ret))
}

fn parse_ty_signature(sig: &str) -> Option<FunctionSignatureKind> {
    // Parse "fn method(self, arg: Type) -> Ret"
    let body = sig.trim();
    let body = body.strip_prefix("fn")?.trim();
    let (_name, rest) = body.split_once('(')?;
    let (params_part, tail) = rest.split_once(')')?;
    let ret_part = if let Some(r) = tail.trim().strip_prefix("->") {
        Some(r.trim())
    } else {
        None
    };

    let mut params = Vec::new();
    let mut self_added = false;

    for (i, param) in params_part.split(',').enumerate() {
        let param = param.trim();
        if param.is_empty() {
            continue;
        }

        if !self_added
            && i == 0
            && (param.starts_with("self")
                || param.starts_with("self:")
                || param.starts_with("self :"))
        {
            self_added = true;
            continue;
        }

        // "arg: Type" or just "Type"
        let (name, ty) = if let Some(idx) = param.rfind(':') {
            let n = param[..idx].trim().to_string();
            if n.is_empty() {
                (format!("arg{}", i), param[idx + 1..].trim().to_string())
            } else {
                (n, param[idx + 1..].trim().to_string())
            }
        } else {
            (format!("arg{}", i), param.to_string())
        };

        params.push(Parameter {
            name: Identifier {
                name,
                span: Span::default(),
            },
            type_annotation: parse_ty_type(&ty),
            span: Span::default(),
        });
    }

    let ret = ret_part.and_then(|r| {
        let r = r.trim();
        if r.is_empty() {
            None
        } else {
            Some(parse_ty_type(r))
        }
    });

    Some(FunctionSignatureKind {
        name: Identifier {
            name: String::new(),
            span: Span::default(),
        }, // filled by caller
        generics: vec![],
        params,
        return_type: ret,
    })
}

fn parse_ty_type(ty: &str) -> Type {
    let ty = ty.trim();
    if let Some(inner) = ty.strip_prefix("ref ") {
        return TypeKind {
            name: "Ref".to_string(),
            generic_args: vec![parse_ty_type(inner.trim())],
        }
        .to_spanned();
    }
    if let Some(i) = ty.find('<') {
        let name = ty[..i].trim();
        let args_str = &ty[i + 1..ty.len() - 1];
        let args: Vec<Type> = args_str
            .split(',')
            .map(|a| parse_ty_type(a.trim()))
            .collect();
        TypeKind {
            name: name.to_string(),
            generic_args: args,
        }
        .to_spanned()
    } else {
        TypeKind {
            name: ty.to_string(),
            generic_args: vec![],
        }
        .to_spanned()
    }
}

fn make_runtime_method_extern_from_sig(
    type_name: &str,
    method_name: &str,
    sig: &FunctionSignatureKind,
) -> Declaration {
    let mut params = Vec::new();
    params.push(Parameter {
        name: Identifier {
            name: "self".to_string(),
            span: Span::default(),
        },
        type_annotation: TypeKind {
            name: type_name.to_string(),
            generic_args: vec![],
        }
        .to_spanned(),
        span: Span::default(),
    });
    params.extend(sig.params.clone());
    make_extern_fn_decl(
        format!("__ty_rt__{}__{}", type_name, method_name),
        (params, sig.return_type.clone()),
        "std",
    )
}

/// Parse LLVM type node (e.g., "i8*", "%struct.Network*", "i64") to Typhoon type string
fn parse_ll_type_node(ll: &str) -> String {
    let ll = ll.trim().trim_end_matches('*');
    ll_type_to_ty(ll).to_string()
}

// Extension trait to make constructing Spanned types more ergonomic
trait ToSpanned {
    fn to_spanned(self) -> Spanned<Self>
    where
        Self: Sized,
    {
        Spanned::new_dummy(self, Span::default())
    }
}

impl<T> ToSpanned for T {}

fn extract_namespace_units(
    modules: Vec<Module>,
) -> Result<HashMap<String, NamespaceUnit>, Vec<String>> {
    let mut errors = Vec::new();
    let mut units: HashMap<String, NamespaceUnit> = HashMap::new();

    for module in modules {
        let ns = match module.name.clone() {
            Some(n) => n,
            None => {
                errors.push("Missing `namespace ...` declaration.".to_string());
                continue;
            }
        };

        let entry = units.entry(ns.clone()).or_insert_with(|| NamespaceUnit {
            declarations: Vec::new(),
            uses: Vec::new(),
        });

        let mut existing_decls = entry
            .declarations
            .iter()
            .filter_map(decl_name)
            .map(|i| i.name.clone())
            .collect::<HashSet<_>>();
        for decl in module.declarations {
            match decl.node {
                DeclarationKind::Use(path) => entry.uses.push(path),
                _ => {
                    if let Some(id) = decl_name(&decl) {
                        if existing_decls.contains(&id.name) {
                            continue;
                        }
                        existing_decls.insert(id.name.clone());
                    }
                    entry.declarations.push(decl);
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(units)
    } else {
        Err(errors)
    }
}

fn decl_name(decl: &Declaration) -> Option<&Identifier> {
    match &decl.node {
        DeclarationKind::Function { name, .. } => Some(name),
        DeclarationKind::Struct { name, .. } => Some(name),
        DeclarationKind::Enum { name, .. } => Some(name),
        DeclarationKind::Newtype { name, .. } => Some(name),
        DeclarationKind::Interface { name, .. } => Some(name),
        _ => None,
    }
}

fn build_namespace_decl_maps(
    units: &HashMap<String, NamespaceUnit>,
) -> Result<HashMap<String, HashMap<String, String>>, Vec<String>> {
    let mut errors = Vec::new();
    let mut out = HashMap::new();

    for (ns, unit) in units {
        let mut map = HashMap::new();
        for decl in &unit.declarations {
            if let Some(id) = decl_name(decl) {
                if map.contains_key(&id.name) {
                    errors.push(format!(
                        "Duplicate declaration '{}' in namespace '{}'",
                        id.name, ns
                    ));
                } else {
                    map.insert(id.name.clone(), mangle(ns, &id.name));
                }
            }
            if let DeclarationKind::Enum { variants, .. } = &decl.node {
                for v in variants {
                    let vname = v.node.name.name.clone();
                    if map.contains_key(&vname) {
                        errors.push(format!(
                            "Duplicate declaration '{}' in namespace '{}'",
                            vname, ns
                        ));
                    } else {
                        map.insert(vname.clone(), mangle(ns, &vname));
                    }
                }
            }
        }
        out.insert(ns.clone(), map);
    }

    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

fn use_target(path: &UsePath) -> Option<(String, Option<String>, bool)> {
    // returns (target_ns, imported_name, wildcard)
    if path.node.wildcard {
        if path.node.segments.is_empty() {
            return None;
        }
        return Some((path.node.segments.join("::"), None, true));
    }
    if path.node.segments.len() < 2 {
        return None;
    }
    let (head, tail) = path.node.segments.split_at(path.node.segments.len() - 1);
    Some((head.join("::"), Some(tail[0].to_string()), false))
}

fn topo_sort(
    names: &HashSet<String>,
    edges: &HashMap<String, HashSet<String>>,
) -> Result<Vec<String>, Vec<String>> {
    #[derive(Copy, Clone, PartialEq, Eq)]
    enum Mark {
        Temp,
        Perm,
    }
    let mut marks: HashMap<String, Mark> = HashMap::new();
    let mut out = Vec::new();
    let mut errors = Vec::new();

    fn visit(
        n: &str,
        edges: &HashMap<String, HashSet<String>>,
        marks: &mut HashMap<String, Mark>,
        out: &mut Vec<String>,
        errors: &mut Vec<String>,
        stack: &mut Vec<String>,
    ) {
        if let Some(m) = marks.get(n).copied() {
            if m == Mark::Perm {
                return;
            }
            if m == Mark::Temp {
                stack.push(n.to_string());
                errors.push(format!(
                    "Cyclic namespace dependency: {}",
                    stack.join(" -> ")
                ));
                stack.pop();
                return;
            }
        }
        marks.insert(n.to_string(), Mark::Temp);
        stack.push(n.to_string());
        if let Some(deps) = edges.get(n) {
            for dep in deps {
                visit(dep, edges, marks, out, errors, stack);
            }
        }
        stack.pop();
        marks.insert(n.to_string(), Mark::Perm);
        out.push(n.to_string());
    }

    for n in names {
        visit(n, edges, &mut marks, &mut out, &mut errors, &mut Vec::new());
    }

    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

fn compute_transitive(
    namespaces: &HashMap<String, NamespaceUnit>,
    entry_ns: &str,
) -> Result<Vec<String>, Vec<String>> {
    let mut errors = Vec::new();
    let mut edges: HashMap<String, HashSet<String>> = HashMap::new();
    for (ns, unit) in namespaces {
        let mut deps = HashSet::new();
        for u in &unit.uses {
            if let Some((target_ns, _name, _wild)) = use_target(u) {
                deps.insert(target_ns);
            } else {
                errors.push(format!(
                    "Invalid use path in namespace '{}': {:?}",
                    ns, u.node.segments
                ));
            }
        }
        edges.insert(ns.clone(), deps);
    }

    if errors.is_empty() == false {
        return Err(errors);
    }

    let mut needed = HashSet::new();
    let mut stack = vec![entry_ns.to_string()];
    while let Some(ns) = stack.pop() {
        if !needed.insert(ns.clone()) {
            continue;
        }
        if let Some(deps) = edges.get(&ns) {
            for dep in deps {
                if !namespaces.contains_key(dep) {
                    errors.push(format!("Unknown namespace '{}' imported by '{}'", dep, ns));
                } else {
                    stack.push(dep.clone());
                }
            }
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    let order = topo_sort(&needed, &edges)?;
    Ok(order)
}

fn build_alias_map(
    ns: &str,
    units: &HashMap<String, NamespaceUnit>,
    decl_maps: &HashMap<String, HashMap<String, String>>,
) -> Result<HashMap<String, String>, Vec<String>> {
    let mut errors = Vec::new();
    let mut alias: HashMap<String, String> = HashMap::new();

    if let Some(own) = decl_maps.get(ns) {
        for (k, v) in own {
            alias.insert(k.clone(), v.clone());
        }
    }

    let unit = units.get(ns).unwrap();
    for u in &unit.uses {
        let Some((target_ns, imported_name, wildcard)) = use_target(u) else {
            errors.push(format!("Invalid use in '{}': {:?}", ns, u.node.segments));
            continue;
        };
        let target_map = match decl_maps.get(&target_ns) {
            Some(m) => m,
            None => {
                errors.push(format!(
                    "Unknown namespace '{}' in use from '{}'",
                    target_ns, ns
                ));
                continue;
            }
        };
        if wildcard {
            for (name, mangled) in target_map {
                if let Some(existing) = alias.get(name) {
                    if existing != mangled {
                        errors.push(format!(
                            "Conflicting import '{}' in namespace '{}' ({} vs {})",
                            name, ns, existing, mangled
                        ));
                    }
                } else {
                    alias.insert(name.clone(), mangled.clone());
                }
            }
        } else if let Some(name) = imported_name {
            let Some(mangled) = target_map.get(&name) else {
                errors.push(format!(
                    "Unknown import '{}' from namespace '{}' (imported by '{}')",
                    name, target_ns, ns
                ));
                continue;
            };
            if let Some(existing) = alias.get(&name) {
                if existing != mangled {
                    errors.push(format!(
                        "Conflicting import '{}' in namespace '{}' ({} vs {})",
                        name, ns, existing, mangled
                    ));
                }
            } else {
                alias.insert(name, mangled.clone());
            }
        }
    }

    if errors.is_empty() {
        Ok(alias)
    } else {
        Err(errors)
    }
}

fn method_symbol(type_name: &Type, method_name: &str) -> String {
    format!("__ty_method__{}__{}", type_name.node.name, method_name)
}

fn expand_impl_and_extension_decls(decl: Declaration) -> Vec<Declaration> {
    match decl.node {
        DeclarationKind::Impl {
            type_name,
            generics: ext_generics,
            methods,
            ..
        } => methods
            .into_iter()
            .map(|mut m| {
                if let DeclarationKind::Function {
                    name,
                    params,
                    generics,
                    ..
                } = &mut m.node
                {
                    name.name = method_symbol(&type_name, &name.name);
                    // Prepend self receiver so func_sigs gets the full param list
                    // (task is prepended later by register_module_sigs)
                    params.insert(
                        0,
                        Parameter {
                            name: Identifier {
                                name: "self".to_string(),
                                span: Span::default(),
                            },
                            type_annotation: type_name.clone(), // the impl's type
                            span: Span::default(),
                        },
                    );
                    let mut merged = ext_generics.clone();
                    merged.extend(generics.drain(..));
                    *generics = merged;
                }
                m
            })
            .collect(),
        DeclarationKind::Extension {
            type_constraint,
            generics: ext_generics,
            methods,
            ..
        } => methods
            .into_iter()
            .map(|mut m| {
                if let DeclarationKind::Function { name, generics, .. } = &mut m.node {
                    name.name = method_symbol(&type_constraint, &name.name);
                    // Prepend the extension's generics so T, E etc. are in scope
                    // for resolver and type checker
                    let mut merged = ext_generics.clone();
                    merged.extend(generics.drain(..));
                    *generics = merged;
                }
                m
            })
            .collect(),
        _ => vec![decl],
    }
}

pub fn compile_project(path: &Path) -> Result<Module, Vec<String>> {
    let mut files = collect_ty_files(path).map_err(|e| vec![e])?;
    files.sort();
    files.dedup();

    let entry_file = if path.is_file() {
        path.to_path_buf()
    } else {
        // Assume main.ty exists in directory if passed folder
        path.join("main.ty")
    };

    let mut modules = Vec::new();
    let mut errors = Vec::new();
    let mut parsed_files = HashSet::new();

    for file in files {
        if parsed_files.contains(&file) {
            continue;
        }
        match parse_file(&file) {
            Ok(m) => {
                parsed_files.insert(file.clone());
                modules.push(m)
            }
            Err(e) => errors.push(format!("{}: {}", file.display(), e)),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    // Load stdlib from .ll file and inject directly as NamespaceUnit
    // Look next to the binary, then next to cwd
    let stdlib_ll_path = std::env::current_exe()
        .ok() // Convert Result to Option
        .and_then(|p| p.parent()?.parent()?.parent()?.to_owned().into())
        // 1st parent: executable folder (e.g., debug/)
        // 2nd parent: build folder (e.g., target/)
        // 3rd parent: project root
        .map(|d| d.join("typhoon-stdlib.ll"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("typhoon-stdlib.ll"));

    // Parse .ty files into units first, then add stdlib directly from LLVM IR
    let mut units = extract_namespace_units(modules)?;

    // Load stdlib from .ll file and parse directly into "std" namespace
    if stdlib_ll_path.exists() {
        let ll_source =
            fs::read_to_string(stdlib_ll_path).map_err(|e| vec![format!("read stdlib: {}", e)])?;

        let stdlib_units = parse_llvm_to_namespace_unit(&ll_source)?;
        for (ns_name, unit) in stdlib_units {
            let entry = units.entry(ns_name).or_insert_with(|| NamespaceUnit {
                declarations: Vec::new(),
                uses: Vec::new(),
            });
            let mut existing_decls = entry
                .declarations
                .iter()
                .filter_map(decl_name)
                .map(|i| i.name.clone())
                .collect::<HashSet<_>>();
            for decl in unit.declarations {
                if let Some(id) = decl_name(&decl) {
                    if existing_decls.contains(&id.name) {
                        continue;
                    }
                    existing_decls.insert(id.name.clone());
                }
                entry.declarations.push(decl);
            }
        }
    }

    if let Some(net_unit) = units.get_mut("std::net") {
        net_unit.uses.push(Spanned::new_dummy(
            UsePathKind {
                segments: vec![
                    "std".to_string(),
                    "result".to_string(),
                    "Result".to_string(),
                ],
                wildcard: false,
            },
            Span::default(),
        ));
    }

    // Find entry namespace by looking up which unit came from a file named
    // after the entry file's stem — or just read the namespace line only
    let entry_ns = {
        let source = fs::read_to_string(&entry_file)
            .map_err(|e| vec![format!("{}: {}", entry_file.display(), e)])?;
        // Grab `namespace <name>` without full parse — no AST, no declarations
        source
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("namespace ")
                    .map(|n| n.trim().to_string())
            })
            .ok_or_else(|| vec!["Could not find entry namespace".to_string()])?
    };

    // Ensure entry unit exists (safety fallback only — or_insert_with is a no-op if already present)
    units
        .entry(entry_ns.clone())
        .or_insert_with(|| NamespaceUnit {
            declarations: Vec::new(),
            uses: Vec::new(),
        });
    let decl_maps = build_namespace_decl_maps(&units)?;
    let order = compute_transitive(&units, &entry_ns)?;

    // 1. Collect declarations from transitive namespaces only.
    let mut global_symbols = HashMap::new();
    for ns in &order {
        let unit = units.get(ns).unwrap();
        for decl in &unit.declarations {
            if let Some(id) = decl_name(decl) {
                let info = match &decl.node {
                    DeclarationKind::Struct { fields, .. } => {
                        let mut field_map = HashMap::new();
                        for (f_id, f_ty) in fields {
                            field_map.insert(f_id.name.clone(), f_ty.node.clone());
                        }
                        crate::resolver::DeclInfo::Struct { fields: field_map }
                    }
                    DeclarationKind::Enum { variants, .. } => {
                        let mut variant_map = HashMap::new();
                        for v in variants {
                            variant_map.insert(
                                v.node.name.name.clone(),
                                crate::resolver::EnumVariantInfo {
                                    name: v.node.name.name.clone(),
                                    payload: v.node.payload.clone().map(|p| p.node),
                                },
                            );
                        }
                        crate::resolver::DeclInfo::Enum {
                            variants: variant_map,
                        }
                    }
                    _ => crate::resolver::DeclInfo::Unresolved,
                };
                global_symbols.insert(id.name.clone(), info);
            }
        }
    }
    // Step 2: Resolve namespaces using explicit import surface (alias map)
    let mut resolver = crate::resolver::Resolver::new();
    for ns in &order {
        let unit = units.get(ns).unwrap();
        let own_decl_names: HashSet<String> = unit
            .declarations
            .iter()
            .filter_map(decl_name)
            .map(|id| id.name.clone())
            .collect();
        let alias = build_alias_map(ns, &units, &decl_maps)?;
        let imports_for_ns: HashMap<String, crate::resolver::DeclInfo> = alias
            .keys()
            .filter(|name| !own_decl_names.contains(*name))
            .filter_map(|name| {
                global_symbols
                    .get(name)
                    .map(|info| (name.clone(), info.clone()))
            })
            .collect();

        // Expand impl/extension blocks so the resolver sees individual fn declarations
        let expanded_decls: Vec<Declaration> = unit
            .declarations
            .iter()
            .cloned()
            .flat_map(|d| expand_impl_and_extension_decls(d))
            .collect();

        let module = Module {
            name: Some(ns.clone()),
            declarations: expanded_decls, // ← expanded, not raw
            span: Span::default(),
        };

        resolver
            .resolve_module(&module, &imports_for_ns)
            .map_err(|e| e)?;
    }

    // Step 3: Build all_decls from transitive namespaces only
    let mut all_decls = Vec::new();
    let mut desugar = Desugar::new();

    for ns in order {
        let alias = build_alias_map(&ns, &units, &decl_maps)?;
        let unit = units.get(&ns).unwrap();
        for mut decl in unit.declarations.clone() {
            desugar.rename_declaration(&mut decl, &alias);
            desugar
                .desugar_declaration(&mut decl)
                .map_err(|e| vec![format!("{}: {}", ns, e)])?;
            for expanded in expand_impl_and_extension_decls(decl) {
                all_decls.push(expanded);
            }
        }
    }

    Ok(Module {
        name: Some(entry_ns),
        declarations: all_decls,
        span: Span::default(),
    })
}

fn ll_type_to_ty(ll: &str) -> &str {
    let ll = ll.trim().trim_end_matches('*');
    match ll {
        "void" => "Unit",
        "i8" | "i8*" => "Str", // opaque ptr / string
        "i16" => "Int16",
        "i32" => "Int32",
        "i64" => "Int64",
        "float" => "Float32",
        "double" => "Float64",
        s if s.starts_with("%struct.Result__") => "Result", // instantiated generic types
        s if s.starts_with("%struct.Option__") => "Option", // instantiated generic types
        s if s.starts_with("%struct.") => {
            // "%struct.Buf*" -> "Buf", "%struct.TyArray" -> "TyArray"
            s.trim_start_matches("%struct.")
        }
        _ => "Str", // fallback: treat unknown ptrs as opaque
    }
}
