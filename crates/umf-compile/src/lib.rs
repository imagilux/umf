//! UMF disk projector.
//!
//! Projects a **bootable-OS OCI image** into a block disk image: read the
//! `org.imagilux.umf.*` boot-manifest labels, materialize the rootfs, shape a
//! GPT/ESP disk (a classic bootloader entry or a UKI), and write the block.
//! The image is the sole input — no recipe, no out-of-band parameters.
//!
//! This is the read/projection side of the build/compile split: `umf build`
//! emits the OCI image; `umf compile` (and, later, boreal) project it here.
//! Lean by design — it depends on `umf-core` (labels), `umf-oci` (layer
//! materialization), and `umf-vmm` (privileged block ops run inside a
//! micro-VM); never on the parser or the container engine.

pub mod error;
pub mod filesystem;
pub mod image;
pub mod partition;
pub mod uki;

pub use error::CompileError;
pub use image::{CompileReport, compile_image};
pub use partition::{DiskGeometry, DiskInputs, DiskProjection, project_disk, validate_geometry};
