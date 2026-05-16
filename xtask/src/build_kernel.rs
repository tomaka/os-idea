use std::{io, path::Path};

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

            // Create the container.
            let options = bollard::query_parameters::CreateContainerOptionsBuilder::default()
                .platform("amd64")
                .build();
            let config = bollard::models::ContainerCreateBody {
                image: Some("kernel-builder:latest".to_string()),
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
                        echo "Done decompressing Linux kernel" && \
                        {{ [ -f filename.tar.gz ] && tar -xzf /root/build-out/build-artifacts.tar.gz -C /root/build || true; }} && \
                        cd /root/src && \
                        make O=/root/build defconfig && \
                        ./scripts/config --file /root/build/.config --enable CONFIG_EFI_STUB && \
                        make O=/root/build olddefconfig && \
                        make O=/root/build -j4 && \
                        tar -czvf /root/build-out/build-artifacts.tar.gz /root/build"#
                    ),
                ]),
                host_config: Some(bollard::config::HostConfig {
                    binds: Some(vec![
                        format!(
                            "{}:/root/src-file.tar.gz",
                            project_root.as_ref().join("linux-7.0.tar.gz").display()
                        ),
                        format!(
                            "{}:/root/build-out",
                            project_root.as_ref().display() // TODO: put somewhere else, like in `target` or whatnot
                        ),
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
                    panic!("{err}");
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
                    panic!("{err}");
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
            panic!("{err}");
        }
    }

    Ok(())
}
