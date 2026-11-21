use std::path::Path;

mod build;
mod build_init;
mod build_kernel;
mod build_wasm_programs;
mod disk_image;

fn main() {
    let project_root = Path::new(&env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .unwrap()
        .to_path_buf();

    let build::BuildOutput {
        bzimage,
        init,
        wasm_programs,
    } = build::build(&project_root);

    disk_image::build_disk_image(bzimage, init, wasm_programs.into_iter(), "image.img").unwrap();
}
