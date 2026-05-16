use std::path::Path;
use std::process;

mod build_kernel;
mod disk_image;

fn main() {
    let project_root = Path::new(&env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .unwrap()
        .to_path_buf();

    build_kernel::build_kernel(&project_root).unwrap();

    let status = process::Command::new(env!("CARGO"))
        .current_dir(&project_root)
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

    disk_image::build_disk_image(
        project_root.join("bzImage"),
        project_root
            .join("target")
            .join("x86_64-unknown-none")
            .join("debug")
            .join("init"),
        "image.bin",
    )
    .unwrap();
}
