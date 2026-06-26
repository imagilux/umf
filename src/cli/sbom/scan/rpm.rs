//! Read the modern sqlite rpmdb (`rpmdb.sqlite`).
//!
//! Recent rpm (Fedora 33+, RHEL 9+, openSUSE Leap 15.4+) keeps its database
//! as sqlite: a `Packages` table of `(hnum, blob)` rows whose `blob` is a
//! verbatim rpm *header* structure. We read each blob and pull the name,
//! version, release, epoch, and arch tags out of it. The legacy Berkeley-DB
//! `Packages` file (older RHEL/CentOS) is a different, much heavier format and
//! is not read here.
//!
//! The header parser treats the blob as untrusted (it comes from an image
//! layer): every offset is bounds-checked and a malformed entry yields `None`
//! rather than a panic.

use std::path::Path;

use super::Package;

// rpm tag numbers (rpmtag.h).
const TAG_NAME: u32 = 1000;
const TAG_VERSION: u32 = 1001;
const TAG_RELEASE: u32 = 1002;
const TAG_EPOCH: u32 = 1003;
const TAG_ARCH: u32 = 1022;
// rpm tag data types (rpmtd.h) we care about.
const TYPE_INT32: u32 = 4;
const TYPE_STRING: u32 = 6;
/// The 8-byte rpm header magic some exporters prepend (`8e ad e8 01` + 4
/// reserved). `headerExport` omits it; we skip it when present.
const HEADER_MAGIC: [u8; 4] = [0x8e, 0xad, 0xe8, 0x01];

/// Open the sqlite rpmdb read-only and parse every package header in it.
pub(super) fn parse_rpmdb(sqlite_path: &Path, os_ns: &str) -> std::io::Result<Vec<Package>> {
    let conn = rusqlite::Connection::open_with_flags(
        sqlite_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(std::io::Error::other)?;
    let mut stmt = conn
        .prepare("SELECT blob FROM Packages")
        .map_err(std::io::Error::other)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .map_err(std::io::Error::other)?;
    let mut out = Vec::new();
    for row in rows {
        let blob = row.map_err(std::io::Error::other)?;
        // A non-package row (e.g. a stray header without a NAME) parses to
        // None and is simply skipped.
        if let Some(pkg) = parse_header(&blob, os_ns) {
            out.push(pkg);
        }
    }
    Ok(out)
}

/// Index entry of interest, captured during the single index pass.
struct RawTag {
    ty: u32,
    offset: usize,
}

/// Parse one rpm header blob into a [`Package`]. Returns `None` if the blob is
/// malformed or carries no `NAME` tag.
fn parse_header(blob: &[u8], os_ns: &str) -> Option<Package> {
    // Optional 8-byte magic, then the index/data lengths.
    let mut pos = 0usize;
    if blob.get(0..4) == Some(&HEADER_MAGIC) {
        pos = 8;
    }
    let nindex = be_u32(blob, pos)? as usize;
    let dlen = be_u32(blob, pos + 4)? as usize;
    let index_start = pos.checked_add(8)?;
    let data_start = index_start.checked_add(nindex.checked_mul(16)?)?;
    let data_end = data_start.checked_add(dlen)?;
    if data_end > blob.len() {
        return None;
    }
    let data = &blob[data_start..data_end];

    let (mut name, mut version, mut release, mut arch, mut epoch_tag) =
        (None, None, None, None, None);
    for i in 0..nindex {
        let base = index_start + i * 16;
        let tag = be_u32(blob, base)?;
        let ty = be_u32(blob, base + 4)?;
        let offset = be_u32(blob, base + 8)? as usize;
        let raw = RawTag { ty, offset };
        match tag {
            TAG_NAME => name = string_tag(data, &raw),
            TAG_VERSION => version = string_tag(data, &raw),
            TAG_RELEASE => release = string_tag(data, &raw),
            TAG_ARCH => arch = string_tag(data, &raw),
            TAG_EPOCH => epoch_tag = Some(raw),
            _ => {}
        }
    }

    let name = name?;
    let version = version.unwrap_or_default();
    let release = release.unwrap_or_default();
    let epoch = epoch_tag
        .filter(|t| t.ty == TYPE_INT32)
        .and_then(|t| be_u32(data, t.offset))
        .filter(|e| *e > 0);

    // SBOM version-release, with the epoch prefixed when present.
    let vr = if release.is_empty() {
        version
    } else {
        format!("{version}-{release}")
    };
    let full_version = match epoch {
        Some(e) => format!("{e}:{vr}"),
        None => vr.clone(),
    };
    let purl = Some(rpm_purl(os_ns, &name, &vr, arch.as_deref(), epoch));
    Some(Package {
        name,
        version: full_version,
        arch,
        purl,
    })
}

/// Read a NUL-terminated string tag from the data store.
fn string_tag(data: &[u8], tag: &RawTag) -> Option<String> {
    if tag.ty != TYPE_STRING {
        return None;
    }
    let slice = data.get(tag.offset..)?;
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    std::str::from_utf8(&slice[..end]).ok().map(str::to_string)
}

/// Big-endian `u32` at `offset`, bounds-checked.
fn be_u32(buf: &[u8], offset: usize) -> Option<u32> {
    let b = buf.get(offset..offset + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// `pkg:rpm/<ns>/<name>@<version-release>` with `arch` / `epoch` qualifiers.
fn rpm_purl(os: &str, name: &str, vr: &str, arch: Option<&str>, epoch: Option<u32>) -> String {
    let mut purl = format!("pkg:rpm/{os}/{name}@{vr}");
    let mut qualifiers = Vec::new();
    if let Some(a) = arch {
        qualifiers.push(format!("arch={a}"));
    }
    if let Some(e) = epoch {
        qualifiers.push(format!("epoch={e}"));
    }
    if !qualifiers.is_empty() {
        purl.push('?');
        purl.push_str(&qualifiers.join("&"));
    }
    purl
}

#[cfg(test)]
mod tests;
