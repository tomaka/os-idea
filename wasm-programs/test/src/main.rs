// Copyright 2026  Pierre Krieger

#![no_std]
#![no_main]

#[global_allocator]
static ALLOCATOR: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

interface_bindings::interface_bindings!("../../interfaces/test.cddl");

#[unsafe(no_mangle)]
pub extern "C" fn main() {
    test::emit_hello_request(&test::HelloRequest("hello world"));
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
