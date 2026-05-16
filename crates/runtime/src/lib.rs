// lib.rs
#[cfg(test)]
mod c_tests {
    use std::process::Command;

    /// Run one C test binary and assert it exits successfully.
    /// stdout is always captured and printed (visible with `cargo test -- --nocapture`).
    /// stderr is printed only on failure.
    fn run_c_binary(label: &str, path: &str) {
        println!("Running C test binary: {} ({})", label, path);

        let output = Command::new(path)
            .output()
            .unwrap_or_else(|e| panic!("Failed to execute '{}': {}", path, e));

        println!(
            "--- {} STDOUT ---\n{}",
            label,
            String::from_utf8_lossy(&output.stdout)
        );

        if !output.status.success() {
            eprintln!(
                "--- {} STDERR ---\n{}",
                label,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        assert!(
            output.status.success(),
            "C test binary '{}' ({}) exited with status: {}",
            label,
            path,
            output.status
        );
    }

    /// Declare one #[test] per C binary.
    /// The env var name must match what build.rs emits:
    ///   C_TEST_PATH_<STEM_UPPERCASE>
    ///
    /// To add a new test file:
    ///   1. Add it to C_TEST_SOURCES in CMakeLists.txt.
    ///   2. Add a c_test! line here using the uppercased stem.
    /// No changes to build.rs are needed.
    macro_rules! c_test {
        ($fn_name:ident, $env_key:literal) => {
            #[test]
            fn $fn_name() {
                run_c_binary(stringify!($fn_name), env!($env_key));
            }
        };
    }

    c_test!(
        test_task01_dangling_ptr,
        "C_TEST_PATH_TEST_TASK01_DANGLING_PTR"
    );
    c_test!(
        test_task02_sscan_mutation,
        "C_TEST_PATH_TEST_TASK02_SSCAN_MUTATION"
    );
    c_test!(
        test_task03_double_close,
        "C_TEST_PATH_TEST_TASK03_DOUBLE_CLOSE"
    );
    c_test!(test_task04_overflow, "C_TEST_PATH_TEST_TASK04_OVERFLOW");
}
