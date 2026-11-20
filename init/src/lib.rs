mod executor;
mod vm;

/// Configuration passed to [`run`].
pub struct Config {
    /// Secret key of the node.
    pub secret_key: [u8; 32],

    /// Messages initially delivered to the implementer of the `executor` interface.
    /// Contains zero or more messages concatenated together with no separator in-between.
    pub initial_executor_messages: &'static [u8],
}

/// Entry point of the init program. Runs the tests and then pauses forever.
pub fn run(config: Config) -> ! {
    let available_programs =
        scan_available_programs_unordered().collect::<hashbrown::HashSet<_, fnv::FnvBuildHasher>>();

    let secret_key = x25519_dalek::StaticSecret::from(config.secret_key);

    println!("Hello world");

    // SAFETY: nothing else in this program installs signal handlers.
    match unsafe { vm::install_trap_handlers() } {
        Ok(trap_handlers) => {
            for (name, wasm, expected) in [
                ("general", TEST_WASM, Ok(vec![vm::Value::I32(42)])),
                ("imports", TEST_IMPORTS_WASM, Ok(vec![vm::Value::I32(42)])),
                (
                    "unreachable",
                    TEST_UNREACHABLE_WASM,
                    Err(vm::Trap::Unreachable),
                ),
                (
                    "division by zero",
                    TEST_DIV_ZERO_WASM,
                    Err(vm::Trap::DivisionByZero),
                ),
                (
                    "division overflow",
                    TEST_DIV_OVERFLOW_WASM,
                    Err(vm::Trap::IntegerOverflow),
                ),
                (
                    "remainder of overflow",
                    TEST_REM_OVERFLOW_WASM,
                    Ok(vec![vm::Value::I32(0)]),
                ),
                (
                    "out of bounds",
                    TEST_OUT_OF_BOUNDS_WASM,
                    Err(vm::Trap::OutOfBounds),
                ),
                (
                    "stack overflow",
                    TEST_STACK_OVERFLOW_WASM,
                    Err(vm::Trap::StackOverflow),
                ),
            ] {
                let result = match run_wasm(&trap_handlers, wasm) {
                    Ok(values) => Ok(values),
                    Err(vm::Error::Trap(trap)) => Err(trap),
                    Err(err) => {
                        println!("Test {name}: FAIL: {err}");
                        continue;
                    }
                };
                if result == expected {
                    println!("Test {name}: OK ({result:?})");
                } else {
                    println!("Test {name}: FAIL: expected {expected:?}, got {result:?}");
                }
            }

            // Check that an instance remains usable after a trap.
            let reuse_result = vm::Module::new(TEST_REUSE_WASM).and_then(|module| {
                let mut instance = vm::Instance::new(&module, &trap_handlers)?;
                let first = instance.call("incr", &[]);
                let trap = instance.call("trap", &[]);
                let second = instance.call("incr", &[]);
                Ok((first, trap, second))
            });
            match reuse_result {
                Ok((
                    Ok(vm::Outcome::Return(first)),
                    Err(vm::Error::Trap(vm::Trap::Unreachable)),
                    Ok(vm::Outcome::Return(second)),
                )) if first == [vm::Value::I32(1)] && second == [vm::Value::I32(3)] => {
                    println!("Test reuse after trap: OK");
                }
                other => println!("Test reuse after trap: FAIL: {other:?}"),
            }
        }
        Err(err) => println!("Failed to install the trap handlers: {err}"),
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

/// Runs the `main` function of the given module, answering calls to the `env.add_one` import.
fn run_wasm(
    trap_handlers: &vm::TrapHandlers,
    wasm_bytes: &[u8],
) -> Result<Vec<vm::Value>, vm::Error> {
    let module = vm::Module::new(wasm_bytes)?;
    let mut instance = vm::Instance::new(&module, trap_handlers)?;
    let mut outcome = instance.call("main", &[])?;
    loop {
        match outcome {
            vm::Outcome::Return(values) => return Ok(values),
            vm::Outcome::ImportCall { import, args } => {
                let import = usize::try_from(import).unwrap_or_else(|_| unreachable!());
                let results = match (module.imports_by_index().nth(import), &args[..]) {
                    (Some(("env", "add_one")), [vm::Value::I32(v)]) => {
                        vec![vm::Value::I32(v.wrapping_add(1))]
                    }
                    _ => return Err(vm::Error::Unsupported("unknown import")),
                };
                outcome = instance.resume(&results)?;
            }
        }
    }
}

/// Test program. Its `main` function returns 42 if everything works as expected.
const TEST_WASM: &[u8] = &wat_to_wasm::wat_to_wasm!(
    r#"
(module
  (memory 1)
  (global $counter (mut i32) (i32.const 0))
  (data (i32.const 16) "\05\00\00\00")

  ;; Recursive factorial with a call.
  (func $fact (param i64) (result i64)
    (if (result i64) (i64.le_u (local.get 0) (i64.const 1))
      (then (i64.const 1))
      (else (i64.mul (local.get 0) (call $fact (i64.sub (local.get 0) (i64.const 1)))))))

  ;; Iterative sum 1..=n using a loop, br_if and a global.
  (func $sum (param i32) (result i32) (local $acc i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (local.get 0)))
        (global.set $counter (i32.add (global.get $counter) (i32.const 1)))
        (local.set $acc (i32.add (local.get $acc) (local.get 0)))
        (local.set 0 (i32.sub (local.get 0) (i32.const 1)))
        (br $l)))
    (local.get $acc))

  ;; br_table based switch.
  (func $switch (param i32) (result i32)
    (block $c (block $b (block $a
      (br_table $a $b $c (local.get 0)))
      (return (i32.const 100)))
      (return (i32.const 200)))
    (i32.const 300))

  ;; Expected: fact(10)=3628800, sum(n from memory @16 = 5) = 15, switch(1)=200, counter=5
  ;; Returns 42 if everything is correct, otherwise a non-42 code.
  (func (export "main") (result i32)
    (if (i64.ne (call $fact (i64.const 10)) (i64.const 3628800)) (then (return (i32.const 1))))
    (i32.store (i32.const 32) (call $sum (i32.load (i32.const 16))))
    (if (i32.ne (i32.load8_u (i32.const 32)) (i32.const 15)) (then (return (i32.const 2))))
    (if (i32.ne (call $switch (i32.const 1)) (i32.const 200)) (then (return (i32.const 3))))
    (if (i32.ne (call $switch (i32.const 7)) (i32.const 300)) (then (return (i32.const 4))))
    (if (i32.ne (global.get $counter) (i32.const 5)) (then (return (i32.const 5))))
    (i32.const 42))
)
"#
);

/// Calls an import three times, once from a nested function, while keeping a local alive
/// across the calls. Returns 42.
const TEST_IMPORTS_WASM: &[u8] = &wat_to_wasm::wat_to_wasm!(
    r#"
(module
  (import "env" "add_one" (func $add_one (param i32) (result i32)))
  (func $add_two (param i32) (result i32)
    (call $add_one (call $add_one (local.get 0))))
  (func (export "main") (result i32) (local $x i32)
    (local.set $x (i32.const 1000))
    (i32.sub
      (i32.add (local.get $x) (call $add_one (call $add_two (i32.const 39))))
      (i32.const 1000)))
)
"#
);

const TEST_UNREACHABLE_WASM: &[u8] = &wat_to_wasm::wat_to_wasm!(
    r#"
(module
  (func (export "main") (result i32)
    unreachable)
)
"#
);

const TEST_DIV_ZERO_WASM: &[u8] = &wat_to_wasm::wat_to_wasm!(
    r#"
(module
  (global $zero (mut i32) (i32.const 0))
  (func (export "main") (result i32)
    (i32.div_s (i32.const 1) (global.get $zero)))
)
"#
);

const TEST_DIV_OVERFLOW_WASM: &[u8] = &wat_to_wasm::wat_to_wasm!(
    r#"
(module
  (global $min (mut i32) (i32.const -2147483648))
  (global $minus_one (mut i32) (i32.const -1))
  (func (export "main") (result i32)
    (i32.div_s (global.get $min) (global.get $minus_one)))
)
"#
);

const TEST_REM_OVERFLOW_WASM: &[u8] = &wat_to_wasm::wat_to_wasm!(
    r#"
(module
  (global $min (mut i32) (i32.const -2147483648))
  (global $minus_one (mut i32) (i32.const -1))
  (func (export "main") (result i32)
    (i32.rem_s (global.get $min) (global.get $minus_one)))
)
"#
);

const TEST_OUT_OF_BOUNDS_WASM: &[u8] = &wat_to_wasm::wat_to_wasm!(
    r#"
(module
  (memory 1)
  (func (export "main") (result i32)
    (i32.load (i32.const 65534)))
)
"#
);

const TEST_STACK_OVERFLOW_WASM: &[u8] = &wat_to_wasm::wat_to_wasm!(
    r#"
(module
  (func $recurse (param i64) (result i64)
    (i64.add (call $recurse (i64.add (local.get 0) (i64.const 1))) (local.get 0)))
  (func (export "main") (result i32)
    (i32.wrap_i64 (call $recurse (i64.const 0))))
)
"#
);

/// `incr` increments a global and returns its new value, and `trap` increments it then traps.
const TEST_REUSE_WASM: &[u8] = &wat_to_wasm::wat_to_wasm!(
    r#"
(module
  (global $counter (mut i32) (i32.const 0))
  (func (export "incr") (result i32)
    (global.set $counter (i32.add (global.get $counter) (i32.const 1)))
    (global.get $counter))
  (func (export "trap") (result i32)
    (global.set $counter (i32.add (global.get $counter) (i32.const 1)))
    unreachable)
)
"#
);
