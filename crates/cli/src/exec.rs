use std::path::Path;
use std::process::Command;

pub fn execute_binary(binary_path: &str) {
    let mut path = Path::new(".").join(binary_path);
    if cfg!(windows) && path.extension().is_none() {
        path.set_extension("exe");
    }

    let status = Command::new(&path)
        .status()
        .expect("Failed to execute binary");

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}
