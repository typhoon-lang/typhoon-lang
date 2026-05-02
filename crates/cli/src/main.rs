use std::env;
use std::path::Path;

mod compile;
mod exec;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 3 {
        print_usage();
        std::process::exit(1);
    }

    let command = &args[1];
    match command.as_str() {
        "build" => {
            if args.len() < 4 {
                eprintln!("Error: 'build' requires <filename> and <output>");
                std::process::exit(1);
            }
            let filename = &args[2];
            let output = &args[3];
            compile(filename, output);
        }
        "run" => {
            let filename = &args[2];
            // Create a temporary output name (e.g., "main.ty" -> "main")
            let output = Path::new(filename)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("temp_bin");

            compile(filename, output);
            exec::execute_binary(output);
        }
        _ => {
            print_usage();
            std::process::exit(1);
        }
    }
}

fn compile(input: &str, output: &str) {
    compile::compile(input, output)
}

fn print_usage() {
    println!("Usage:");
    println!("  tyc build <filename> <output>  - Compile to a specific output");
    println!("  tyc run   <filename>           - Compile and run immediately");
}
