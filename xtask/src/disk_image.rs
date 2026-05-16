use std::{fs, io, path::Path};

pub fn build_disk_image(
    kernel_file_path: impl AsRef<Path>,
    init_program_path: impl AsRef<Path>,
    disk_image_path: impl AsRef<Path>,
) -> Result<(), io::Error> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(disk_image_path)?;
    file.set_len(512 * 1024 * 1024)?;
    file.sync_data()?;

    let (mut file, start_sector, sectors_length) = {
        gpt::mbr::ProtectiveMBR::with_lb_size(
            u32::try_from((512 * 1024 * 1024) / 512).unwrap() - 1,
        )
        .overwrite_lba0(&mut file)
        .unwrap(); // TODO: don't panic; convert error

        let mut disk = gpt::GptConfig::new()
            .writable(true)
            .create_from_device(file, None)
            .unwrap(); // TODO: don't panic; convert error

        let (start_sector, sectors_length) = disk
            .find_free_sectors()
            .first()
            .copied()
            .ok_or_else(|| io::Error::other("No free sectors available"))?;

        // TODO: not sure whether necessary
        let sectors_length = ((sectors_length * 512) / 4096) * 4096 / 512;

        disk.add_partition(
            "boot",
            sectors_length * 512,
            gpt::partition_types::EFI,
            0,
            None,
        )
        .unwrap(); // TODO: don't panic; convert error
        let file = disk.write().unwrap(); // TODO: don't panic; convert error

        (file, start_sector, sectors_length)
    };

    {
        let mut partition_slice = fscommon::StreamSlice::new(
            &mut file,
            start_sector * 512,
            (start_sector + sectors_length) * 512,
        )?;

        fatfs::format_volume(
            &mut partition_slice,
            fatfs::FormatVolumeOptions::new()
                .fat_type(fatfs::FatType::Fat32)
                .bytes_per_cluster(4096)
                .bytes_per_sector(512)
                .total_sectors(u32::try_from(sectors_length).unwrap())
                .volume_label(*b"BOOT       "),
        )?;

        {
            let fs = fatfs::FileSystem::new(&mut partition_slice, fatfs::FsOptions::new())?;

            {
                let root_dir = fs.root_dir();
                let efi_dir = root_dir.create_dir("EFI")?;
                let boot_dir = efi_dir.create_dir("BOOT")?;
                let mut kernel_writer = boot_dir.create_file("BOOTX64.EFI")?;
                io::copy(&mut fs::File::open(kernel_file_path)?, &mut kernel_writer)?;

                let mut initramfs_writer = root_dir.create_file("initramfs.cpio.gz")?;
                let init_program = fs::read(init_program_path)?;
                io::Write::write_all(&mut initramfs_writer, &initramfs(&init_program))?;
            }

            fs.unmount()?;
        }

        io::Write::flush(&mut partition_slice)?;
    }

    io::Write::flush(&mut file)?;
    file.sync_data()?;
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
