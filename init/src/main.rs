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

    test(&[
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f,
        0x03, 0x02, 0x01, 0x00, 0x0a, 0x09, 0x01, 0x07, 0x00, 0x41, 0x2a, 0x0b,
    ]);

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

fn test(wasm_bytes: &[u8]) {
    let mut func_bytecode: &[u8] = &[];
    let parser = wasmparser::Parser::new(0);
    for payload in parser.parse_all(wasm_bytes) {
        if let Ok(wasmparser::Payload::CodeSectionEntry(code)) = payload {
            func_bytecode = code.range().get(wasm_bytes).unwrap();
        }
    }

    let mut flag_builder = cranelift_codegen::settings::builder();
    flag_builder.set("opt_level", "speed").unwrap();

    let isa = cranelift_codegen::isa::lookup(target_lexicon::triple!("x86_64"))
        .unwrap()
        .finish(cranelift_codegen::settings::Flags::new(flag_builder))
        .unwrap();

    let mut env = cranelift_wasm::DummyEnvironment::new(isa.frontend_config());

    let mut clif_function = cranelift_codegen::ir::Function::new();
    cranelift_wasm::FuncTranslator::new()
        .translate(
            &wasmparser::FunctionBody::new(0, func_bytecode),
            0,
            &mut clif_function,
            &mut env,
        )
        .unwrap();

    let mut context = cranelift_codegen::Context::new();
    context.func = clif_function;

    let mut code_memory = cranelift_codegen::binemit::CodeMemory::new();
    let compiled_info = context.compile(&*isa, &mut Default::default()).unwrap();

    let mut machine_code_buffer = vec![0u8; compiled_info.code_size as usize];
    unsafe {
        context.emit_to_memory(&*isa, machine_code_buffer.as_mut_ptr(), &mut code_memory);
    }
}
