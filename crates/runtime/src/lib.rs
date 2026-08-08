// lib.rs
#[cfg(test)]
mod c_tests {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// How long a single C test binary is allowed to run before we kill it
    /// and fail the test with a clear message, instead of the process
    /// hanging silently until the CI job's own (much coarser) step timeout
    /// eventually kills the whole runner.
    const C_TEST_TIMEOUT: Duration = Duration::from_secs(60);

    /// Run one C test binary and assert it exits successfully within
    /// C_TEST_TIMEOUT.
    ///
    /// stdout/stderr are inherited (not captured) so output streams live to
    /// the CI log as the child produces it — this matters specifically for
    /// a hang: `Command::output()` pipes and buffers both streams
    /// internally and only hands anything back after the child exits, so a
    /// binary that never exits produces zero visible output right up until
    /// something external kills the job, no matter what capture flags are
    /// passed to `cargo test` itself (those only affect this process's own
    /// print!/println!, not a child's inherited fds).
    fn run_c_binary(label: &str, path: &str) {
        println!("Running C test binary: {} ({})", label, path);

        let mut child = Command::new(path)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|e| panic!("Failed to execute '{}': {}", path, e));

        let start = Instant::now();
        let status = loop {
            match child
                .try_wait()
                .unwrap_or_else(|e| panic!("Failed to poll '{}': {}", path, e))
            {
                Some(status) => break status,
                None => {
                    if start.elapsed() >= C_TEST_TIMEOUT {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!(
                            "C test binary '{}' ({}) timed out after {:?} — killed. \
                             See the streamed output above for the last thing it \
                             printed before it stalled.",
                            label, path, C_TEST_TIMEOUT
                        );
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        };

        assert!(
            status.success(),
            "C test binary '{}' ({}) exited with status: {}",
            label,
            path,
            status
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
        test_task03_double_close,
        "C_TEST_PATH_TEST_TASK03_DOUBLE_CLOSE"
    );

    // Phase 2 — socket architecture rewrite
    c_test!(
        test_phase2_listener_close,
        "C_TEST_PATH_TEST_PHASE2_LISTENER_CLOSE"
    );
    c_test!(
        test_phase2_accept_write_close,
        "C_TEST_PATH_TEST_PHASE2_ACCEPT_WRITE_CLOSE"
    );
    c_test!(
        test_phase2_coroutine_loopback,
        "C_TEST_PATH_TEST_PHASE2_COROUTINE_LOOPBACK"
    );
    c_test!(test_phase2_into_chan, "C_TEST_PATH_TEST_PHASE2_INTO_CHAN");
    c_test!(
        test_phase2_write_read_roundtrip,
        "C_TEST_PATH_TEST_PHASE2_WRITE_READ_ROUNDTRIP"
    );

    // Phase 3 — file and stdio as linear types
    c_test!(
        test_phase3_large_sprintf,
        "C_TEST_PATH_TEST_PHASE3_LARGE_SPRINTF"
    );
    c_test!(
        test_phase3_buf_growth_10k,
        "C_TEST_PATH_TEST_PHASE3_BUF_GROWTH_10K"
    );
    c_test!(
        test_phase3_file_chunked_read,
        "C_TEST_PATH_TEST_PHASE3_FILE_CHUNKED_READ"
    );
    c_test!(
        test_phase3_file_lifecycle,
        "C_TEST_PATH_TEST_PHASE3_FILE_LIFECYCLE"
    );

    // Phase 4 — platform IO driver integration
    c_test!(test_phase4_fdset, "C_TEST_PATH_TEST_PHASE4_FDSET");
    c_test!(test_phase4_mock_io, "C_TEST_PATH_TEST_PHASE4_MOCK_IO");
    c_test!(test_phase4_net_fdset, "C_TEST_PATH_TEST_PHASE4_NET_FDSET");
    c_test!(
        test_phase4_fdset_live_worker,
        "C_TEST_PATH_TEST_PHASE4_FDSET_LIVE_WORKER"
    );
    #[cfg(windows)]
    c_test!(
        test_phase3_file_iocp_coroutine,
        "C_TEST_PATH_TEST_PHASE3_FILE_IOCP_COROUTINE"
    );
    c_test!(
        test_phase4_fdset_100_sockets_asan,
        "C_TEST_PATH_TEST_PHASE4_FDSET_100_SOCKETS_ASAN"
    );
    #[cfg(target_os = "linux")]
    c_test!(
        test_phase4_linux_1000_coroutines,
        "C_TEST_PATH_TEST_PHASE4_LINUX_1000_COROUTINES"
    );
    #[cfg(target_os = "macos")]
    c_test!(
        test_phase4_macos_coroutine_loopback,
        "C_TEST_PATH_TEST_PHASE4_MACOS_COROUTINE_LOOPBACK"
    );
}
