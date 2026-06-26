//! OCI registry client and on-disk image-layout cache.
//!
//! Two layers, kept separate so the cache is usable without the network:
//!
//! * [`layout`] — on-disk OCI image layout (a content-addressable blob store plus an
//!   `index.json`), interoperable with `crane` / `skopeo` / any other consumer of the
//!   OCI image-layout spec.
//! * [`client`] — async client that talks the OCI distribution protocol, pulling from
//!   and pushing to remote registries while materialising into an [`ImageLayout`].
//!
//! Both layers report errors through a single [`RegistryError`] type.
//!
//! The transport itself is delegated to the [`oci_client`] crate — UMF re-exports its
//! [`Reference`] and [`RegistryAuth`] types so callers do not need to depend on
//! `oci-client` directly.

pub mod auth;
pub mod client;
pub mod error;
pub mod layout;
pub mod referrers;
pub mod search;

pub use auth::{CredentialOverride, resolve_auth_for};
pub use client::{RegistryClient, default_client_config};
pub use error::RegistryError;
pub use layout::{IMAGE_LAYOUT_VERSION, ImageLayout, REF_NAME_ANNOTATION, sha256_digest};
pub use referrers::{ReferrerDescriptor, ReferrersIndex, fallback_tag};
pub use search::{SearchRegistries, is_qualified, resolution_candidates};

pub use oci_client::Reference;
pub use oci_client::secrets::RegistryAuth;
