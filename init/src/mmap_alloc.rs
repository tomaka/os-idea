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
}
