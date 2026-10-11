use std::sync::atomic::{AtomicUsize, Ordering};

use libdeflater::*;
#[cfg(feature = "parallel")]
use rayon::prelude::*;

#[cfg(not(feature = "parallel"))]
use crate::rayon::prelude::*;
use crate::{PngError, PngResult};

pub fn deflate(data: &[u8], level: u8, max_size: Option<usize>) -> PngResult<Vec<u8>> {
    let mut compressor = Compressor::new(CompressionLvl::new(level.into()).unwrap());
    let capacity = max_size.unwrap_or_else(|| compressor.zlib_compress_bound(data.len()));
    let mut dest = vec![0; capacity];
    let len = compressor
        .zlib_compress(data, &mut dest)
        .map_err(|err| match err {
            CompressionError::InsufficientSpace => PngError::DeflatedDataTooLong(capacity),
        })?;
    dest.truncate(len);
    Ok(dest)
}

/// `data` as a zlib stream at `level` (10 or above), in parts (as many as
/// `part` bytes fit in the data, of equal size, so none is short)
/// compressed at once, each with the data before it as its window, and
/// written one after another between the zlib header and the Adler-32 of
/// the whole. Once the parts done pass `max_size`, the others aren't
/// started.
pub fn deflate_in_parts(data: &[u8], level: u8, part: usize, max_size: Option<usize>) -> PngResult<Vec<u8>> {
    let count = data.len() / part;
    let part = data.len().div_ceil(count);
    let max = max_size.unwrap_or(usize::MAX);
    // The zlib header and the Adler-32.
    let written = AtomicUsize::new(6);
    let parts: Vec<Option<Vec<u8>>> = (0..count)
        .into_par_iter()
        .with_max_len(1)
        .map(|k| {
            if written.load(Ordering::Relaxed) > max {
                return None;
            }
            let range = k * part..((k + 1) * part).min(data.len());
            let mut compressor = Compressor::new(CompressionLvl::new(level.into()).unwrap());
            let mut dest = vec![0; compressor.deflate_compress_part_bound(range.len())];
            let len = compressor
                .deflate_compress_part(data, range, &mut dest)
                .expect("the output buffer has the bound size, and the level is 10 or above");
            dest.truncate(len);
            written.fetch_add(len, Ordering::Relaxed);
            Some(dest)
        })
        .collect();
    let parts: Option<Vec<Vec<u8>>> = parts.into_iter().collect();
    let Some(parts) = parts.filter(|_| written.into_inner() <= max) else {
        return Err(PngError::DeflatedDataTooLong(max));
    };
    // As libdeflate writes it from level 8 on: deflate, 32 KiB window, slowest.
    let mut dest = vec![0x78, 0xDA];
    for part in parts {
        dest.extend_from_slice(&part);
    }
    let mut adler = Adler32::new();
    adler.update(data);
    dest.extend_from_slice(&adler.sum().to_be_bytes());
    Ok(dest)
}

pub fn inflate(data: &[u8], out_size: usize) -> PngResult<Vec<u8>> {
    let mut decompressor = Decompressor::new();
    let mut dest = vec![0; out_size];
    let len = decompressor
        .zlib_decompress(data, &mut dest)
        .map_err(|err| match err {
            DecompressionError::BadData => PngError::InvalidData,
            DecompressionError::InsufficientSpace => PngError::InflatedDataTooLong(out_size),
        })?;
    dest.truncate(len);
    Ok(dest)
}

#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = Crc::new();
    crc.update(data);
    crc.sum()
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::*;
    use crate::Deflater;

    /// Data compressed in parts reads back as it was, also with a length
    /// the parts don't divide and at the level where the parse differs;
    /// below level 10 and below two parts, it is compressed in one piece.
    #[test]
    fn parts_read_back() {
        let mut x: u32 = 7;
        let data: Vec<u8> = (0..1_000_003u32)
            .map(|i| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                if i % 5 == 0 { (x >> 24) as u8 } else { (i / 61 % 199) as u8 }
            })
            .collect();
        let part = NonZeroUsize::new(300_000);
        for level in [9, 11, 13] {
            let parts = Deflater::Libdeflater { compression: level, part }.deflate(&data, None).unwrap();
            assert_eq!(inflate(&parts, data.len()).unwrap(), data);
            let whole = deflate(&data, level, None).unwrap();
            assert_eq!(parts == whole, level < 10);
            assert!(parts.len() < whole.len() + whole.len() / 50);
            let short = Deflater::Libdeflater { compression: level, part }.deflate(&data[..500_000], None).unwrap();
            assert_eq!(short, deflate(&data[..500_000], level, None).unwrap());
        }
    }
}
