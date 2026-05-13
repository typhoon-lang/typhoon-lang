use std::env;
use std::path::Path;

mod compile;
mod exec;
mod link;

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
                eprintln!("Usage: tyc build <filename> <output> [--compile | --link]");
                std::process::exit(1);
            }
            let filename = &args[2];
            let output = &args[3];
            let flag = args.get(4).map(|s| s.as_str());

            match flag {
                Some("--compile") => {
                    compile::compile_to_ir(filename, output);
                }
                Some("--link") => {
                    link::link_ir(filename, output);
                }
                None => {
                    compile::compile(filename, output);
                }
                _ => {
                    eprintln!("Unknown flag: {}", flag.unwrap());
                    std::process::exit(1);
                }
            }
        }
        "run" => {
            let filename = &args[2];
            let output = Path::new(filename)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("temp_bin");

            let output_path = if cfg!(windows) {
                format!("{}.exe", output)
            } else {
                output.to_string()
            };

            compile(filename, &output_path);
            exec::execute_binary(&output_path);
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
    println!("  tyc build <filename> <output> [--compile | --link] - Build, compile IR, or link");
    println!("  tyc run   <filename>                                - Compile and run immediately");
}
