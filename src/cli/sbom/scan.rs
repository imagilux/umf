//! Read an installed-package inventory from a materialized image rootfs.
//!
//! Each supported package manager keeps a plain-text database at a well-known
//! path; [`scan_rootfs`] tries them in turn and parses whichever is present
//! (an image normally has exactly one). The result feeds the SBOM document
//! builders in [`super::generate`]. Text databases only for now — dpkg, apk,
//! and pacman; the rpm sqlite database lands in a follow-up.

use std::path::Path;

/// One installed package, distilled to the fields an SBOM needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Package {
    /// Package name.
    pub(crate) name: String,
    /// Version string, verbatim from the package database.
    pub(crate) version: String,
    /// Architecture, when the database records one.
    pub(crate) arch: Option<String>,
    /// Package URL (purl), when one can be formed.
    pub(crate) purl: Option<String>,
}

/// A rootfs package inventory plus the distro id it was read from.
pub(crate) struct Inventory {
    /// `/etc/os-release` `ID` (e.g. `debian`, `alpine`), best-effort.
    pub(crate) os_id: Option<String>,
    /// Installed packages, sorted by (name, version) and de-duplicated so the
    /// emitted SBOM is byte-reproducible.
    pub(crate) packages: Vec<Package>,
}

/// Scan `root` for a recognised package database and return its inventory.
///
/// Tries dpkg, then apk, then pacman; the first present database wins. An
/// image with no recognised database yields an empty package list (still a
/// valid, if uninformative, SBOM) rather than an error.
pub(crate) fn scan_rootfs(root: &Path) -> std::io::Result<Inventory> {
    let os_id = read_os_id(root);

    let dpkg = root.join("var/lib/dpkg/status");
    if dpkg.is_file() {
        let body = std::fs::read_to_string(&dpkg)?;
        let ns = os_id.as_deref().unwrap_or("debian").to_string();
        let packages = parse_dpkg(&body, &ns);
        return Ok(finish(os_id, packages));
    }
    let apk = root.join("lib/apk/db/installed");
    if apk.is_file() {
        let body = std::fs::read_to_string(&apk)?;
        let ns = os_id.as_deref().unwrap_or("alpine").to_string();
        let packages = parse_apk(&body, &ns);
        return Ok(finish(os_id, packages));
    }
    let pacman = root.join("var/lib/pacman/local");
    if pacman.is_dir() {
        return Ok(finish(os_id, parse_pacman(&pacman)?));
    }
    // rpm: the modern sqlite rpmdb. Newer distros keep it under
    // /usr/lib/sysimage/rpm; older ones under /var/lib/rpm (often a symlink to
    // the former). The legacy Berkeley-DB `Packages` file is not read.
    for rpmdb in [
        "usr/lib/sysimage/rpm/rpmdb.sqlite",
        "var/lib/rpm/rpmdb.sqlite",
    ] {
        let path = root.join(rpmdb);
        if path.is_file() {
            let ns = os_id.as_deref().unwrap_or("redhat").to_string();
            return Ok(finish(os_id, rpm::parse_rpmdb(&path, &ns)?));
        }
    }
    Ok(Inventory {
        os_id,
        packages: Vec::new(),
    })
}

/// Sort + dedup the package list for reproducibility and bundle it with the
/// distro id.
fn finish(os_id: Option<String>, mut packages: Vec<Package>) -> Inventory {
    packages.sort_by(|a, b| {
        (a.name.as_str(), a.version.as_str()).cmp(&(b.name.as_str(), b.version.as_str()))
    });
    packages.dedup();
    Inventory { os_id, packages }
}

/// `/etc/os-release` `ID=...` (quotes stripped), best-effort.
fn read_os_id(root: &Path) -> Option<String> {
    let body = std::fs::read_to_string(root.join("etc/os-release")).ok()?;
    body.lines().find_map(|line| {
        line.strip_prefix("ID=")
            .map(|v| v.trim().trim_matches('"').to_string())
    })
}

/// Parse a dpkg `status` file (deb822: blank-line-separated paragraphs).
/// Only entries whose `Status` ends in `installed` count — a removed package
/// can linger with config files but is not actually present.
fn parse_dpkg(body: &str, os_ns: &str) -> Vec<Package> {
    let mut out = Vec::new();
    for para in body.split("\n\n") {
        let (mut name, mut version, mut arch, mut installed) = (None, None, None, false);
        for line in para.lines() {
            if let Some(v) = line.strip_prefix("Package:") {
                name = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("Version:") {
                version = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("Architecture:") {
                arch = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("Status:") {
                installed = v.trim().ends_with("installed");
            }
        }
        if let (Some(name), Some(version)) = (name, version)
            && installed
        {
            let purl = Some(purl("deb", os_ns, &name, &version, arch.as_deref()));
            out.push(Package {
                name,
                version,
                arch,
                purl,
            });
        }
    }
    out
}

/// Parse an apk `installed` database (blank-line-separated records, one
/// `<letter>:<value>` per line; `P` name, `V` version, `A` arch).
fn parse_apk(body: &str, os_ns: &str) -> Vec<Package> {
    let mut out = Vec::new();
    for rec in body.split("\n\n") {
        let (mut name, mut version, mut arch) = (None, None, None);
        for line in rec.lines() {
            match line.split_once(':') {
                Some(("P", v)) => name = Some(v.trim().to_string()),
                Some(("V", v)) => version = Some(v.trim().to_string()),
                Some(("A", v)) => arch = Some(v.trim().to_string()),
                _ => {}
            }
        }
        if let (Some(name), Some(version)) = (name, version) {
            let purl = Some(purl("apk", os_ns, &name, &version, arch.as_deref()));
            out.push(Package {
                name,
                version,
                arch,
                purl,
            });
        }
    }
    out
}

/// Parse a pacman local database: `var/lib/pacman/local/<pkg>/desc`, each a
/// sectioned file with `%NAME%` / `%VERSION%` / `%ARCH%` headers followed by
/// their value on the next line.
fn parse_pacman(dir: &Path) -> std::io::Result<Vec<Package>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let desc = entry?.path().join("desc");
        if !desc.is_file() {
            continue;
        }
        let body = std::fs::read_to_string(&desc)?;
        let (name, version, arch) = parse_pacman_desc(&body);
        if let (Some(name), Some(version)) = (name, version) {
            // pacman maps to the purl `alpm` (Arch Linux package manager) type.
            let purl = Some(purl("alpm", "arch", &name, &version, arch.as_deref()));
            out.push(Package {
                name,
                version,
                arch,
                purl,
            });
        }
    }
    Ok(out)
}

/// Pull `%NAME%` / `%VERSION%` / `%ARCH%` out of a pacman `desc` file. Each
/// header sits on its own line with the value on the following line.
fn parse_pacman_desc(body: &str) -> (Option<String>, Option<String>, Option<String>) {
    let (mut name, mut version, mut arch) = (None, None, None);
    let mut lines = body.lines();
    while let Some(header) = lines.next() {
        match header.trim() {
            "%NAME%" => name = lines.next().map(|s| s.trim().to_string()),
            "%VERSION%" => version = lines.next().map(|s| s.trim().to_string()),
            "%ARCH%" => arch = lines.next().map(|s| s.trim().to_string()),
            _ => {}
        }
    }
    (name, version, arch)
}

/// Build a Package URL (purl) of the given type and namespace, appending the
/// architecture qualifier when known.
fn purl(ty: &str, namespace: &str, name: &str, version: &str, arch: Option<&str>) -> String {
    let mut p = format!("pkg:{ty}/{namespace}/{name}@{version}");
    if let Some(a) = arch {
        p.push_str("?arch=");
        p.push_str(a);
    }
    p
}

mod rpm;

#[cfg(test)]
mod tests;
