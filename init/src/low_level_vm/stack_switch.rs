//! Switching between a main stack and a fiber stack, on x86-64 with the System V ABI.
//!
//! A *fiber* is a function that runs on its own native stack, and whose execution can be
//! suspended and resumed. The *main stack* designates the stack of whoever resumes the fiber.
//! A [`Context`] owns the stack of a fiber, allocated with an allocator provided by the user of
//! this module, and stores the stack pointers of both sides while the other side is running.
//!
//! # Lifecycle
//!
//! 1. Create a [`Context`] in a pinned [`MaybeUninit`] with [`Context::new`], passing a
//!    function (the *entry point*) together with two arguments for it. This allocates the fiber
//!    stack, but doesn't run anything.
//! 2. Call [`resume`]. This switches to the fiber stack and runs the fiber until one of the
//!    following happens, which makes [`resume`] return:
//!    - The entry point returns. [`resume`] returns [`Outcome::Finished`].
//!    - The fiber calls [`stack_switch_yield`]. [`resume`] returns [`Outcome::Yielded`]. A later
//!      call to [`resume`] makes [`stack_switch_yield`] return, and the fiber continues where
//!      it was suspended.
//!    - The execution jumps to [`stack_switch_abandon`], typically from a signal handler that
//!      modified the interrupted context. [`resume`] returns [`Outcome::Abandoned`].
//! 3. After [`Outcome::Finished`] or [`Outcome::Abandoned`], the [`Context`] can be freed with
//!    [`Context::deinit`], which returns the allocator, and a new [`Context`] can then be
//!    created in the same location.
//!
//! Dropping the [`Context`] or passing it to [`Context::deinit`] frees the fiber stack.
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
//! abandoned, or freed while suspended, the content of its stack is simply forgotten.
//!
//! Panics must never unwind through a switch. In other words, the entry point of a fiber must
//! never unwind.
//!
//! Code running in a fiber observes the thread-local storage of the thread that calls [`resume`].

use alloc::alloc::Global;
use core::{
    alloc::{AllocError, Allocator, Layout},
    arch::global_asm,
    fmt,
    marker::PhantomPinned,
    mem::{offset_of, MaybeUninit},
    pin::Pin,
    ptr::{self, NonNull},
};

/// Stack of a fiber, and stack pointers of the main side and of the fiber side, each saved while
/// the other side runs.
///
/// The stack is allocated with `TAlloc` when the [`Context`] is created, and deallocated when it
/// is dropped.
///
/// [`Context::new`] and [`resume`] take a pinned [`Context`], as the fiber keeps a pointer to it.
// The assembly code accesses `main_rsp` and `fiber_rsp`. Since they come before any field whose
// type depends on `TAlloc`, `repr(C)` guarantees that their offsets are the same whatever
// `TAlloc` is.
#[repr(C)]
pub struct Context<TAlloc: Allocator> {
    /// Stack pointer of the main side, saved while the fiber runs.
    main_rsp: usize,
    /// Stack pointer of the fiber, saved while the main side runs.
    fiber_rsp: usize,
    /// Stack of the fiber, allocated with `allocator` and `stack_layout`.
    stack: NonNull<u8>,
    stack_layout: Layout,
    allocator: TAlloc,
    /// The fiber keeps a pointer to the [`Context`].
    _pinned: PhantomPinned,
}

impl<TAlloc: Allocator> Context<TAlloc> {
    /// Allocates a fiber stack with `allocator` and `stack_layout`, and initializes `slot` with
    /// a [`Context`] that owns it, prepared so that the first call to [`resume`] calls
    /// `entry_point(arg0, arg1)` on the fiber stack.
    ///
    /// This function doesn't run anything and doesn't switch stacks. It only writes a frame of
    /// 72 bytes at the top of the fiber stack and records its location in the [`Context`].
    ///
    /// If `slot` already contains a [`Context`], it is overwritten without being dropped. Use
    /// [`Context::deinit`] beforehand in order to free its stack.
    ///
    /// The memory returned by `allocator` must be readable and writable.
    ///
    /// Running out of stack space while a fiber runs isn't detected by this module. A stack
    /// overflow writes to the memory located right below the stack, unless `allocator` makes
    /// accessing that memory fault.
    ///
    /// # Panic
    ///
    /// Panics if the alignment of `stack_layout` is less than 16, if its size isn't a multiple
    /// of 16, or if its size is less than 72 bytes.
    ///
    /// # Safety
    ///
    /// - The fiber stack must not be accessed by anything other than the fiber until the fiber
    ///   is finished or abandoned.
    /// - `entry_point` must never unwind, and it must be sound to call it with `arg0` and `arg1`
    ///   as parameters.
    pub unsafe fn new<T, U>(
        slot: Pin<&mut MaybeUninit<Self>>,
        allocator: TAlloc,
        stack_layout: Layout,
        entry_point: unsafe extern "sysv64" fn(*mut T, *mut U),
        arg0: *mut T,
        arg1: *mut U,
    ) -> Result<Pin<&mut Self>, AllocError> {
        assert!(stack_layout.align() >= 16);
        assert!(stack_layout.size().is_multiple_of(16));
        // The frame written below is 72 bytes.
        assert!(stack_layout.size() >= 72);
        let stack = allocator.allocate(stack_layout)?.cast::<u8>();

        // SAFETY: nothing is moved out of `slot`.
        let slot = unsafe { Pin::get_unchecked_mut(slot) };
        // SAFETY: the stack is `stack_layout.size()` bytes long.
        let stack_top = unsafe { stack.add(stack_layout.size()) };
        debug_assert!(stack_top.as_ptr().addr().is_multiple_of(16));

        // Frame popped by the raw switch when the fiber is resumed for the first time: the
        // callee-saved registers, then the address `ret` jumps to. The two last slots keep the
        // stack pointer aligned on 16 bytes when `stack_switch_fiber_start` calls the entry
        // point.
        let frame = [
            slot.as_mut_ptr().addr(),                       // r15
            (entry_point as *const ()).addr(),              // r14
            arg1.addr(),                                    // r13
            arg0.addr(),                                    // r12
            0,                                              // rbx
            0,                                              // rbp
            (stack_switch_fiber_start as *const ()).addr(), // return address
            0,
            0,
        ];
        // SAFETY: the stack is large enough for the frame as checked above.
        let frame_start = unsafe {
            let frame_start = stack_top.as_ptr().cast::<usize>().sub(frame.len());
            frame_start.copy_from_nonoverlapping(frame.as_ptr(), frame.len());
            frame_start
        };

        let ctx = slot.write(Context {
            main_rsp: 0,
            fiber_rsp: frame_start.addr(),
            stack,
            stack_layout,
            allocator,
            _pinned: PhantomPinned,
        });
        // SAFETY: `ctx` is inside of `slot`, which is pinned.
        Ok(unsafe { Pin::new_unchecked(ctx) })
    }

    /// Frees the fiber stack of the [`Context`] contained in `slot`, and returns its allocator.
    /// `slot` no longer contains a [`Context`] afterwards.
    ///
    /// Nothing that lives on the fiber stack is dropped.
    ///
    /// # Safety
    ///
    /// - `slot` must contain a [`Context`].
    /// - This [`Context`] must not currently be being resumed.
    pub unsafe fn deinit(slot: Pin<&mut MaybeUninit<Self>>) -> TAlloc {
        // SAFETY: nothing is moved out of `slot`.
        let ctx = unsafe { Pin::get_unchecked_mut(slot) }.as_mut_ptr();
        // SAFETY: the caller guarantees that `slot` contains a `Context`. Its content is
        // considered uninitialized afterwards, so the allocator is never duplicated. Since the
        // fiber can't run anymore, nothing points to the `Context`.
        unsafe {
            let allocator = ptr::read(&raw const (*ctx).allocator);
            // `stack` was allocated with `allocator` and `stack_layout`.
            allocator.deallocate((*ctx).stack, (*ctx).stack_layout);
            allocator
        }
    }

    /// Returns the memory of the fiber stack. The fiber stack grows downwards, starting from the
    /// end of this memory.
    #[inline]
    pub fn stack(&self) -> NonNull<[u8]> {
        NonNull::slice_from_raw_parts(self.stack, self.stack_layout.size())
    }
}

impl<TAlloc: Allocator> Drop for Context<TAlloc> {
    fn drop(&mut self) {
        // SAFETY: `stack` was allocated with `allocator` and `stack_layout`.
        unsafe { self.allocator.deallocate(self.stack, self.stack_layout) };
    }
}

impl<TAlloc: Allocator> fmt::Debug for Context<TAlloc> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context").finish()
    }
}

/// Reason why [`resume`] has returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The entry point of the fiber has returned. The [`Context`] can no longer be
    /// resumed.
    Finished,
    /// The fiber has called [`stack_switch_yield`]. It can be continued with [`resume`].
    Yielded,
    /// The execution has jumped to [`stack_switch_abandon`]. The [`Context`] can no
    /// longer be resumed.
    Abandoned,
}

/// Switches to the fiber stack of `ctx` and runs the fiber until it finishes, yields, or is
/// abandoned. See [`Outcome`].
///
/// Can be called from any stack, including from the stack of a different fiber.
///
/// # Safety
///
/// - Since `ctx` has been created, [`resume`] must either have never been called with it or have
///   last returned [`Outcome::Yielded`].
/// - `ctx` must not currently be being resumed. In particular, the fiber of `ctx` must not call
///   [`resume`] with `ctx`.
/// - Running the fiber until it yields, finishes, or is abandoned must be sound.
pub unsafe fn resume<TAlloc: Allocator>(ctx: Pin<&mut Context<TAlloc>>) -> Outcome {
    // SAFETY: nothing is moved out of `ctx`.
    let ctx = unsafe { Pin::get_unchecked_mut(ctx) };
    // SAFETY: the caller guarantees that the fiber of `ctx` can be resumed.
    let outcome = unsafe { stack_switch_raw(&raw mut ctx.main_rsp, ctx.fiber_rsp, 0) };
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
    // `Context::new`, with `r12` = first argument, `r13` = second argument, `r14` = entry point,
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

    // `stack_switch_yield(ctx: *mut ())`
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

    // See the comment on `Context` for why `Global` can be used regardless of the allocator.
    main_rsp = const offset_of!(Context<Global>, main_rsp),
    fiber_rsp = const offset_of!(Context<Global>, fiber_rsp),
    finished = const OUTCOME_FINISHED,
    yielded = const OUTCOME_YIELDED,
    abandoned = const OUTCOME_ABANDONED,
);

unsafe extern "sysv64" {
    fn stack_switch_raw(save_rsp: *mut usize, new_rsp: usize, value: usize) -> usize;
    fn stack_switch_fiber_start();

    /// Suspends the fiber of `ctx`, which is a pointer to a [`Context`] (with any allocator)
    /// cast to `*mut ()`, and makes the pending call to [`resume`] return
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
    pub fn stack_switch_yield(ctx: *mut ());

    /// Abandons the fiber of `ctx`, which is a pointer to a [`Context`] (with any allocator)
    /// cast to `*mut ()`, and makes the pending call to [`resume`] return
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
    pub fn stack_switch_abandon(ctx: *mut ());
}

/// Value returned by the raw switch on the main side when the entry point has returned.
const OUTCOME_FINISHED: usize = 0;
/// Value returned by the raw switch on the main side when the fiber has yielded.
const OUTCOME_YIELDED: usize = 1;
/// Value returned by the raw switch on the main side when the fiber has been abandoned.
const OUTCOME_ABANDONED: usize = 2;
