mod deflater;
use std::{fmt, fmt::Display, num::NonZeroUsize};

pub use deflater::{crc32, deflate, deflate_in_parts, inflate};

use crate::{PngError, PngResult};

#[cfg(feature = "zopfli")]
mod zopfli_oxipng;
#[cfg(feature = "zopfli")]
pub use zopfli::Options as ZopfliOptions;
#[cfg(feature = "zopfli")]
pub use zopfli_oxipng::deflate as zopfli_deflate;

/// DEFLATE algorithms supported by oxipng (for use in [`Options`][crate::Options])
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deflater {
    /// Use libdeflater.
    Libdeflater {
        /// Which compression level to use on the file (0-14)
        compression: u8,
        /// From level 10 on, data of at least two parts is compressed in
        /// parts of about this many bytes at once, each with the data
        /// before it as its window: one stream all the same, slightly
        /// different.
        part: Option<NonZeroUsize>,
    },
    #[cfg(feature = "zopfli")]
    /// Use the better but slower Zopfli implementation
    Zopfli(ZopfliOptions),
}

impl Deflater {
    pub(crate) fn deflate(self, data: &[u8], max_size: Option<usize>) -> PngResult<Vec<u8>> {
        let compressed = match self {
            Self::Libdeflater { compression, part: Some(part) } if compression >= 10 && data.len() >= 2 * part.get() => {
                deflate_in_parts(data, compression, part.get(), max_size)?
            }
            Self::Libdeflater { compression, .. } => deflate(data, compression, max_size)?,
            #[cfg(feature = "zopfli")]
            Self::Zopfli(options) => zopfli_deflate(data, options)?,
        };
        if let Some(max) = max_size
            && compressed.len() > max
        {
            return Err(PngError::DeflatedDataTooLong(max));
        }
        Ok(compressed)
    }
}

impl Display for Deflater {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Libdeflater { compression, part: None } => write!(f, "zc = {compression}"),
            Self::Libdeflater { compression, part: Some(part) } => {
                write!(f, "zc = {compression}, parts of {} MiB", part.get() >> 20)
            }
            #[cfg(feature = "zopfli")]
            Self::Zopfli(options) => write!(f, "zopfli, zi = {}", options.iteration_count),
        }
    }
}
