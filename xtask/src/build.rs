use crate::{build_init, build_kernel};
use core::iter;
use std::path::{Path, PathBuf};

/// Outcome of [`build`].
pub struct BuildOutput {
    /// Path to the kernel `bzImage`.
    pub bzimage: PathBuf,
    /// Path to the init ramdisk.
    pub init: PathBuf,
    /// List of Wasm programs to include at the root.
    pub wasm_programs: Vec<([u8; 32], PathBuf)>,
}

interface_bindings::interface_bindings_definitions_only!("../interfaces/executor.cddl");

/// Builds the kernel and the init ramdisk.
///
/// # Panic
///
/// Panics if building the kernel fails.
pub fn build(project_root: &Path) -> BuildOutput {
    let init_secret_key = x25519_dalek::StaticSecret::from(rand::random::<[u8; 32]>());
    let init_public_key = x25519_dalek::PublicKey::from(&init_secret_key);

    let bzimage = build_kernel::build_kernel(project_root).unwrap();

    let messages = iter::once(executor::StartRequest(
        [0; 32],
        executor::Interfaces(Default::default()),
    ));

    let init = build_init::build_init(build_init::Config {
        secret_key: *init_secret_key.as_bytes(),
        initial_executor_messages: messages.fold(Vec::new(), |mut out_buffer, message| {
            message.encode_into(|slice| out_buffer.extend_from_slice(slice));
            out_buffer
        }),
    });

    BuildOutput {
        bzimage,
        init,
        wasm_programs: Vec::new(), // TODO:
    }
}
