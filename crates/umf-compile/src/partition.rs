//! Low-level disk-format layer: GPT table, ESP (FAT32) population, ROOTFS
//! partition write, and the [`PartitionView`] windowed Read/Write/Seek adapter
//! that lets [`fatfs`] and the squashfs writer see a partition slice of the
//! disk file as a standalone device starting at offset 0.
//!
//! All projection is **userspace** — the `gpt` crate writes the table, `fatfs`
//! formats and populates the ESP, and `backhand` packs the rootfs. No loop
//! devices, no `mkfs` subprocess, no privilege. [`project_disk`] is the single
//! entry point; it takes [`DiskInputs`] (geometry + a materialized rootfs +
//! the kernel/initrd/cmdline) and never references the builder's internals.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use fatfs::{FatType, FileSystem, FormatVolumeOptions, FsOptions};
use gpt::mbr::ProtectiveMBR;
use gpt::{GptConfig, partition_types};
use tracing::{debug, info};
use umf_core::architecture::Architecture;
use umf_core::boot::{ROOTFS_FSTYPE, ROOTFS_PARTLABEL};

use crate::error::CompileError;
use crate::filesystem::write_squashfs_from_dir;

// ── Constants ───────────────────────────────────────────────────────────────

/// Logical block (sector) size the GPT layout is computed in.
pub(crate) const LOGICAL_BLOCK_SIZE: u64 = 512;

/// Reserved sectors per GPT header + partition table (in 512-byte LBAs).
/// LBA 0 is the protective MBR, LBA 1 the primary header, LBA 2..=33 the
/// primary entry array, mirrored at end-of-disk for the backup. ~17 KiB/side.
const GPT_RESERVED_LBA: u64 = 34;

/// Minimum disk-too-small threshold for the ROOTFS partition — just large
/// enough to fit a SquashFS header + a small rootfs (~32 MiB).
const DEFAULT_ROOTFS_MIN_SIZE_BYTES: u64 = 32 * 1024 * 1024;

/// Default total sparse disk-image size (2 GiB) — the size used when the
/// caller doesn't override geometry. The image is sparse, so this is an upper
/// bound, not the bytes actually written.
pub const DEFAULT_DISK_SIZE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Default ESP partition size (500 MiB) — comfortably holds a kernel, an
/// initramfs, and a bootloader / UKI.
pub const DEFAULT_ESP_SIZE_BYTES: u64 = 500 * 1024 * 1024;

/// FAT volume label written into the ESP. FAT32 labels are exactly 11 bytes
/// (space-padded).
const ESP_VOLUME_LABEL: &[u8; 11] = b"UMF-ESP    ";

/// systemd-boot entry filename emitted under `<ESP>/loader/entries/`.
/// `loader.conf`'s `default` key references this name (minus `.conf`).
const LOADER_ENTRY_NAME: &str = "umf.conf";

/// `loader/loader.conf` — pick the UMF entry by default with a short
/// countdown. Every bootable build emits exactly one boot entry (the kernel
/// from FROM), so the always-with-entry form is the only variant needed.
const DEFAULT_LOADER_CONF: &str = "\
# loader.conf — installed by the UMF projector.
default umf
timeout 3
console-mode max
";

// ── Public projection API ─────────────────────────────────────────────────────

/// Disk + ESP geometry for a projection.
#[derive(Debug, Clone, Copy)]
pub struct DiskGeometry {
    /// Total sparse disk image size, in bytes.
    pub disk_size_bytes: u64,
    /// ESP partition size, in bytes.
    pub esp_size_bytes: u64,
}

impl Default for DiskGeometry {
    /// The conventional geometry: [`DEFAULT_DISK_SIZE_BYTES`] /
    /// [`DEFAULT_ESP_SIZE_BYTES`].
    fn default() -> Self {
        Self {
            disk_size_bytes: DEFAULT_DISK_SIZE_BYTES,
            esp_size_bytes: DEFAULT_ESP_SIZE_BYTES,
        }
    }
}

impl DiskGeometry {
    /// Block-cache variant key for this geometry: the disk and ESP sizes packed
    /// as two zero-padded 16-hex-digit fields. Two compiles with the same source
    /// image but different geometry land in distinct cache slots; identical
    /// geometry hits the same slot. The CLI uses this instead of hand-rolling
    /// the format string at each call site.
    pub fn cache_variant(&self) -> String {
        format!("{:016x}{:016x}", self.disk_size_bytes, self.esp_size_bytes)
    }
}

/// Inputs to project a bootable-OS rootfs + kernel into a GPT/ESP disk image.
/// Borrowed throughout — the projector reads, never owns.
pub struct DiskInputs<'a> {
    /// Disk + ESP geometry.
    pub geometry: DiskGeometry,
    /// Materialized rootfs directory written to the ROOTFS partition.
    pub rootfs_dir: &'a Path,
    /// Kernel image (vmlinuz) copied onto the ESP / wrapped into the UKI.
    pub vmlinuz: &'a Path,
    /// Kernel release string (UKI `--uname`; the loader entry's `version`).
    pub kernel_release: &'a str,
    /// Bootloader `.efi` to install at the ESP fallback path; `None` ⇒ UKI.
    pub bootloader_efi: Option<&'a [u8]>,
    /// Generated initramfs `(bytes, filename)`; `None` for the appliance shape.
    pub initrd: Option<(&'a [u8], &'a str)>,
    /// Target architecture (selects the UEFI fallback filename).
    pub architecture: Architecture,
    /// Extra kernel-cmdline tokens — the appliance `init=<path> [-- args]`
    /// fragment; empty for init-system builds.
    pub extra_cmdline: &'a str,
}

/// Byte offsets/sizes [`project_disk`] laid down, for inspection + reporting.
#[derive(Debug, Clone, Copy)]
pub struct DiskProjection {
    /// Total disk image size.
    pub disk_size_bytes: u64,
    /// ESP partition start offset.
    pub esp_start_bytes: u64,
    /// ESP partition size.
    pub esp_size_bytes: u64,
    /// ROOTFS partition start offset.
    pub rootfs_start_bytes: u64,
    /// ROOTFS partition size.
    pub rootfs_size_bytes: u64,
}

/// Project `inputs` into a bootable GPT disk image at `disk_path`: lay down the
/// protective MBR + GPT (ESP + ROOTFS), populate the ESP (a classic loader
/// entry or a UKI), and write the rootfs partition as SquashFS.
pub fn project_disk(
    disk_path: &Path,
    inputs: &DiskInputs<'_>,
) -> Result<DiskProjection, CompileError> {
    let plan = compute_disk_plan(&inputs.geometry)?;
    let layout = write_gpt(disk_path, &plan)?;

    let esp_start = layout.esp_first_lba * LOGICAL_BLOCK_SIZE;
    let esp_size = layout.esp_size_lba * LOGICAL_BLOCK_SIZE;
    populate_esp(
        disk_path,
        esp_start,
        esp_size,
        inputs.bootloader_efi,
        inputs.vmlinuz,
        inputs.kernel_release,
        inputs.initrd,
        inputs.architecture,
        inputs.extra_cmdline,
    )?;

    let rootfs_start = layout.rootfs_first_lba * LOGICAL_BLOCK_SIZE;
    let rootfs_size = layout.rootfs_size_lba * LOGICAL_BLOCK_SIZE;
    write_rootfs_partition(disk_path, rootfs_start, rootfs_size, inputs.rootfs_dir)?;

    Ok(DiskProjection {
        disk_size_bytes: plan.disk_size_bytes,
        esp_start_bytes: esp_start,
        esp_size_bytes: esp_size,
        rootfs_start_bytes: rootfs_start,
        rootfs_size_bytes: rootfs_size,
    })
}

// ── Disk plan ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub(crate) struct DiskPlan {
    pub(crate) disk_size_bytes: u64,
    pub(crate) esp_size_bytes: u64,
}

pub(crate) fn compute_disk_plan(geom: &DiskGeometry) -> Result<DiskPlan, CompileError> {
    // `esp_size_bytes` is a raw u64 from a CLI flag; an absurd `--esp-size`
    // would overflow this sum (debug panic; release wrap to a tiny `required`
    // that the `<` guard below would then wave through). Compute it checked and
    // treat overflow as "the disk can never be large enough".
    let required = GPT_RESERVED_LBA
        .checked_mul(2 * LOGICAL_BLOCK_SIZE)
        .and_then(|gpt_overhead| geom.esp_size_bytes.checked_add(gpt_overhead))
        .ok_or(CompileError::DiskTooSmall {
            disk: geom.disk_size_bytes,
            required: u64::MAX,
        })?;
    if geom.disk_size_bytes < required {
        return Err(CompileError::DiskTooSmall {
            disk: geom.disk_size_bytes,
            required,
        });
    }
    Ok(DiskPlan {
        disk_size_bytes: geom.disk_size_bytes,
        esp_size_bytes: geom.esp_size_bytes,
    })
}

/// Validate that `geom` can hold the ESP + GPT overhead — a cheap pre-flight so
/// callers can reject an undersized disk before expensive resolution/staging.
pub fn validate_geometry(geom: &DiskGeometry) -> Result<(), CompileError> {
    compute_disk_plan(geom).map(|_| ())
}

// ── GPT emission ────────────────────────────────────────────────────────────

/// Where each partition ended up after [`write_gpt`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct PartitionLayout {
    pub(crate) esp_first_lba: u64,
    pub(crate) esp_size_lba: u64,
    pub(crate) rootfs_first_lba: u64,
    pub(crate) rootfs_size_lba: u64,
}

/// Create the disk file with the requested sparse length and lay down a
/// protective MBR (LBA 0), GPT primary header + table, the ESP, and a ROOTFS
/// partition filling the remaining space up to the GPT backup. The `gpt` crate
/// handles backup-header placement at end-of-disk.
pub(crate) fn write_gpt(
    disk_path: &Path,
    plan: &DiskPlan,
) -> Result<PartitionLayout, CompileError> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(disk_path)?;
    file.set_len(plan.disk_size_bytes)?;

    // Protective MBR at LBA 0. Without this, every BIOS-aware tool — and most
    // UEFI firmware — refuses to recognise the disk as GPT. The PMBR spans
    // (total_lba - 1) sectors after itself, capped at u32::MAX.
    let total_lba = plan.disk_size_bytes / LOGICAL_BLOCK_SIZE;
    let mbr_size_lba = u32::try_from(total_lba.saturating_sub(1)).unwrap_or(u32::MAX);
    let mbr = ProtectiveMBR::with_lb_size(mbr_size_lba);
    mbr.overwrite_lba0(&mut file)?;

    // Hand the file to the gpt crate by value — `write()` returns it back.
    let mut disk = GptConfig::new()
        .writable(true)
        .change_partition_count(true)
        .create_from_device(file, None)?;

    let esp_id = disk.add_partition("ESP", plan.esp_size_bytes, partition_types::EFI, 0, None)?;
    let esp = disk
        .partitions()
        .get(&esp_id)
        .ok_or_else(|| io::Error::other("gpt crate did not record the ESP it just added"))?
        .clone();
    let esp_first_lba = esp.first_lba;
    let esp_size_lba = esp.last_lba - esp.first_lba + 1;
    debug!(
        first_lba = esp_first_lba,
        size_lba = esp_size_lba,
        "ESP placed by GPT allocator",
    );

    // Use whatever the GPT allocator says is free after the ESP, minus the
    // 33-sector cushion the backup needs at end-of-disk. `find_free_sectors`
    // reports `(start, length_in_lba)` sorted by `start`; take the largest.
    let too_small = || CompileError::DiskTooSmall {
        disk: plan.disk_size_bytes,
        // `esp_size_bytes` is a raw u64 from a CLI flag; saturate rather than
        // wrap so the reported minimum stays an upper-bound sentinel, never a
        // small wrapped value.
        required: plan
            .esp_size_bytes
            .saturating_add(DEFAULT_ROOTFS_MIN_SIZE_BYTES),
    };
    let free = disk.find_free_sectors();
    let largest = free
        .into_iter()
        .max_by_key(|(_, len)| *len)
        .ok_or_else(too_small)?;
    // `largest.1` is an LBA count from the GPT allocator; multiplying by the
    // sector size can overflow on a pathological geometry, so do it checked and
    // fall back to the disk-too-small path.
    let rootfs_bytes = largest
        .1
        .checked_mul(LOGICAL_BLOCK_SIZE)
        .ok_or_else(too_small)?;
    if rootfs_bytes < DEFAULT_ROOTFS_MIN_SIZE_BYTES {
        return Err(too_small());
    }
    let rootfs_id = disk.add_partition(
        ROOTFS_PARTLABEL,
        rootfs_bytes,
        partition_types::LINUX_FS,
        0,
        None,
    )?;
    let rootfs = disk
        .partitions()
        .get(&rootfs_id)
        .ok_or_else(|| io::Error::other("gpt crate did not record the ROOTFS partition"))?
        .clone();
    let rootfs_first_lba = rootfs.first_lba;
    let rootfs_size_lba = rootfs.last_lba - rootfs.first_lba + 1;
    debug!(
        first_lba = rootfs_first_lba,
        size_lba = rootfs_size_lba,
        "ROOTFS placed by GPT allocator",
    );

    disk.write()?;
    Ok(PartitionLayout {
        esp_first_lba,
        esp_size_lba,
        rootfs_first_lba,
        rootfs_size_lba,
    })
}

// ── ESP install ─────────────────────────────────────────────────────────────

/// Format the ESP as FAT32 and populate it. With a bootloader: install it at
/// the architecture's UEFI fallback path, copy `vmlinuz` + the initramfs into
/// the ESP root, and emit the systemd-boot `loader/loader.conf` +
/// `loader/entries/umf.conf`. Without one (`bootloader_efi == None`): wrap the
/// kernel + initramfs + cmdline into a single UKI at the fallback path.
#[allow(clippy::too_many_arguments)]
pub(crate) fn populate_esp(
    disk_path: &Path,
    esp_start_bytes: u64,
    esp_size_bytes: u64,
    bootloader_efi: Option<&[u8]>,
    vmlinuz: &Path,
    kernel_release: &str,
    initrd: Option<(&[u8], &str)>,
    architecture: Architecture,
    extra_cmdline: &str,
) -> Result<(), CompileError> {
    let file = OpenOptions::new().read(true).write(true).open(disk_path)?;
    let mut view = PartitionView::new(file, esp_start_bytes, esp_size_bytes, "ESP");
    view.seek(SeekFrom::Start(0))?;
    fatfs::format_volume(
        &mut view,
        FormatVolumeOptions::new()
            .fat_type(FatType::Fat32)
            .volume_label(*ESP_VOLUME_LABEL),
    )?;
    view.seek(SeekFrom::Start(0))?;

    let fs = FileSystem::new(view, FsOptions::new())?;

    // The kernel command line is shared by both packaging paths: the `options`
    // line of the classic loader entry, and the `.cmdline` baked into the UKI.
    let cmdline = boot_cmdline(extra_cmdline, architecture);

    {
        let root = fs.root_dir();
        let efi = root.create_dir("EFI")?;
        let boot = efi.create_dir("BOOT")?;
        let fallback = architecture.uefi_fallback_filename();

        match bootloader_efi {
            // ── UKI (flavor=uki): no bootloader. Wrap the kernel +
            //    initramfs + cmdline in a single systemd-stub `.efi` the
            //    firmware boots directly from the ESP fallback path. ──
            None => {
                let uki = crate::uki::build_uki(
                    vmlinuz,
                    initrd.map(|(b, _)| b),
                    &cmdline,
                    kernel_release,
                    architecture,
                )?;
                let mut bootx = boot.create_file(fallback)?;
                bootx.write_all(&uki)?;
                bootx.flush()?;
                // No loader/ tree and no loose vmlinuz/initrd — the UKI is
                // self-contained.
            }

            // ── Classic (BOOTLOADER systemd-boot/grub): install the bootloader
            //    on the ESP; it loads a loose kernel + initrd via a loader
            //    entry. ──
            Some(efi_bytes) => {
                let mut bootx = boot.create_file(fallback)?;
                bootx.write_all(efi_bytes)?;
                bootx.flush()?;

                let loader_dir = root.create_dir("loader")?;
                let mut loader_conf = loader_dir.create_file("loader.conf")?;
                loader_conf.write_all(DEFAULT_LOADER_CONF.as_bytes())?;
                loader_conf.flush()?;

                let vmlinuz_name = vmlinuz
                    .file_name()
                    .ok_or_else(|| io::Error::other("kernel vmlinuz path has no filename"))?
                    .to_string_lossy()
                    .into_owned();

                let mut src = std::fs::File::open(vmlinuz)?;
                let mut dst = root.create_file(&vmlinuz_name)?;
                std::io::copy(&mut src, &mut dst)?;
                dst.flush()?;

                let initrd_line = if let Some((initrd_bytes, initrd_name)) = initrd {
                    let mut initrd_dst = root.create_file(initrd_name)?;
                    initrd_dst.write_all(initrd_bytes)?;
                    initrd_dst.flush()?;
                    format!("initrd /{initrd_name}\n")
                } else {
                    String::new()
                };

                let entries_dir = loader_dir.create_dir("entries")?;
                let mut entry = entries_dir.create_file(LOADER_ENTRY_NAME)?;
                let entry_body = format!(
                    "title UMF\nversion {kernel_release}\nlinux /{vmlinuz_name}\n{initrd_line}options {cmdline}\n",
                );
                entry.write_all(entry_body.as_bytes())?;
                entry.flush()?;
            }
        }
    }
    fs.unmount()?;
    Ok(())
}

/// The kernel command line shared by the classic loader entry and the UKI.
/// `extra_cmdline` carries the ` init=<binary> [-- args]` fragment for a binary
/// ENTRYPOINT (empty for init-system builds).
///
/// `root=PARTLABEL=ROOTFS` references the GPT partition by its (deterministic)
/// name rather than a bus-specific node like `/dev/vda2`, so the *same* disk
/// boots on virtio (`/dev/vda`), NVMe (`/dev/nvme0n1p2`), and SATA (`/dev/sda2`)
/// without the root device enumerating differently. The ROOTFS
/// partition is created with the GPT name `ROOTFS`. PARTLABEL is preferred over
/// PARTUUID here because it stays deterministic without UMF having to control
/// the GPT partition GUID (which the `gpt` crate randomizes).
fn boot_cmdline(extra_cmdline: &str, architecture: Architecture) -> String {
    format!(
        "root=PARTLABEL={partlabel} rootfstype={fstype} ro console={console},115200n8{extra_cmdline}",
        partlabel = ROOTFS_PARTLABEL,
        fstype = ROOTFS_FSTYPE,
        console = architecture.serial_console(),
    )
}

// ── ROOTFS partition write ──────────────────────────────────────────────────

pub(crate) fn write_rootfs_partition(
    disk_path: &Path,
    partition_start: u64,
    partition_size: u64,
    rootfs_dir: &Path,
) -> Result<(), CompileError> {
    let file = OpenOptions::new().read(true).write(true).open(disk_path)?;
    let mut view = PartitionView::new(file, partition_start, partition_size, ROOTFS_PARTLABEL);
    view.seek(SeekFrom::Start(0))?;
    let report = write_squashfs_from_dir(rootfs_dir, &mut view)?;
    info!(
        files = report.files,
        dirs = report.directories,
        symlinks = report.symlinks,
        skipped = report.skipped_special_nodes,
        "ROOTFS partition populated (squashfs)",
    );
    Ok(())
}

// ── PartitionView (Read + Write + Seek over a partition slice of the disk) ───

/// A windowed view of a disk file restricted to one partition.
///
/// Translates relative offsets into absolute file offsets so [`fatfs`] and the
/// squashfs writer can format/populate the partition while seeing its start as
/// offset 0 and its length as the device size. Owns the underlying `File`.
/// Public so callers (and tests) can read back a projected partition.
pub struct PartitionView {
    file: File,
    start_bytes: u64,
    length_bytes: u64,
    pos_bytes: u64,
    /// Partition name used in the "<label> partition full" overflow error. A
    /// `PartitionView` backs both the ESP and the ROOTFS write, so the label
    /// keeps the diagnostic from misreporting which one filled up.
    label: &'static str,
}

impl PartitionView {
    /// Wrap `file`, exposing the `length_bytes`-long window starting at
    /// `start_bytes` as a device that begins at offset 0. `label` names the
    /// partition in the overflow diagnostic (e.g. `"ESP"` / `"ROOTFS"`).
    pub fn new(file: File, start_bytes: u64, length_bytes: u64, label: &'static str) -> Self {
        Self {
            file,
            start_bytes,
            length_bytes,
            pos_bytes: 0,
            label,
        }
    }

    fn remaining(&self) -> u64 {
        self.length_bytes.saturating_sub(self.pos_bytes)
    }
}

impl Read for PartitionView {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let max = self.remaining().min(buf.len() as u64) as usize;
        if max == 0 {
            return Ok(0);
        }
        self.file
            .seek(SeekFrom::Start(self.start_bytes + self.pos_bytes))?;
        let n = self.file.read(&mut buf[..max])?;
        self.pos_bytes += n as u64;
        Ok(n)
    }
}

impl Write for PartitionView {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let max = self.remaining().min(buf.len() as u64) as usize;
        if max == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("{} partition full", self.label),
            ));
        }
        self.file
            .seek(SeekFrom::Start(self.start_bytes + self.pos_bytes))?;
        let n = self.file.write(&buf[..max])?;
        self.pos_bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for PartitionView {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(d) => self.length_bytes as i64 + d,
            SeekFrom::Current(d) => self.pos_bytes as i64 + d,
        };
        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "negative seek in PartitionView",
            ));
        }
        self.pos_bytes = new_pos as u64;
        Ok(self.pos_bytes)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
