//! Minimal WebAssembly virtual machine.
//!
//! Parses a Wasm module with `wasmparser`, translates every function to Cranelift IR by hand
//! (using `cranelift-frontend`), compiles it to x86-64 machine code with `cranelift-codegen`,
//! and executes it directly.
//!
//! Supported subset:
//!
//! - Function imports only, each with exactly one parameter and one result. No tables, no
//!   start function.
//! - Only `i32` and `i64` value types (no floats, no SIMD, no references).
//! - Control flow (`block`, `loop`, `if`/`else`, `br`, `br_if`, `br_table`, `return`,
//!   `unreachable`), direct calls, locals, globals, all integer arithmetic.
//! - At most one linear memory, with explicit bounds checks.
//!
//! # Execution model
//!
//! Every [`Instance`] owns a native stack, separate from the stack of the host, on which the
//! Wasm code runs. Switching between the host stack and the Wasm stack is done by a small
//! assembly routine (`init_vm_switch`) that saves the callee-saved registers and the stack
//! pointer of one side and restores the ones of the other side.
//!
//! Execution of the Wasm code switches back to the host stack when:
//!
//! - The called function returns.
//! - The Wasm code calls an imported function, and the callback registered with
//!   [`Builder::register_import`] returns [`Outcome::Park`]. The host gets a
//!   [`RunOutcome::ImportCall`] and can later continue the execution with [`Instance::resume`].
//! - The Wasm code traps. Cranelift compiles traps into `ud2` instructions or relies on the
//!   CPU raising an exception. The resulting `SIGILL`, `SIGFPE` or `SIGSEGV` is caught by a
//!   signal handler (see [`install_trap_handlers`]) that redirects the execution to a routine
//!   that abandons the Wasm stack and jumps back to the host stack.
//!
//! The callbacks registered with [`Builder::register_import`] are called directly by the Wasm
//! code (through `init_vm_call_import`), on the Wasm stack and without any stack switch. They
//! receive a mutable reference to the context passed to [`Instance::new`]. If a callback returns
//! [`Outcome::Resume`], the Wasm code continues immediately.
//!
//! The memory of the compiled code, of the native stacks, and of the linear memories is
//! obtained from implementations of [`Allocator`] passed by the user. `memory.grow` calls
//! [`Allocator::grow_zeroed`] on the allocator of the linear memory of the instance.
//!
//! The allocator of the native stacks is wrapped in a [`WithStackGuard`], so that a stack
//! overflow faults in the guard region below the stack instead of overwriting the memory located
//! there. Such a fault is reported as [`Trap::StackOverflow`] if it happens in the Wasm code.

use core::{
    alloc::{Allocator, Layout},
    arch::global_asm,
    cell::Cell,
    fmt,
    marker::PhantomData,
    mem::offset_of,
    ptr::{self, NonNull},
    slice,
};
use std::collections::BTreeMap;

use crate::alloc_stack_guard::{self, WithStackGuard};

use cranelift_codegen::{
    Context, FinalizedRelocTarget,
    binemit::Reloc,
    control::ControlPlane,
    ir::{
        self, AbiParam, BlockArg, BlockCall, ExtFuncData, ExternalName, InstBuilder, JumpTableData,
        MemFlagsData, Signature, TrapCode, UserExternalName, UserFuncName, condcodes::IntCC, types,
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
/// Namespace of the [`UserExternalName`]s that designate routines of the host:
/// `init_vm_yield` at index 0, and `init_vm_call_import` at index 1.
const NAMESPACE_HOST: u32 = 2;

/// Layout of the native stack of each instance.
const STACK_LAYOUT: Layout = match Layout::from_size_align(1024 * 1024, 16) {
    Ok(layout) => layout,
    Err(_) => panic!(),
};
/// Alignment of the allocations of the linear memory of each instance.
const MEMORY_ALIGN: usize = 16;
/// Size in bytes of the alternate signal stack, if one has to be allocated.
const SIGNAL_STACK_SIZE: usize = 64 * 1024;

/// Value returned by `init_vm_switch` on the host side when the Wasm function has returned.
const OUTCOME_FINISHED: u64 = 0;
/// Value returned by `init_vm_switch` on the host side when the Wasm code calls an import.
const OUTCOME_IMPORT: u64 = 1;
/// Value returned by `init_vm_switch` on the host side when the Wasm code has trapped.
const OUTCOME_TRAP: u64 = 2;

global_asm!(
    // `init_vm_switch(save_rsp: *mut u64, new_rsp: u64, value: u64) -> u64`
    //
    // Pushes the callee-saved registers on the current stack, writes the stack pointer to
    // `save_rsp`, then switches to the stack `new_rsp`, pops the callee-saved registers from
    // it and returns `value` to whoever had pushed them.
    ".text",
    ".p2align 4",
    ".globl init_vm_switch",
    "init_vm_switch:",
    "push rbp",
    "push rbx",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov [rdi], rsp",
    "mov rsp, rsi",
    "mov rax, rdx",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
    // `init_vm_fiber_start`
    //
    // Reached through the `ret` of `init_vm_switch` when switching to a fresh stack prepared
    // by `Instance::call`, with `r12` = `vmctx`, `r13` = values buffer, `r14` = trampoline.
    ".p2align 4",
    ".globl init_vm_fiber_start",
    "init_vm_fiber_start:",
    "mov rdi, r12",
    "mov rsi, r13",
    "call r14",
    "lea rdi, [r12 + {wasm_rsp}]",
    "mov rsi, [r12 + {host_rsp}]",
    "mov edx, {finished}",
    "call init_vm_switch",
    "ud2",
    // `init_vm_yield(vmctx: *mut VmCtx)`
    //
    // Called by the Wasm code in order to call an import. Switches back to the host, and
    // returns when the host resumes the execution.
    ".p2align 4",
    ".globl init_vm_yield",
    "init_vm_yield:",
    "mov rsi, [rdi + {host_rsp}]",
    "lea rdi, [rdi + {wasm_rsp}]",
    "mov edx, {import}",
    "jmp init_vm_switch",
    // `init_vm_trap_landing(vmctx: *mut VmCtx)`
    //
    // Jumped to by the signal handler when the Wasm code traps. Abandons the Wasm stack and
    // returns to the host.
    ".p2align 4",
    ".globl init_vm_trap_landing",
    "init_vm_trap_landing:",
    "mov rsp, [rdi + {host_rsp}]",
    "mov eax, {trap}",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
    // `init_vm_sigreturn`
    //
    // Restorer of the signal handlers, as required by `rt_sigaction`.
    ".p2align 4",
    ".globl init_vm_sigreturn",
    "init_vm_sigreturn:",
    "mov eax, 15", // `rt_sigreturn`
    "syscall",
    "ud2",
    wasm_rsp = const offset_of!(VmCtx, wasm_rsp),
    host_rsp = const offset_of!(VmCtx, host_rsp),
    finished = const OUTCOME_FINISHED,
    import = const OUTCOME_IMPORT,
    trap = const OUTCOME_TRAP,
);

// The assembly code only accesses the fields of `VmCtx` that are FFI-safe.
#[allow(improper_ctypes)]
unsafe extern "sysv64" {
    fn init_vm_switch(save_rsp: *mut u64, new_rsp: u64, value: u64) -> u64;
    fn init_vm_fiber_start();
    fn init_vm_yield(vmctx: *mut VmCtx);
    fn init_vm_trap_landing(vmctx: *mut VmCtx);
    fn init_vm_sigreturn();
}

/// Value returned by `init_vm_call_import`, in the `rax` and `rdx` registers.
#[repr(C)]
struct CallImportOutput {
    /// Non-zero if the Wasm code must yield to the host by calling `init_vm_yield`.
    must_yield: u64,
    /// If `must_yield` is zero, the value returned by the import, zero-extended to 64 bits.
    result: u64,
}

/// Called by the Wasm code in order to call the import whose index is `import`, on the Wasm
/// stack. `param` is the parameter passed by the Wasm code, zero-extended to 64 bits.
///
/// Calls the callback of the import and returns its result. If the callback parks the
/// execution, the import index and the parameter are written to `import_idx` and the values
/// buffer before returning, so that the host can find them after the Wasm code has yielded.
///
/// `TCtxt` must be the type of the context that `ctxt` points to, and `import_callbacks` must
/// point to callbacks taking this context.
///
/// Note that a panic in the callback aborts the process, as unwinding out of this function
/// isn't possible.
unsafe extern "sysv64" fn init_vm_call_import<TCtxt>(
    vmctx: *mut VmCtx,
    import: u32,
    param: u64,
) -> CallImportOutput {
    unsafe {
        let sig = &*(*vmctx).funcs.add(import as usize);
        // `Builder::build` checks that all imports have exactly one parameter.
        let params = ParamsAccess {
            values: slice::from_ref(&param),
            tys: &sig.params,
        };
        let callbacks = (*vmctx)
            .import_callbacks
            .cast::<fn(&mut TCtxt, ParamsAccess<'_>) -> Outcome>();
        // The host can't access the context while the Wasm code runs, as `Instance::call` and
        // `Instance::resume` take `&mut self`.
        let ctxt = &mut *(*vmctx).ctxt.cast::<TCtxt>();
        let outcome = (*callbacks.add(import as usize))(ctxt, params);
        // `Builder::build` checks that all imports have exactly one result.
        let result = match (outcome, sig.results[0]) {
            (Outcome::Resume(Value::I32(v)), types::I32) => u64::from(v as u32),
            (Outcome::Resume(Value::I64(v)), types::I64) => v as u64,
            (Outcome::Resume(_), _) => {
                (*vmctx).bad_import_result = true;
                return CallImportOutput {
                    must_yield: 1,
                    result: 0,
                };
            }
            (Outcome::Park, _) => {
                (*vmctx).import_idx = import;
                // `values_len` is always at least 1.
                (*vmctx).values.write(param);
                return CallImportOutput {
                    must_yield: 1,
                    result: 0,
                };
            }
        };
        CallImportOutput {
            must_yield: 0,
            result,
        }
    }
}

/// Called by the Wasm code in order to execute `memory.grow`, with the number of pages to add
/// to the memory, on the Wasm stack.
///
/// Grows the memory with [`Allocator::grow_zeroed`] and returns the previous number of pages,
/// or `u32::MAX` if the memory can't be grown.
///
/// `TMemAlloc` must be the type of the allocator that `mem_allocator` points to.
///
/// Note that a panic in the allocator aborts the process, as unwinding out of this function
/// isn't possible.
unsafe extern "sysv64" fn init_vm_memory_grow<TMemAlloc: Allocator>(
    vmctx: *mut VmCtx,
    delta: u32,
) -> u32 {
    unsafe {
        let old_len = (*vmctx).mem_len;
        let old_pages = old_len / WASM_PAGE_SIZE;
        // `mem_max_pages` is at most 65536, as checked by the validator, hence the number of
        // pages always fits in a `u32` and is never equal to `u32::MAX`.
        let old_pages_u32 = u32::try_from(old_pages).unwrap_or_else(|_| unreachable!());
        // Can't overflow, as both numbers fit in 32 bits.
        let new_pages = old_pages + u64::from(delta);
        if new_pages > (*vmctx).mem_max_pages {
            return u32::MAX;
        }
        if delta == 0 {
            return old_pages_u32;
        }

        let new_len = new_pages * WASM_PAGE_SIZE;
        let Some(new_layout) = usize::try_from(new_len)
            .ok()
            .and_then(|size| Layout::from_size_align(size, MEMORY_ALIGN).ok())
        else {
            return u32::MAX;
        };
        // The current memory has been successfully allocated with this layout.
        let old_layout = Layout::from_size_align(
            usize::try_from(old_len).unwrap_or_else(|_| unreachable!()),
            MEMORY_ALIGN,
        )
        .unwrap_or_else(|_| unreachable!());

        let allocator = &*(*vmctx).mem_allocator.cast::<TMemAlloc>();
        match allocator.grow_zeroed((*vmctx).mem_base, old_layout, new_layout) {
            Ok(new_memory) => {
                (*vmctx).mem_base = new_memory.cast::<u8>();
                (*vmctx).mem_len = new_len;
                old_pages_u32
            }
            Err(_) => u32::MAX,
        }
    }
}

thread_local! {
    /// Context of the instance currently executing Wasm code on this thread, or null if none.
    ///
    /// Read by the signal handler in order to determine whether a fault comes from Wasm code.
    static CURRENT_VMCTX: Cell<*mut VmCtx> = const { Cell::new(ptr::null_mut()) };
}

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
    /// Failed to allocate memory.
    Allocation,
    /// The requested export doesn't exist or isn't a function.
    NoSuchExport,
    /// The parameters passed to [`Instance::call`] or [`Instance::resume`], or the values
    /// returned through [`Outcome::Resume`], don't match the function signature.
    BadArgs,
    /// The number of imports registered with [`Builder::register_import`] doesn't match the
    /// number of imports of the Wasm module.
    ImportsCountMismatch,
    /// [`Instance::resume`] was called while no execution is suspended.
    NotSuspended,
    /// Failed to install the signal handlers.
    SignalHandlers,
    /// The Wasm code has trapped.
    Trap(Trap),
}

/// Reason why the execution of Wasm code has trapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trap {
    /// An `unreachable` instruction was executed.
    Unreachable,
    /// The memory was accessed out of its bounds.
    OutOfBounds,
    /// An integer division or remainder by zero.
    DivisionByZero,
    /// The result of a signed integer division doesn't fit in its type.
    IntegerOverflow,
    /// The native stack of the instance is exhausted.
    StackOverflow,
    /// The Wasm code has faulted for an unknown reason. Indicates a bug in the VM.
    Unknown,
}

/// What happened to the execution of Wasm code after [`Instance::call`] or
/// [`Instance::resume`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// The called function has returned these values.
    Return(Vec<Value>),
    /// The Wasm code has called the imported function whose index is `import` (see
    /// [`Module::imports_by_index`]) with the given argument, and the callback registered for
    /// this import has returned [`Outcome::Park`]. Call [`Instance::resume`] with the return
    /// value in order to continue the execution.
    ImportCall { import: u32, args: Value },
}

/// Value returned by the callbacks registered with [`Builder::register_import`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Continue the execution of the Wasm code immediately, with this value as return value of
    /// the import.
    Resume(Value),
    /// Suspend the execution of the Wasm code. [`Instance::call`] or [`Instance::resume`]
    /// returns a [`RunOutcome::ImportCall`].
    Park,
}

/// Access to the parameters that the Wasm code has passed to an imported function.
#[derive(Clone, Copy)]
pub struct ParamsAccess<'a> {
    /// Raw values of the parameters, as stored in the buffer shared with the Wasm code.
    values: &'a [u64],
    /// Types of the parameters. Has the same length as `values`.
    tys: &'a [ir::Type],
}

impl ParamsAccess<'_> {
    /// Returns the number of parameters.
    #[inline]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns the parameter with the given index, or `None` if `index` is out of range.
    pub fn get(&self, index: usize) -> Option<Value> {
        let raw = *self.values.get(index)?;
        if self.tys[index] == types::I32 {
            Some(Value::I32(raw as u32 as i32))
        } else {
            Some(Value::I64(raw as i64))
        }
    }
}

impl fmt::Debug for ParamsAccess<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries((0..self.len()).filter_map(|index| self.get(index)))
            .finish()
    }
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
            Error::Allocation => write!(f, "failed to allocate memory"),
            Error::NoSuchExport => write!(f, "no such exported function"),
            Error::BadArgs => write!(f, "arguments don't match the function signature"),
            Error::ImportsCountMismatch => {
                write!(f, "number of registered imports doesn't match the module")
            }
            Error::NotSuspended => write!(f, "no execution is suspended"),
            Error::SignalHandlers => write!(f, "failed to install the signal handlers"),
            Error::Trap(trap) => write!(f, "Wasm trap: {trap:?}"),
        }
    }
}

/// Context passed as hidden first parameter to every compiled function.
#[repr(C)]
struct VmCtx {
    /// Linear memory, allocated with `mem_allocator`, [`MEMORY_ALIGN`] and a size of `mem_len`.
    mem_base: NonNull<u8>,
    /// Size in bytes of the linear memory. Always a multiple of [`WASM_PAGE_SIZE`].
    mem_len: u64,
    /// Maximum number of pages of the linear memory. At most 65536.
    mem_max_pages: u64,
    /// Allocator of the linear memory, whose type is erased. Owned by the [`Instance`].
    mem_allocator: *const (),
    /// [`init_vm_memory_grow`] instantiated with the type of `mem_allocator`. Called by the
    /// compiled code.
    mem_grow: unsafe extern "sysv64" fn(*mut VmCtx, u32) -> u32,
    globals: *mut u64,
    /// Buffer used to pass parameters and results between the host and the Wasm code.
    values: *mut u64,
    /// Index of the import whose callback has parked the execution, written by
    /// `init_vm_call_import` before the Wasm code yields.
    import_idx: u32,
    /// Set to `true` by `init_vm_call_import` if the callback of an import has returned a
    /// value of the wrong type, in which case the Wasm code yields.
    bad_import_result: bool,
    /// Signature of each function of the module, imported functions first.
    funcs: *const FuncSig,
    /// Callback of each imported function, whose type is erased. Points to the
    /// `import_callbacks` field of the [`Module`].
    import_callbacks: *const (),
    /// Context passed to the import callbacks, whose type is erased. Owned by the [`Instance`].
    ctxt: *mut (),
    /// Signal number of the latest trap, written by the signal handler.
    trap_signal: u32,
    /// Instruction pointer of the latest trap, written by the signal handler.
    trap_pc: u64,
    /// Faulting address of the latest trap, written by the signal handler.
    trap_addr: u64,
    /// Stack pointer of the host, saved while the Wasm code runs.
    host_rsp: u64,
    /// Stack pointer of the Wasm code, saved while the host runs.
    wasm_rsp: u64,
    /// Range of addresses of the compiled code of the module.
    code_start: u64,
    code_end: u64,
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
            params: ty
                .params()
                .iter()
                .map(|t| val_type(*t))
                .collect::<Result<_, _>>()?,
            results: ty
                .results()
                .iter()
                .map(|t| val_type(*t))
                .collect::<Result<_, _>>()?,
        })
    }

    /// Cranelift signature of the compiled function: `vmctx` followed by the Wasm params.
    fn to_clif(&self) -> Signature {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(types::I64));
        sig.params
            .extend(self.params.iter().map(|t| AbiParam::new(*t)));
        sig.returns
            .extend(self.results.iter().map(|t| AbiParam::new(*t)));
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
///
/// The compiled code is stored in memory allocated by `TCodeAlloc`, which must be executable.
///
/// The callbacks of the imports receive a mutable reference to a `TCtxt`, passed when creating
/// an [`Instance`].
pub struct Module<TCodeAlloc: Allocator, TCtxt> {
    /// Compiled code, allocated with `code_allocator` and `code_layout`.
    code: NonNull<u8>,
    code_layout: Layout,
    code_allocator: TCodeAlloc,
    /// Signature of each function, imported functions first.
    funcs: Vec<FuncSig>,
    /// Imported functions: (module name, field name).
    imports: Vec<(String, String)>,
    /// Callback of each imported function, in the same order as `imports`.
    import_callbacks: Vec<fn(&mut TCtxt, ParamsAccess<'_>) -> Outcome>,
    /// Exported functions: name => (function index, offset of the trampoline in `code`).
    exports: BTreeMap<String, (u32, usize)>,
    /// Minimum number of pages of the memory, if any.
    memory_pages: Option<u64>,
    /// Maximum number of pages of the memory. 0 if there is no memory.
    memory_max_pages: u64,
    globals: Vec<Value>,
    /// Active data segments: (offset in memory, bytes).
    data: Vec<(u32, Vec<u8>)>,
    /// Offset in `code` of each trapping instruction => trap code.
    traps: BTreeMap<usize, TrapCode>,
    /// Number of `u64` slots of the buffer used to pass parameters and results.
    values_len: usize,
}

/// Prototype for a [`Module`].
pub struct Builder<'a, TCodeAlloc: Allocator, TCtxt> {
    wasm: &'a [u8],
    code_allocator: TCodeAlloc,
    /// Callbacks registered with [`Builder::register_import`], in order.
    import_callbacks: Vec<fn(&mut TCtxt, ParamsAccess<'_>) -> Outcome>,
}

impl<'a, TCodeAlloc: Allocator, TCtxt> Builder<'a, TCodeAlloc, TCtxt> {
    /// Initializes a new builder for the given Wasm module.
    ///
    /// The compiled code will be written to memory allocated with `code_allocator`. This memory
    /// must be executable.
    #[inline]
    pub fn new(wasm: &'a [u8], code_allocator: TCodeAlloc) -> Self {
        Builder {
            wasm,
            code_allocator,
            import_callbacks: Vec::new(),
        }
    }

    /// Registers the callback of the next imported function.
    ///
    /// Imports must be registered in the same order as they are declared in the Wasm module,
    /// and the number of registered imports must be equal to the number of imports of the Wasm
    /// module, otherwise [`Builder::build`] returns [`Error::ImportsCountMismatch`].
    ///
    /// Whenever the Wasm code calls this import, `callback` is called with the context of the
    /// [`Instance`] (see [`Instance::new`]) and the parameter passed by the Wasm code. See
    /// [`Outcome`].
    ///
    /// Only imported functions with exactly one parameter and one result are supported.
    ///
    /// # Constraints on the callback
    ///
    /// The callback is called directly by the compiled Wasm code, from within
    /// [`Instance::call`] or [`Instance::resume`] and on the same thread, but on the native stack
    /// of the [`Instance`] rather than on the stack of the host. As a consequence:
    ///
    /// - The callback shares the 1 MiB native stack of the instance with the Wasm code that is
    ///   currently executing. It should avoid deep recursion and large stack allocations. A
    ///   stack overflow in the callback faults in the guard region below that stack, which kills
    ///   the process (see below).
    /// - A panic in the callback aborts the entire process, because unwinding through the frames
    ///   of the Wasm code isn't possible.
    /// - A fault in the callback (for example a segmentation fault) isn't reported as a Wasm
    ///   trap, and kills the process with the default action of the signal.
    /// - If the callback blocks, the execution of the Wasm code and the thread are blocked as
    ///   well. Use [`Outcome::Park`] to give control back to the host instead.
    ///
    /// The value passed to [`Outcome::Resume`] must have the type of the result of the import.
    /// Otherwise, the execution is abandoned and [`Instance::call`] or [`Instance::resume`]
    /// returns [`Error::BadArgs`].
    #[inline]
    pub fn register_import(&mut self, callback: fn(&mut TCtxt, ParamsAccess<'_>) -> Outcome) {
        self.import_callbacks.push(callback);
    }

    /// Parses, validates and compiles the Wasm module.
    pub fn build(self) -> Result<Module<TCodeAlloc, TCtxt>, Error> {
        let Builder {
            wasm,
            code_allocator,
            import_callbacks,
        } = self;

        wasmparser::Validator::new().validate_all(wasm)?;

        let mut types = Vec::new();
        let mut imports = Vec::new();
        let mut func_types = Vec::new();
        let mut memory_pages = None;
        let mut memory_max_pages = 0;
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
                        // The validator checks that the limits are at most 65536 pages.
                        memory_max_pages = mem.maximum.unwrap_or(65536);
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
                Payload::ImportSection(reader) => {
                    for import in reader.into_imports() {
                        let import = import?;
                        let wasmparser::TypeRef::Func(ty) = import.ty else {
                            return Err(Error::Unsupported("non-function import"));
                        };
                        // The type section always precedes the import section.
                        let sig = &types[ty as usize];
                        if sig.params.len() != 1 || sig.results.len() != 1 {
                            return Err(Error::Unsupported(
                                "import without exactly one parameter and one result",
                            ));
                        }
                        // Imports always precede the function section.
                        func_types.push(ty);
                        imports.push((String::from(import.module), String::from(import.name)));
                    }
                }
                Payload::StartSection { .. } => return Err(Error::Unsupported("start function")),
                Payload::TableSection(_) | Payload::ElementSection(_) => {
                    return Err(Error::Unsupported("tables"));
                }
                Payload::TagSection(_) => return Err(Error::Unsupported("exceptions")),
                _ => {}
            }
        }

        if import_callbacks.len() != imports.len() {
            return Err(Error::ImportsCountMismatch);
        }

        let funcs = func_types
            .iter()
            .map(|idx| types[*idx as usize].clone())
            .collect::<Vec<FuncSig>>();
        let values_len = funcs
            .iter()
            .map(|sig| sig.params.len().max(sig.results.len()))
            .max()
            .unwrap_or(0)
            .max(1);

        let isa = build_isa()?;
        let mut ctx = Context::new();
        let mut builder_ctx = FunctionBuilderContext::new();

        // Machine code of all the compiled functions, concatenated.
        let mut code = Vec::<u8>::new();
        // (namespace, index) => offset in `code`.
        let mut offsets = BTreeMap::<(u32, u32), usize>::new();
        // (offset in `code`, kind, target, addend)
        let mut relocs = Vec::<(usize, Reloc, (u32, u32), i64)>::new();
        // Offset in `code` of each trapping instruction => trap code.
        let mut traps = BTreeMap::<usize, TrapCode>::new();

        // Compile all the Wasm functions, followed with one trampoline per exported function.
        // Bodies are those of the non-imported functions.
        for item_idx in 0..(bodies.len() + exports.len()) {
            ctx.clear();
            let (namespace, index) = if let Some(body) = bodies.get(item_idx) {
                let func_idx = imports.len() + item_idx;
                ctx.func.signature = funcs[func_idx].to_clif();
                let builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
                let mut translator =
                    Translator::new(builder, &types, &funcs, imports.len(), &globals, func_idx);
                translator.translate(body)?;
                translator.builder.finalize(isa.frontend_config());
                (NAMESPACE_WASM_FUNC, func_idx as u32)
            } else {
                let export_idx = item_idx - bodies.len();
                let func_idx = exports[export_idx].1;
                if (func_idx as usize) < imports.len() {
                    return Err(Error::Unsupported("re-exported import"));
                }
                build_trampoline(
                    &mut ctx.func,
                    &mut builder_ctx,
                    &funcs[func_idx as usize],
                    func_idx,
                    isa.frontend_config(),
                );
                (NAMESPACE_TRAMPOLINE, export_idx as u32)
            };
            ctx.func.name = UserFuncName::user(namespace, index);

            let compiled = ctx
                .compile(&*isa, &mut ControlPlane::default())
                .map_err(|_| Error::Compile)?;

            while code.len() % 16 != 0 {
                code.push(0xcc); // int3
            }
            let base = code.len();
            code.extend_from_slice(compiled.code_buffer());
            offsets.insert((namespace, index), base);
            let compiled_relocs = compiled.buffer.relocs().to_vec();
            for trap in compiled.buffer.traps() {
                traps.insert(base + trap.offset as usize, trap.code);
            }

            for reloc in compiled_relocs {
                let target = match &reloc.target {
                    FinalizedRelocTarget::ExternalName(ExternalName::User(name)) => {
                        let name = &ctx.func.params.user_named_funcs()[*name];
                        (name.namespace, name.index)
                    }
                    _ => return Err(Error::Unsupported("relocation target")),
                };
                relocs.push((
                    base + reloc.offset as usize,
                    reloc.kind,
                    target,
                    reloc.addend,
                ));
            }
        }

        // Copy the code to executable memory and resolve the relocations.
        // Functions are aligned on 16 bytes relative to the start of the code.
        let code_layout =
            Layout::from_size_align(code.len().max(1), 16).map_err(|_| Error::Allocation)?;
        let code_mem = code_allocator
            .allocate(code_layout)
            .map_err(|_| Error::Allocation)?
            .cast::<u8>();
        let code_base = code_mem.as_ptr() as usize;
        unsafe {
            ptr::copy_nonoverlapping(code.as_ptr(), code_mem.as_ptr(), code.len());
        }

        for (offset, kind, target, addend) in relocs {
            let target_addr = if target == (NAMESPACE_HOST, 0) {
                (init_vm_yield as *const ()).addr() as i64 + addend
            } else if target == (NAMESPACE_HOST, 1) {
                (init_vm_call_import::<TCtxt> as *const ()).addr() as i64 + addend
            } else {
                (code_base + offsets[&target]) as i64 + addend
            };
            let at = unsafe { code_mem.as_ptr().add(offset) };
            match kind {
                Reloc::X86CallPCRel4 | Reloc::X86PCRel4 => {
                    let delta = target_addr - (code_base + offset) as i64;
                    let delta = i32::try_from(delta).map_err(|_| Error::Compile)?;
                    unsafe { ptr::write_unaligned(at as *mut i32, delta) };
                }
                Reloc::Abs8 => unsafe { ptr::write_unaligned(at as *mut i64, target_addr) },
                _ => return Err(Error::Unsupported("relocation kind")),
            }
        }

        let exports = exports
            .into_iter()
            .enumerate()
            .map(|(export_idx, (name, func_idx))| {
                let offset = offsets[&(NAMESPACE_TRAMPOLINE, export_idx as u32)];
                (name, (func_idx, offset))
            })
            .collect();

        Ok(Module {
            code: code_mem,
            code_layout,
            code_allocator,
            funcs,
            imports,
            import_callbacks,
            exports,
            memory_pages,
            memory_max_pages,
            globals,
            data,
            traps,
            values_len,
        })
    }
}

impl<TCodeAlloc: Allocator, TCtxt> Module<TCodeAlloc, TCtxt> {
    /// Returns the list of imported functions, as `(module name, field name)`, ordered by
    /// import index.
    pub fn imports_by_index(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.imports.iter().map(|(m, n)| (&m[..], &n[..]))
    }
}

impl<TCodeAlloc: Allocator, TCtxt> AsRef<Module<TCodeAlloc, TCtxt>> for Module<TCodeAlloc, TCtxt> {
    #[inline]
    fn as_ref(&self) -> &Module<TCodeAlloc, TCtxt> {
        self
    }
}

impl<TCodeAlloc: Allocator, TCtxt> Drop for Module<TCodeAlloc, TCtxt> {
    fn drop(&mut self) {
        // SAFETY: `code` was allocated with `code_allocator` and `code_layout`.
        unsafe { self.code_allocator.deallocate(self.code, self.code_layout) };
    }
}

fn build_isa() -> Result<OwnedTargetIsa, Error> {
    let mut flags = settings::builder();
    flags
        .set("opt_level", "speed")
        .map_err(|_| Error::Compile)?;
    flags.set("is_pic", "false").map_err(|_| Error::Compile)?;
    // Makes sure that functions with a large stack frame can't skip over the guard region.
    flags
        .set("enable_probestack", "true")
        .map_err(|_| Error::Compile)?;
    flags
        .set("probestack_strategy", "inline")
        .map_err(|_| Error::Compile)?;
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

/// Proof that the signal handlers that catch traps have been installed.
///
/// See [`install_trap_handlers`].
pub struct TrapHandlers {
    _private: (),
}

/// Layout of `struct sigaction` as expected by the `rt_sigaction` system call on x86-64.
#[repr(C)]
struct KernelSigaction {
    handler: usize,
    flags: u64,
    restorer: usize,
    mask: u64,
}

/// Layout of `stack_t` as expected by the `sigaltstack` system call on x86-64.
#[repr(C)]
struct KernelStack {
    sp: usize,
    flags: i32,
    size: usize,
}

impl KernelSigaction {
    const SIG_DFL: usize = 0;
    const SA_SIGINFO: u64 = 0x4;
    const SA_ONSTACK: u64 = 0x0800_0000;
    const SA_RESTORER: u64 = 0x0400_0000;
    const SIGILL: u32 = 4;
    const SIGFPE: u32 = 8;
    const SIGSEGV: u32 = 11;
}

/// Installs process-wide handlers for `SIGILL`, `SIGFPE` and `SIGSEGV`, which are necessary in
/// order to recover from traps in Wasm code.
///
/// Faults that don't originate from Wasm code restore the default action of the signal, which
/// terminates the process. In particular, this replaces the handler that the Rust standard
/// library installs to report stack overflows of the host.
///
/// Also makes sure that the current thread has an alternate signal stack, which is necessary
/// in order to handle stack overflows of the Wasm code. The Rust standard library normally
/// already installs one on every thread. Wasm code must only be executed on threads that have
/// an alternate signal stack.
///
/// # Safety
///
/// No other code must install handlers for these signals afterwards.
pub unsafe fn install_trap_handlers() -> Result<TrapHandlers, Error> {
    let mut current_stack = KernelStack {
        sp: 0,
        flags: 0,
        size: 0,
    };
    unsafe {
        syscalls::syscall!(
            syscalls::Sysno::sigaltstack,
            0usize,
            &raw mut current_stack as usize
        )
    }
    .map_err(|_| Error::SignalHandlers)?;
    // `SS_DISABLE` is 2.
    if current_stack.flags & 2 != 0 {
        // The alternate stack must remain valid for as long as the thread exists, hence why
        // it is leaked.
        let stack = Box::leak(vec![0u8; SIGNAL_STACK_SIZE].into_boxed_slice());
        let new_stack = KernelStack {
            sp: stack.as_mut_ptr() as usize,
            flags: 0,
            size: stack.len(),
        };
        unsafe {
            syscalls::syscall!(
                syscalls::Sysno::sigaltstack,
                &raw const new_stack as usize,
                0usize
            )
        }
        .map_err(|_| Error::SignalHandlers)?;
    }

    let action = KernelSigaction {
        handler: (trap_signal_handler as *const ()).addr(),
        flags: KernelSigaction::SA_SIGINFO
            | KernelSigaction::SA_ONSTACK
            | KernelSigaction::SA_RESTORER,
        restorer: (init_vm_sigreturn as *const ()).addr(),
        mask: 0,
    };
    for signal in [
        KernelSigaction::SIGILL,
        KernelSigaction::SIGFPE,
        KernelSigaction::SIGSEGV,
    ] {
        unsafe {
            syscalls::syscall!(
                syscalls::Sysno::rt_sigaction,
                signal,
                &raw const action as usize,
                0usize,
                8usize
            )
        }
        .map_err(|_| Error::SignalHandlers)?;
    }

    Ok(TrapHandlers { _private: () })
}

/// Handler of `SIGILL`, `SIGFPE` and `SIGSEGV`.
///
/// If the fault comes from Wasm code, records the details of the fault in the `VmCtx` and
/// modifies the interrupted context so that the execution continues in `init_vm_trap_landing`.
unsafe extern "C" fn trap_signal_handler(signal: u32, info: *mut u8, context: *mut u8) {
    // Offsets in `siginfo_t` and `ucontext_t` on x86-64 Linux.
    const SI_ADDR_OFFSET: usize = 16;
    const GREGS_OFFSET: usize = 40;
    const REG_RDI: usize = 8;
    const REG_RIP: usize = 16;

    unsafe {
        let gregs = context.add(GREGS_OFFSET).cast::<u64>();
        let pc = gregs.add(REG_RIP).read_unaligned();
        let fault_addr = info.add(SI_ADDR_OFFSET).cast::<u64>().read_unaligned();

        let vmctx = CURRENT_VMCTX.get();
        if !vmctx.is_null() && pc >= (*vmctx).code_start && pc < (*vmctx).code_end {
            (*vmctx).trap_signal = signal;
            (*vmctx).trap_pc = pc;
            (*vmctx).trap_addr = fault_addr;
            gregs
                .add(REG_RIP)
                .write_unaligned((init_vm_trap_landing as *const ()).addr() as u64);
            gregs.add(REG_RDI).write_unaligned(vmctx as u64);
            return;
        }

        // Not a Wasm trap. Restore the default action. The faulting instruction is executed
        // again when the handler returns, which then terminates the process.
        let action = KernelSigaction {
            handler: KernelSigaction::SIG_DFL,
            flags: 0,
            restorer: 0,
            mask: 0,
        };
        let _ = syscalls::syscall!(
            syscalls::Sysno::rt_sigaction,
            signal,
            &raw const action as usize,
            0usize,
            8usize
        );
    }
}

/// An instantiated module, with its own memory, globals, and native stack.
///
/// The native stack is allocated with `TStackAlloc`, and the linear memory with `TMemAlloc`.
/// The instance owns a `TCtxt`, passed to the callbacks of the imports.
pub struct Instance<
    TModule: AsRef<Module<TCodeAlloc, TCtxt>>,
    TCodeAlloc: Allocator,
    TCtxt,
    TStackAlloc: Allocator,
    TMemAlloc: Allocator,
> {
    module: TModule,
    /// Obtained from `Box::into_raw`. Accessed by the compiled code, hence why it is a raw
    /// pointer.
    vmctx: *mut VmCtx,
    state: InstanceState,
    /// Native stack on which the Wasm code runs, allocated with `stack_allocator` and
    /// [`STACK_LAYOUT`].
    stack: NonNull<u8>,
    stack_allocator: WithStackGuard<TStackAlloc>,
    /// Allocator of the linear memory, obtained from `Box::into_raw`. Accessed through
    /// `vmctx` by [`init_vm_memory_grow`], hence why it is boxed and a raw pointer.
    memory_allocator: NonNull<TMemAlloc>,
    /// Context passed to the import callbacks, obtained from `Box::into_raw`. Accessed through
    /// `vmctx` by [`init_vm_call_import`], hence why it is boxed and a raw pointer.
    ctxt: NonNull<TCtxt>,
    // These fields are accessed through raw pointers in `vmctx`.
    _globals: Vec<u64>,
    _values: Vec<u64>,
    _code_alloc: PhantomData<TCodeAlloc>,
}

enum InstanceState {
    /// No execution is in progress.
    Idle,
    /// The execution of `export_func` is suspended while the host executes `import`.
    Suspended { export_func: u32, import: u32 },
}

impl<
    TModule: AsRef<Module<TCodeAlloc, TCtxt>>,
    TCodeAlloc: Allocator,
    TCtxt,
    TStackAlloc: Allocator,
    TMemAlloc: Allocator,
> Instance<TModule, TCodeAlloc, TCtxt, TStackAlloc, TMemAlloc>
{
    /// Instantiates the given module. Its native stack is allocated with `stack_allocator`, and
    /// its linear memory with `memory_allocator`.
    ///
    /// `ctxt` is passed to the callbacks of the imports (see [`Builder::register_import`]), and
    /// can be accessed with [`Instance::ctxt`] and [`Instance::ctxt_mut`].
    ///
    /// The stack is allocated through a [`WithStackGuard`] wrapping around `stack_allocator`,
    /// meaning that the memory obtained from `stack_allocator` must be readable and writable,
    /// and that `mprotect` must be usable on it.
    ///
    /// The linear memory is allocated with [`Allocator::allocate_zeroed`], and `memory.grow`
    /// calls [`Allocator::grow_zeroed`]. These are called on the native stack of the instance
    /// (see [`Builder::register_import`] for the consequences), and a panic in
    /// [`Allocator::grow_zeroed`] aborts the process.
    pub fn new(
        module: TModule,
        _trap_handlers: &TrapHandlers,
        ctxt: TCtxt,
        stack_allocator: TStackAlloc,
        memory_allocator: TMemAlloc,
    ) -> Result<Self, Error> {
        let module_ref = module.as_ref();
        let mem_len = module_ref.memory_pages.unwrap_or(0) * WASM_PAGE_SIZE;
        let mem_layout = usize::try_from(mem_len)
            .ok()
            .and_then(|size| Layout::from_size_align(size, MEMORY_ALIGN).ok())
            .ok_or(Error::Allocation)?;
        let memory = memory_allocator
            .allocate_zeroed(mem_layout)
            .map_err(|_| Error::Allocation)?
            .cast::<u8>();
        // SAFETY: `memory` was just allocated with `mem_layout`, and is only accessed through
        // this slice until the end of the loop below.
        let memory_slice = unsafe { slice::from_raw_parts_mut(memory.as_ptr(), mem_layout.size()) };
        for (offset, bytes) in &module_ref.data {
            // The validator doesn't check that data segments fit in the memory.
            let Some(dest) = usize::try_from(*offset)
                .ok()
                .and_then(|start| memory_slice.get_mut(start..)?.get_mut(..bytes.len()))
            else {
                // SAFETY: `memory` was allocated with `memory_allocator` and `mem_layout`.
                unsafe { memory_allocator.deallocate(memory, mem_layout) };
                return Err(Error::Unsupported("out-of-bounds data segment"));
            };
            dest.copy_from_slice(bytes);
        }

        let mut globals = module_ref
            .globals
            .iter()
            .map(|g| match *g {
                Value::I32(v) => u64::from(v as u32),
                Value::I64(v) => v as u64,
            })
            .collect::<Vec<u64>>();

        let mut values = vec![0u64; module_ref.values_len];

        let stack_allocator = WithStackGuard::new(stack_allocator);
        let stack = match stack_allocator.allocate(STACK_LAYOUT) {
            Ok(stack) => stack.cast::<u8>(),
            Err(_) => {
                // SAFETY: `memory` was allocated with `memory_allocator` and `mem_layout`.
                unsafe { memory_allocator.deallocate(memory, mem_layout) };
                return Err(Error::Allocation);
            }
        };

        // Moving an allocator doesn't invalidate the memory blocks it has allocated.
        let memory_allocator = NonNull::from(Box::leak(Box::new(memory_allocator)));
        let ctxt = NonNull::from(Box::leak(Box::new(ctxt)));

        let code_start = module_ref.code.as_ptr() as u64;
        let vmctx = Box::into_raw(Box::new(VmCtx {
            mem_base: memory,
            mem_len,
            mem_max_pages: module_ref.memory_max_pages,
            mem_allocator: memory_allocator.as_ptr().cast_const().cast::<()>(),
            mem_grow: init_vm_memory_grow::<TMemAlloc>,
            globals: globals.as_mut_ptr(),
            values: values.as_mut_ptr(),
            import_idx: 0,
            bad_import_result: false,
            funcs: module_ref.funcs.as_ptr(),
            import_callbacks: module_ref.import_callbacks.as_ptr().cast::<()>(),
            ctxt: ctxt.as_ptr().cast::<()>(),
            trap_signal: 0,
            trap_pc: 0,
            trap_addr: 0,
            host_rsp: 0,
            wasm_rsp: 0,
            code_start,
            code_end: code_start + module_ref.code_layout.size() as u64,
        }));

        Ok(Instance {
            module,
            vmctx,
            state: InstanceState::Idle,
            stack,
            stack_allocator,
            memory_allocator,
            ctxt,
            _globals: globals,
            _values: values,
            _code_alloc: PhantomData,
        })
    }

    /// Returns a reference to the context passed to [`Instance::new`].
    #[inline]
    pub fn ctxt(&self) -> &TCtxt {
        // SAFETY: `ctxt` is valid, and only accessed mutably by the import callbacks while the
        // Wasm code runs, which requires `&mut self`.
        unsafe { self.ctxt.as_ref() }
    }

    /// Returns a mutable reference to the context passed to [`Instance::new`].
    #[inline]
    pub fn ctxt_mut(&mut self) -> &mut TCtxt {
        // SAFETY: `ctxt` is valid, and only accessed by the import callbacks while the Wasm code
        // runs, which requires `&mut self`.
        unsafe { self.ctxt.as_mut() }
    }

    /// Starts executing the exported function with the given name.
    ///
    /// If the execution of a previous call is suspended, it is discarded.
    pub fn call(&mut self, export: &str, args: &[Value]) -> Result<RunOutcome, Error> {
        let module = self.module.as_ref();
        let (func_idx, offset) = *module.exports.get(export).ok_or(Error::NoSuchExport)?;
        self.write_values(&module.funcs[func_idx as usize].params, args)?;
        self.state = InstanceState::Idle;

        // Prepare the Wasm stack so that `init_vm_switch` "returns" into `init_vm_fiber_start`
        // with the expected registers, and with a stack pointer aligned on 16 bytes.
        let stack_top = self.stack.as_ptr() as u64 + STACK_LAYOUT.size() as u64;
        let frame: [u64; 9] = [
            0,                                                // r15
            module.code.as_ptr() as u64 + offset as u64,      // r14: trampoline
            unsafe { (*self.vmctx).values } as u64,           // r13
            self.vmctx as u64,                                // r12
            0,                                                // rbx
            0,                                                // rbp
            (init_vm_fiber_start as *const ()).addr() as u64, // return address
            0,
            0,
        ];
        let frame_start = stack_top - 9 * 8;
        unsafe {
            ptr::copy_nonoverlapping(frame.as_ptr(), frame_start as *mut u64, frame.len());
            (*self.vmctx).wasm_rsp = frame_start;
        }

        self.run(func_idx)
    }

    /// Resumes the execution suspended in a call to an import, passing the given return value
    /// of that import.
    pub fn resume(&mut self, result: Value) -> Result<RunOutcome, Error> {
        let InstanceState::Suspended {
            export_func,
            import,
        } = self.state
        else {
            return Err(Error::NotSuspended);
        };
        let module = self.module.as_ref();
        self.write_values(
            &module.funcs[import as usize].results,
            slice::from_ref(&result),
        )?;
        self.run(export_func)
    }

    /// Switches to the Wasm stack, and returns when the execution of `export_func` finishes,
    /// gets suspended, or traps.
    fn run(&mut self, export_func: u32) -> Result<RunOutcome, Error> {
        let vmctx = self.vmctx;
        let module = self.module.as_ref();

        let previous = CURRENT_VMCTX.replace(vmctx);
        let outcome = unsafe { init_vm_switch(&raw mut (*vmctx).host_rsp, (*vmctx).wasm_rsp, 0) };
        CURRENT_VMCTX.set(previous);

        match outcome {
            OUTCOME_FINISHED => {
                self.state = InstanceState::Idle;
                Ok(RunOutcome::Return(
                    self.read_values(&module.funcs[export_func as usize].results),
                ))
            }
            OUTCOME_IMPORT => {
                if unsafe { (*vmctx).bad_import_result } {
                    // The execution can't continue, as the import has no valid return value.
                    // Abandon it.
                    unsafe { (*vmctx).bad_import_result = false };
                    self.state = InstanceState::Idle;
                    return Err(Error::BadArgs);
                }
                let import = unsafe { (*vmctx).import_idx };
                let sig = &module.funcs[import as usize];
                self.state = InstanceState::Suspended {
                    export_func,
                    import,
                };
                // `values_len` is at least the number of parameters and results of every
                // function.
                let params = ParamsAccess {
                    values: unsafe { slice::from_raw_parts((*vmctx).values, sig.params.len()) },
                    tys: &sig.params,
                };
                Ok(RunOutcome::ImportCall {
                    import,
                    // `Builder::build` checks that all imports have exactly one parameter.
                    args: params.get(0).unwrap_or_else(|| unreachable!()),
                })
            }
            OUTCOME_TRAP => {
                self.state = InstanceState::Idle;
                let pc = unsafe { (*vmctx).trap_pc };

                let fault_addr = unsafe { (*vmctx).trap_addr };
                let stack_start =
                    u64::try_from(self.stack.as_ptr().addr()).unwrap_or_else(|_| unreachable!());
                let guard_start = stack_start
                    - u64::try_from(alloc_stack_guard::guard_size(STACK_LAYOUT))
                        .unwrap_or_else(|_| unreachable!());
                // Signal number of `SIGSEGV` on x86-64 Linux.
                if unsafe { (*vmctx).trap_signal } == 11
                    && fault_addr >= guard_start
                    && fault_addr < stack_start
                {
                    return Err(Error::Trap(Trap::StackOverflow));
                }

                let code_offset = usize::try_from(pc - module.code.as_ptr() as u64)
                    .unwrap_or_else(|_| unreachable!());
                let trap = match module.traps.get(&code_offset) {
                    Some(&TrapCode::HEAP_OUT_OF_BOUNDS) => Trap::OutOfBounds,
                    Some(&TrapCode::INTEGER_DIVISION_BY_ZERO) => Trap::DivisionByZero,
                    Some(&TrapCode::INTEGER_OVERFLOW) => Trap::IntegerOverflow,
                    Some(&TrapCode::STACK_OVERFLOW) => Trap::StackOverflow,
                    Some(code) if Some(*code) == TrapCode::user(1) => Trap::Unreachable,
                    _ => Trap::Unknown,
                };
                Err(Error::Trap(trap))
            }
            _ => unreachable!(),
        }
    }

    /// Writes the given values to the buffer shared with the Wasm code, after checking that
    /// they match the given types.
    fn write_values(&self, tys: &[ir::Type], values: &[Value]) -> Result<(), Error> {
        if values.len() != tys.len() {
            return Err(Error::BadArgs);
        }
        for (i, (value, ty)) in values.iter().zip(tys).enumerate() {
            let raw = match (*value, *ty) {
                (Value::I32(v), types::I32) => u64::from(v as u32),
                (Value::I64(v), types::I64) => v as u64,
                _ => return Err(Error::BadArgs),
            };
            // `values_len` is at least the number of parameters and results of every function.
            unsafe { (*self.vmctx).values.add(i).write(raw) };
        }
        Ok(())
    }

    /// Reads values of the given types from the buffer shared with the Wasm code.
    fn read_values(&self, tys: &[ir::Type]) -> Vec<Value> {
        tys.iter()
            .enumerate()
            .map(|(i, ty)| {
                // `values_len` is at least the number of parameters and results of every
                // function.
                let raw = unsafe { (*self.vmctx).values.add(i).read() };
                if *ty == types::I32 {
                    Value::I32(raw as u32 as i32)
                } else {
                    Value::I64(raw as i64)
                }
            })
            .collect::<Vec<_>>()
    }
}

impl<
    TModule: AsRef<Module<TCodeAlloc, TCtxt>>,
    TCodeAlloc: Allocator,
    TCtxt,
    TStackAlloc: Allocator,
    TMemAlloc: Allocator,
> Drop for Instance<TModule, TCodeAlloc, TCtxt, TStackAlloc, TMemAlloc>
{
    fn drop(&mut self) {
        // The Wasm stack contains no Rust frames, so a suspended execution can simply be
        // discarded.
        let vmctx = unsafe { Box::from_raw(self.vmctx) };
        let memory_allocator = unsafe { Box::from_raw(self.memory_allocator.as_ptr()) };
        // The memory was successfully allocated or grown with this layout.
        let mem_layout = Layout::from_size_align(
            usize::try_from(vmctx.mem_len).unwrap_or_else(|_| unreachable!()),
            MEMORY_ALIGN,
        )
        .unwrap_or_else(|_| unreachable!());
        // SAFETY: `mem_base` was allocated with `memory_allocator` and `mem_layout`.
        unsafe { memory_allocator.deallocate(vmctx.mem_base, mem_layout) };
        // SAFETY: `stack` was allocated with `stack_allocator` and `STACK_LAYOUT`.
        unsafe { self.stack_allocator.deallocate(self.stack, STACK_LAYOUT) };
        // SAFETY: `ctxt` was obtained from `Box::into_raw`.
        drop(unsafe { Box::from_raw(self.ctxt.as_ptr()) });
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
    /// Number of imported functions, which are the first entries of `funcs`.
    num_imports: usize,
    globals: &'a [Value],
    func_idx: usize,
    vmctx: ir::Value,
    locals: Vec<Variable>,
    stack: Vec<ir::Value>,
    frames: Vec<Frame>,
    reachable: bool,
    func_refs: BTreeMap<u32, ir::FuncRef>,
    /// Reference to `init_vm_yield`.
    yield_ref: ir::FuncRef,
    /// Reference to `init_vm_call_import`.
    call_import_ref: ir::FuncRef,
}

impl<'a> Translator<'a> {
    fn new(
        mut builder: FunctionBuilder<'a>,
        types: &'a [FuncSig],
        funcs: &'a [FuncSig],
        num_imports: usize,
        globals: &'a [Value],
        func_idx: usize,
    ) -> Self {
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let vmctx = builder.block_params(entry)[0];

        let mut yield_sig = Signature::new(CallConv::SystemV);
        yield_sig.params.push(AbiParam::new(types::I64));
        let mut call_import_sig = yield_sig.clone();
        call_import_sig.params.push(AbiParam::new(types::I32));
        call_import_sig.params.push(AbiParam::new(types::I64));
        call_import_sig.returns.push(AbiParam::new(types::I64));
        call_import_sig.returns.push(AbiParam::new(types::I64));

        let signature = builder.import_signature(yield_sig);
        let name = builder
            .func
            .declare_imported_user_function(UserExternalName::new(NAMESPACE_HOST, 0));
        let yield_ref = builder.import_function(ExtFuncData {
            name: ExternalName::user(name),
            signature,
            colocated: false,
            patchable: false,
        });

        let signature = builder.import_signature(call_import_sig);
        let name = builder
            .func
            .declare_imported_user_function(UserExternalName::new(NAMESPACE_HOST, 1));
        let call_import_ref = builder.import_function(ExtFuncData {
            name: ExternalName::user(name),
            signature,
            colocated: false,
            patchable: false,
        });

        Translator {
            builder,
            types,
            funcs,
            num_imports,
            globals,
            func_idx,
            vmctx,
            locals: Vec::new(),
            stack: Vec::new(),
            frames: Vec::new(),
            reachable: true,
            func_refs: BTreeMap::new(),
            yield_ref,
            call_import_ref,
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
        self.builder.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            self.vmctx,
            offset as i32,
        )
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
            Operator::Call { function_index } if (function_index as usize) < self.num_imports => {
                // Calling an import consists in calling `init_vm_call_import`, which calls the
                // callback of the import and returns its result. If it indicates that the Wasm
                // code must yield, the Wasm code switches back to the host with `init_vm_yield`,
                // and the host writes the result to the values buffer before resuming the
                // execution.
                // `Builder::build` checks that all imports have exactly one parameter and one
                // result.
                let funcs = self.funcs;
                let result_ty = funcs[function_index as usize].results[0];
                let param = self.stack.pop().unwrap_or_else(|| unreachable!());
                let param = if self.builder.func.dfg.value_type(param) == i64t {
                    param
                } else {
                    self.builder.ins().uextend(i64t, param)
                };
                let import_idx = self.builder.ins().iconst(i32t, i64::from(function_index));
                let call = self
                    .builder
                    .ins()
                    .call(self.call_import_ref, &[self.vmctx, import_idx, param]);
                let (must_yield, result) = match *self.builder.inst_results(call) {
                    [must_yield, result] => (must_yield, result),
                    _ => unreachable!(),
                };

                let yield_block = self.builder.create_block();
                let continue_block = self.builder.create_block();
                let raw_result = self.builder.append_block_param(continue_block, i64t);
                self.builder.ins().brif(
                    must_yield,
                    yield_block,
                    &[],
                    continue_block,
                    &[BlockArg::Value(result)],
                );

                self.builder.switch_to_block(yield_block);
                self.builder.ins().call(self.yield_ref, &[self.vmctx]);
                let values = self.vmctx_load(offset_of!(VmCtx, values));
                let parked_result =
                    self.builder
                        .ins()
                        .load(i64t, MemFlagsData::trusted(), values, 0);
                self.builder
                    .ins()
                    .jump(continue_block, &[BlockArg::Value(parked_result)]);

                self.builder.switch_to_block(continue_block);
                let result = if result_ty == i64t {
                    raw_result
                } else {
                    self.builder.ins().ireduce(result_ty, raw_result)
                };
                self.stack.push(result);
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
                // Calls the `init_vm_memory_grow` function stored in the `VmCtx`, which is
                // specific to the type of allocator of the instance. The memory might move, but
                // `mem_base` and `mem_len` are loaded again from the `VmCtx` at each access.
                let delta = self.stack.pop().unwrap_or_else(|| unreachable!());
                let mut sig = Signature::new(CallConv::SystemV);
                sig.params.push(AbiParam::new(i64t));
                sig.params.push(AbiParam::new(i32t));
                sig.returns.push(AbiParam::new(i32t));
                let sig = self.builder.import_signature(sig);
                let callee = self.vmctx_load(offset_of!(VmCtx, mem_grow));
                let call = self
                    .builder
                    .ins()
                    .call_indirect(sig, callee, &[self.vmctx, delta]);
                let v = self.builder.inst_results(call)[0];
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
                let c = self
                    .builder
                    .ins()
                    .icmp(IntCC::UnsignedLessThanOrEqual, a, b);
                let v = self.builder.ins().uextend(types::I32, c);
                self.stack.push(v);
            }
            Operator::I32GeS | Operator::I64GeS => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let c = self
                    .builder
                    .ins()
                    .icmp(IntCC::SignedGreaterThanOrEqual, a, b);
                let v = self.builder.ins().uextend(types::I32, c);
                self.stack.push(v);
            }
            Operator::I32GeU | Operator::I64GeU => {
                let b = self.stack.pop().unwrap_or_else(|| unreachable!());
                let a = self.stack.pop().unwrap_or_else(|| unreachable!());
                let c = self
                    .builder
                    .ins()
                    .icmp(IntCC::UnsignedGreaterThanOrEqual, a, b);
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
