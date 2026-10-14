use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};

/// Fixed-size memory buffer strictly aligned to physical disk sector boundaries (4096 bytes)
/// required for unbuffered Direct I/O (`FILE_FLAG_NO_BUFFERING`).
pub struct AlignedBuffer {
    ptr: *mut u8,
    layout: Layout,
}

impl AlignedBuffer {
    pub fn new(size: usize, align: usize) -> Self {
        let layout = Layout::from_size_align(size, align).expect("Invalid memory layout");
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        Self { ptr, layout }
    }

    #[inline]
    pub fn as_slice_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        unsafe {
            dealloc(self.ptr, self.layout);
        }
    }
}

unsafe impl Send for AlignedBuffer {}
unsafe impl Sync for AlignedBuffer {}
