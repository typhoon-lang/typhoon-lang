use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use typhoon_lang::codegen::Codegen;
use typhoon_lang::driver::compile_project;
use typhoon_lang::liveness::LiveAnalyzer;
use typhoon_lang::resolver::Resolver;
use typhoon_lang::type_inference::TypeChecker;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: tpc <input.ty> [output]");
        std::process::exit(1);
    }

    let input = &args[1];
    let output = if args.len() > 2 {
        args[2].clone()
    } else {
        "a.out".to_string()
    };

    let module = match compile_project(Path::new(input)) {
        Ok(m) => m,
        Err(errs) => {
            for e in errs {
                eprintln!("Compile error: {}", e);
            }
            std::process::exit(1);
        }
    };

    let mut resolver = Resolver::new();
    if let Err(errors) = resolver.resolve_module(&module) {
        for err in errors {
            eprintln!("Resolve error: {}", err);
        }
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
            for err in errors {
                eprintln!("Liveness error: {}", err);
            }
            std::process::exit(1);
        }
    };

    let ir = Codegen::lower_module(&module, checker.types(), result);
    let ir_text = ir.to_llvm_ir();

    let ll_path = Path::new(&output).with_extension("ll");
    if let Err(err) = fs::write(&ll_path, ir_text) {
        eprintln!("Failed to write IR file {}: {}", ll_path.display(), err);
        std::process::exit(1);
    }

    let build_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("build")
        .join("output");

    let mut cmd = Command::new("clang");

    // 1. Specify the input IR (use -x ir if your file doesn't end in .ll)
    cmd.arg("-v").arg(ll_path.as_os_str());

    // 2. Stop 'ir' mode so it doesn't try to parse the library as IR
    cmd.arg("-x").arg("none");

    // 3. Set the library search path (the DIRECTORY)
    cmd.arg("-L").arg(build_dir.as_os_str());

    // 4. Set the library name
    // Clang will find runtime.lib on Windows or libruntime.a on Unix
    cmd.arg("-lruntime");
    // Networking runtime uses Winsock on Windows.
    if cfg!(windows) {
        cmd.arg("-lWs2_32");
    }

    // 5. Platform-Specific Glue
    if cfg!(windows) {
        // Forces Clang to link against the Static CRT (libcmt.lib)
        // This resolves the "__imp_strtod" errors
        cmd.arg("-fms-runtime-lib=static");
        // Explicitly ignore the debug runtime to prevent LNK4098 conflicts
        cmd.arg("-Wl,/NODEFAULTLIB:LIBCMTD");
    } else {
        // Linux and macOS need to link the math library and threads
        // if your runtime uses any C-standard headers or threading.
        cmd.arg("-lm");
        cmd.arg("-lpthread");
        cmd.arg("-fno-omit-frame-pointer");

        if cfg!(target_os = "linux") {
            // Required for some low-level runtime features on Linux
            cmd.arg("-ldl");
        }
    }

    // 6. Add output flags
    cmd.arg("-o").arg(&output);

    match cmd.status() {
        Ok(status) if status.success() => {
            println!("Wrote {}", output);
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
