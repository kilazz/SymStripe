use std::io::{Cursor, Error};
use zstd::bulk::{Compressor, decompress as bulk_decompress};
use zstd::zstd_safe::CParameter;

/// DirectStorage / Microsoft ATG L2/L3 Cache Limit (2^18 = 256 KB)
pub const ZSTD_ATG_WINDOW_LOG: u32 = 18;

pub struct ZstdAtgCompressor;

impl ZstdAtgCompressor {
    /// Compresses a buffer with WindowLog = 18 and prepends the 4-byte uncompressed length
    /// for instant, single-pass zero-allocation bulk decompression.
    pub fn compress_atg(data: &[u8], level: i32) -> Result<Vec<u8>, Error> {
        if data.is_empty() {
            return Ok(Vec::new());
        }

        let mut compressor = Compressor::new(level.clamp(1, 19))?;
        compressor
            .set_parameter(CParameter::WindowLog(ZSTD_ATG_WINDOW_LOG))
            .map_err(|e| Error::other(e.to_string()))?;

        let compressed = compressor.compress(data)?;

        let mut output = Vec::with_capacity(4 + compressed.len());
        output.extend_from_slice(&(data.len() as u32).to_le_bytes());
        output.extend_from_slice(&compressed);
        Ok(output)
    }

    /// Decompresses an ATG-compressed chunk directly into a single target buffer.
    pub fn decompress_atg(data: &[u8]) -> Result<Vec<u8>, Error> {
        if data.is_empty() {
            return Ok(Vec::new());
        }

        // Fast-path: single-pass bulk decompression using the prepended 4-byte size
        if data.len() >= 4 {
            let orig_len = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
            if orig_len > 0
                && orig_len <= 128 * 1024 * 1024
                && let Ok(decompressed) = bulk_decompress(&data[4..], orig_len)
            {
                return Ok(decompressed);
            }
        }

        // Backward-compatible fallback for legacy streams
        zstd::stream::decode_all(Cursor::new(data))
    }
}
