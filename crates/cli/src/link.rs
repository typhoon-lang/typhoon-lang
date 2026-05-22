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

fn merge_ir_text(ir_file: &str, stdlib_ll: &PathBuf, merged_path: &str) -> std::io::Result<()> {
    let user_ir = std::fs::read_to_string(ir_file)?;
    let user_ir = user_ir
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            !(line.starts_with("declare ") && line.contains("@__ty_method__"))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let stdlib_ir = std::fs::read_to_string(stdlib_ll)?;
    let stdlib_without_types = stdlib_ir
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            !line.starts_with("declare ")
                && !(line.contains(" = type ")
                    && (line.starts_with("%struct.")
                        || line.starts_with("%enum.")
                        || line.starts_with("%newtype.")))
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        merged_path,
        format!("{}\n{}\n", user_ir, stdlib_without_types),
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
