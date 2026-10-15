use super::cache::CachedPart;
use super::fs::{DIRECT_PAGE_SIZE, SymStripePoolFs};
use super::node::{FileNode, PartKey, VfsCodec};
use crate::win32::{
    CloseHandle, CreateFileW, CreateIoCompletionPort, ERROR_IO_PENDING, FILE_FLAG_OVERLAPPED,
    GENERIC_READ, GetFileSizeEx, GetLastError, GetQueuedCompletionStatus, INVALID_HANDLE_VALUE,
    OPEN_EXISTING, OVERLAPPED, ReadFile,
};
use parking_lot::RwLock;
use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub struct IocpDispatcher {
    port: crate::win32::Handle,
}

unsafe impl Send for IocpDispatcher {}
unsafe impl Sync for IocpDispatcher {}

#[repr(C)]
struct IocpContext {
    overlapped: OVERLAPPED,
    file_handle: crate::win32::Handle,
    part_key: PartKey,
    codec: VfsCodec,
    buffer: Vec<u8>,
    fs_ref: SymStripePoolFs,
    node: Arc<FileNode>,
    is_direct_page: bool,
    direct_expected_len: usize,
}

impl IocpDispatcher {
    pub fn new() -> Arc<Self> {
        let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 0) };
        Arc::new(Self { port })
    }

    pub fn start_workers(self: &Arc<Self>, num_workers: usize) {
        for _ in 0..num_workers {
            let dispatcher = Arc::clone(self);
            std::thread::spawn(move || {
                Self::worker_loop(dispatcher.port);
            });
        }
    }

    fn worker_loop(port: crate::win32::Handle) {
        loop {
            let mut bytes_transferred = 0;
            let mut comp_key = 0;
            let mut overlapped_ptr: *mut OVERLAPPED = ptr::null_mut();

            let ok = unsafe {
                GetQueuedCompletionStatus(
                    port,
                    &mut bytes_transferred,
                    &mut comp_key,
                    &mut overlapped_ptr,
                    0xFFFFFFFF,
                )
            };

            if overlapped_ptr.is_null() {
                continue;
            }

            let mut ctx = unsafe { Box::from_raw(overlapped_ptr as *mut IocpContext) };
            unsafe { CloseHandle(ctx.file_handle) };

            if ok == 0 {
                ctx.fs_ref.prefetch_in_flight.lock().remove(&ctx.part_key);
                let mut inflight = ctx.fs_ref.in_flight_loads.lock();
                if let Some(cv) = inflight.remove(&ctx.part_key) {
                    cv.notify_all();
                }
                continue;
            }

            if ctx.is_direct_page {
                let actual_len = ctx.direct_expected_len.min(bytes_transferred as usize);
                ctx.buffer.truncate(actual_len);

                let new_part = Arc::new(RwLock::new(CachedPart {
                    data: ctx.buffer,
                    dirty: false,
                    referenced: AtomicBool::new(true),
                    codec: VfsCodec::Store,
                    rel_path: ctx.node.rel_path(),
                    file_hash: ctx.part_key.file_hash,
                    part_index: ctx.part_key.part_index,
                }));

                ctx.fs_ref.insert_into_cache(ctx.part_key, new_part);
                continue;
            }

            ctx.buffer.truncate(bytes_transferred as usize);

            let data = match ctx.codec {
                VfsCodec::Lz4 => {
                    lz4_flex::decompress_size_prepended(&ctx.buffer).unwrap_or_default()
                }
                VfsCodec::Zstd => crate::zstd_atg::ZstdAtgCompressor::decompress_atg(&ctx.buffer)
                    .unwrap_or_default(),
                VfsCodec::Store => std::mem::take(&mut ctx.buffer),
            };

            let new_part = Arc::new(RwLock::new(CachedPart {
                data,
                dirty: false,
                referenced: AtomicBool::new(true),
                codec: ctx.codec,
                rel_path: ctx.node.rel_path(),
                file_hash: ctx.part_key.file_hash,
                part_index: ctx.part_key.part_index,
            }));

            ctx.fs_ref.insert_into_cache(ctx.part_key, new_part);
        }
    }

    pub fn submit_prefetch(
        &self,
        fs_ref: SymStripePoolFs,
        node: Arc<FileNode>,
        part_key: PartKey,
        target_drive_idx: usize,
    ) {
        let rel = node.rel_path();
        let (store_lz4, legacy_lz4) =
            fs_ref.resolve_part_paths(target_drive_idx, &rel, part_key.part_index, "lz4");
        let (store_zst, legacy_zst) =
            fs_ref.resolve_part_paths(target_drive_idx, &rel, part_key.part_index, "zst");
        let (store_raw, legacy_raw) =
            fs_ref.resolve_part_paths(target_drive_idx, &rel, part_key.part_index, "raw");

        let (path, codec) = if store_lz4.exists() {
            (store_lz4, VfsCodec::Lz4)
        } else if legacy_lz4.exists() {
            (legacy_lz4, VfsCodec::Lz4)
        } else if store_zst.exists() {
            (store_zst, VfsCodec::Zstd)
        } else if legacy_zst.exists() {
            (legacy_zst, VfsCodec::Zstd)
        } else if store_raw.exists() {
            (store_raw, VfsCodec::Store)
        } else if legacy_raw.exists() {
            (legacy_raw, VfsCodec::Store)
        } else {
            fs_ref.prefetch_in_flight.lock().remove(&part_key);
            return;
        };

        let wide_path = crate::win32::to_wide_null(path.to_str().unwrap());
        let file_handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                GENERIC_READ,
                1 | 2, // FILE_SHARE_READ | WRITE
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };

        if file_handle == INVALID_HANDLE_VALUE {
            fs_ref.prefetch_in_flight.lock().remove(&part_key);
            return;
        }

        let mut size: i64 = 0;
        unsafe { GetFileSizeEx(file_handle, &mut size) };
        if size <= 0 {
            unsafe { CloseHandle(file_handle) };
            fs_ref.prefetch_in_flight.lock().remove(&part_key);
            return;
        }

        let buf = vec![0u8; size as usize];

        let mut ctx = Box::new(IocpContext {
            overlapped: OVERLAPPED {
                internal: 0,
                internal_high: 0,
                offset: 0,
                offset_high: 0,
                h_event: ptr::null_mut(),
            },
            file_handle,
            part_key,
            codec,
            buffer: buf,
            fs_ref,
            node,
            is_direct_page: false,
            direct_expected_len: 0,
        });

        unsafe { CreateIoCompletionPort(file_handle, self.port, 0, 0) };

        let mut read_bytes = 0;
        let ok = unsafe {
            ReadFile(
                file_handle,
                ctx.buffer.as_mut_ptr() as *mut c_void,
                size as u32,
                &mut read_bytes,
                &mut ctx.overlapped,
            )
        };

        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                unsafe { CloseHandle(file_handle) };
                ctx.fs_ref.prefetch_in_flight.lock().remove(&ctx.part_key);
                return;
            }
        }

        let _ = Box::into_raw(ctx);
    }

    pub fn submit_prefetch_direct(
        &self,
        fs_ref: SymStripePoolFs,
        node: Arc<FileNode>,
        page_index: u64,
        target_drive_idx: usize,
    ) {
        let part_key = PartKey {
            file_hash: node.file_hash.load(std::sync::atomic::Ordering::Relaxed),
            part_index: page_index,
        };

        let current_file_size = *node.file_size.lock();
        let page_offset = page_index * DIRECT_PAGE_SIZE;
        if page_offset >= current_file_size {
            fs_ref.prefetch_in_flight.lock().remove(&part_key);
            return;
        }

        let expected_len = (current_file_size - page_offset).min(DIRECT_PAGE_SIZE) as usize;
        let p = fs_ref.members[target_drive_idx].join(node.rel_path());

        let wide_path = crate::win32::to_wide_null(p.to_str().unwrap());
        let file_handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                GENERIC_READ,
                1 | 2,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };

        if file_handle == INVALID_HANDLE_VALUE {
            fs_ref.prefetch_in_flight.lock().remove(&part_key);
            return;
        }

        let buf = vec![0u8; expected_len];
        let offset_low = (page_offset & 0xFFFFFFFF) as u32;
        let offset_high = ((page_offset >> 32) & 0xFFFFFFFF) as u32;

        let mut ctx = Box::new(IocpContext {
            overlapped: OVERLAPPED {
                internal: 0,
                internal_high: 0,
                offset: offset_low,
                offset_high,
                h_event: ptr::null_mut(),
            },
            file_handle,
            part_key,
            codec: VfsCodec::Store,
            buffer: buf,
            fs_ref,
            node,
            is_direct_page: true,
            direct_expected_len: expected_len,
        });

        unsafe { CreateIoCompletionPort(file_handle, self.port, 0, 0) };

        let mut read_bytes = 0;
        let ok = unsafe {
            ReadFile(
                file_handle,
                ctx.buffer.as_mut_ptr() as *mut c_void,
                expected_len as u32,
                &mut read_bytes,
                &mut ctx.overlapped,
            )
        };

        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                unsafe { CloseHandle(file_handle) };
                ctx.fs_ref.prefetch_in_flight.lock().remove(&ctx.part_key);
                return;
            }
        }

        let _ = Box::into_raw(ctx);
    }
}
