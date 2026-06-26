//! SSRF egress policy for the rootless build's egress proxy.
//!
//! A rootful container build NATs its egress out of an isolated network
//! namespace (see [`crate::ContainerNet`]), so the build's connections keep the
//! build's own source address and the host's host-internal services stay
//! unreachable. A *rootless* build has no `CAP_NET_ADMIN`, so it can't build
//! that namespace; instead its RUN steps reach the network through an egress
//! proxy that re-originates each connection **from the host network
//! namespace**. That re-origination is the hazard: a connection the build asks
//! for to `127.0.0.1`, `169.254.169.254` (the cloud-metadata IP), or an
//! RFC 1918 address would resolve against the *host's* view of the network and
//! could reach host-internal services or a cloud metadata endpoint, i.e. a
//! server-side request forgery (SSRF) out of the sandbox.
//!
//! This module is the *policy core* the proxy consults: pure
//! classification (is an [`IpAddr`] host-internal, and in which category) plus
//! an [`EgressPolicy`] that, by default, denies every host-internal category
//! and lets an operator selectively re-allow some. It performs **no DNS and no
//! network I/O** — the resolver and the proxy that enforce these decisions are
//! separate, later work. The proxy is expected to:
//!
//! - resolve the build's requested hostname to its A/AAAA set, then keep only
//!   the addresses [`EgressPolicy::filter_resolved`] returns (closing the
//!   DNS-rebinding window), and
//! - call [`EgressPolicy::check`] again at connect time on the literal address
//!   it is about to dial.
//!
//! The default ([`EgressPolicy::default`]) is the secure one: all five
//! categories denied. Operators widen it with `UMF_ROOTLESS_NET_ALLOW` or the
//! programmatic [`EgressPolicy::with_allowed`] / [`EgressPolicy::from_allow_list`].

use std::collections::BTreeSet;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use thiserror::Error;

/// Environment variable that re-allows egress categories for a rootless build,
/// as a comma/space-separated list of [`AddressCategory`] names (e.g.
/// `rfc1918,ula`). Unset or empty leaves the secure default (deny all) in place.
pub const ALLOW_ENV: &str = "UMF_ROOTLESS_NET_ALLOW";

/// A class of host-internal / non-public destination address.
///
/// These are the categories an SSRF policy reasons about: each names a family
/// of addresses that a re-originated egress connection should refuse by default
/// because resolving it from the host namespace can reach something the sandbox
/// shouldn't. The canonical string form is kebab-case (see [`Display`] /
/// [`FromStr`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AddressCategory {
    /// Loopback, plus the unspecified address. IPv4 `127.0.0.0/8` and
    /// `0.0.0.0`; IPv6 `::1` and `::`. (The unspecified address is folded in
    /// here rather than given its own category: it is non-routable and, dialed,
    /// behaves like loopback / "this host", so denying it with loopback is the
    /// natural grouping.) Canonical name: `loopback`.
    Loopback,
    /// Link-local. IPv4 `169.254.0.0/16` (which **includes** `169.254.169.254`,
    /// the cloud-metadata service IP) and IPv6 `fe80::/10`. Canonical name:
    /// `link-local`.
    LinkLocal,
    /// RFC 1918 private IPv4: `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`.
    /// Canonical name: `rfc1918`.
    Rfc1918,
    /// IPv6 unique-local addresses (ULA), `fc00::/7`. Canonical name: `ula`.
    UniqueLocal,
    /// RFC 6598 carrier-grade NAT shared address space, `100.64.0.0/10`.
    /// Canonical name: `cgnat`.
    Cgnat,
}

impl AddressCategory {
    /// Every category, in canonical order. Used to seed the default deny set and
    /// to render the list of valid names in error messages.
    pub const ALL: [AddressCategory; 5] = [
        AddressCategory::Loopback,
        AddressCategory::LinkLocal,
        AddressCategory::Rfc1918,
        AddressCategory::UniqueLocal,
        AddressCategory::Cgnat,
    ];

    /// The canonical kebab-case name (`loopback`, `link-local`, `rfc1918`,
    /// `ula`, `cgnat`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            AddressCategory::Loopback => "loopback",
            AddressCategory::LinkLocal => "link-local",
            AddressCategory::Rfc1918 => "rfc1918",
            AddressCategory::UniqueLocal => "ula",
            AddressCategory::Cgnat => "cgnat",
        }
    }
}

impl fmt::Display for AddressCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error parsing an [`AddressCategory`] from its name: the offending token plus
/// the list of valid names, so the message is actionable.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown egress category {token:?} (valid: {})", valid_names())]
pub struct ParseCategoryError {
    /// The token that did not match any known category.
    pub token: String,
}

/// The valid category names, comma-joined — for [`ParseCategoryError`]'s message.
fn valid_names() -> String {
    AddressCategory::ALL
        .iter()
        .map(|c| c.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

impl FromStr for AddressCategory {
    type Err = ParseCategoryError;

    /// Parse a category name, case-insensitively and trimming surrounding
    /// whitespace. `rfc-1918` and `linklocal` are accepted as aliases of
    /// `rfc1918` / `link-local` so a stray (or missing) hyphen isn't a hard
    /// error.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let norm = s.trim().to_ascii_lowercase();
        match norm.as_str() {
            "loopback" => Ok(AddressCategory::Loopback),
            "link-local" | "linklocal" => Ok(AddressCategory::LinkLocal),
            "rfc1918" | "rfc-1918" => Ok(AddressCategory::Rfc1918),
            "ula" | "unique-local" => Ok(AddressCategory::UniqueLocal),
            "cgnat" => Ok(AddressCategory::Cgnat),
            _ => Err(ParseCategoryError { token: norm }),
        }
    }
}

/// Classify `ip` into a host-internal [`AddressCategory`], or `None` if it is a
/// public (routable) address the policy never restricts.
///
/// IPv4-mapped IPv6 addresses (`::ffff:0:0/96`) are unwrapped to their embedded
/// IPv4 address and classified as that v4 address, so `::ffff:127.0.0.1`
/// classifies as [`AddressCategory::Loopback`] just like `127.0.0.1` — the
/// re-originated connection would hit the same host. Multicast and other
/// special-purpose ranges that are not a TCP SSRF vector return `None` (they
/// are out of scope for this policy core); document any future widening here.
///
/// The CIDR membership is computed by hand from the fixed prefixes (no `ipnet`
/// dependency): the ranges never change, so a few masked-prefix comparisons are
/// simpler than pulling in a crate.
#[must_use]
pub fn classify(ip: IpAddr) -> Option<AddressCategory> {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        // Unwrap an IPv4-mapped v6 (`::ffff:a.b.c.d`) and classify the v4.
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => classify_v4(v4),
            None => classify_v6(v6),
        },
    }
}

/// Classify an IPv4 address against the v4 host-internal prefixes.
fn classify_v4(v4: Ipv4Addr) -> Option<AddressCategory> {
    let o = v4.octets();
    // Loopback 127.0.0.0/8, plus the unspecified address 0.0.0.0.
    if o[0] == 127 || v4.is_unspecified() {
        return Some(AddressCategory::Loopback);
    }
    // Link-local 169.254.0.0/16 — includes 169.254.169.254 (cloud metadata).
    if o[0] == 169 && o[1] == 254 {
        return Some(AddressCategory::LinkLocal);
    }
    // CGNAT 100.64.0.0/10 (RFC 6598). The /10 covers 100.64–100.127.
    if o[0] == 100 && (o[1] & 0xc0) == 0x40 {
        return Some(AddressCategory::Cgnat);
    }
    // RFC 1918: 10/8, 172.16/12, 192.168/16.
    if o[0] == 10 || (o[0] == 172 && (0x10..=0x1f).contains(&o[1])) || (o[0] == 192 && o[1] == 168)
    {
        return Some(AddressCategory::Rfc1918);
    }
    None
}

/// Classify a non-IPv4-mapped IPv6 address against the v6 host-internal prefixes.
fn classify_v6(v6: Ipv6Addr) -> Option<AddressCategory> {
    // Loopback ::1, plus the unspecified address ::.
    if v6.is_loopback() || v6.is_unspecified() {
        return Some(AddressCategory::Loopback);
    }
    let first = v6.octets()[0];
    let second = v6.octets()[1];
    // Link-local fe80::/10 — top 10 bits 1111 1110 10.
    if first == 0xfe && (second & 0xc0) == 0x80 {
        return Some(AddressCategory::LinkLocal);
    }
    // Unique-local fc00::/7 — top 7 bits 1111 110 (covers fc00:: and fd00::).
    if (first & 0xfe) == 0xfc {
        return Some(AddressCategory::UniqueLocal);
    }
    None
}

/// The outcome of evaluating an address against an [`EgressPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressDecision {
    /// The connection is permitted: either a public address, or a host-internal
    /// category the operator re-allowed.
    Allow,
    /// The connection is refused; the field is the category that triggered the
    /// denial.
    Deny(AddressCategory),
}

/// A refused egress destination — the error form of [`EgressPolicy::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("egress to {ip} denied: {category} address is host-internal (allow it with {ALLOW_ENV})")]
pub struct Denied {
    /// The address that was refused.
    pub ip: IpAddr,
    /// The category that triggered the denial.
    pub category: AddressCategory,
}

/// The rootless egress policy: the set of host-internal [`AddressCategory`]s
/// that are refused. The secure [`Default`] denies every category; an operator
/// override removes categories from the set to re-allow them.
///
/// This is the object the egress proxy consults; it holds no DNS or socket
/// state and is cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressPolicy {
    /// Categories that are refused. A category absent from this set is allowed.
    denied: BTreeSet<AddressCategory>,
}

impl Default for EgressPolicy {
    /// Deny all five host-internal categories — the secure rootless default.
    fn default() -> Self {
        Self {
            denied: AddressCategory::ALL.into_iter().collect(),
        }
    }
}

impl EgressPolicy {
    /// A policy that denies *every* host-internal category. Same as
    /// [`Default`], spelled explicitly for call sites that want the intent named.
    #[must_use]
    pub fn deny_all() -> Self {
        Self::default()
    }

    /// The secure default with `allow` removed from the deny set, i.e. those
    /// categories are re-allowed and every other host-internal category stays
    /// denied. Duplicates and categories not in the default set are harmless.
    #[must_use]
    pub fn with_allowed(allow: &[AddressCategory]) -> Self {
        let mut policy = Self::default();
        for cat in allow {
            policy.denied.remove(cat);
        }
        policy
    }

    /// Parse an allow-list spec — a comma- and/or whitespace-separated list of
    /// category names — into a policy that re-allows exactly those categories.
    /// An empty (or all-whitespace) spec yields the deny-all default. Repeated
    /// names are fine; an unrecognised name is a [`ParseCategoryError`].
    ///
    /// # Errors
    /// [`ParseCategoryError`] for the first token that is not a valid category
    /// name.
    pub fn from_allow_list(spec: &str) -> Result<Self, ParseCategoryError> {
        let mut allow = Vec::new();
        for token in spec.split([',', ' ', '\t', '\n']) {
            if token.trim().is_empty() {
                continue;
            }
            allow.push(AddressCategory::from_str(token)?);
        }
        Ok(Self::with_allowed(&allow))
    }

    /// Build a policy from the [`ALLOW_ENV`] (`UMF_ROOTLESS_NET_ALLOW`)
    /// environment variable: its value is parsed as an allow-list. An unset
    /// variable (or one that isn't valid UTF-8) yields the deny-all default.
    ///
    /// # Errors
    /// [`ParseCategoryError`] if the variable is set to a value containing an
    /// unknown category name.
    pub fn from_env() -> Result<Self, ParseCategoryError> {
        match std::env::var(ALLOW_ENV) {
            Ok(spec) => Self::from_allow_list(&spec),
            Err(_) => Ok(Self::default()),
        }
    }

    /// Whether `category` is currently denied by this policy.
    #[must_use]
    pub fn denies(&self, category: AddressCategory) -> bool {
        self.denied.contains(&category)
    }

    /// Decide whether a connection to `ip` is allowed. A public address (one
    /// [`classify`] returns `None` for) is always [`EgressDecision::Allow`]; a
    /// host-internal address is [`EgressDecision::Deny`] when its category is in
    /// the deny set and [`EgressDecision::Allow`] when the operator re-allowed it.
    #[must_use]
    pub fn decision(&self, ip: IpAddr) -> EgressDecision {
        match classify(ip) {
            Some(cat) if self.denied.contains(&cat) => EgressDecision::Deny(cat),
            _ => EgressDecision::Allow,
        }
    }

    /// Connect-time validation: `Ok(())` if a connection to `ip` is permitted,
    /// `Err(`[`Denied`]`)` if it is refused. The proxy calls this on the literal
    /// address it is about to dial.
    ///
    /// # Errors
    /// [`Denied`] when `ip` falls in a denied category.
    pub fn check(&self, ip: IpAddr) -> Result<(), Denied> {
        match self.decision(ip) {
            EgressDecision::Allow => Ok(()),
            EgressDecision::Deny(category) => Err(Denied { ip, category }),
        }
    }

    /// Keep only the allowed addresses from a resolved A/AAAA set, preserving
    /// order. The DNS-forwarding path calls this after resolving a hostname: any
    /// address that would be denied is dropped, so a name that resolves to a mix
    /// of public and host-internal addresses yields only the public ones (and a
    /// name resolving entirely to denied addresses yields an empty vec, which
    /// the proxy should treat as an unresolvable / refused destination).
    #[must_use]
    pub fn filter_resolved(&self, ips: &[IpAddr]) -> Vec<IpAddr> {
        ips.iter()
            .copied()
            .filter(|&ip| matches!(self.decision(ip), EgressDecision::Allow))
            .collect()
    }
}

#[cfg(test)]
mod tests;
