use std::{fs, path::Path, path::PathBuf, process};

/// Builds the init program and returns the path to the resulting executable.
///
/// The `init` crate is a library. This function creates a temporary Cargo project that depends
/// on it and whose `main` function calls `init::run()` with a randomly-generated secret key,
/// then builds that project.
/// The build artifacts are stored in the `target` directory at the root of the repository, so
/// that they are kept between builds even though the temporary project is deleted.
///
/// # Panic
///
/// Panics if creating the temporary project fails or if the build fails.
pub fn build_init() -> PathBuf {
    let project_root = Path::new(&env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .unwrap_or_else(|| unreachable!())
        .to_path_buf();
    let init_dir = project_root.join("init");
    let target_dir = project_root.join("target");

    let tmp = tempfile::tempdir()
        .unwrap_or_else(|err| panic!("failed to create temporary directory: {err}"));

    // `{:?}` produces a quoted and escaped string that is also a valid TOML basic string.
    let cargo_toml = format!(
        "[package]\n\
         name = \"init-runner\"\n\
         version = \"0.1.0\"\n\
         edition = \"2024\"\n\
         \n\
         [workspace]\n\
         \n\
         [dependencies]\n\
         init = {{ path = {:?} }}\n",
        init_dir.to_str().unwrap_or_else(|| panic!("non-UTF-8 project path")),
    );
    fs::write(tmp.path().join("Cargo.toml"), cargo_toml)
        .unwrap_or_else(|err| panic!("failed to write Cargo.toml: {err}"));
    fs::create_dir(tmp.path().join("src"))
        .unwrap_or_else(|err| panic!("failed to create src directory: {err}"));
    // The `Debug` implementation of `[u8; 32]` produces a valid Rust array expression.
    let secret_key = rand::random::<[u8; 32]>();
    fs::write(
        tmp.path().join("src").join("main.rs"),
        format!(
            "fn main() {{\n    \
                 init::run(init::Config {{\n        \
                     secret_key: {secret_key:?},\n        \
                     initial_executor_messages: &[],\n    \
                 }})\n\
             }}\n"
        ),
    )
    .unwrap_or_else(|err| panic!("failed to write main.rs: {err}"));
    // Reuse the lock file of `init` so that dependency versions stay pinned.
    fs::copy(init_dir.join("Cargo.lock"), tmp.path().join("Cargo.lock"))
        .unwrap_or_else(|err| panic!("failed to copy Cargo.lock: {err}"));

    let status = process::Command::new(env!("CARGO"))
        .current_dir(tmp.path())
        .arg("build")
        .args(["--target", "x86_64-unknown-linux-musl"])
        .arg("--target-dir")
        .arg(&target_dir)
        // Cargo looks for configuration files starting from the current directory, which is
        // the temporary directory, so the configuration of `init` must be passed explicitly.
        .arg("--config")
        .arg(init_dir.join(".cargo").join("config.toml"))
        .status()
        .unwrap_or_else(|err| panic!("failed to run cargo: {err}"));
    if !status.success() {
        panic!("cargo build failed");
    }

    target_dir
        .join("x86_64-unknown-linux-musl")
        .join("debug")
        .join("init-runner")
}
