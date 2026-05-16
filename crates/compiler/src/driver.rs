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

        let mut entry = units.entry(ns.clone()).or_insert_with(|| NamespaceUnit {
            name: ns.clone(),
            declarations: Vec::new(),
            uses: Vec::new(),
        });

        let mut existing_decls = entry
            .declarations
            .iter()
            .filter_map(decl_name)
            .map(|i| i.name.clone())
            .collect::<HashSet<_>>();
        println!("Existing decls in {}: {:?}", ns, existing_decls);

        for decl in module.declarations {
            match decl.node {
                DeclarationKind::Use(path) => entry.uses.push(path),
                _ => {
                    if let Some(id) = decl_name(&decl) {
                        if existing_decls.contains(&id.name) {
                            println!("Skipping duplicate: {} in {}", id.name, ns);
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
            type_name,
            generics: ext_generics,
            methods,
            ..
        } => methods
            .into_iter()
            .map(|mut m| {
                if let DeclarationKind::Function { name, generics, .. } = &mut m.node {
                    name.name = method_symbol(&type_name, &name.name);
                    // Prepend the impl's generics so T, E etc. are in scope
                    // for resolver and type checker
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

    // Load stdlib from .ll file and inject as synthetic parsed module
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
    println!("Exe {:?}", std::env::current_exe());
    println!("Path {:?}", stdlib_ll_path);
    if stdlib_ll_path.exists() {
        let ll_source =
            fs::read_to_string(stdlib_ll_path).map_err(|e| vec![format!("read stdlib: {}", e)])?;
        let ty_source = parse_ll_as_ty_source(&ll_source);

        // Parse the generated .ty source as a module
        let tokens = crate::lexer::Lexer::new(ty_source).tokenize();
        match crate::parser::Parser::new(tokens).parse_module() {
            Ok(m) => modules.push(m),
            Err(e) => return Err(vec![format!("stdlib parse error: {}", e)]),
        }
    }
    println!(
        "modules count: {}, namespaces: {:?}",
        modules.len(),
        modules
            .iter()
            .filter_map(|m| m.name.as_deref())
            .collect::<Vec<_>>()
    );

    let mut units = extract_namespace_units(modules)?;

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
            name: entry_ns.clone(),
            declarations: Vec::new(),
            uses: Vec::new(),
        });
    let decl_maps = build_namespace_decl_maps(&units)?;
    let order = compute_transitive(&units, &entry_ns)?;

    // 1. First, collect all declarations from all namespaces (global index).
    let mut global_symbols = HashMap::new();
    for (ns, unit) in &units {
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
    println!(
        "global_symbols keys: {:?}",
        global_symbols.keys().collect::<Vec<_>>()
    );

    // Step 2: Resolve all namespaces using global context
    let mut resolver = crate::resolver::Resolver::new();
    for ns in &order {
        let unit = units.get(ns).unwrap();
        if ns == "std" {
            continue;
        }

        let own_decl_names: HashSet<String> = unit
            .declarations
            .iter()
            .filter_map(decl_name)
            .map(|id| id.name.clone())
            .collect();

        let imports_for_ns: HashMap<String, crate::resolver::DeclInfo> = global_symbols
            .iter()
            .filter(|(name, _)| !own_decl_names.contains(*name))
            .map(|(k, v)| (k.clone(), v.clone()))
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

    // Step 3: Build all_decls — always include std first, then ordered namespaces
    let mut all_decls = Vec::new();
    let mut desugar = Desugar::new();

    // Always expand std into all_decls so type checker can find stdlib methods
    // (temporary until explicit `use std::...` imports drive inclusion)
    if let Some(std_unit) = units.get("std") {
        for decl in std_unit.declarations.clone() {
            for expanded in expand_impl_and_extension_decls(decl) {
                all_decls.push(expanded);
            }
        }
    }

    for ns in order {
        if ns == "std" {
            continue;
        } // already added above
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
        s if s.starts_with("%struct.") => {
            // "%struct.Buf*" -> "Buf", "%struct.TyArray" -> "TyArray"
            s.trim_start_matches("%struct.")
        }
        _ => "Str", // fallback: treat unknown ptrs as opaque
    }
}

fn parse_ll_as_ty_source(ll_source: &str) -> String {
    let mut struct_names = Vec::new();
    let mut enum_decls = Vec::new();
    let mut free_fns = Vec::new();
    // (type_name, raw_ty_sig_line) — full Typhoon fn signature string,
    // either from a `; @ty_sig:` annotation or inferred from LLVM types.
    let mut methods: Vec<(String, String)> = Vec::new();
    let mut pending_ty_sig: Option<String> = None;

    for line in ll_source.lines() {
        let line = line.trim();

        // ; @ty_sig: fn consume(self, ch: ref chan<Int8>)
        if let Some(sig) = line.strip_prefix("; @ty_sig:") {
            pending_ty_sig = Some(sig.trim().to_string());
            continue;
        }

        // %struct.Foo = type { ... }  OR  %struct.Foo = type opaque
        if let Some(rest) = line.strip_prefix("%struct.") {
            if let Some(name) = rest.split('=').next().map(|s| s.trim()) {
                struct_names.push(name.to_string());
            }
            continue;
        }

        // %enum.Option<T> = type { Some(T), None }
        // %enum.Result<T, E> = type { Ok(T), Err(E) }
        if let Some(rest) = line.strip_prefix("%enum.") {
            // Extract "Name<generics>" from before the '='
            if let Some(lhs) = rest.split('=').next().map(|s| s.trim()) {
                // Extract variants from inside "type { ... }"
                if let Some(body) = rest.split('{').nth(1).and_then(|s| s.split('}').next()) {
                    // lhs = "Option<T>" or "Result<T, E>"
                    let (enum_name, generics) = if let Some(i) = lhs.find('<') {
                        (&lhs[..i], &lhs[i..]) // ("Option", "<T>")
                    } else {
                        (lhs, "")
                    };
                    // body = " Some(T), None " or " Ok(T), Err(E) "
                    let variants = body
                        .split(',')
                        .map(|v| v.trim().to_string())
                        .filter(|v| !v.is_empty())
                        .collect::<Vec<_>>()
                        .join(", ");
                    enum_decls.push(format!("enum {}{} {{ {} }}", enum_name, generics, variants));
                }
            }
            continue;
        }

        let method_rest = if let Some(r) = line.strip_prefix("declare ") {
            Some(("declare", r))
        } else if let Some(r) = line.strip_prefix("define ") {
            Some(("define", r))
        } else {
            None
        };

        if let Some((kind, rest)) = method_rest {
            if rest.contains("@__ty_method__") {
                // existing method-parsing logic — works for both declare and define
                let type_name = rest
                    .split("@__ty_method__")
                    .nth(1)
                    .and_then(|s| s.split("__").next())
                    .map(|s| s.to_string());

                if let Some(type_name) = type_name {
                    if let Some(sig) = pending_ty_sig.take() {
                        // Use the explicit Typhoon signature from the annotation
                        methods.push((type_name, sig));
                    } else if let Some((_, method_name, params, ret)) = parse_ll_method_decl(rest) {
                        // Fall back to inferring from LLVM types
                        let ret_str = if ret == "Unit" {
                            String::new()
                        } else {
                            format!(" -> {}", ret)
                        };
                        let params_str = if params.is_empty() {
                            String::new()
                        } else {
                            format!(", {}", params.join(", "))
                        };
                        let sig = format!("fn {}(self{}){} {{}}", method_name, params_str, ret_str);
                        methods.push((type_name, sig));
                    }
                }
            } else {
                // This block correctly handles all other 'declare' statements,
                // including @__ty_rt__ functions, by treating them as regular free functions.
                // only emit free-fn externs from 'declare', not 'define'
                if let Some(sig) = parse_ll_declare_to_ty(rest) {
                    free_fns.push(sig);
                }
            }
            pending_ty_sig = None;
        } else if !line.starts_with(';') {
            pending_ty_sig = None;
        }
    }

    let mut out = String::from("namespace std\nextern \"C\" {\n");
    for sig in &free_fns {
        out.push_str(&format!("    {}\n", sig));
    }
    out.push_str("}\n");

    // Emit struct declarations
    let mut emitted = std::collections::HashSet::new();
    for name in &struct_names {
        let ty_name = if name == "TyArray" {
            "Array"
        } else {
            name.as_str()
        };
        if emitted.insert(ty_name.to_string()) {
            out.push_str(&format!("struct {} {{}}\n", ty_name));
        }
    }

    // Emit enum declarations
    for decl in &enum_decls {
        out.push_str(&format!("{}\n", decl));
    }

    // Emit impl blocks grouped by type
    let mut by_type: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (type_name, sig) in methods {
        by_type.entry(type_name).or_default().push(sig);
    }
    for (type_name, sigs) in &by_type {
        out.push_str(&format!("impl {} {{\n", type_name));
        for sig in sigs {
            // Ensure every method has a body — annotation may omit it
            let line = if sig.contains('{') {
                sig.clone()
            } else {
                format!("{} {{}}", sig)
            };
            out.push_str(&format!("    {}\n", line));
        }
        out.push_str("}\n");
    }

    println!("Output: {}", out);

    out
}

// New helper for __ty_method__Type__method declarations
fn parse_ll_method_decl(rest: &str) -> Option<(String, String, Vec<String>, String)> {
    // rest = "i8* @__ty_method__Network__listen(%struct.Network*, i8*)"
    let (ret_ll, after_at) = rest.split_once('@')?;
    let ret_ty = ll_type_to_ty(ret_ll.trim()).to_string();

    let (name, args_part) = after_at.split_once('(')?;
    // name = "__ty_method__Network__listen"
    let name = name.trim().trim_start_matches("__ty_method__");
    // Split on first __ to get TypeName and method
    let (type_name, method_name) = name.split_once("__")?;

    let args_part = args_part.trim_end_matches(')');
    let mut params = Vec::new();
    // Skip first arg (self pointer = %struct.TypeName*)
    let args: Vec<&str> = args_part.split(',').collect();
    for (i, arg) in args.iter().enumerate().skip(1) {
        let arg = arg.trim();
        if arg == "..." || arg.is_empty() {
            continue;
        }
        // Extract LLVM type (e.g., "i8*", "%struct.Network*")
        let ll_ty = arg.split_whitespace().next()?;
        let ty = ll_type_to_ty(ll_ty);
        params.push(format!("arg{}: {}", i, ty));
    }

    Some((
        type_name.to_string(),   // Correctly extracted type name (e.g., "Network")
        method_name.to_string(), // Correctly extracted method name (e.g., "listen")
        params,
        ret_ty,
    ))
}

fn parse_ll_declare_to_ty(rest: &str) -> Option<String> {
    // rest = "i32 @ty_net_listen(i8* %addr, i32 %port)"
    let (ret_ll, after_at) = rest.split_once('@')?;
    let ret_ty = ll_type_to_ty(ret_ll.trim());

    let (name, args_part) = after_at.split_once('(')?;
    let name = name.trim();
    let args_part = args_part.trim_end_matches(')');

    let mut params = Vec::new();
    let mut variadic = false;
    if !args_part.trim().is_empty() {
        for (i, arg) in args_part.split(',').enumerate() {
            let arg = arg.trim();
            if arg == "..." {
                variadic = true;
                continue;
            }
            if arg.is_empty() {
                continue;
            }
            // "i8* %task"  or  "%struct.Buf* %out"  or  just "i8*"
            let ll_ty = arg.split_whitespace().next()?;
            let ty = ll_type_to_ty(ll_ty);
            params.push(format!("arg{}: {}", i, ty));
        }
    }
    if variadic {
        params.push("...".to_string());
    }

    let ret_annotation = if ret_ty == "Unit" {
        String::new()
    } else {
        format!(" -> {}", ret_ty)
    };

    Some(format!(
        "fn {}({}){}",
        name,
        params.join(", "),
        ret_annotation
    ))
}
