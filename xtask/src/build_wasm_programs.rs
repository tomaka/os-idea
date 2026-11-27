use crate::cargo;
use std::{fs, path::Path, path::PathBuf, process};

/// Builds the Wasm programs of the `wasm-programs` workspace, and returns the BLAKE3 hash of
/// each program alongside the path to its binary, in no particular order.
///
/// The programs are built for the `wasm32v1-none` target.
///
/// # Panic
///
/// Panics if the build fails or if reading a resulting binary fails.
pub fn build_wasm_programs_unordered(
    multi: &indicatif::MultiProgress,
    project_root: &Path,
) -> Vec<([u8; 32], PathBuf)> {
    let workspace_dir = project_root.join("wasm-programs");
    let bar = multi.add(indicatif::ProgressBar::new_spinner());
    bar.set_style(
        indicatif::ProgressStyle::with_template("{spinner:.green} {prefix:>14.bold} {wide_msg}")
            .unwrap_or_else(|_| unreachable!()),
    );
    bar.set_prefix("Wasm programs");
    bar.enable_steady_tick(core::time::Duration::from_millis(100));

    let status = cargo::run(
        multi,
        &bar,
        process::Command::new(env!("CARGO"))
            .current_dir(&workspace_dir)
            .args(["build", "--release", "--workspace"])
            .args(["--target", "wasm32v1-none"]),
    );
    if !status.success() {
        bar.abandon_with_message("failed");
        panic!("cargo build of the Wasm programs failed");
    }

    // TODO: list the members of the workspace instead of hardcoding them?
    let programs = ["test"]
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
        .collect::<Vec<_>>();

    bar.finish_with_message("done");
    programs
}
