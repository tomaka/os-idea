#![no_std]
#![no_main]

use core::ffi;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // TODO: do something
    loop {}
}

#[unsafe(no_mangle)]
extern "C" fn main(_argc: ffi::c_int, _argv: *const *const ffi::c_char) -> ffi::c_int {
    unsafe {
        syscalls::syscall!(syscalls::Sysno::clone); // TODO: dummy
    }

    // TODO: do something
    0
}
