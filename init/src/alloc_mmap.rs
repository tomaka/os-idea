//! Allocator that obtains memory directly from the kernel through `mmap`.

use core::{
    alloc::{AllocError, Allocator, Layout},
    ptr::{self, NonNull},
};

/// Implementation of [`Allocator`] where each allocation is a separate anonymous memory
/// mapping obtained through the `mmap` system call.
///
/// Allocations are always aligned to the page size (4096 bytes), and alignments larger than
/// this are not supported.
///
/// Note that the [`Allocator::shrink`] function is very slow as it is expected to be
/// never called.
#[derive(Clone, Copy)]
pub struct Mmap {
    /// Protection flags passed to `mmap`.
    prot: usize,
}

impl Mmap {
    const PROT_READ: usize = 0x1;
    const PROT_WRITE: usize = 0x2;
    const PROT_EXEC: usize = 0x4;
    const MAP_PRIVATE: usize = 0x02;
    const MAP_ANONYMOUS: usize = 0x20;
    const MREMAP_MAYMOVE: usize = 0x1;
    const PAGE_SIZE: usize = 4096;

    /// Returns an allocator whose allocations are readable and writable.
    #[inline]
    pub fn read_write() -> Self {
        Mmap {
            prot: Self::PROT_READ | Self::PROT_WRITE,
        }
    }

    /// Returns an allocator whose allocations are readable, writable, and executable.
    // TODO: memory that is both writable and executable is a security weakness; add a way to
    // switch allocations from writable to executable once their content has been written
    #[inline]
    pub fn read_write_exec() -> Self {
        Mmap {
            prot: Self::PROT_READ | Self::PROT_WRITE | Self::PROT_EXEC,
        }
    }
}

// SAFETY: every allocation is a distinct memory mapping that remains valid until it is passed
// to `deallocate`, and copying an `Mmap` produces an allocator that can deallocate the
// allocations of the original.
unsafe impl Allocator for Mmap {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if layout.align() > Self::PAGE_SIZE {
            return Err(AllocError);
        }

        if layout.size() == 0 {
            let dangling = NonNull::new(ptr::without_provenance_mut::<u8>(layout.align()))
                .unwrap_or_else(|| unreachable!());
            return Ok(NonNull::slice_from_raw_parts(dangling, 0));
        }

        // SAFETY: an anonymous mapping at an address chosen by the kernel doesn't affect any
        // existing memory.
        let addr = unsafe {
            syscalls::syscall!(
                syscalls::Sysno::mmap,
                0usize,
                layout.size(),
                self.prot,
                Self::MAP_PRIVATE | Self::MAP_ANONYMOUS,
                usize::MAX, // fd = -1
                0usize
            )
        }
        .map_err(|_| AllocError)?;

        let ptr = NonNull::new(ptr::with_exposed_provenance_mut::<u8>(addr)).ok_or(AllocError)?;
        Ok(NonNull::slice_from_raw_parts(ptr, layout.size()))
    }

    #[inline]
    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        // Anonymous mappings are always zero-filled by the kernel.
        self.allocate(layout)
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if layout.size() == 0 {
            return;
        }

        // SAFETY: the caller guarantees that `ptr` and `layout` designate a mapping that we
        // have returned in `allocate`.
        unsafe {
            let _ = syscalls::syscall!(
                syscalls::Sysno::munmap,
                ptr.as_ptr().expose_provenance(),
                layout.size()
            );
        }
    }

    unsafe fn grow(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if new_layout.align() > Self::PAGE_SIZE {
            return Err(AllocError);
        }

        // Zero-sized allocations aren't backed by a mapping, and passing an old size of 0 to
        // `mremap` has a special meaning.
        if old_layout.size() == 0 {
            return self.allocate(new_layout);
        }

        // SAFETY: the caller guarantees that `ptr` and `old_layout` designate a mapping that we
        // have returned in `allocate`, and `MREMAP_MAYMOVE` lets the kernel pick a new address
        // that doesn't overlap any existing memory.
        let addr = unsafe {
            syscalls::syscall!(
                syscalls::Sysno::mremap,
                ptr.as_ptr().expose_provenance(),
                old_layout.size(),
                new_layout.size(),
                Self::MREMAP_MAYMOVE
            )
        }
        .map_err(|_| AllocError)?;

        let ptr = NonNull::new(ptr::with_exposed_provenance_mut::<u8>(addr)).ok_or(AllocError)?;
        Ok(NonNull::slice_from_raw_parts(ptr, new_layout.size()))
    }

    #[inline]
    unsafe fn grow_zeroed(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // Pages added by `mremap` are zero-filled by the kernel. The bytes after the old size
        // within the last page of the original mapping are also zero, because `allocate` and
        // `grow` return exactly the requested size, meaning that these bytes have never been
        // written to, and `shrink` zeroes them.
        // SAFETY: the requirements are the same as the ones of `grow`.
        unsafe { self.grow(ptr, old_layout, new_layout) }
    }

    unsafe fn shrink(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if new_layout.align() > Self::PAGE_SIZE {
            return Err(AllocError);
        }

        if new_layout.size() == 0 {
            // SAFETY: the caller guarantees that `ptr` and `old_layout` designate an allocation
            // that we have returned in `allocate`.
            unsafe { self.deallocate(ptr, old_layout) };
            let dangling = NonNull::new(ptr::without_provenance_mut::<u8>(new_layout.align()))
                .unwrap_or_else(|| unreachable!());
            return Ok(NonNull::slice_from_raw_parts(dangling, 0));
        }

        // `old_layout.size()` can't be 0, as it is superior or equal to `new_layout.size()`.
        // SAFETY: the caller guarantees that `ptr` and `old_layout` designate a mapping that we
        // have returned in `allocate`. Without `MREMAP_MAYMOVE`, the mapping is shrunk in place
        // by unmapping its trailing pages.
        unsafe {
            syscalls::syscall!(
                syscalls::Sysno::mremap,
                ptr.as_ptr().expose_provenance(),
                old_layout.size(),
                new_layout.size(),
                0usize
            )
        }
        .map_err(|_| AllocError)?;

        // Zero the bytes after the new size within the last remaining page, so that
        // `grow_zeroed` can assume that they are zero.
        let new_size = new_layout.size();
        let zero_end = new_size
            .next_multiple_of(Self::PAGE_SIZE)
            .min(old_layout.size());
        // SAFETY: `new_size..zero_end` is within the bounds of the old allocation and within
        // the pages that are still mapped.
        unsafe {
            ptr.add(new_size).write_bytes(0, zero_end - new_size);
        }

        Ok(NonNull::slice_from_raw_parts(ptr, new_size))
    }
}
