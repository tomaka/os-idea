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
    let available_programs =
        scan_available_programs_unordered().collect::<hashbrown::HashSet<_, fnv::FnvBuildHasher>>();

    let secret_key = x25519_dalek::StaticSecret::from(config.secret_key);

    println!("Hello world");

    // SAFETY: nothing else in this program installs signal handlers.
    let Ok(trap_handlers) = (unsafe { low_level_vm::install_trap_handlers() }) else {
        println!("Failed to install the trap handlers: {err}");
        // As PID 1, exiting would make the kernel panic. Pause forever instead.
        loop {
            std::thread::park();
        }
    };

    // Execute each available program one after the other.
    for program in &available_programs {
        let mut path = String::from("/");
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

        let module = match executor::ModuleBuilder::<()>::new(&wasm) {
            Ok(builder) => builder.build(alloc_mmap::Mmap::read_write_exec()),
            Err(err) => {
                println!("Program {path}: {err}");
                continue;
            }
        };
        let module = match module {
            Ok(module) => alloc::sync::Arc::new(module),
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

        let instance = executor::InstanceBuilder::new(
            module,
            &private_key,
            (),
            alloc_mmap::Mmap::read_write(),
            alloc_mmap::Mmap::read_write(),
            &trap_handlers,
        )
        .build();
        let instance = match instance {
            Ok(instance) => instance,
            Err(err) => {
                println!("Program {path}: failed to instantiate: {err}");
                continue;
            }
        };

        match instance.start() {
            Ok(values) => println!("Program {path}: returned {values:?}"),
            Err(err) => println!("Program {path}: failed: {err}"),
        }
    }

    // As PID 1, exiting would make the kernel panic. Pause forever instead.
    loop {
        std::thread::park();
    }
}

/// Scans the root directory `/` for files named `<hash>.wasm`, where `<hash>` is the lowercase
/// hexadecimal representation of 32 bytes, and yields these 32 bytes in no particular order.
///
/// Files whose name doesn't match this format are ignored. Errors while reading the directory or
/// its entries are silently ignored as well.
pub fn scan_available_programs_unordered() -> impl Iterator<Item = [u8; 32]> {
    std::fs::read_dir("/")
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
