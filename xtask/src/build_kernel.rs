use std::{fs, io, path::Path};

/// Compiles the Linux kernel using Docker.
///
/// This is insane, but the Linux kernel contains multiple files with names that differ only by
/// casing, meaning that it can't be cloned and can't be built on case-sensitive file systems,
/// which MacOS and Windows use by default. The kernel people are too stubborn about this to
/// fix this.
/// While we would like to have the Linux kernel as a submodule and leave the build artifacts
/// as individual files in a cache directory, this is just not possible.
/// Instead, we just a `tar` of the kernel in the repository and untar it before building. When
/// it comes to build artifacts, we  `tar` and `untar` them at every build.
pub fn build_kernel(project_root: impl AsRef<Path>) -> Result<(), io::Error> {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async move {
            let docker = bollard::Docker::connect_with_local_defaults()
                .expect("Failed to connect to Docker daemon");

            // TODO: better image tag? don't use a tag at all?
            build_image(&docker, "kernel-builder:latest".to_string()).await?;
            actually_build_kernel(
                &docker,
                project_root.as_ref().join("linux-7.0.tar.gz"),
                project_root.as_ref().join("build-artifacts.tar.gz"),
                "kernel-builder:latest".to_string(),
            )
            .await?;
            extract_specific_file(
                project_root.as_ref().join("build-artifacts.tar.gz"),
                "arch/x86/boot/bzImage",
                project_root.as_ref().join("bzImage"),
            )?;

            Ok(())
        })
}

/// Builds the Docker image used to build the Linux kernel, and gives it the given image tag.
///
/// Assumes that we are within tokio, which we are because this is a private function not
/// called from outside this module.
async fn build_image(docker: &bollard::Docker, image_tag: String) -> Result<(), io::Error> {
    let mut header = tar::Header::new_gnu();
    header.set_size(0);
    header.set_cksum();

    let dockerfile_content = include_bytes!("../kernel-builder-Dockerfile");
    let mut header = tar::Header::new_gnu();
    header.set_size(dockerfile_content.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();

    let mut tar_builder = tar::Builder::new(Vec::new());
    tar_builder
        .append_data(
            &mut header,
            "Dockerfile",
            io::Cursor::new(dockerfile_content),
        )
        .unwrap();
    let tar_data = tar_builder.into_inner()?;

    match futures_util::TryStreamExt::try_for_each(
        docker.build_image(
            bollard::query_parameters::BuildImageOptions {
                t: Some(image_tag),
                platform: "amd64".to_string(),
                dockerfile: "Dockerfile".to_string(),
                rm: true,
                ..Default::default()
            },
            None,
            Some(bollard::body_full(tar_data.into())),
        ),
        |_info| async move { Ok(()) },
    )
    .await
    {
        Ok(()) => {}
        Err(err) => {
            // TODO: print logs
            panic!("{err}"); // TODO: don't panic?
        }
    }

    Ok(())
}

/// Builds the Docker image used to build the Linux kernel, and gives it the given image tag.
///
/// Assumes that we are within tokio, which we are because this is a private function not
/// called from outside this module.
///
/// The `kernel_tar_path` is assumed to contain a directory that contains the actual source code.
/// This is because this is what GitHub gives you when you download a `.tar` of source code.
async fn actually_build_kernel(
    docker: &bollard::Docker,
    kernel_tar_path: impl AsRef<Path>,
    build_artifacts_path: impl AsRef<Path>,
    image_tag: String,
) -> Result<(), io::Error> {
    let build_artifacts_path = build_artifacts_path.as_ref();
    let build_artifacts_dir = build_artifacts_path.parent().unwrap();
    let build_artifacts_file = build_artifacts_path.file_name().unwrap();

    // Create the container.
    let options = bollard::query_parameters::CreateContainerOptionsBuilder::default()
        .platform("amd64")
        .build();
    let config = bollard::models::ContainerCreateBody {
        image: Some(image_tag),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        cmd: Some(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!(
                r#"
                        mkdir -p /root/build && \
                        mkdir -p /root/src && \
                        echo "Decompressing Linux kernel" && \
                        tar -xzf /root/src-file.tar.gz -C /root/src --strip-components=1 && \
                        echo "Finished decompressing Linux kernel" && \
                        {{ [ -f /root/build-out/{build_artifacts_file} ] && {{ echo "Decompressing build artifacts" && tar -xzf /root/build-out/{build_artifacts_file} -C /root/build && echo "Finished decompressing build artifacts"; }} || true; }} && \
                        cd /root/src && \
                        make O=/root/build defconfig && \
                        ./scripts/config --file /root/build/.config --enable CONFIG_EFI_STUB && \
                        ./scripts/config --file /root/build/.config --enable BUILTIN_DTB && \
                        ./scripts/config --file /root/build/.config --enable CMDLINE_BOOL && \
                        ./scripts/config --file /root/build/.config --set-str CMDLINE "initrd=\initramfs.cpio.gz console=ttyS0" && \
                        make O=/root/build olddefconfig && \
                        make O=/root/build -j4 && \
                        echo "Compressing build artifacts" && \
                        tar -czf /root/artifactz.tar.gz -C /root/build --transform 's|^\./||' . && \
                        mv /root/artifactz.tar.gz /root/build-out/{build_artifacts_file}"#,
                build_artifacts_file = build_artifacts_file.to_str().unwrap(), // TODO: escape the file name, or rename it temporarily or something like that, in case it has weird characters?
            ),
        ]),
        host_config: Some(bollard::config::HostConfig {
            binds: Some(vec![
                format!(
                    "{}:/root/src-file.tar.gz",
                    kernel_tar_path.as_ref().display()
                ),
                format!("{}:/root/build-out", build_artifacts_dir.display()),
            ]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let container_id = docker
        .create_container(Some(options), config)
        .await
        .unwrap()
        .id;

    match docker.start_container(&container_id, None).await {
        Ok(()) => {}
        Err(err) => {
            let _ = docker
                .remove_container(
                    &container_id,
                    Some(bollard::query_parameters::RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await;
            panic!("{err}"); // TODO: don't panic?
        }
    }

    // Wait for the container to finish executing its command.
    match futures_util::TryStreamExt::try_for_each(
        docker.wait_container(&container_id, None),
        |_| async move { Ok(()) },
    )
    .await
    {
        Ok(()) => {}
        Err(err) => {
            // TODO: restore
            /*let _ = docker
            .remove_container(
                &container_id,
                Some(bollard::query_parameters::RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;*/
            // TODO: print logs
            panic!("{err}"); // TODO: don't panic?
        }
    }

    // Destroy the container.
    let _ = docker
        .remove_container(
            &container_id,
            Some(bollard::query_parameters::RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;

    Ok(())
}

fn extract_specific_file(
    archive_path: impl AsRef<Path>,
    target_file: &str,
    output_file: impl AsRef<Path>,
) -> Result<(), io::Error> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(io::BufReader::new(
        fs::File::open(archive_path)?,
    )));

    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.to_str() == Some(target_file) {
            entry.unpack(output_file)?;
            return Ok(());
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("File `{}` not found in the archive", target_file),
    ))
}
