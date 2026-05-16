use std::{fs, io, path::Path};

pub fn build_disk_image(
    kernel_file_path: impl AsRef<Path>,
    init_program_path: impl AsRef<Path>,
    disk_image_path: impl AsRef<Path>,
) -> Result<(), io::Error> {
    let mut file = fs::File::create(&disk_image_path)?;
    file.set_len(512 * 1024 * 1024)?;
    io::Write::flush(&mut file)?;
    drop(file);

    let mut buf_stream = fscommon::BufStream::new(
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(disk_image_path)?,
    );

    fatfs::format_volume(
        &mut buf_stream,
        // TODO: volume label
        fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat32),
    )?;

    let fs = fatfs::FileSystem::new(buf_stream, fatfs::FsOptions::new())?;

    let root_dir = fs.root_dir();

    let mut kernel_writer = root_dir
        .create_dir("EFI")?
        .create_dir("BOOT")?
        .create_file("BOOTX64.EFI")?;
    io::copy(&mut fs::File::open(kernel_file_path)?, &mut kernel_writer)?;

    let mut initramfs_writer = root_dir.create_file("initramfs.cpio.gz")?;
    let init_program = fs::read(init_program_path)?;
    io::Write::write_all(&mut initramfs_writer, &initramfs(&init_program))?;

    Ok(())
}

/// Returns the `initramfs.cpio.gz`.
///
/// It contains `/dev/console`, `/dev/null`, and `init`.
fn initramfs(init_program: &[u8]) -> Vec<u8> {
    let mut final_output = Vec::new();

    let mut zip_encoder =
        flate2::write::GzEncoder::new(&mut final_output, flate2::Compression::best());

    cpio::NewcBuilder::new("dev")
        .ino(1)
        .mode(0o040755) // drwxr-xr-x
        .set_mode_file_type(cpio::newc::ModeFileType::Directory)
        .write(&mut zip_encoder, 0)
        .finish()
        .unwrap();

    cpio::NewcBuilder::new("dev/console")
        .ino(2)
        .mode(0o020622)
        .rdev_major(5)
        .rdev_minor(1)
        .set_mode_file_type(cpio::newc::ModeFileType::Char)
        .write(&mut zip_encoder, 0)
        .finish()
        .unwrap();

    cpio::NewcBuilder::new("dev/null")
        .ino(3)
        .mode(0o020666)
        .rdev_major(1)
        .rdev_minor(3)
        .set_mode_file_type(cpio::newc::ModeFileType::Char)
        .write(&mut zip_encoder, 0)
        .finish()
        .unwrap();

    let mut writer = cpio::NewcBuilder::new("init")
        .ino(4)
        .mode(0o100644)
        .write(&mut zip_encoder, u32::try_from(init_program.len()).unwrap());
    io::Write::write_all(&mut writer, init_program).unwrap();
    writer.finish().unwrap();

    zip_encoder.finish().unwrap();
    final_output
}
