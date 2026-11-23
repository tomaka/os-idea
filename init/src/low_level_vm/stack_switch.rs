//! Switching between a main stack and a fiber stack, on x86-64 with the System V ABI.
//!
//! A *fiber* is a function that runs on its own native stack, provided by the user of this
//! module, and whose execution can be suspended and resumed. The *main stack* designates the
//! stack of whoever resumes the fiber. A [`Context`] stores the stack pointers of both sides
//! while the other side is running.
//!
//! # Lifecycle
//!
//! 1. Create a [`Context`] with [`Context::new`], and put it at a location where it doesn't
//!    move for as long as a fiber is prepared.
//! 2. Call [`prepare`], passing a stack and a function (the *entry point*) together with two
//!    arguments for it. This doesn't run anything.
//! 3. Call [`resume`]. This switches to the fiber stack and runs the fiber until one of the
//!    following happens, which makes [`resume`] return:
//!    - The entry point returns. [`resume`] returns [`Outcome::Finished`].
//!    - The fiber calls [`stack_switch_yield`]. [`resume`] returns [`Outcome::Yielded`]. A later
//!      call to [`resume`] makes [`stack_switch_yield`] return, and the fiber continues where
//!      it was suspended.
//!    - The execution jumps to [`stack_switch_abandon`], typically from a signal handler that
//!      modified the interrupted context. [`resume`] returns [`Outcome::Abandoned`].
//! 4. After [`Outcome::Finished`] or [`Outcome::Abandoned`], the [`Context`] can be prepared
//!    again with [`prepare`], optionally with a different stack.
//!
//! # What is switched and what isn't
//!
//! A switch only saves and restores the stack pointer and the registers that the System V ABI
//! defines as callee-saved (`rbx`, `rbp`, `r12` to `r15`). From the point of view of the code on
//! either side, [`resume`] and [`stack_switch_yield`] behave like regular function calls: the
//! registers that are caller-saved are clobbered.
//!
//! The control bits of `MXCSR`, the x87 control word, and the direction flag are not switched,
//! and both sides must leave them in the state required by the ABI.
//!
//! Nothing that lives on the fiber stack is ever dropped by this module. When a fiber is
//! abandoned, or prepared again while suspended, the content of its stack is simply forgotten.
//!
//! Panics must never unwind through a switch. In other words, the entry point of a fiber must
//! never unwind.
//!
//! Code running in a fiber observes the thread-local storage of the thread that calls [`resume`].

use core::{arch::global_asm, fmt, mem::offset_of, ptr::NonNull};

/// Value returned by the raw switch on the main side when the entry point has returned.
const OUTCOME_FINISHED: usize = 0;
/// Value returned by the raw switch on the main side when the fiber has yielded.
const OUTCOME_YIELDED: usize = 1;
/// Value returned by the raw switch on the main side when the fiber has been abandoned.
const OUTCOME_ABANDONED: usize = 2;

/// Number of 64-bit slots written by [`prepare`] at the top of the fiber stack.
const FRAME_SLOTS: usize = 9;

/// Stack pointers of the main side and of the fiber side, each saved while the other side runs.
///
/// A [`Context`] must not be moved or destroyed from the moment it is passed to [`prepare`] until
/// [`resume`] returns [`Outcome::Finished`] or [`Outcome::Abandoned`], as the fiber keeps a
/// pointer to it.
#[repr(C)]
pub struct Context {
    /// Stack pointer of the main side, saved while the fiber runs.
    main_rsp: usize,
    /// Stack pointer of the fiber, saved while the main side runs.
    fiber_rsp: usize,
}

impl Context {
    /// Returns a new [`Context`] that has not been prepared yet.
    pub const fn new() -> Self {
        Context {
            main_rsp: 0,
            fiber_rsp: 0,
        }
    }
}

impl Default for Context {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context").finish()
    }
}

/// Reason why [`resume`] has returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The entry point of the fiber has returned. The [`Context`] must be passed to [`prepare`]
    /// again before it can be resumed.
    Finished,
    /// The fiber has called [`stack_switch_yield`]. It can be continued with [`resume`].
    Yielded,
    /// The execution has jumped to [`stack_switch_abandon`]. The [`Context`] must be passed to
    /// [`prepare`] again before it can be resumed.
    Abandoned,
}

/// Prepares `ctx` so that the next call to [`resume`] calls `entry_point(arg0, arg1)` on the
/// stack whose top is `stack_top`.
///
/// This function doesn't run anything and doesn't switch stacks. It only writes a small frame
/// right below `stack_top` and records its location in `ctx`.
///
/// If `ctx` was already prepared, or if its fiber was suspended, the previous fiber is discarded
/// without anything on its stack being dropped.
///
/// # Safety
///
/// - `ctx` must point to a valid [`Context`] that isn't currently being resumed, and that
///   follows the rules in the documentation of [`Context`].
/// - `stack_top` must be aligned on 16 bytes and must point one byte past the end of a memory
///   region that is readable and writable, isn't accessed by anything else until the fiber is
///   finished or abandoned, and is large enough for 72 bytes plus everything that the entry
///   point needs.
/// - `entry_point` must be the address of a function with the signature
///   `unsafe extern "sysv64" fn(*mut (), *mut ())`, which must never unwind, and for which it is
///   sound to be called with `arg0` and `arg1` as parameters.
pub unsafe fn prepare(
    ctx: *mut Context,
    stack_top: NonNull<u8>,
    entry_point: NonNull<u8>,
    arg0: *mut (),
    arg1: *mut (),
) {
    // Frame popped by the raw switch when the fiber is resumed for the first time: the
    // callee-saved registers, then the address `ret` jumps to. The two last slots keep the stack
    // pointer aligned on 16 bytes when `stack_switch_fiber_start` calls the entry point.
    let frame: [usize; FRAME_SLOTS] = [
        ctx.addr(),                                     // r15
        entry_point.as_ptr().addr(),                    // r14
        arg1.addr(),                                    // r13
        arg0.addr(),                                    // r12
        0,                                              // rbx
        0,                                              // rbp
        (stack_switch_fiber_start as *const ()).addr(), // return address
        0,
        0,
    ];
    // SAFETY: the caller guarantees that the memory below `stack_top` is writable and large
    // enough, and that `ctx` is valid.
    unsafe {
        let frame_start = stack_top.as_ptr().cast::<usize>().sub(FRAME_SLOTS);
        frame_start.copy_from_nonoverlapping(frame.as_ptr(), FRAME_SLOTS);
        (*ctx).fiber_rsp = frame_start.addr();
    }
}

/// Switches to the fiber stack of `ctx` and runs the fiber until it finishes, yields, or is
/// abandoned. See [`Outcome`].
///
/// Can be called from any stack, including from the stack of a different fiber.
///
/// # Safety
///
/// - `ctx` must point to a valid [`Context`] that follows the rules in the documentation of
///   [`Context`].
/// - `ctx` must have been passed to [`prepare`], and since then [`resume`] must either have never
///   been called with it or have last returned [`Outcome::Yielded`].
/// - `ctx` must not currently be being resumed. In particular, the fiber of `ctx` must not call
///   [`resume`] with `ctx`.
/// - Running the fiber until it yields, finishes, or is abandoned must be sound.
pub unsafe fn resume(ctx: *mut Context) -> Outcome {
    // SAFETY: the caller guarantees that `ctx` is valid and that its fiber can be resumed.
    let outcome = unsafe { stack_switch_raw(&raw mut (*ctx).main_rsp, (*ctx).fiber_rsp, 0) };
    match outcome {
        OUTCOME_FINISHED => Outcome::Finished,
        OUTCOME_YIELDED => Outcome::Yielded,
        OUTCOME_ABANDONED => Outcome::Abandoned,
        // Only the assembly routines of this module switch to the main side, and they always
        // pass one of the values above.
        _ => unreachable!(),
    }
}

global_asm!(
    // `stack_switch_raw(save_rsp: *mut usize, new_rsp: usize, value: usize) -> usize`
    //
    // Pushes the callee-saved registers on the current stack, writes the stack pointer to
    // `save_rsp`, then switches to the stack `new_rsp`, pops the callee-saved registers from
    // it and returns `value` to whoever had pushed them.
    ".text",
    ".p2align 4",
    ".globl stack_switch_raw",
    "stack_switch_raw:",
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
    // `stack_switch_fiber_start`
    //
    // Reached through the `ret` of `stack_switch_raw` when switching to a stack prepared by
    // `prepare`, with `r12` = first argument, `r13` = second argument, `r14` = entry point,
    // `r15` = context. `r15` is callee-saved, and thus still contains the context after the
    // entry point returns.
    ".p2align 4",
    ".globl stack_switch_fiber_start",
    "stack_switch_fiber_start:",
    "mov rdi, r12",
    "mov rsi, r13",
    "call r14",
    "lea rdi, [r15 + {fiber_rsp}]",
    "mov rsi, [r15 + {main_rsp}]",
    "mov edx, {finished}",
    "call stack_switch_raw",
    "ud2",
    // `stack_switch_yield(ctx: *mut Context)`
    ".p2align 4",
    ".globl stack_switch_yield",
    "stack_switch_yield:",
    "mov rsi, [rdi + {main_rsp}]",
    "lea rdi, [rdi + {fiber_rsp}]",
    "mov edx, {yielded}",
    "jmp stack_switch_raw",
    // `stack_switch_abandon`, jumped to with `rdi` = `ctx`.
    ".p2align 4",
    ".globl stack_switch_abandon",
    "stack_switch_abandon:",
    "mov rsp, [rdi + {main_rsp}]",
    "mov eax, {abandoned}",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
    // `stack_switch_sigreturn`
    ".p2align 4",
    ".globl stack_switch_sigreturn",
    "stack_switch_sigreturn:",
    "mov eax, 15", // `rt_sigreturn`
    "syscall",
    "ud2",
    main_rsp = const offset_of!(Context, main_rsp),
    fiber_rsp = const offset_of!(Context, fiber_rsp),
    finished = const OUTCOME_FINISHED,
    yielded = const OUTCOME_YIELDED,
    abandoned = const OUTCOME_ABANDONED,
);

unsafe extern "sysv64" {
    fn stack_switch_raw(save_rsp: *mut usize, new_rsp: usize, value: usize) -> usize;
    fn stack_switch_fiber_start();

    /// Suspends the fiber of `ctx`, and makes the pending call to [`resume`] return
    /// [`Outcome::Yielded`]. Returns when [`resume`] is called again with `ctx`.
    ///
    /// This function is exposed as a raw symbol so that its address can be used by code that
    /// isn't written in Rust, for example generated machine code. It follows the `sysv64`
    /// calling convention.
    ///
    /// # Safety
    ///
    /// - Must be called from the fiber of `ctx`, while the call to [`resume`] that runs this
    ///   fiber is still in progress.
    /// - Running the main side until it resumes the fiber again must be sound.
    pub fn stack_switch_yield(ctx: *mut Context);

    /// Abandons the fiber of `ctx`, and makes the pending call to [`resume`] return
    /// [`Outcome::Abandoned`]. Never returns.
    ///
    /// This symbol must be *jumped to* rather than called, with the `rdi` register containing
    /// `ctx`. The value of the stack pointer and of the other registers doesn't matter. The
    /// typical use case is a signal handler that catches a fault in the fiber and modifies the
    /// interrupted context so that the execution continues at the address of this symbol,
    /// with `rdi` set to `ctx`. The fault can be of any kind, including a stack overflow.
    ///
    /// The `extern` declaration only exists in order to obtain the address of this symbol.
    /// Calling it as a Rust function is undefined behavior.
    ///
    /// # Safety
    ///
    /// - The call to [`resume`] that runs the fiber of `ctx` must still be in progress, and the
    ///   execution must be either in this fiber or in a signal handler that interrupted it.
    /// - Nothing on the fiber stack must require being dropped.
    pub fn stack_switch_abandon(ctx: *mut Context);

    /// Executes the `rt_sigreturn` Linux system call, and never returns.
    ///
    /// Linux on x86-64 requires the `SA_RESTORER` flag, and the address of a function such as
    /// this one in the `sa_restorer` field, when a signal handler is installed with the
    /// `rt_sigaction` system call. The signal handler returns to this function, which restores
    /// the possibly modified interrupted context. This is how a signal handler can redirect
    /// the execution to [`stack_switch_abandon`].
    ///
    /// The `extern` declaration only exists in order to obtain the address of this symbol.
    /// Calling it as a Rust function is undefined behavior.
    pub fn stack_switch_sigreturn();
}
