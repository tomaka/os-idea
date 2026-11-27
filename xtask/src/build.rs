use crate::{build_init, build_kernel, build_wasm_programs};
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

/// Builds the kernel, the init ramdisk, and the Wasm programs.
///
/// # Panic
///
/// Panics if building the kernel or the Wasm programs fails.
pub fn build(multi: &indicatif::MultiProgress, project_root: &Path) -> BuildOutput {
    let overall = multi.add(indicatif::ProgressBar::new(3));
    overall.set_style(
        indicatif::ProgressStyle::with_template("{prefix:>16.bold} [{bar:30}] {pos}/{len}")
            .unwrap_or_else(|_| unreachable!())
            .progress_chars("=> "),
    );
    overall.set_prefix("Building");

    let init_secret_key = x25519_dalek::StaticSecret::from(rand::random::<[u8; 32]>());
    let init_public_key = x25519_dalek::PublicKey::from(&init_secret_key);

    let bzimage = build_kernel::build_kernel(multi, project_root).unwrap();
    overall.inc(1);
    let wasm_programs = build_wasm_programs::build_wasm_programs_unordered(multi, project_root);
    overall.inc(1);

    let messages = iter::once(executor::StartRequest(
        [0; 32],
        executor::Interfaces(Default::default()),
    ));

    let init = build_init::build_init(
        multi,
        build_init::Config {
            secret_key: *init_secret_key.as_bytes(),
            initial_executor_messages: messages.fold(Vec::new(), |mut out_buffer, message| {
                message.encode_into(|slice| out_buffer.extend_from_slice(slice));
                out_buffer
            }),
        },
    );
    overall.inc(1);
    overall.finish_and_clear();

    BuildOutput {
        bzimage,
        init,
        wasm_programs,
    }
}
