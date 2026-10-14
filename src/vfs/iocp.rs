use super::cache::CachedPart;
use super::fs::SymStripePoolFs;
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

        // Successfully queued! Leak the box so Windows kernel owns it until completion.
        let _ = Box::into_raw(ctx);
    }
}
