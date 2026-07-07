use std::env;
use std::path::PathBuf;
use std::process::Command;

/// Locate `typhoon-stdlib.ll` next to the binary, falling back to cwd.
/// Mirrors the lookup logic in driver.rs so both always find the same file.
fn find_stdlib_ll() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|p| p.parent()?.parent()?.parent().map(|p| p.to_path_buf()))
        .map(|d| d.join("typhoon-stdlib.ll"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("typhoon-stdlib.ll"))
}

/// Returns true if a `define` line is a net method wrapper whose correct body
/// is provided by typhoon-stdlib.ll.  Codegen emits broken self-recursive stubs
/// for these; we strip them from user IR so the stdlib definition wins.
fn is_net_method_wrapper(define_line: &str) -> bool {
    define_line.contains("@__ty_method__Network__")
        || define_line.contains("@__ty_method__Listener__")
        || define_line.contains("@__ty_method__Socket__")
}

/// True if a `%struct.X = type ...` line has a real body rather than being
/// an opaque forward-declaration placeholder.
fn is_concrete_type_line(trimmed: &str) -> bool {
    !trimmed.contains("= type opaque")
}

fn merge_ir_text(ir_file: &str, stdlib_ll: &PathBuf, merged_path: &str) -> std::io::Result<()> {
    let user_ir = std::fs::read_to_string(ir_file)?;

    // First pass over stdlib IR: just record which type names it defines
    // concretely, so the user-IR filtering pass below can drop opaque user
    // stubs in favor of a concrete stdlib definition of the same name.
    let stdlib_ir_preview = std::fs::read_to_string(stdlib_ll)?;
    let mut stdlib_concrete_types = std::collections::HashSet::new();
    for line in stdlib_ir_preview.lines() {
        let trimmed = line.trim_start();
        if (trimmed.starts_with("%struct.")
            || trimmed.starts_with("%enum.")
            || trimmed.starts_with("%newtype."))
            && trimmed.contains(" = type ")
            && is_concrete_type_line(trimmed)
        {
            if let Some(name) = trimmed.split_whitespace().next() {
                stdlib_concrete_types.insert(name.to_string());
            }
        }
    }

    let (user_ir_filtered, user_struct_types, user_concrete_types, user_define_names, user_declare_names) = {
        let lines: Vec<&str> = user_ir.lines().collect();
        let mut struct_types = std::collections::HashSet::new();
        let mut concrete_types = std::collections::HashSet::new();
        let mut define_names = std::collections::HashSet::new();
        let mut declare_names = std::collections::HashSet::new();
        for &line in &lines {
            let trimmed = line.trim_start();
            // Collect struct type names defined in user IR
            if trimmed.starts_with("%struct.")
                || trimmed.starts_with("%enum.")
                || trimmed.starts_with("%newtype.")
            {
                if let Some(name) = trimmed.split_whitespace().next() {
                    struct_types.insert(name.to_string());
                    if is_concrete_type_line(trimmed) {
                        concrete_types.insert(name.to_string());
                    }
                }
            }
            if trimmed.starts_with("define ") {
                if let Some((_, after_at)) = trimmed.split_once('@') {
                    if let Some(name) = after_at.split('(').next().map(str::trim) {
                        define_names.insert(name.to_string());
                    }
                }
            }
            if trimmed.starts_with("declare ") {
                if let Some((_, after_at)) = trimmed.split_once('@') {
                    if let Some(name) = after_at.split('(').next().map(str::trim) {
                        declare_names.insert(name.to_string());
                    }
                }
            }
        }
        // Strip from user IR:
        //   - any `declare @__ty_method__*`  (forward decls the stdlib defines)
        //   - any `define @__ty_method__Network__*`, `@__ty_method__Listener__*`,
        //     or `@__ty_method__Socket__*` function bodies — codegen emits
        //     self-recursive stubs for these; the correct bodies live in the stdlib.
        let mut filtered: Vec<&str> = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let trimmed = lines[i].trim_start();
            // Drop declare-stubs for any __ty_method__ symbol.
            if trimmed.starts_with("declare ") && trimmed.contains("@__ty_method__") {
                i += 1;
                continue;
            }
            // Drop user IR's own opaque struct/enum/newtype stub when the
            // stdlib defines that same type concretely — the stdlib's
            // definition wins and the opaque line would otherwise redefine
            // the same struct name, or (as here) get emitted while the
            // stdlib's concrete copy is dropped because "user IR already
            // has a definition", leaving nothing concrete on either side.
            if (trimmed.starts_with("%struct.")
                || trimmed.starts_with("%enum.")
                || trimmed.starts_with("%newtype."))
                && trimmed.contains(" = type ")
                && !is_concrete_type_line(trimmed)
            {
                if let Some(name) = trimmed.split_whitespace().next() {
                    if stdlib_concrete_types.contains(name) {
                        i += 1;
                        continue;
                    }
                }
            }
            // Drop codegen-emitted define bodies for net method wrappers.
            if trimmed.starts_with("define ") && is_net_method_wrapper(trimmed) {
                // Skip the entire function body by counting braces.
                let mut depth = 0i32;
                let mut entered = false;
                while i < lines.len() {
                    let l = lines[i];
                    depth += l.matches('{').count() as i32;
                    depth -= l.matches('}').count() as i32;
                    i += 1;
                    if depth > 0 {
                        entered = true;
                    }
                    if entered && depth <= 0 {
                        break;
                    }
                }
                continue;
            }
            filtered.push(lines[i]);
            i += 1;
        }
        (filtered.join("\n"), struct_types, concrete_types, define_names, declare_names)
    };
    let stdlib_ir = std::fs::read_to_string(stdlib_ll)?;
    let stdlib_lines: Vec<&str> = stdlib_ir.lines().collect();
    let mut stdlib_out_lines = Vec::new();
    let mut i = 0;
    while i < stdlib_lines.len() {
        let line = stdlib_lines[i].trim_start();
        // Drop `declare` lines from stdlib — they are already present in user IR
        // (codegen emits them) and duplicates cause llvm errors.
        if line.starts_with("declare ") {
            // Only drop if user IR already declares (or defines) this
            // symbol itself. Previously every stdlib `declare` was dropped
            // unconditionally on the assumption user IR always has it —
            // false for runtime functions (e.g. __ty_rt__Listener__accept)
            // that are only called from inside a kept net-wrapper body
            // (__ty_method__Listener__accept) when user code never
            // touches that type directly and so never declares the
            // runtime function itself.
            let name = line
                .split_once('@')
                .and_then(|(_, after)| after.split('(').next())
                .map(str::trim);
            let redundant = name
                .map(|n| user_declare_names.contains(n) || user_define_names.contains(n))
                .unwrap_or(false);
            if redundant {
                i += 1;
                continue;
            }
        }
        if line.starts_with("define ") {
            if let Some((_, after_at)) = line.split_once('@') {
                if let Some(name) = after_at.split('(').next().map(str::trim) {
                    if user_define_names.contains(name) && !is_net_method_wrapper(line) {
                        let mut depth = 0i32;
                        let mut entered = false;
                        while i < stdlib_lines.len() {
                            let l = stdlib_lines[i];
                            depth += l.matches('{').count() as i32;
                            depth -= l.matches('}').count() as i32;
                            i += 1;
                            if depth > 0 {
                                entered = true;
                            }
                            if entered && depth <= 0 {
                                break;
                            }
                        }
                        continue;
                    }
                }
            }
        }
        // Drop struct/enum/newtype type definitions that the user IR already defines.
        if line.contains(" = type ")
            && (line.starts_with("%struct.")
                || line.starts_with("%enum.")
                || line.starts_with("%newtype."))
        {
            if let Some(name) = line.split_whitespace().next() {
                // Drop stdlib's definition only if user IR already defines
                // this type *concretely*. A merely-opaque user stub (e.g.
                // for a Result<T,E> instantiation main.ty never touches,
                // such as Result<Listener,Int32> when the program never
                // uses Network) must not shadow a concrete stdlib
                // definition — that previously left the type undefined on
                // both sides after merge, which is an implicitly opaque
                // struct as far as LLVM is concerned.
                //
                // But when the stdlib's own line is *also* just opaque
                // (e.g. Buf, an intentionally-opaque handle type whose real
                // definition lives in the C runtime, not LLVM IR), fall
                // back to deduping against any user declaration at all —
                // otherwise two identical `type opaque` lines for the same
                // name both survive and clang reports "redefinition of
                // type".
                let stdlib_line_concrete = is_concrete_type_line(line);
                let drop = if user_concrete_types.contains(name) {
                    true
                } else if !stdlib_line_concrete {
                    user_struct_types.contains(name)
                } else {
                    false
                };
                if drop {
                    i += 1;
                    continue;
                }
            }
        }
        // Everything else — including the correct net wrapper define bodies — is kept.
        stdlib_out_lines.push(stdlib_lines[i]);
        i += 1;
    }
    let stdlib_out = stdlib_out_lines.join("\n");
    std::fs::write(
        merged_path,
        format!("{}\n{}\n", user_ir_filtered, stdlib_out),
    )
}

pub fn link_ir(ir_file: &str, output: &str) {
    let mut build_dir = env::current_exe()
        .expect("Failed to get current exe path")
        .parent()
        .unwrap()
        .to_path_buf();
    build_dir.push("lib");

    // typhoon-stdlib.ll contains the LLVM IR definitions for the networking
    // method wrappers (__ty_method__Network__listen, __ty_method__Listener__accept,
    // __ty_method__Socket__consume, __ty_method__Socket__close).
    //
    // These wrappers allocate concrete Result structs that are only *defined*
    // in the codegen-emitted user IR (main.ll).  The two modules must therefore
    // be merged at the IR level with llvm-link before clang compiles them to
    // native code — passing both files to clang independently triggers
    // "Cannot allocate unsized type" because clang compiles each .ll in
    // isolation where the struct is still opaque.
    let stdlib_ll = find_stdlib_ll();

    // Choose the input that clang will actually compile: either the merged IR
    // (when stdlib exists and llvm-link succeeds) or the bare user IR as a
    // fallback that at least links non-networking programs.
    let merged_path = format!("{}.linked.ll", ir_file);
    let compile_input: String = if stdlib_ll.exists() {
        match merge_ir_text(ir_file, &stdlib_ll, &merged_path) {
            Ok(()) => merged_path.clone(),
            Err(e) => {
                eprintln!(
                    "warning: failed to merge typhoon-stdlib.ll ({}); \
                     networking symbols will be unresolved",
                    e
                );
                ir_file.to_string()
            }
        }
    } else {
        eprintln!(
            "warning: typhoon-stdlib.ll not found (searched {:?}); \
             networking symbols will be unresolved",
            stdlib_ll
        );
        ir_file.to_string()
    };

    let final_output = if cfg!(windows) && !output.ends_with(".exe") {
        format!("{}.exe", output)
    } else {
        output.to_string()
    };

    let mut cmd = Command::new("clang");
    cmd.arg("-v").arg(&compile_input);
    cmd.arg("-x").arg("none");
    cmd.arg("-L").arg(build_dir.as_os_str());
    cmd.arg("-lruntime");

    if cfg!(windows) {
        cmd.arg("-fms-runtime-lib=static");
        cmd.arg("-Wl,/NODEFAULTLIB:LIBCMTD");
        cmd.arg("-lWs2_32");
    } else {
        cmd.arg("-lm")
            .arg("-lpthread")
            .arg("-fno-omit-frame-pointer");
        if cfg!(target_os = "linux") {
            cmd.arg("-ldl");
        }
    }

    cmd.arg("-o").arg(&final_output);

    let clang_status = cmd.status();

    // Clean up the temporary merged IR regardless of clang's outcome.
    // if compile_input == merged_path {
    //     let _ = std::fs::remove_file(&merged_path);
    // }

    match clang_status {
        Ok(status) if status.success() => {
            println!("Linked successfully to: {}", final_output);
        }
        Ok(status) => {
            eprintln!("clang failed with status {}", status);
            std::process::exit(1);
        }
        Err(err) => {
            eprintln!("Failed to invoke clang: {}", err);
            std::process::exit(1);
        }
    }
}
