//! Allocator wrapper that puts a guard region at the start of each allocation.

use core::{
    alloc::{AllocError, Allocator, Layout},
    ptr::NonNull,
};

/// Implementation of [`Allocator`] that wraps around another allocator and puts an inaccessible
/// guard region at the start of each allocation.
///
/// This is meant to be used to allocate native stacks. Since stacks grow downwards, a stack
/// overflow accesses the guard region and triggers a segmentation fault rather than silently
/// corrupting the memory located before the stack.
///
/// The guard region is one page (4096 bytes) long, or as long as the alignment of the
/// allocation if this alignment is larger than a page. The inner allocator is always asked for
/// allocations aligned to at least the page size.
///
/// The guard region is made inaccessible through the `mprotect` system call, and is made
/// readable and writable again before being handed back to the inner allocator.
///
/// [`Allocator::grow`] and [`Allocator::shrink`] always allocate a new memory block, copy the
/// data, then deallocate the old block.
#[derive(Clone, Copy)]
pub struct WithStackGuard<A> {
    inner: A,
}

impl<A> WithStackGuard<A> {
    const PROT_NONE: usize = 0x0;
    const PROT_READ: usize = 0x1;
    const PROT_WRITE: usize = 0x2;

    /// Wraps around the given allocator.
    #[inline]
    pub fn new(inner: A) -> Self {
        WithStackGuard { inner }
    }
}

/// Returns the size in bytes of the guard region that [`WithStackGuard`] puts right before
/// allocations made with the given layout.
#[inline]
pub fn guard_size(layout: Layout) -> usize {
    layout.align().max(4096)
}

// SAFETY: every allocation is a sub-range of a distinct allocation of the inner allocator, that
// remains valid until it is passed to `deallocate`. Copying or cloning a `WithStackGuard`
// produces an allocator that can deallocate the allocations of the original if this is the
// case for the inner allocator as well.
unsafe impl<A: Allocator> Allocator for WithStackGuard<A> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let guard_size = guard_size(layout);
        let inner_layout = Layout::from_size_align(
            layout.size().checked_add(guard_size).ok_or(AllocError)?,
            guard_size,
        )
        .map_err(|_| AllocError)?;

        let inner_ptr = self.inner.allocate(inner_layout)?;
        let base = inner_ptr.cast::<u8>();

        // SAFETY: `base..base + guard_size` is within the allocation that we have just obtained
        // and is aligned to the page size.
        let protect_result = unsafe {
            syscalls::syscall!(
                syscalls::Sysno::mprotect,
                base.as_ptr().expose_provenance(),
                guard_size,
                Self::PROT_NONE
            )
        };
        if protect_result.is_err() {
            // SAFETY: the allocation was just returned by `allocate` with this layout.
            unsafe { self.inner.deallocate(base, inner_layout) };
            return Err(AllocError);
        }

        // SAFETY: the inner allocation is at least `guard_size` bytes long.
        let ptr = unsafe { base.add(guard_size) };
        Ok(NonNull::slice_from_raw_parts(ptr, inner_ptr.len() - guard_size))
    }

    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let guard_size = guard_size(layout);
        let inner_layout = Layout::from_size_align(
            layout.size().checked_add(guard_size).ok_or(AllocError)?,
            guard_size,
        )
        .map_err(|_| AllocError)?;

        let inner_ptr = self.inner.allocate_zeroed(inner_layout)?;
        let base = inner_ptr.cast::<u8>();

        // SAFETY: `base..base + guard_size` is within the allocation that we have just obtained
        // and is aligned to the page size.
        let protect_result = unsafe {
            syscalls::syscall!(
                syscalls::Sysno::mprotect,
                base.as_ptr().expose_provenance(),
                guard_size,
                Self::PROT_NONE
            )
        };
        if protect_result.is_err() {
            // SAFETY: the allocation was just returned by `allocate_zeroed` with this layout.
            unsafe { self.inner.deallocate(base, inner_layout) };
            return Err(AllocError);
        }

        // SAFETY: the inner allocation is at least `guard_size` bytes long.
        let ptr = unsafe { base.add(guard_size) };
        Ok(NonNull::slice_from_raw_parts(ptr, inner_ptr.len() - guard_size))
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        let guard_size = guard_size(layout);
        // The same layout was successfully built in `allocate` or `allocate_zeroed`. The size
        // passed by the caller is at most the length returned by these functions, and thus
        // can't overflow.
        let inner_layout = Layout::from_size_align(layout.size() + guard_size, guard_size)
            .unwrap_or_else(|_| unreachable!());

        // SAFETY: the caller guarantees that `ptr` has been returned by `allocate` or
        // `allocate_zeroed`, which always return a pointer `guard_size` bytes after the start
        // of the inner allocation.
        let base = unsafe { ptr.sub(guard_size) };

        // SAFETY: `base..base + guard_size` is the guard region of the allocation, which is
        // aligned to the page size and isn't used by anything else.
        let unprotect_result = unsafe {
            syscalls::syscall!(
                syscalls::Sysno::mprotect,
                base.as_ptr().expose_provenance(),
                guard_size,
                Self::PROT_READ | Self::PROT_WRITE
            )
        };
        if unprotect_result.is_err() {
            // The inner allocator might access the guard region, for example to write some
            // bookkeeping information in it. Leaking the memory is the only safe option.
            return;
        }

        // SAFETY: the allocation was returned by the inner allocator with this layout.
        unsafe { self.inner.deallocate(base, inner_layout) };
    }
}
