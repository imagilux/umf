//! Content-format fingerprinting by magic number.
//!
//! A small, Rust-native `file(1)` for the archive / image formats `ADD` accepts
//! as a source. [`detect`] is pure byte inspection (no subprocess, no
//! decompression, no external crate): a source string can only *suggest* a type
//! (a scheme, a `:tag`), so once the bytes are in hand we confirm what they
//! actually are. [`is_oci_layout`] goes one step further for the OCI-image case,
//! peeking a tar's entries for the `oci-layout` marker.

use std::io::Read;

use flate2::read::GzDecoder;

/// A recognised content format, by leading magic number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// gzip stream (`1f 8b`), usually wrapping a tar.
    Gzip,
    /// zstandard stream (`28 b5 2f fd`).
    Zstd,
    /// xz stream (`fd 37 7a 58 5a 00`).
    Xz,
    /// bzip2 stream (`BZh`).
    Bzip2,
    /// Uncompressed POSIX/ustar tar (the `ustar` magic at offset 257).
    Tar,
    /// SquashFS image (`hsqs` little-endian / `sqsh` big-endian superblock).
    Squashfs,
    /// No recognised archive/image magic — treat as a raw file.
    Unknown,
}

impl Format {
    /// `true` for the compression wrappers (gzip/zstd/xz/bzip2), which normally
    /// wrap a tar stream rather than being a filesystem themselves.
    #[must_use]
    pub fn is_compressed(self) -> bool {
        matches!(self, Self::Gzip | Self::Zstd | Self::Xz | Self::Bzip2)
    }

    /// Lower-case label for diagnostics / logging.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Zstd => "zstd",
            Self::Xz => "xz",
            Self::Bzip2 => "bzip2",
            Self::Tar => "tar",
            Self::Squashfs => "squashfs",
            Self::Unknown => "unknown",
        }
    }
}

/// Classify `bytes` by magic number. Inspects only a short prefix; pass at least
/// the first 512 bytes for reliable tar detection (the `ustar` magic lives at
/// offset 257). Never decompresses, so a gzipped tar reports [`Format::Gzip`],
/// not [`Format::Tar`].
#[must_use]
pub fn detect(bytes: &[u8]) -> Format {
    const ZSTD: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
    const XZ: [u8; 6] = [0xFD, b'7', b'z', b'X', b'Z', 0x00];

    if bytes.starts_with(&[0x1F, 0x8B]) {
        Format::Gzip
    } else if bytes.starts_with(&ZSTD) {
        Format::Zstd
    } else if bytes.starts_with(&XZ) {
        Format::Xz
    } else if bytes.starts_with(b"BZh") {
        Format::Bzip2
    } else if bytes.starts_with(b"hsqs") || bytes.starts_with(b"sqsh") {
        Format::Squashfs
    } else if is_tar(bytes) {
        Format::Tar
    } else {
        Format::Unknown
    }
}

/// POSIX/ustar tar carries the `ustar` magic at byte offset 257.
fn is_tar(bytes: &[u8]) -> bool {
    const OFFSET: usize = 257;
    bytes.len() >= OFFSET + 5 && &bytes[OFFSET..OFFSET + 5] == b"ustar"
}

/// `true` if `bytes` is a tar archive (optionally gzip-compressed) laid out as
/// an **OCI image** — an `oci-layout` marker at the root, per the OCI Image
/// Layout spec — rather than a plain filesystem tarball. This is how an
/// `ADD`-ed blob that is really an OCI image export (`docker save` /
/// `skopeo copy oci-archive:`) is told apart from an ordinary rootfs tarball.
///
/// Only gzip and uncompressed tar are inspected (the formats those tools emit);
/// other compressions return `false`, and the caller treats the blob as an
/// opaque archive.
#[must_use]
pub fn is_oci_layout(bytes: &[u8]) -> bool {
    fn scan<R: Read>(reader: R) -> bool {
        let mut archive = tar::Archive::new(reader);
        let Ok(entries) = archive.entries() else {
            return false;
        };
        for entry in entries.flatten() {
            if let Ok(path) = entry.path() {
                let p = path.to_string_lossy();
                if p.trim_start_matches("./").trim_end_matches('/') == "oci-layout" {
                    return true;
                }
            }
        }
        false
    }

    match detect(bytes) {
        Format::Gzip => scan(GzDecoder::new(bytes)),
        Format::Tar => scan(bytes),
        _ => false,
    }
}

#[cfg(test)]
mod tests;
