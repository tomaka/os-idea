use std::path::Path;

mod build_init;
mod build_kernel;
mod disk_image;

fn main() {
    let project_root = Path::new(&env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .unwrap()
        .to_path_buf();

    let bzimage = build_kernel::build_kernel(&project_root).unwrap();

    let init = build_init::build_init();

    disk_image::build_disk_image(bzimage, init, "image.img").unwrap();
}
