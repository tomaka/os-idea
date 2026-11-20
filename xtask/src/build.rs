use std::path::{Path, PathBuf};

use crate::{build_init, build_kernel};

/// Outcome of [`build`].
pub struct BuildOutput {
    /// Path to the kernel `bzImage`.
    pub bzimage: PathBuf,
    /// Path to the init ramdisk.
    pub init: PathBuf,
    /// List of Wasm programs to include at the root.
    pub wasm_programs: Vec<([u8; 32], PathBuf)>,
}

/// Builds the kernel and the init ramdisk.
///
/// # Panic
///
/// Panics if building the kernel fails.
pub fn build(project_root: &Path) -> BuildOutput {
    let init_secret_key = x25519_dalek::StaticSecret::from(rand::random::<[u8; 32]>());
    let init_public_key = x25519_dalek::PublicKey::from(&init_secret_key);

    let bzimage = build_kernel::build_kernel(project_root).unwrap();

    let init = build_init::build_init(build_init::Config {
        secret_key: *init_secret_key.as_bytes(),
        initial_executor_messages: Vec::new(), // TODO:
    });

    BuildOutput {
        bzimage,
        init,
        wasm_programs: Vec::new(), // TODO:
    }
}
