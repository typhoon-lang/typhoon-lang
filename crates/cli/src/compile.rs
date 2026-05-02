use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use typhoon_compiler::codegen::Codegen;
use typhoon_compiler::driver::compile_project;
use typhoon_compiler::liveness::LiveAnalyzer;
use typhoon_compiler::resolver::Resolver;
use typhoon_compiler::type_inference::TypeChecker;

pub fn compile(input: &str, output: &str) {
    let module = match compile_project(Path::new(input)) {
        Ok(m) => m,
        Err(errs) => {
            for e in errs { eprintln!("Compile error: {}", e); }
            std::process::exit(1);
        }
    };

    let mut resolver = Resolver::new();
    if let Err(errors) = resolver.resolve_module(&module) {
        for err in errors { eprintln!("Resolve error: {}", err); }
        std::process::exit(1);
    }

    let mut checker = TypeChecker::new();
    if let Err(err) = checker.check_module(&module) {
        eprintln!("Type error: {:?}", err);
        std::process::exit(1);
    }

    let mut liveness = LiveAnalyzer::new();
    let result = match liveness.analyze_module(&module) {
        Ok(drop_map) => drop_map,
        Err(errors) => {
            for err in errors { eprintln!("Liveness error: {}", err); }
            std::process::exit(1);
        }
    };

    let ir = Codegen::lower_module(&module, checker.types(), result);
    let ir_text = ir.to_llvm_ir();

    let ll_path = Path::new(output).with_extension("ll");
    if let Err(err) = fs::write(&ll_path, ir_text) {
        eprintln!("Failed to write IR file: {}", err);
        std::process::exit(1);
    }

    let mut build_dir = env::current_exe()
        .expect("Failed to get current exe path")
        .parent()
        .unwrap()
        .to_path_buf();
    build_dir.push("lib");

    let mut cmd = Command::new("clang");
    cmd.arg("-v").arg(ll_path.as_os_str());
    cmd.arg("-x").arg("none");
    cmd.arg("-L").arg(build_dir.as_os_str());
    cmd.arg("-lruntime");

    if cfg!(windows) {
        cmd.arg("-fms-runtime-lib=static");
        cmd.arg("-Wl,/NODEFAULTLIB:LIBCMTD");
        cmd.arg("-lWs2_32");
    } else {
        cmd.arg("-lm").arg("-lpthread").arg("-fno-omit-frame-pointer");
        if cfg!(target_os = "linux") { cmd.arg("-ldl"); }
    }

    cmd.arg("-o").arg(output);

    match cmd.status() {
        Ok(status) if status.success() => {
            println!("Compiled successfully to: {}", output);
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
