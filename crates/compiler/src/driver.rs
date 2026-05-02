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
    name: String,
    declarations: Vec<Declaration>,
    uses: Vec<UsePath>,
}

fn collect_ty_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
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
    let ns = ns.replace("::", "__");
    format!("{}__{}", ns, name)
}

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
            name: ns.clone(),
            declarations: Vec::new(),
            uses: Vec::new(),
        });

        for decl in module.declarations {
            match decl.node {
                DeclarationKind::Use(path) => entry.uses.push(path),
                _ => entry.declarations.push(decl),
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
    // topo_sort returns deps first due to postorder push; reverse for deterministic "deps then dependents"
    let mut order = order;
    order.reverse();
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
            type_name, methods, ..
        } => methods
            .into_iter()
            .map(|mut m| {
                if let DeclarationKind::Function { name, .. } = &mut m.node {
                    name.name = method_symbol(&type_name, &name.name);
                }
                m
            })
            .collect(),
        DeclarationKind::Extension {
            type_constraint,
            methods,
            ..
        } => methods
            .into_iter()
            .map(|mut m| {
                if let DeclarationKind::Function { name, .. } = &mut m.node {
                    name.name = method_symbol(&type_constraint, &name.name);
                }
                m
            })
            .collect(),
        _ => vec![decl],
    }
}

pub fn compile_project(entry_file: &Path) -> Result<Module, Vec<String>> {
    let root = entry_file.parent().ok_or_else(|| {
        vec![format!(
            "Entry file has no parent: {}",
            entry_file.display()
        )]
    })?;

    let files = collect_ty_files(root).map_err(|e| vec![e])?;

    println!("<<<< {:?}", files);
    let mut modules = Vec::new();
    let mut errors = Vec::new();
    for file in files {
        match parse_file(&file) {
            Ok(m) => modules.push(m),
            Err(e) => errors.push(format!("{}: {}", file.display(), e)),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    let units = extract_namespace_units(modules)?;
    let entry_module =
        parse_file(entry_file).map_err(|e| vec![format!("{}: {}", entry_file.display(), e)])?;
    let entry_ns = entry_module.name.clone().ok_or_else(|| {
        vec![format!(
            "Entry file missing `namespace`: {}",
            entry_file.display()
        )]
    })?;

    let decl_maps = build_namespace_decl_maps(&units)?;
    let order = compute_transitive(&units, &entry_ns)?;

    // Build per-namespace alias maps and then rename + desugar declarations.
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
