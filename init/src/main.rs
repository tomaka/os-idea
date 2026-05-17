#![no_std]
#![no_main]

use core::ffi;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // TODO: do something
    loop {}
}

#[unsafe(no_mangle)]
extern "C" fn _start(_argc: ffi::c_int, _argv: *const *const ffi::c_char) -> ffi::c_int {
    stdout("Hello world\n");

    // Pause forever.
    loop {
        unsafe {
            let _ = syscalls::syscall!(syscalls::Sysno::pause);
        }
    }
}

fn stdout(msg: &str) {
    unsafe {
        let _ = syscalls::syscall!(
            syscalls::Sysno::write,
            1, // stdout
            msg.as_ptr(),
            msg.len()
        );
    }
}
