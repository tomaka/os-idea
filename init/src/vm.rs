//! Minimal WebAssembly virtual machine.
//!
//! Parses a Wasm module with `wasmparser`, translates every function to Cranelift IR by hand
//! (using `cranelift-frontend`), compiles it to x86-64 machine code with `cranelift-codegen`,
//! and executes it directly.
//!
//! Supported subset:
//!
//! - No imports, no tables, no start function.
//! - Only `i32` and `i64` value types (no floats, no SIMD, no references).
//! - Control flow (`block`, `loop`, `if`/`else`, `br`, `br_if`, `br_table`, `return`,
//!   `unreachable`), direct calls, locals, globals, all integer arithmetic.
//! - At most one linear memory, with explicit bounds checks. `memory.grow` always fails.
//!
//! TODO: traps (out-of-bounds access, division by zero, `unreachable`, ...) are compiled into
//! `ud2` instructions and thus currently raise a `SIGILL` that kills the process. They should
//! instead be caught and reported as errors.

use std::{collections::BTreeMap, fmt, mem::offset_of, ptr};

use cranelift_codegen::{
    Context, FinalizedRelocTarget,
    binemit::Reloc,
    control::ControlPlane,
    ir::{
        self, AbiParam, BlockArg, BlockCall, ExtFuncData, ExternalName, InstBuilder,
        JumpTableData, MemFlagsData, Signature, TrapCode, UserExternalName, UserFuncName,
        condcodes::IntCC, types,
    },
    isa::{CallConv, OwnedTargetIsa},
    settings::{self, Configurable as _},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use wasmparser::{BlockType, FuncType, Operator, Payload, ValType};

/// Size of a Wasm memory page in bytes.
const WASM_PAGE_SIZE: u64 = 65536;

/// Namespace of the [`UserExternalName`]s that designate Wasm functions.
const NAMESPACE_WASM_FUNC: u32 = 0;
/// Namespace of the [`UserExternalName`]s that designate trampolines.
const NAMESPACE_TRAMPOLINE: u32 = 1;

/// A Wasm value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value {
    I32(i32),
    I64(i64),
}

/// Error that can happen when compiling or running a module.
#[derive(Debug)]
pub enum Error {
    /// The Wasm bytecode is malformed or invalid.
    Parse(wasmparser::BinaryReaderError),
    /// The module uses a feature that this VM doesn't support.
    Unsupported(&'static str),
    /// Cranelift failed to compile a function.
    Compile,
    /// Failed to allocate executable memory.
    Mmap,
    /// The requested export doesn't exist or isn't a function.
    NoSuchExport,
    /// The parameters passed to [`Instance::call`] don't match the function signature.
    BadArgs,
}

impl From<wasmparser::BinaryReaderError> for Error {
    fn from(err: wasmparser::BinaryReaderError) -> Self {
        Error::Parse(err)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Parse(err) => write!(f, "invalid Wasm: {err}"),
            Error::Unsupported(what) => write!(f, "unsupported Wasm feature: {what}"),
            Error::Compile => write!(f, "compilation failed"),
            Error::Mmap => write!(f, "failed to allocate executable memory"),
            Error::NoSuchExport => write!(f, "no such exported function"),
            Error::BadArgs => write!(f, "arguments don't match the function signature"),
        }
    }
}

/// Context passed as hidden first parameter to every compiled function.
#[repr(C)]
struct VmCtx {
    mem_base: *mut u8,
    mem_len: u64,
    globals: *mut u64,
}

/// Signature of a Wasm function, restricted to the supported types.
#[derive(Debug, Clone)]
struct FuncSig {
    params: Vec<ir::Type>,
    results: Vec<ir::Type>,
}

impl FuncSig {
    fn from_wasm(ty: &FuncType) -> Result<Self, Error> {
        Ok(FuncSig {
            params: ty.params().iter().map(|t| val_type(*t)).collect::<Result<_, _>>()?,
            results: ty.results().iter().map(|t| val_type(*t)).collect::<Result<_, _>>()?,
        })
    }

    /// Cranelift signature of the compiled function: `vmctx` followed by the Wasm params.
    fn to_clif(&self) -> Signature {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(types::I64));
        sig.params.extend(self.params.iter().map(|t| AbiParam::new(*t)));
        sig.returns.extend(self.results.iter().map(|t| AbiParam::new(*t)));
        sig
    }
}

fn val_type(ty: ValType) -> Result<ir::Type, Error> {
    match ty {
        ValType::I32 => Ok(types::I32),
        ValType::I64 => Ok(types::I64),
        _ => Err(Error::Unsupported("non-integer value type")),
    }
}

/// Evaluates a constant expression consisting of a single `i32.const` or `i64.const`.
fn eval_const_expr(expr: &wasmparser::ConstExpr) -> Result<Value, Error> {
    let mut reader = expr.get_operators_reader();
    let value = match reader.read()? {
        Operator::I32Const { value } => Value::I32(value),
        Operator::I64Const { value } => Value::I64(value),
        _ => return Err(Error::Unsupported("non-trivial constant expression")),
    };
    match reader.read()? {
        Operator::End => Ok(value),
        _ => Err(Error::Unsupported("non-trivial constant expression")),
    }
}

/// A compiled Wasm module.
pub struct Module {
    code: ExecMemory,
    /// Signature of each function.
    funcs: Vec<FuncSig>,
    /// Exported functions: name => (function index, offset of the trampoline in `code`).
    exports: BTreeMap<String, (u32, usize)>,
    /// Minimum number of pages of the memory, if any.
    memory_pages: Option<u64>,
    globals: Vec<Value>,
    /// Active data segments: (offset in memory, bytes).
    data: Vec<(u32, Vec<u8>)>,
}

impl Module {
    /// Parses, validates and compiles the given Wasm module.
    pub fn new(wasm: &[u8]) -> Result<Self, Error> {
        wasmparser::Validator::new().validate_all(wasm)?;

        let mut types = Vec::new();
        let mut func_types = Vec::new();
        let mut memory_pages = None;
        let mut globals = Vec::new();
        let mut exports = Vec::new();
        let mut data = Vec::new();
        let mut bodies = Vec::new();

        for payload in wasmparser::Parser::new(0).parse_all(wasm) {
            match payload? {
                Payload::TypeSection(reader) => {
                    for ty in reader.into_iter_err_on_gc_types() {
                        types.push(FuncSig::from_wasm(&ty?)?);
                    }
                }
                Payload::FunctionSection(reader) => {
                    for ty in reader {
                        func_types.push(ty?);
                    }
                }
                Payload::MemorySection(reader) => {
                    for mem in reader {
                        let mem = mem?;
                        if mem.memory64 || mem.shared || mem.page_size_log2.is_some() {
                            return Err(Error::Unsupported("non-standard memory"));
                        }
                        if memory_pages.is_some() {
                            return Err(Error::Unsupported("multiple memories"));
                        }
                        memory_pages = Some(mem.initial);
                    }
                }
                Payload::GlobalSection(reader) => {
                    for global in reader {
                        let global = global?;
                        val_type(global.ty.content_type)?;
                        globals.push(eval_const_expr(&global.init_expr)?);
                    }
                }
                Payload::ExportSection(reader) => {
                    for export in reader {
                        let export = export?;
                        if export.kind == wasmparser::ExternalKind::Func {
                            exports.push((String::from(export.name), export.index));
                        }
                    }
                }
                Payload::DataSection(reader) => {
                    for segment in reader {
                        let segment = segment?;
                        match segment.kind {
                            wasmparser::DataKind::Active {
                                memory_index: 0,
                                offset_expr,
                            } => {
                                let Value::I32(offset) = eval_const_expr(&offset_expr)? else {
                                    return Err(Error::Unsupported("data segment offset"));
                                };
                                data.push((offset as u32, segment.data.to_vec()));
                            }
                            _ => return Err(Error::Unsupported("passive data segment")),
                        }
                    }
                }
                Payload::CodeSectionEntry(body) => bodies.push(body),
                Payload::ImportSection(_) => return Err(Error::Unsupported("imports")),
                Payload::StartSection { .. } => return Err(Error::Unsupported("start function")),
                Payload::TableSection(_) | Payload::ElementSection(_) => {
                    return Err(Error::Unsupported("tables"));
                }
                Payload::TagSection(_) => return Err(Error::Unsupported("exceptions")),
                _ => {}
            }
        }

        let funcs: Vec<FuncSig> = func_types
            .iter()
            .map(|idx| types[*idx as usize].clone())
            .collect();

        let isa = build_isa()?;
        let mut linker = Linker::default();
        let mut ctx = Context::new();
        let mut builder_ctx = FunctionBuilderContext::new();

        // Compile all the Wasm functions.
        for (func_idx, body) in bodies.iter().enumerate() {
            ctx.clear();
            ctx.func.signature = funcs[func_idx].to_clif();
            ctx.func.name = UserFuncName::user(NAMESPACE_WASM_FUNC, func_idx as u32);
            let builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
            let mut translator = Translator::new(builder, &types, &funcs, &globals, func_idx);
            translator.translate(body)?;
            translator.builder.finalize(isa.frontend_config());
            linker.add(&isa, &mut ctx, NAMESPACE_WASM_FUNC, func_idx as u32)?;
        }

        // Compile one trampoline per exported function.
        for (export_idx, (_, func_idx)) in exports.iter().enumerate() {
            ctx.clear();
            build_trampoline(
                &mut ctx.func,
                &mut builder_ctx,
                &funcs[*func_idx as usize],
                *func_idx,
                isa.frontend_config(),
            );
            ctx.func.name = UserFuncName::user(NAMESPACE_TRAMPOLINE, export_idx as u32);
            linker.add(&isa, &mut ctx, NAMESPACE_TRAMPOLINE, export_idx as u32)?;
        }

        let (code, offsets) = linker.link()?;
        let exports = exports
            .into_iter()
            .enumerate()
            .map(|(export_idx, (name, func_idx))| {
                let offset = offsets[&(NAMESPACE_TRAMPOLINE, export_idx as u32)];
                (name, (func_idx, offset))
            })
            .collect();

        Ok(Module {
            code,
            funcs,
            exports,
            memory_pages,
            globals,
            data,
        })
    }
}

fn build_isa() -> Result<OwnedTargetIsa, Error> {
    let mut flags = settings::builder();
    flags.set("opt_level", "speed").map_err(|_| Error::Compile)?;
    flags.set("is_pic", "false").map_err(|_| Error::Compile)?;
    flags
        .set("enable_multi_ret_implicit_sret", "true")
        .map_err(|_| Error::Compile)?;
    cranelift_codegen::isa::lookup(target_lexicon::triple!("x86_64"))
        .map_err(|_| Error::Compile)?
        .finish(settings::Flags::new(flags))
        .map_err(|_| Error::Compile)
}

/// Builds a function `extern "sysv64" fn(vmctx: *mut VmCtx, values: *mut u64)` that reads the
/// parameters from `values`, calls the given Wasm function, then writes back the results into
/// `values`.
fn build_trampoline(
    func: &mut ir::Function,
    builder_ctx: &mut FunctionBuilderContext,
    sig: &FuncSig,
    func_idx: u32,
    frontend_config: cranelift_codegen::isa::TargetFrontendConfig,
) {
    let mut tramp_sig = Signature::new(CallConv::SystemV);
    tramp_sig.params.push(AbiParam::new(types::I64));
    tramp_sig.params.push(AbiParam::new(types::I64));
    func.signature = tramp_sig;

    let mut builder = FunctionBuilder::new(func, builder_ctx);
    let callee = import_wasm_func(&mut builder, sig, func_idx);

    let entry = builder.create_block();
    builder.append_block_params_for_function_params(entry);
    builder.switch_to_block(entry);
    let vmctx = builder.block_params(entry)[0];
    let values = builder.block_params(entry)[1];

    let mut args = vec![vmctx];
    for (i, ty) in sig.params.iter().enumerate() {
        let v = builder
            .ins()
            .load(*ty, MemFlagsData::trusted(), values, (i * 8) as i32);
        args.push(v);
    }
    let call = builder.ins().call(callee, &args);
    let results = builder.inst_results(call).to_vec();
    for (i, v) in results.into_iter().enumerate() {
        builder
            .ins()
            .store(MemFlagsData::trusted(), v, values, (i * 8) as i32);
    }
    builder.ins().return_(&[]);
    builder.seal_all_blocks();
    builder.finalize(frontend_config);
}

/// Declares, in the function being built, a reference to the given Wasm function.
fn import_wasm_func(builder: &mut FunctionBuilder, sig: &FuncSig, func_idx: u32) -> ir::FuncRef {
    let signature = builder.import_signature(sig.to_clif());
    let name = builder
        .func
        .declare_imported_user_function(UserExternalName::new(NAMESPACE_WASM_FUNC, func_idx));
    builder.import_function(ExtFuncData {
        name: ExternalName::user(name),
        signature,
        colocated: true,
        patchable: false,
    })
}

/// Accumulates compiled functions, then copies them to executable memory and resolves the
/// relocations between them.
#[derive(Default)]
struct Linker {
    code: Vec<u8>,
    /// (namespace, index) => offset in `code`.
    offsets: BTreeMap<(u32, u32), usize>,
    /// (offset in `code`, kind, target, addend)
    relocs: Vec<(usize, Reloc, (u32, u32), i64)>,
}

impl Linker {
    fn add(
        &mut self,
        isa: &OwnedTargetIsa,
        ctx: &mut Context,
        namespace: u32,
        index: u32,
    ) -> Result<(), Error> {
        let compiled = ctx
            .compile(&**isa, &mut ControlPlane::default())
            .map_err(|_| Error::Compile)?;

        while self.code.len() % 16 != 0 {
            self.code.push(0xcc); // int3
        }
        let base = self.code.len();
        self.code.extend_from_slice(compiled.code_buffer());
        self.offsets.insert((namespace, index), base);
        let relocs = compiled.buffer.relocs().to_vec();

        for reloc in relocs {
            let target = match &reloc.target {
                FinalizedRelocTarget::ExternalName(ExternalName::User(name)) => {
                    let name = &ctx.func.params.user_named_funcs()[*name];
                    (name.namespace, name.index)
                }
                _ => return Err(Error::Unsupported("relocation target")),
            };
            self.relocs
                .push((base + reloc.offset as usize, reloc.kind, target, reloc.addend));
        }

        Ok(())
    }

    fn link(self) -> Result<(ExecMemory, BTreeMap<(u32, u32), usize>), Error> {
        let mut mem = ExecMemory::new(self.code.len().max(1))?;
        let base = mem.ptr as usize;
        unsafe {
            ptr::copy_nonoverlapping(self.code.as_ptr(), mem.ptr, self.code.len());
        }

        for (offset, kind, target, addend) in self.relocs {
            let target_addr = (base + self.offsets[&target]) as i64 + addend;
            let at = unsafe { mem.ptr.add(offset) };
            match kind {
                Reloc::X86CallPCRel4 | Reloc::X86PCRel4 => {
                    let delta = target_addr - (base + offset) as i64;
                    let delta = i32::try_from(delta).map_err(|_| Error::Compile)?;
                    unsafe { ptr::write_unaligned(at as *mut i32, delta) };
                }
                Reloc::Abs8 => unsafe { ptr::write_unaligned(at as *mut i64, target_addr) },
                _ => return Err(Error::Unsupported("relocation kind")),
            }
        }

        mem.make_executable()?;
        Ok((mem, self.offsets))
    }
}

/// Memory region obtained through `mmap`, later made executable.
struct ExecMemory {
    ptr: *mut u8,
    len: usize,
}

impl ExecMemory {
    const PROT_READ: usize = 0x1;
    const PROT_WRITE: usize = 0x2;
    const PROT_EXEC: usize = 0x4;
    const MAP_PRIVATE: usize = 0x02;
    const MAP_ANONYMOUS: usize = 0x20;

    fn new(len: usize) -> Result<Self, Error> {
        let ptr = unsafe {
            syscalls::syscall!(
                syscalls::Sysno::mmap,
                0usize,
                len,
                Self::PROT_READ | Self::PROT_WRITE,
                Self::MAP_PRIVATE | Self::MAP_ANONYMOUS,
                usize::MAX, // fd = -1
                0usize
            )
        }
        .map_err(|_| Error::Mmap)?;
        Ok(ExecMemory {
            ptr: ptr as *mut u8,
            len,
        })
    }

    fn make_executable(&mut self) -> Result<(), Error> {
        unsafe {
            syscalls::syscall!(
                syscalls::Sysno::mprotect,
                self.ptr as usize,
                self.len,
                Self::PROT_READ | Self::PROT_EXEC
            )
        }
        .map_err(|_| Error::Mmap)?;
        Ok(())
    }
}

impl Drop for ExecMemory {
    fn drop(&mut self) {
        unsafe {
            let _ = syscalls::syscall!(syscalls::Sysno::munmap, self.ptr as usize, self.len);
        }
    }
}

/// An instantiated module, with its own memory and globals.
pub struct Instance<'m> {
    module: &'m Module,
    vmctx: Box<VmCtx>,
    // These fields are accessed through raw pointers in `vmctx`.
    _memory: Vec<u8>,
    _globals: Vec<u64>,
}

impl<'m> Instance<'m> {
    pub fn new(module: &'m Module) -> Result<Self, Error> {
        let mem_len = module.memory_pages.unwrap_or(0) * WASM_PAGE_SIZE;
        let mut memory = vec![0u8; usize::try_from(mem_len).map_err(|_| Error::Mmap)?];
        for (offset, bytes) in &module.data {
            // Bounds were already checked by the validator only if the memory is large enough;
            // check again to be safe.
            let start = *offset as usize;
            let dest = memory
                .get_mut(start..start + bytes.len())
                .ok_or(Error::Unsupported("out-of-bounds data segment"))?;
            dest.copy_from_slice(bytes);
        }

        let mut globals: Vec<u64> = module
            .globals
            .iter()
            .map(|g| match *g {
                Value::I32(v) => u64::from(v as u32),
                Value::I64(v) => v as u64,
            })
            .collect();

        let vmctx = Box::new(VmCtx {
            mem_base: memory.as_mut_ptr(),
            mem_len,
            globals: globals.as_mut_ptr(),
        });

        Ok(Instance {
            module,
            vmctx,
            _memory: memory,
            _globals: globals,
        })
    }

    /// Calls the exported function with the given name.
    pub fn call(&mut self, export: &str, args: &[Value]) -> Result<Vec<Value>, Error> {
        let (func_idx, offset) = *self.module.exports.get(export).ok_or(Error::NoSuchExport)?;
        let sig = &self.module.funcs[func_idx as usize];

        if args.len() != sig.params.len() {
            return Err(Error::BadArgs);
        }
        let mut values = vec![0u64; sig.params.len().max(sig.results.len()).max(1)];
        for ((arg, ty), slot) in args.iter().zip(&sig.params).zip(&mut values) {
            *slot = match (*arg, *ty) {
                (Value::I32(v), types::I32) => u64::from(v as u32),
                (Value::I64(v), types::I64) => v as u64,
                _ => return Err(Error::BadArgs),
            };
        }

        unsafe {
            let trampoline: extern "sysv64" fn(*mut VmCtx, *mut u64) =
                std::mem::transmute(self.module.code.ptr.add(offset));
            trampoline(&mut *self.vmctx, values.as_mut_ptr());
        }

        Ok(sig
            .results
            .iter()
            .zip(values)
            .map(|(ty, v)| {
                if *ty == types::I32 {
                    Value::I32(v as u32 as i32)
                } else {
                    Value::I64(v as i64)
                }
            })
            .collect())
    }
}

/// Entry in the control stack of the translator.
struct Frame {
    kind: FrameKind,
    /// Block that follows the construct. Its parameters are the results of the construct.
    dest: ir::Block,
    /// Whether `dest` is reachable (through a branch or fallthrough).
    dest_reached: bool,
    num_params: usize,
    num_results: usize,
    /// Height of the operand stack at the start of the construct, excluding its parameters.
    height: usize,
}

enum FrameKind {
    Block,
    Loop {
        header: ir::Block,
    },
    If {
        else_block: ir::Block,
        else_seen: bool,
        /// Parameters of the `if`, to restore when entering the `else` branch.
        params: Vec<ir::Value>,
    },
    /// Construct found in unreachable code. Only used for keeping track of nesting.
    Dead,
}

/// Translates the body of a single Wasm function into Cranelift IR.
struct Translator<'a> {
    builder: FunctionBuilder<'a>,
    types: &'a [FuncSig],
    funcs: &'a [FuncSig],
    globals: &'a [Value],
    func_idx: usize,
    vmctx: ir::Value,
    locals: Vec<Variable>,
    stack: Vec<ir::Value>,
    frames: Vec<Frame>,
    reachable: bool,
    func_refs: BTreeMap<u32, ir::FuncRef>,
}

impl<'a> Translator<'a> {
    fn new(
        mut builder: FunctionBuilder<'a>,
        types: &'a [FuncSig],
        funcs: &'a [FuncSig],
        globals: &'a [Value],
        func_idx: usize,
    ) -> Self {
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let vmctx = builder.block_params(entry)[0];

        Translator {
            builder,
            types,
            funcs,
            globals,
            func_idx,
            vmctx,
            locals: Vec::new(),
            stack: Vec::new(),
            frames: Vec::new(),
            reachable: true,
            func_refs: BTreeMap::new(),
        }
    }

    fn translate(&mut self, body: &wasmparser::FunctionBody) -> Result<(), Error> {
        let funcs = self.funcs;
        let sig = &funcs[self.func_idx];
        let entry = self.builder.current_block().unwrap();

        // Parameters.
        for (i, ty) in sig.params.iter().enumerate() {
            let var = self.builder.declare_var(*ty);
            let val = self.builder.block_params(entry)[i + 1];
            self.builder.def_var(var, val);
            self.locals.push(var);
        }

        // Declared locals, initialized to zero.
        for local in body.get_locals_reader()? {
            let (count, ty) = local?;
            let ty = val_type(ty)?;
            for _ in 0..count {
                let var = self.builder.declare_var(ty);
                let zero = self.builder.ins().iconst(ty, 0);
                self.builder.def_var(var, zero);
                self.locals.push(var);
            }
        }

        // Frame of the function body itself. Its destination is the return block.
        let results = sig.results.clone();
        let ret_block = self.create_block_with_params(&results);
        self.frames.push(Frame {
            kind: FrameKind::Block,
            dest: ret_block,
            dest_reached: false,
            num_params: 0,
            num_results: results.len(),
            height: 0,
        });

        let mut reader = body.get_operators_reader()?;
        while !reader.eof() {
            let op = reader.read()?;
            self.translate_op(op)?;
        }

        // The final `end` has switched to the return block.
        if self.reachable {
            let rets = self.stack.split_off(self.stack.len() - results.len());
            self.builder.ins().return_(&rets);
        }

        self.builder.seal_all_blocks();
        Ok(())
    }

    fn create_block_with_params(&mut self, tys: &[ir::Type]) -> ir::Block {
        let block = self.builder.create_block();
        for ty in tys {
            self.builder.append_block_param(block, *ty);
        }
        block
    }

    fn block_type(&self, ty: BlockType) -> Result<(Vec<ir::Type>, Vec<ir::Type>), Error> {
        Ok(match ty {
            BlockType::Empty => (Vec::new(), Vec::new()),
            BlockType::Type(t) => (Vec::new(), vec![val_type(t)?]),
            BlockType::FuncType(idx) => {
                let sig = &self.types[idx as usize];
                (sig.params.clone(), sig.results.clone())
            }
        })
    }

    fn top_n(&self, n: usize) -> Vec<BlockArg> {
        self.stack[self.stack.len() - n..]
            .iter()
            .map(|v| BlockArg::Value(*v))
            .collect()
    }

    /// Returns the block to jump to and the number of arguments when branching to the given
    /// relative depth.
    fn branch_target(&mut self, depth: u32) -> (ir::Block, usize) {
        let idx = self.frames.len() - 1 - depth as usize;
        let frame = &mut self.frames[idx];
        match frame.kind {
            FrameKind::Loop { header } => (header, frame.num_params),
            _ => {
                frame.dest_reached = true;
                (frame.dest, frame.num_results)
            }
        }
    }

    fn vmctx_load(&mut self, offset: usize) -> ir::Value {
        self.builder
            .ins()
            .load(types::I64, MemFlagsData::trusted(), self.vmctx, offset as i32)
    }

    /// Pops a Wasm address, bounds-checks it, and returns the native address to access.
    fn mem_addr(&mut self, memarg: &wasmparser::MemArg, access_size: u64) -> ir::Value {
        let addr = self.stack.pop().unwrap_or_else(|| unreachable!());
        let addr = self.builder.ins().uextend(types::I64, addr);
        let mem_len = self.vmctx_load(offset_of!(VmCtx, mem_len));
        // Can't overflow, as `addr` and `memarg.offset` both fit in 32 bits.
        let end = self
            .builder
            .ins()
            .iadd_imm_u(addr, (memarg.offset + access_size) as i64);
        let oob = self
            .builder
            .ins()
            .icmp(IntCC::UnsignedGreaterThan, end, mem_len);
        self.builder.ins().trapnz(oob, TrapCode::HEAP_OUT_OF_BOUNDS);
        let base = self.vmctx_load(offset_of!(VmCtx, mem_base));
        let ptr = self.builder.ins().iadd(base, addr);
        self.builder.ins().iadd_imm_u(ptr, memarg.offset as i64)
    }

    fn load(
        &mut self,
        memarg: &wasmparser::MemArg,
        size: u64,
        f: impl FnOnce(&mut FunctionBuilder, MemFlagsData, ir::Value) -> ir::Value,
    ) {
        let ptr = self.mem_addr(memarg, size);
        let v = f(&mut self.builder, MemFlagsData::new(), ptr);
        self.stack.push(v);
    }

    fn store(
        &mut self,
        memarg: &wasmparser::MemArg,
        size: u64,
        f: impl FnOnce(&mut FunctionBuilder, MemFlagsData, ir::Value, ir::Value),
    ) {
        let val = self.stack.pop().unwrap_or_else(|| unreachable!());
        let ptr = self.mem_addr(memarg, size);
        f(&mut self.builder, MemFlagsData::new(), val, ptr);
    }

    fn translate_op(&mut self, op: Operator) -> Result<(), Error> {
        // In unreachable code, only keep track of the nesting of control constructs.
        if !self.reachable {
            match op {
                Operator::Block { .. } | Operator::Loop { .. } | Operator::If { .. } => {
                    self.frames.push(Frame {
                        kind: FrameKind::Dead,
                        dest: self.frames.last().unwrap().dest,
                        dest_reached: false,
                        num_params: 0,
                        num_results: 0,
                        height: self.stack.len(),
                    });
                    return Ok(());
                }
                Operator::Else | Operator::End => {}
                _ => return Ok(()),
            }
        }

        let i32t = types::I32;
        let i64t = types::I64;

        match op {
            Operator::Nop => {}
            Operator::Unreachable => {
                self.builder.ins().trap(TrapCode::user(1).unwrap());
                self.reachable = false;
            }

            Operator::Block { blockty } => {
                let (params, results) = self.block_type(blockty)?;
                let dest = self.create_block_with_params(&results);
                self.frames.push(Frame {
                    kind: FrameKind::Block,
                    dest,
                    dest_reached: false,
                    num_params: params.len(),
                    num_results: results.len(),
                    height: self.stack.len() - params.len(),
                });
            }
            Operator::Loop { blockty } => {
                let (params, results) = self.block_type(blockty)?;
                let header = self.create_block_with_params(&params);
                let dest = self.create_block_with_params(&results);
                let args = self.top_n(params.len());
                self.builder.ins().jump(header, &args);
                let height = self.stack.len() - params.len();
                self.stack.truncate(height);
                self.builder.switch_to_block(header);
                self.stack
                    .extend_from_slice(self.builder.block_params(header));
                self.frames.push(Frame {
                    kind: FrameKind::Loop { header },
                    dest,
                    dest_reached: false,
                    num_params: params.len(),
                    num_results: results.len(),
                    height,
                });
            }
            Operator::If { blockty } => {
                let cond = self.stack.pop().unwrap_or_else(|| unreachable!());
                let (params, results) = self.block_type(blockty)?;
                let then_block = self.builder.create_block();
                let else_block = self.builder.create_block();
                let dest = self.create_block_with_params(&results);
                self.builder
                    .ins()
                    .brif(cond, then_block, &[], else_block, &[]);
                self.builder.switch_to_block(then_block);
                let height = self.stack.len() - params.len();
                self.frames.push(Frame {
                    kind: FrameKind::If {
                        else_block,
                        else_seen: false,
                        params: self.stack[height..].to_vec(),
                    },
                    dest,
                    dest_reached: false,
                    num_params: params.len(),
                    num_results: results.len(),
                    height,
                });
            }
            Operator::Else => {
                let reachable = self.reachable;
                let args = if reachable {
                    self.top_n(self.frames.last().unwrap().num_results)
                } else {
                    Vec::new()
                };
                let frame = self.frames.last_mut().unwrap();
                let FrameKind::If {
                    else_block,
                    else_seen,
                    params,
                } = &mut frame.kind
                else {
                    // `else` of an `if` located in unreachable code.
                    return Ok(());
                };
                if reachable {
                    self.builder.ins().jump(frame.dest, &args);
                    frame.dest_reached = true;
                }
                *else_seen = true;
                let else_block = *else_block;
                let params = params.clone();
                self.stack.truncate(frame.height);
                self.stack.extend_from_slice(&params);
                self.builder.switch_to_block(else_block);
                self.reachable = true;
            }
            Operator::End => {
                let mut frame = self.frames.pop().unwrap();
                if let FrameKind::Dead = frame.kind {
                    return Ok(());
                }
                if self.reachable {
                    let args = self.top_n(frame.num_results);
                    self.builder.ins().jump(frame.dest, &args);
                    frame.dest_reached = true;
                }
                if let FrameKind::If {
                    else_block,
                    else_seen: false,
                    params,
                } = &frame.kind
                {
                    // Implicit empty `else` branch: forward the parameters as results.
                    self.builder.switch_to_block(*else_block);
                    let args: Vec<BlockArg> = params.iter().map(|v| BlockArg::Value(*v)).collect();
                    self.builder.ins().jump(frame.dest, &args);
                    frame.dest_reached = true;
                }
                self.stack.truncate(frame.height);
                self.builder.switch_to_block(frame.dest);
                self.stack
                    .extend_from_slice(self.builder.block_params(frame.dest));
                self.reachable = frame.dest_reached;
            }
            Operator::Br { relative_depth } => {
                let (target, n) = self.branch_target(relative_depth);
                let args = self.top_n(n);
                self.builder.ins().jump(target, &args);
                self.reachable = false;
            }
            Operator::BrIf { relative_depth } => {
                let cond = self.stack.pop().unwrap_or_else(|| unreachable!());
                let (target, n) = self.branch_target(relative_depth);
                let args = self.top_n(n);
                let next = self.builder.create_block();
                self.builder.ins().brif(cond, target, &args, next, &[]);
                self.builder.switch_to_block(next);
            }
            Operator::BrTable { targets } => {
                let index = self.stack.pop().unwrap_or_else(|| unreachable!());
                let mut depths = Vec::new();
                for t in targets.targets() {
                    depths.push(t?);
                }
                let (default, n) = self.branch_target(targets.default());
                let args = self.top_n(n);
                let pool = &mut self.builder.func.dfg.value_lists;
                let default_call = BlockCall::new(default, args.iter().copied(), pool);
                let mut table = Vec::with_capacity(depths.len());
                for depth in depths {
                    let (target, _) = self.branch_target(depth);
                    let pool = &mut self.builder.func.dfg.value_lists;
                    table.push(BlockCall::new(target, args.iter().copied(), pool));
                }
                let jt = self
                    .builder
                    .create_jump_table(JumpTableData::new(default_call, &table));
                self.builder.ins().br_table(index, jt);
                self.reachable = false;
            }
            Operator::Return => {
                let n = self.funcs[self.func_idx].results.len();
                let rets = self.stack[self.stack.len() - n..].to_vec();
                self.builder.ins().return_(&rets);
                self.reachable = false;
            }
            Operator::Call { function_index } => {
                let funcs = self.funcs;
                let sig = &funcs[function_index as usize];
                let (num_params, num_results) = (sig.params.len(), sig.results.len());
                let func_ref = match self.func_refs.get(&function_index) {
                    Some(r) => *r,
                    None => {
                        let r = import_wasm_func(&mut self.builder, sig, function_index);
                        self.func_refs.insert(function_index, r);
                        r
                    }
                };
                let mut args = vec![self.vmctx];
                args.extend(self.stack.drain(self.stack.len() - num_params..));
                let call = self.builder.ins().call(func_ref, &args);
                let results = self.builder.inst_results(call);
                debug_assert_eq!(results.len(), num_results);
                self.stack.extend_from_slice(results);
            }

            Operator::Drop => {
                self.stack.pop().unwrap_or_else(|| unreachable!());
            }
            Operator::Select | Operator::TypedSelect { .. } => {
                let cond = self.stack.pop().unwrap_or_else(|| unreachable!());
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().select(cond, a, b);
                self.stack.push(v);
            }

            Operator::LocalGet { local_index } => {
                let v = self.builder.use_var(self.locals[local_index as usize]);
                self.stack.push(v);
            }
            Operator::LocalSet { local_index } => {
                let v = self.stack.pop().unwrap_or_else(|| unreachable!());
                self.builder.def_var(self.locals[local_index as usize], v);
            }
            Operator::LocalTee { local_index } => {
                let v = *self.stack.last().unwrap();
                self.builder.def_var(self.locals[local_index as usize], v);
            }
            Operator::GlobalGet { global_index } => {
                let ty = match self.globals[global_index as usize] {
                    Value::I32(_) => i32t,
                    Value::I64(_) => i64t,
                };
                let globals = self.vmctx_load(offset_of!(VmCtx, globals));
                let v = self.builder.ins().load(
                    ty,
                    MemFlagsData::trusted(),
                    globals,
                    (global_index * 8) as i32,
                );
                self.stack.push(v);
            }
            Operator::GlobalSet { global_index } => {
                let v = self.stack.pop().unwrap_or_else(|| unreachable!());
                let globals = self.vmctx_load(offset_of!(VmCtx, globals));
                self.builder.ins().store(
                    MemFlagsData::trusted(),
                    v,
                    globals,
                    (global_index * 8) as i32,
                );
            }

            Operator::I32Load { memarg } => {
                self.load(&memarg, 4, |b, f, p| b.ins().load(i32t, f, p, 0))
            }
            Operator::I64Load { memarg } => {
                self.load(&memarg, 8, |b, f, p| b.ins().load(i64t, f, p, 0))
            }
            Operator::I32Load8S { memarg } => {
                self.load(&memarg, 1, |b, f, p| b.ins().sload8(i32t, f, p, 0))
            }
            Operator::I32Load8U { memarg } => {
                self.load(&memarg, 1, |b, f, p| b.ins().uload8(i32t, f, p, 0))
            }
            Operator::I32Load16S { memarg } => {
                self.load(&memarg, 2, |b, f, p| b.ins().sload16(i32t, f, p, 0))
            }
            Operator::I32Load16U { memarg } => {
                self.load(&memarg, 2, |b, f, p| b.ins().uload16(i32t, f, p, 0))
            }
            Operator::I64Load8S { memarg } => {
                self.load(&memarg, 1, |b, f, p| b.ins().sload8(i64t, f, p, 0))
            }
            Operator::I64Load8U { memarg } => {
                self.load(&memarg, 1, |b, f, p| b.ins().uload8(i64t, f, p, 0))
            }
            Operator::I64Load16S { memarg } => {
                self.load(&memarg, 2, |b, f, p| b.ins().sload16(i64t, f, p, 0))
            }
            Operator::I64Load16U { memarg } => {
                self.load(&memarg, 2, |b, f, p| b.ins().uload16(i64t, f, p, 0))
            }
            Operator::I64Load32S { memarg } => {
                self.load(&memarg, 4, |b, f, p| b.ins().sload32(f, p, 0))
            }
            Operator::I64Load32U { memarg } => {
                self.load(&memarg, 4, |b, f, p| b.ins().uload32(f, p, 0))
            }
            Operator::I32Store { memarg } => self.store(&memarg, 4, |b, f, v, p| {
                b.ins().store(f, v, p, 0);
            }),
            Operator::I64Store { memarg } => self.store(&memarg, 8, |b, f, v, p| {
                b.ins().store(f, v, p, 0);
            }),
            Operator::I32Store8 { memarg } | Operator::I64Store8 { memarg } => {
                self.store(&memarg, 1, |b, f, v, p| {
                    b.ins().istore8(f, v, p, 0);
                })
            }
            Operator::I32Store16 { memarg } | Operator::I64Store16 { memarg } => {
                self.store(&memarg, 2, |b, f, v, p| {
                    b.ins().istore16(f, v, p, 0);
                })
            }
            Operator::I64Store32 { memarg } => self.store(&memarg, 4, |b, f, v, p| {
                b.ins().istore32(f, v, p, 0);
            }),
            Operator::MemorySize { .. } => {
                let len = self.vmctx_load(offset_of!(VmCtx, mem_len));
                let pages = self.builder.ins().ushr_imm_u(len, 16);
                let v = self.builder.ins().ireduce(i32t, pages);
                self.stack.push(v);
            }
            Operator::MemoryGrow { .. } => {
                // TODO: growing memory isn't supported; always report a failure
                self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().iconst(i32t, -1);
                self.stack.push(v);
            }

            Operator::I32Const { value } => {
                let v = self.builder.ins().iconst(i32t, i64::from(value));
                self.stack.push(v);
            }
            Operator::I64Const { value } => {
                let v = self.builder.ins().iconst(i64t, value);
                self.stack.push(v);
            }

            Operator::I32Eqz | Operator::I64Eqz => {
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let c = self.builder.ins().icmp_imm_u(IntCC::Equal, a, 0);
                let v = self.builder.ins().uextend(types::I32, c);
                self.stack.push(v);
            }
            Operator::I32Eq | Operator::I64Eq => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let c = self.builder.ins().icmp(IntCC::Equal, a, b);
                let v = self.builder.ins().uextend(types::I32, c);
                self.stack.push(v);
            }
            Operator::I32Ne | Operator::I64Ne => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let c = self.builder.ins().icmp(IntCC::NotEqual, a, b);
                let v = self.builder.ins().uextend(types::I32, c);
                self.stack.push(v);
            }
            Operator::I32LtS | Operator::I64LtS => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let c = self.builder.ins().icmp(IntCC::SignedLessThan, a, b);
                let v = self.builder.ins().uextend(types::I32, c);
                self.stack.push(v);
            }
            Operator::I32LtU | Operator::I64LtU => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let c = self.builder.ins().icmp(IntCC::UnsignedLessThan, a, b);
                let v = self.builder.ins().uextend(types::I32, c);
                self.stack.push(v);
            }
            Operator::I32GtS | Operator::I64GtS => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let c = self.builder.ins().icmp(IntCC::SignedGreaterThan, a, b);
                let v = self.builder.ins().uextend(types::I32, c);
                self.stack.push(v);
            }
            Operator::I32GtU | Operator::I64GtU => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let c = self.builder.ins().icmp(IntCC::UnsignedGreaterThan, a, b);
                let v = self.builder.ins().uextend(types::I32, c);
                self.stack.push(v);
            }
            Operator::I32LeS | Operator::I64LeS => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let c = self.builder.ins().icmp(IntCC::SignedLessThanOrEqual, a, b);
                let v = self.builder.ins().uextend(types::I32, c);
                self.stack.push(v);
            }
            Operator::I32LeU | Operator::I64LeU => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let c = self.builder.ins().icmp(IntCC::UnsignedLessThanOrEqual, a, b);
                let v = self.builder.ins().uextend(types::I32, c);
                self.stack.push(v);
            }
            Operator::I32GeS | Operator::I64GeS => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let c = self.builder.ins().icmp(IntCC::SignedGreaterThanOrEqual, a, b);
                let v = self.builder.ins().uextend(types::I32, c);
                self.stack.push(v);
            }
            Operator::I32GeU | Operator::I64GeU => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let c = self.builder.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, a, b);
                let v = self.builder.ins().uextend(types::I32, c);
                self.stack.push(v);
            }

            Operator::I32Clz | Operator::I64Clz => {
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().clz(a);
                self.stack.push(v);
            }
            Operator::I32Ctz | Operator::I64Ctz => {
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().ctz(a);
                self.stack.push(v);
            }
            Operator::I32Popcnt | Operator::I64Popcnt => {
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().popcnt(a);
                self.stack.push(v);
            }
            Operator::I32Add | Operator::I64Add => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().iadd(a, b);
                self.stack.push(v);
            }
            Operator::I32Sub | Operator::I64Sub => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().isub(a, b);
                self.stack.push(v);
            }
            Operator::I32Mul | Operator::I64Mul => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().imul(a, b);
                self.stack.push(v);
            }
            // Cranelift's division and remainder trap on division by zero and on overflow,
            // and `srem(INT_MIN, -1)` returns 0, which matches Wasm semantics.
            Operator::I32DivS | Operator::I64DivS => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().sdiv(a, b);
                self.stack.push(v);
            }
            Operator::I32DivU | Operator::I64DivU => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().udiv(a, b);
                self.stack.push(v);
            }
            Operator::I32RemS | Operator::I64RemS => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().srem(a, b);
                self.stack.push(v);
            }
            Operator::I32RemU | Operator::I64RemU => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().urem(a, b);
                self.stack.push(v);
            }
            Operator::I32And | Operator::I64And => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().band(a, b);
                self.stack.push(v);
            }
            Operator::I32Or | Operator::I64Or => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().bor(a, b);
                self.stack.push(v);
            }
            Operator::I32Xor | Operator::I64Xor => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().bxor(a, b);
                self.stack.push(v);
            }
            // Cranelift masks the shift amount, like Wasm does.
            Operator::I32Shl | Operator::I64Shl => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().ishl(a, b);
                self.stack.push(v);
            }
            Operator::I32ShrS | Operator::I64ShrS => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().sshr(a, b);
                self.stack.push(v);
            }
            Operator::I32ShrU | Operator::I64ShrU => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().ushr(a, b);
                self.stack.push(v);
            }
            Operator::I32Rotl | Operator::I64Rotl => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().rotl(a, b);
                self.stack.push(v);
            }
            Operator::I32Rotr | Operator::I64Rotr => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().rotr(a, b);
                self.stack.push(v);
            }

            Operator::I32WrapI64 => {
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().ireduce(i32t, a);
                self.stack.push(v);
            }
            Operator::I64ExtendI32S => {
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().sextend(i64t, a);
                self.stack.push(v);
            }
            Operator::I64ExtendI32U => {
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let v = self.builder.ins().uextend(i64t, a);
                self.stack.push(v);
            }
            Operator::I32Extend8S => {
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.builder.ins().ireduce(types::I8, a);
                let v = self.builder.ins().sextend(i32t, a);
                self.stack.push(v);
            }
            Operator::I32Extend16S => {
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.builder.ins().ireduce(types::I16, a);
                let v = self.builder.ins().sextend(i32t, a);
                self.stack.push(v);
            }
            Operator::I64Extend8S => {
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.builder.ins().ireduce(types::I8, a);
                let v = self.builder.ins().sextend(i64t, a);
                self.stack.push(v);
            }
            Operator::I64Extend16S => {
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.builder.ins().ireduce(types::I16, a);
                let v = self.builder.ins().sextend(i64t, a);
                self.stack.push(v);
            }
            Operator::I64Extend32S => {
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.builder.ins().ireduce(i32t, a);
                let v = self.builder.ins().sextend(i64t, a);
                self.stack.push(v);
            }

            _ => return Err(Error::Unsupported("operator")),
        }

        Ok(())
    }
}
