use std::env;
use std::process::Command;

pub fn link_ir(ir_file: &str, output: &str) {
    let mut build_dir = env::current_exe()
        .expect("Failed to get current exe path")
        .parent()
        .unwrap()
        .to_path_buf();
    build_dir.push("lib");

    let mut cmd = Command::new("clang");
    cmd.arg("-v").arg(ir_file);
    cmd.arg("-x").arg("none");
    cmd.arg("-L").arg(build_dir.as_os_str());
    cmd.arg("-lruntime");

    if cfg!(windows) {
        cmd.arg("-fms-runtime-lib=static");
        cmd.arg("-Wl,/NODEFAULTLIB:LIBCMTD");
        cmd.arg("-lWs2_32");
    } else {
        cmd.arg("-lm")
            .arg("-lpthread")
            .arg("-fno-omit-frame-pointer");
        if cfg!(target_os = "linux") {
            cmd.arg("-ldl");
        }
    }

    let final_output = if cfg!(windows) && !output.ends_with(".exe") {
        format!("{}.exe", output)
    } else {
        output.to_string()
    };
    cmd.arg("-o").arg(&final_output);

    match cmd.status() {
        Ok(status) if status.success() => {
            println!("Linked successfully to: {}", final_output);
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
