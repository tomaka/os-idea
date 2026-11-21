use std::{fs, path::Path, path::PathBuf, process};

/// Builds the Wasm programs of the `wasm-programs` workspace, and returns the BLAKE3 hash of
/// each program alongside the path to its binary, in no particular order.
///
/// The programs are built for the `wasm32v1-none` target.
///
/// # Panic
///
/// Panics if the build fails or if reading a resulting binary fails.
pub fn build_wasm_programs_unordered(project_root: &Path) -> Vec<([u8; 32], PathBuf)> {
    let workspace_dir = project_root.join("wasm-programs");

    let status = process::Command::new(env!("CARGO"))
        .current_dir(&workspace_dir)
        .args(["build", "--release", "--workspace"])
        .args(["--target", "wasm32v1-none"])
        .status()
        .unwrap_or_else(|err| panic!("failed to run cargo: {err}"));
    if !status.success() {
        panic!("cargo build of the Wasm programs failed");
    }

    // TODO: list the members of the workspace instead of hardcoding them?
    ["test"]
        .into_iter()
        .map(|program_name| {
            let path = workspace_dir
                .join("target")
                .join("wasm32v1-none")
                .join("release")
                .join(format!("{program_name}.wasm"));
            let wasm = fs::read(&path)
                .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
            (*blake3::hash(&wasm).as_bytes(), path)
        })
        .collect::<Vec<_>>()
}
