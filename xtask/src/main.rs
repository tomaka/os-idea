use std::path::Path;
use std::process;

fn main() {
    let project_root = Path::new(&env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .unwrap()
        .to_path_buf();

    let status = process::Command::new(env!("CARGO"))
        .current_dir(project_root)
        .args([
            "build",
            "--target",
            "x86_64-unknown-none",
            "--package",
            "init",
        ])
        .status()
        .unwrap();
    if !status.success() {
        panic!("cargo build failed");
    }
}
