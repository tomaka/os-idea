extern crate alloc;

mod alloc_mmap;
mod alloc_stack_guard;
mod executor;
mod low_level_vm;

/// Configuration passed to [`run`].
pub struct Config {
    /// Secret key of the program.
    pub secret_key: [u8; 32],

    /// Messages initially delivered to the implementer of the `executor` interface.
    /// Contains zero or more messages concatenated together with no separator in-between.
    pub initial_executor_messages: &'static [u8],
}

/// Entry point of the init program. Runs the available programs and then pauses forever.
pub fn run(config: Config) -> ! {
    // The programs are stored on the boot partition, which is mounted read-only at `/boot`.
    // In order to find it, mount `sysfs` and `devtmpfs`, then look for the block device whose
    // GPT partition name is `boot`.
    for directory in ["/sys", "/boot"] {
        match std::fs::create_dir(directory) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => println!("Failed to create {directory}: {err}"),
        }
    }
    // SAFETY: all the parameters are valid NUL-terminated strings, and mounting these file
    // systems doesn't affect the memory of this program.
    if let Err(err) = unsafe {
        syscalls::syscall!(
            syscalls::Sysno::mount,
            c"sysfs".as_ptr(),
            c"/sys".as_ptr(),
            c"sysfs".as_ptr(),
            0usize,
            0usize
        )
    } {
        println!("Failed to mount /sys: {err}");
    }
    // SAFETY: see above.
    if let Err(err) = unsafe {
        syscalls::syscall!(
            syscalls::Sysno::mount,
            c"devtmpfs".as_ptr(),
            c"/dev".as_ptr(),
            c"devtmpfs".as_ptr(),
            0usize,
            0usize
        )
    } {
        println!("Failed to mount /dev: {err}");
    }
    let boot_device = std::fs::read_dir("/sys/class/block")
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let uevent = std::fs::read_to_string(entry.ok()?.path().join("uevent")).ok()?;
            if !uevent.lines().any(|line| line == "PARTNAME=boot") {
                return None;
            }
            let device_name = uevent
                .lines()
                .find_map(|line| line.strip_prefix("DEVNAME="))?;
            alloc::ffi::CString::new(format!("/dev/{device_name}")).ok()
        })
        .next();
    match boot_device {
        Some(boot_device) => {
            // SAFETY: all the parameters are valid NUL-terminated strings, and mounting a file
            // system doesn't affect the memory of this program.
            if let Err(err) = unsafe {
                syscalls::syscall!(
                    syscalls::Sysno::mount,
                    boot_device.as_ptr(),
                    c"/boot".as_ptr(),
                    c"vfat".as_ptr(), // TODO: don't hardcode?
                    1usize,           // MS_RDONLY
                    0usize
                )
            } {
                println!("Failed to mount {boot_device:?} at /boot: {err}");
            }
        }
        None => println!("Boot partition not found"),
    }

    let available_programs = scan_available_programs_unordered(std::path::Path::new("/boot"))
        .collect::<hashbrown::HashSet<_, fnv::FnvBuildHasher>>();

    let secret_key = x25519_dalek::StaticSecret::from(config.secret_key);

    println!("Hello world");

    // SAFETY: nothing else in this program installs signal handlers.
    let trap_handlers = match unsafe { low_level_vm::install_trap_handlers() } {
        Ok(trap_handlers) => trap_handlers,
        Err(err) => {
            println!("Failed to install the trap handlers: {err}");
            // As PID 1, exiting would make the kernel panic. Pause forever instead.
            loop {
                std::thread::park();
            }
        }
    };

    let executor = executor::ExecutorBuilder::new(trap_handlers).build();

    // Instantiate each available program.
    let mut num_instances = 0usize;
    for program in &available_programs {
        let mut path = String::from("/boot/");
        for byte in program {
            path.push_str(&format!("{byte:02x}"));
        }
        path.push_str(".wasm");

        let wasm = match std::fs::read(&path) {
            Ok(wasm) => wasm,
            Err(err) => {
                println!("Program {path}: failed to read: {err}");
                continue;
            }
        };

        let module = match executor::ModuleBuilder::new(&wasm, alloc_mmap::Mmap::read_write_exec())
            .build()
        {
            Ok(module) => module,
            Err(err) => {
                println!("Program {path}: failed to compile: {err}");
                continue;
            }
        };

        // TODO: the private key should be generated when the program is launched for
        // the first time, and then persisted
        let mut private_key = [0u8; 32];
        // SAFETY: `private_key` is a valid writable buffer of the given length.
        if let Err(err) = unsafe {
            syscalls::syscall!(
                syscalls::Sysno::getrandom,
                private_key.as_mut_ptr(),
                private_key.len(),
                0
            )
        } {
            println!("Program {path}: failed to generate private key: {err}");
            continue;
        }
        let public_key =
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(private_key));

        let instance = executor::InstanceBuilder::new(
            &module,
            public_key.to_bytes(),
            alloc_mmap::Mmap::read_write(),
            alloc_mmap::Mmap::read_write(),
        );
        match instance.build(&executor) {
            Ok(()) => num_instances += 1,
            Err(err) => println!("Program {path}: failed to instantiate: {err}"),
        }
    }

    // Run each instance once.
    // TODO: instances are never pushed back to the queue at the moment, so calling `run` more
    // often than that would do nothing
    for _ in 0..num_instances {
        executor.run();
    }

    // As PID 1, exiting would make the kernel panic. Pause forever instead.
    loop {
        std::thread::park();
    }
}

/// Scans the given directory for files named `<hash>.wasm`, where `<hash>` is the lowercase
/// hexadecimal representation of 32 bytes, and yields these 32 bytes in no particular order.
///
/// Files whose name doesn't match this format are ignored. Errors while reading the directory or
/// its entries are silently ignored as well.
pub fn scan_available_programs_unordered(
    directory: &std::path::Path,
) -> impl Iterator<Item = [u8; 32]> {
    std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let file_name = entry.file_name();
            let hex = file_name.as_encoded_bytes().strip_suffix(b".wasm")?;
            if hex.len() != 64 {
                return None;
            }

            let mut hash = [0u8; 32];
            for (byte, digits) in hash.iter_mut().zip(hex.chunks_exact(2)) {
                for digit in digits {
                    let nibble = match digit {
                        b'0'..=b'9' => digit - b'0',
                        b'a'..=b'f' => digit - b'a' + 10,
                        _ => return None,
                    };
                    *byte = (*byte << 4) | nibble;
                }
            }

            // Checked last in order to avoid a system call for unrelated directory entries.
            // Note that this follows symbolic links.
            if !entry.path().is_file() {
                return None;
            }

            Some(hash)
        })
}
