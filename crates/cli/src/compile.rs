use std::fs;
use std::path::Path;

use typhoon_compiler::codegen::Codegen;
use typhoon_compiler::driver::compile_project;
use typhoon_compiler::liveness::LiveAnalyzer;
use typhoon_compiler::type_inference::TypeChecker;

pub fn compile(input: &str, output: &str) {
    let ir_path = compile_to_ir(input, output);
    crate::link::link_ir(ir_path.to_str().unwrap(), output);
}

pub fn compile_to_ir(input: &str, output: &str) -> std::path::PathBuf {
    let module = match compile_project(Path::new(input)) {
        Ok(m) => m,
        Err(errs) => {
            for e in errs {
                eprintln!("Compile error: {}", e);
            }
            std::process::exit(1);
        }
    };

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

    let ir = Codegen::lower_module(&module, checker.types(), &checker.specializations, result);
    let ir_text = ir.to_llvm_ir();

    let ll_path = Path::new(output).with_extension("ll");
    if let Err(err) = fs::write(&ll_path, ir_text) {
        eprintln!("Failed to write IR file: {}", err);
        std::process::exit(1);
    }
    ll_path
}
