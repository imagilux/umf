//! UMF OCI primitives.
//!
//! Image emission, on-disk image-layout cache, registry client (pull/push),
//! and build staging. Consumed by `umf-engine` (the in-process container
//! build engine) and `umf-builder` (target lowering and build pipelines).
//!
//! ## Dependency direction
//!
//! `umf-oci` depends only on [`umf_core`]. The builder, the engine, and the
//! CLI all sit above; none of them are referenced from here. There are no
//! cycles.
//!
//! ## Module map
//!
//! - [`image`] — emit a valid OCI image (manifest + config + layer blobs)
//!   into an [`registry::ImageLayout`]. Pure producer; takes ready-made
//!   layers, never executes anything itself.
//! - [`registry`] — on-disk image-layout cache plus the
//!   distribution-protocol client that pulls into / pushes from it.
//! - [`staging`] — temporary directory that accumulates filesystem content
//!   across a build (rootfs unpack, kernel install, RUN-step diffs, …)
//!   before it gets packed into a layer or written into a disk image.
//! - [`materialize`] — the read side: apply a finished image's layers (in
//!   order, honouring whiteouts) into a rootfs directory, for disk
//!   projection. Engine-free — the projector and boreal depend on this.
//! - [`erofs`] — encode a cached lower layer as an erofs image in the
//!   layout's `cache/erofs/` sidecar, content-addressed on diff_id. The
//!   engine mounts these as overlayfs lowers instead of unpacking
//!   directories. Optional acceleration with a graceful
//!   unpack fallback when `mkfs.erofs` is absent.
//! - `test_registry` (feature `test-server`) — in-process OCI
//!   distribution v2 server used by round-trip tests. Replaces the
//!   `registry:2` container the build-time tests used to spin up via
//!   podman, so the test suite runs unconditionally with no host
//!   container runtime needed.

pub mod archive;
pub mod erofs;
pub mod format;
pub mod image;
pub mod materialize;
pub mod registry;
pub mod staging;

#[cfg(feature = "test-server")]
pub mod test_registry;
