extern crate cmake;
use cmake::Config;
use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let dst = Config::new(".").build();

    // 1. Define the "pretty" path: target/lib/
    // We navigate up from the hashed build dir to the general target dir
    let out_dir = env::var("OUT_DIR").unwrap();
    let mut pretty_path = PathBuf::from(out_dir);
    pretty_path.pop(); // pop 'out'
    pretty_path.pop(); // pop the 'hash' dir
    pretty_path.pop(); // pop 'build'
    pretty_path.push("lib"); // This puts it in target/debug/lib (or release)

    // 2. Create the directory if it doesn't exist
    fs::create_dir_all(&pretty_path).unwrap();

    // 3. Copy the file
    let lib_name = if cfg!(target_os = "windows") {
        "runtime.lib"
    } else {
        "libruntime.a"
    };
    let src_path = dst.join("lib").join(lib_name);

    let dest_path = pretty_path.join(lib_name);
    // Use a match or if-let to handle the "Already Exists" case
    if let Err(e) = fs::create_dir_all(&pretty_path) {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            panic!("Failed to create lib directory: {}", e);
        }
    }

    fs::copy(&src_path, &dest_path).expect("Failed to copy library to pretty path");

    // 4. Tell Cargo to look in the pretty path
    println!("cargo:rustc-link-search=native={}", pretty_path.display());
    println!("cargo:rustc-link-lib=static=runtime");
}
