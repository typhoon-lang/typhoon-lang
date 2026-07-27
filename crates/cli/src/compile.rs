use std::collections::HashMap;
use std::fs;
use std::path::Path;

use typhoon_compiler::codegen::Codegen;
use typhoon_compiler::driver::compile_project;
use typhoon_compiler::error::{CompileError, SimpleError};
use typhoon_compiler::liveness::LiveAnalyzer;
use typhoon_compiler::span::Span;
use typhoon_compiler::type_inference::TypeChecker;

use crate::error_display::ErrorWriter;

fn render_simple_errors(errs: &[SimpleError], input_path: &Path) {
    let writer = ErrorWriter::new();
    for err in errs {
        let title = match err.code.as_str() {
            "E0425" => "cannot find value in this scope",
            "E0006" => "duplicate declaration",
            "E0008" => "invalid use path",
            "E0009" => "unknown namespace",
            "E0010" => "conflicting import",
            "E0011" => "unknown import",
            "E0012" => "unknown type",
            "E0001" => "I/O error",
            "E0002" => "missing @ty_sig annotation",
            "E0003" => "invalid @ty_sig annotation",
            "E0004" => "@ty_sig method name mismatch",
            "E0005" => "missing namespace declaration",
            "E0007" => "cyclic namespace dependency",
            _ => "parse error",
        };
        let ce = CompileError::error(&err.code, title, err.span, &err.message);
        writer.render(&ce, input_path);
    }
}

fn render_errors(errs: &[CompileError], input_path: &Path) {
    let writer = ErrorWriter::new();
    for err in errs {
        writer.render(err, input_path);
    }
}

fn span_from_string(s: &str) -> Span {
    // Parse "line:col (start..end)" format from format_span
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() >= 2 {
        let loc = parts[0]; // "line:col"
        let range = parts.get(1).map(|s| s.trim_matches(|c| c == '(' || c == ')')).unwrap_or("0..0");
        let line_col: Vec<&str> = loc.split(':').collect();
        if line_col.len() == 2 {
            let line = line_col[0].parse().unwrap_or(0);
            let col = line_col[1].parse().unwrap_or(0);
            let range_parts: Vec<&str> = range.split("..").collect();
            let start = range_parts[0].parse().unwrap_or(0);
            let end = range_parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(start);
            return Span::new(start, end, line, col);
        }
    }
    Span::default()
}

pub fn compile(input: &str, output: &str) {
    let ir_path = compile_to_ir(input, output);
    crate::link::link_ir(ir_path.to_str().unwrap(), output);
}

pub fn compile_to_ir(input: &str, output: &str) -> std::path::PathBuf {
    let input_path = Path::new(input);
    let (module, imports, original_ns_by_symbol) = match compile_project(input_path) {
        Ok(m) => m,
        Err(errs) => {
            render_simple_errors(&errs, input_path);
            std::process::exit(1);
        }
    };

    let mut checker = TypeChecker::new();
    if let Err(err) = checker.check_module(&module, &imports) {
        let ce = CompileError::from_type_error(err, &checker.solver);
        render_errors(&[ce], input_path);
        std::process::exit(1);
    }

    let mut liveness = LiveAnalyzer::new();
    let result = match liveness.analyze_module(&module) {
        Ok(drop_map) => drop_map,
        Err(errors) => {
            let compile_errors: Vec<CompileError> = errors
                .into_iter()
                .enumerate()
                .map(|(i, e)| {
                    let span = if e.contains("span ") {
                        if let Some(span_idx) = e.find("span ") {
                            let span_str = &e[span_idx + 5..];
                            span_from_string(span_str.split(']').next().unwrap_or(""))
                        } else {
                            Span::default()
                        }
                    } else {
                        Span::default()
                    };
                    CompileError::error(
                        &format!("E{:04}", 4000 + i),
                        "ownership error",
                        span,
                        &e,
                    )
                })
                .collect();
            render_errors(&compile_errors, input_path);
            std::process::exit(1);
        }
    };

    let ir = Codegen::lower_module(
        &module,
        checker.types(),
        &checker.specializations,
        result,
        &original_ns_by_symbol,
        &checker.registry.enum_variants,
    );
    let ir_text = ir.to_llvm_ir();

    let ll_path = Path::new(output).with_extension("ll");
    if let Err(err) = fs::write(&ll_path, ir_text) {
        eprintln!("Failed to write IR file: {}", err);
        std::process::exit(1);
    }
    ll_path
}