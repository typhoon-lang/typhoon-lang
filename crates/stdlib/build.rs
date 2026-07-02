use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Get paths from Cargo environment
    let workspace_dir = PathBuf::from(env::var("CARGO_WORKSPACE_DIR").unwrap());
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Calculate workspace root (crates/stdlib -> crates -> root)
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    let target_dir = workspace_root.join("target");

    let stdlib_src = manifest_dir.join("src");
    let stdlib_out = workspace_dir.join("typhoon-stdlib");

    // Skip if no source directory
    if !stdlib_src.exists() {
        println!("cargo:warning=No src/ directory found, skipping");
        return;
    }

    // Find tyc binary (check both debug and release)
    let tyc = ["debug", "release"]
        .iter()
        .find_map(|profile| {
            let exe = target_dir.join(profile).join("tyc");
            let exe_win = target_dir.join(profile).join("tyc.exe");

            if exe.exists() {
                Some(exe)
            } else if exe_win.exists() {
                Some(exe_win)
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("tyc binary not found. Run: cargo build -p typhoon-cli"));

    println!("cargo:warning=Using tyc: {}", tyc.display());
    println!("cargo:warning=Output path: {}", stdlib_out.display());

    // Create output directory
    std::fs::create_dir_all(&stdlib_out).expect("Failed to create output dir");

    // Run compiler
    let status = Command::new(&tyc)
        .arg("build")
        .arg(&stdlib_src)
        .arg(&stdlib_out)
        .arg("--compile")
        .status()
        .expect("Failed to execute tyc");

    if !status.success() {
        panic!("Typhoon compilation failed");
    }

    println!("cargo:rerun-if-changed=src/");
    println!("cargo:rerun-if-changed=../compiler/src/");
}
