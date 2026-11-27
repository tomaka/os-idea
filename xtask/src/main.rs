use std::{
    path::{Path, PathBuf},
    process,
};

mod build;
mod build_init;
mod build_kernel;
mod build_wasm_programs;
mod cargo;
mod disk_image;

#[derive(clap::Parser)]
#[command(about = "Build tooling for the project")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Builds the disk image at `target/image.img`.
    Build,
    /// Builds the disk image, then boots it with QEMU.
    Run {
        /// Path to the UEFI firmware (OVMF) to use. Auto-detected if not provided.
        #[arg(long)]
        firmware: Option<PathBuf>,
        /// Name or path of the QEMU binary to use.
        #[arg(long, default_value = "qemu-system-x86_64")]
        qemu: PathBuf,
    },
}

fn main() {
    let cli = <Cli as clap::Parser>::parse();

    let project_root = Path::new(&env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .unwrap()
        .to_path_buf();

    let image_path = project_root.join("target").join("image.img");

    let multi = indicatif::MultiProgress::new();

    let build::BuildOutput {
        bzimage,
        init,
        wasm_programs,
    } = build::build(&multi, &project_root);

    disk_image::build_disk_image(
        &multi,
        bzimage,
        init,
        wasm_programs.into_iter(),
        &image_path,
    )
    .unwrap();

    match cli.command {
        Command::Build => {}
        Command::Run { firmware, qemu } => {
            let firmware = match firmware {
                Some(firmware) => firmware,
                None => {
                    let Some(firmware) = find_firmware(&qemu) else {
                        eprintln!(
                            "Could not find a UEFI firmware for QEMU. Please pass one with `--firmware`."
                        );
                        process::exit(1);
                    };
                    firmware
                }
            };

            let status = process::Command::new(&qemu)
                .arg("-machine")
                .arg("q35")
                .arg("-m")
                .arg("1G")
                .arg("-drive")
                .arg(format!(
                    "if=pflash,format=raw,readonly=on,file={}",
                    firmware.display()
                ))
                .arg("-drive")
                .arg(format!("format=raw,file={}", image_path.display()))
                .arg("-nographic")
                .status();

            match status {
                Ok(status) if status.success() => {}
                Ok(status) => process::exit(status.code().unwrap_or(1)),
                Err(err) => {
                    eprintln!("Failed to start `{}`: {err}", qemu.display());
                    process::exit(1);
                }
            }
        }
    }
}

/// Tries to find an x86_64 UEFI firmware file usable by QEMU.
///
/// Looks first in the `share/qemu` directory next to the QEMU binary (which is where Homebrew
/// installs it), then in common Linux distribution locations.
fn find_firmware(qemu: &Path) -> Option<PathBuf> {
    let qemu_path = if qemu.components().count() > 1 {
        Some(qemu.to_path_buf())
    } else {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(qemu))
                .find(|candidate| candidate.is_file())
        })
    };

    let mut candidates = Vec::new();
    if let Some(prefix) = qemu_path
        .and_then(|path| path.canonicalize().ok())
        .and_then(|path| path.ancestors().nth(2).map(Path::to_path_buf))
    {
        candidates.push(prefix.join("share").join("qemu").join("edk2-x86_64-code.fd"));
    }
    candidates.extend(
        [
            "/usr/share/qemu/edk2-x86_64-code.fd",
            "/usr/share/OVMF/OVMF_CODE.fd",
            "/usr/share/OVMF/OVMF_CODE_4M.fd",
            "/usr/share/edk2/x64/OVMF_CODE.fd",
            "/usr/share/edk2/ovmf/OVMF_CODE.fd",
        ]
        .into_iter()
        .map(PathBuf::from),
    );

    candidates.into_iter().find(|candidate| candidate.is_file())
}
