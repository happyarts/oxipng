use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};

use libdeflater::*;
#[cfg(feature = "parallel")]
use rayon::prelude::*;

#[cfg(not(feature = "parallel"))]
use crate::rayon::prelude::*;
use crate::{PngError, PngResult};

/// The window a part sees of the data before it.
const WINDOW: usize = 32768;

/// `data` as a zlib stream at `level`; see `Deflater::Libdeflater` for `part`.
pub fn deflate(
    data: &[u8],
    level: u8,
    part: Option<NonZeroUsize>,
    max_size: Option<usize>,
) -> PngResult<Vec<u8>> {
    if let Some(part) = part.map(NonZeroUsize::get)
        && level >= 10
        && data.len() >= 2 * part
    {
        return deflate_in_parts(data, level, part, max_size);
    }
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

/// The parts (as many as `part` fits in the data, of equal size, so none is
/// short) compressed at once, then written one after another between the
/// zlib header and the Adler-32 of the whole. Once the parts done pass
/// `max_size`, the others aren't started.
fn deflate_in_parts(data: &[u8], level: u8, part: usize, max_size: Option<usize>) -> PngResult<Vec<u8>> {
    let part = data.len().div_ceil(data.len() / part);
    let starts: Vec<usize> = (0..data.len()).step_by(part).collect();
    let max = max_size.unwrap_or(usize::MAX);
    let written = AtomicUsize::new(6);
    let parts: Vec<PngResult<Vec<u8>>> = starts
        .par_iter()
        .with_max_len(1)
        .map(|&start| {
            if written.load(Ordering::Relaxed) > max {
                return Err(PngError::DeflatedDataTooLong(max));
            }
            let end = (start + part).min(data.len());
            let window = start.min(WINDOW);
            let mut compressor = Compressor::new(CompressionLvl::new(level.into()).unwrap());
            let capacity = compressor.deflate_compress_bound(end - start) + 16;
            let mut dest = vec![0; capacity];
            let len = compressor
                .deflate_compress_part(&data[start - window..end], window, end == data.len(), &mut dest)
                .map_err(|_| PngError::DeflatedDataTooLong(capacity))?;
            dest.truncate(len);
            written.fetch_add(len, Ordering::Relaxed);
            Ok(dest)
        })
        .collect();
    // As libdeflate writes it from level 8 on: deflate, 32 KiB window, slowest.
    let mut dest = vec![0x78, 0xDA];
    for part in parts {
        dest.extend_from_slice(&part?);
        if let Some(max) = max_size
            && dest.len() + 4 > max
        {
            return Err(PngError::DeflatedDataTooLong(max));
        }
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
    use super::*;

    /// Data compressed in parts reads back as it was, also with a length
    /// the parts don't divide and at the level where the parse differs.
    #[test]
    fn parts_read_back() {
        let mut x: u32 = 7;
        let data: Vec<u8> = (0..1_000_003u32)
            .map(|i| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                if i % 5 == 0 { (x >> 24) as u8 } else { (i / 61 % 199) as u8 }
            })
            .collect();
        for level in [11, 13] {
            let part = NonZeroUsize::new(300_000);
            let parts = deflate(&data, level, part, None).unwrap();
            assert_eq!(inflate(&parts, data.len()).unwrap(), data);
            let whole = deflate(&data, level, None, None).unwrap();
            assert!(parts.len() < whole.len() + whole.len() / 50);
            // Below level 10 and below two parts, one piece.
            assert_eq!(deflate(&data, 9, part, None).unwrap(), deflate(&data, 9, None, None).unwrap());
            assert_eq!(deflate(&data[..500_000], level, part, None).unwrap(), deflate(&data[..500_000], level, None, None).unwrap());
        }
    }
}
