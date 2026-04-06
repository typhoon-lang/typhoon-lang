use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use typhoon_lang::codegen::Codegen;
use typhoon_lang::lexer::Lexer;
use typhoon_lang::parser::Parser;
use typhoon_lang::resolver::Resolver;
use typhoon_lang::type_inference::TypeChecker;
use typhoon_lang::liveness::LiveAnalyzer;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: typhoon-lang <input.ty> [output]");
        std::process::exit(1);
    }

    let input = &args[1];
    let output = if args.len() > 2 {
        args[2].clone()
    } else {
        "a.out".to_string()
    };

    let source = match fs::read_to_string(input) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("Failed to read {}: {}", input, err);
            std::process::exit(1);
        }
    };

    let tokens = Lexer::new(source).tokenize();
    let module = match Parser::new(tokens).parse_module() {
        Ok(m) => m,
        Err(err) => {
            eprintln!("Parse error: {}", err);
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
    if let Err(errors) = liveness.analyze_module(&module) {
        for err in errors {
            eprintln!("Liveness error: {}", err);
        }
        std::process::exit(1);
    }

    let ir = Codegen::lower_module(&module);
    let ir_text = ir.to_llvm_ir();

    let ll_path = Path::new(&output).with_extension("ll");
    if let Err(err) = fs::write(&ll_path, ir_text) {
        eprintln!("Failed to write IR file {}: {}", ll_path.display(), err);
        std::process::exit(1);
    }

    match Command::new("clang")
        .arg(ll_path.as_os_str())
        .arg("-x")
        .arg("ir")
        .arg("-o")
        .arg(&output)
        .status()
    {
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
