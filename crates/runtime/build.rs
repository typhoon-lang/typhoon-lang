extern crate cmake;
use cmake::Config;
use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let dst = Config::new(".")
        .define("BUILD_TESTS", "OFF") // Enable the test option we added
        .build();

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

    // 4. Locate every test binary CMake installed under bin/.
    // CMake installs them as <dst>/bin/<name> (or <name>.exe on Windows).
    // We read the directory so build.rs never needs to know the list — it is
    // the single source of truth that lives in CMakeLists.txt.
    let bin_dir = dst.join("bin");
    let entries = fs::read_dir(&bin_dir)
        .map(|read_dir| read_dir.collect::<Vec<_>>())
        .unwrap_or_default();

    let mut found_any = false;
    for entry in entries {
        let entry = entry.expect("Failed to read bin dir entry");
        let path = entry.path();

        // Skip non-files and non-executables (e.g. .pdb on Windows).
        if !path.is_file() {
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !ext.is_empty() && ext != "exe" {
            continue;
        }

        // Derive an ALL_CAPS env-var-safe key from the stem, e.g.
        //   main_test              -> C_TEST_PATH_MAIN_TEST
        //   test_task03_double_close -> C_TEST_PATH_TEST_TASK03_DOUBLE_CLOSE
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("Non-UTF8 binary name");
        let key = format!("C_TEST_PATH_{}", stem.to_uppercase());

        // 5. Emit each path so lib.rs can retrieve it with env!().
        println!("cargo:rustc-env={}={}", key, path.display());
        found_any = true;
    }

    // if !found_any {
    //     panic!("BUILD_TESTS=ON but no test binaries found in {:?}", bin_dir);
    // }

    // 6. Tell Cargo to look in the pretty path
    println!("cargo:rustc-link-search=native={}", pretty_path.display());
    println!("cargo:rustc-link-lib=static=runtime");
}
