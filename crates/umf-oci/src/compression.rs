//! Sniff-and-decode for (possibly compressed) tar streams.
//!
//! One place decides how a payload's leading magic maps to a decoder. Before
//! this module the decision was made twice — once in [`crate::materialize`]
//! for layer blobs, once in [`crate::staging`] for fetched `ADD <url>`
//! payloads — and the two had already drifted: layers understood gzip and
//! zstd, staging additionally understood xz and bzip2. A layer compressed
//! with a codec staging could read was rejected on the layer path for no
//! reason a caller could have predicted.
//!
//! [`crate::format`] stays pure byte inspection; this module owns the
//! decoders and the decompression ceiling that guards them.

use std::io::{self, Read};

use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use liblzma::read::XzDecoder;
use thiserror::Error;

use crate::format::{self, Format};
use crate::materialize::{CappedReader, max_uncompressed_layer_bytes, read_full};

/// Why a payload could not be turned into a tar byte stream.
#[derive(Debug, Error)]
pub enum DecodeError {
    /// The payload is a recognised format that is not, and cannot contain, a
    /// tar stream — a zip (a seekable container with its own index) or a
    /// squashfs filesystem image.
    #[error(
        "`{0}` payload cannot be read as a tar stream (expected plain tar, or \
         a gzip/zstd/xz/bzip2-compressed tar)"
    )]
    NotATar(&'static str),

    /// Constructing the decoder failed (a malformed zstd frame header, …).
    #[error("initialising the {format} decoder: {source}")]
    Decoder {
        /// The codec whose decoder failed to start.
        format: &'static str,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },

    /// Reading the payload's leading magic failed.
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
}

/// Classify `source` by its leading magic and wrap it in the matching
/// decoder, returning the format alongside a reader that yields plain tar
/// bytes.
///
/// The returned reader is capped at [`max_uncompressed_layer_bytes`] so a
/// decompression bomb aborts the read rather than filling the disk — the cap
/// applies to every codec, including the uncompressed case, so the guarantee
/// does not depend on which branch was taken.
///
/// An uncompressed tar is returned as-is: "no compression" is a case this
/// function handles, not one the caller has to special-case.
pub fn decode_tar_stream<'a, R: Read + 'a>(
    source: R,
) -> Result<(Format, Box<dyn Read + 'a>), DecodeError> {
    decode_tar_stream_capped(source, max_uncompressed_layer_bytes())
}

/// [`decode_tar_stream`] with an explicit ceiling, for tests that need to
/// drive the cap without touching a process-wide environment variable.
pub fn decode_tar_stream_capped<'a, R: Read + 'a>(
    source: R,
    cap: u64,
) -> Result<(Format, Box<dyn Read + 'a>), DecodeError> {
    // Peek a full tar block. The compression magics all sit in the first six
    // bytes, but `ustar` is at offset 257 — peeking only the shorter prefix
    // would make an uncompressed tar indistinguishable from arbitrary bytes
    // and report it as `Unknown`. Reading 512 costs nothing and lets the
    // returned `Format` be honest about what the payload is.
    //
    // The peek is copied into an owned cursor rather than chained from a
    // stack buffer so the composed reader can outlive this frame.
    let mut reader = io::BufReader::new(source);
    let mut peek = [0u8; 512];
    let peeked = read_full(&mut reader, &mut peek)?;
    let format = format::detect(&peek[..peeked]);
    let combined = io::Cursor::new(peek[..peeked].to_vec()).chain(reader);

    let decoded: Box<dyn Read + 'a> = match format {
        Format::Gzip => Box::new(CappedReader::new(GzDecoder::new(combined), cap)),
        Format::Zstd => {
            let decoder = zstd::stream::read::Decoder::new(combined).map_err(|source| {
                DecodeError::Decoder {
                    format: "zstd",
                    source,
                }
            })?;
            Box::new(CappedReader::new(decoder, cap))
        }
        Format::Xz => Box::new(CappedReader::new(XzDecoder::new(combined), cap)),
        Format::Bzip2 => Box::new(CappedReader::new(BzDecoder::new(combined), cap)),
        // Not tar streams, and no decoder would make them one.
        Format::Zip => return Err(DecodeError::NotATar("zip")),
        Format::Squashfs => return Err(DecodeError::NotATar("squashfs")),
        // `Unknown` covers a headerless or truncated payload; passing it
        // through lets the tar reader produce the real diagnostic rather than
        // this layer guessing.
        Format::Tar | Format::Unknown => Box::new(CappedReader::new(combined, cap)),
    };
    Ok((format, decoded))
}

#[cfg(test)]
mod tests;
