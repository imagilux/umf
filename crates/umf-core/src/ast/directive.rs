//! Per-directive AST nodes.
//!
//! All current spec directives, grouped:
//! - **Metadata** — [`Label`], [`Env`], [`Arg`]
//! - **Build steps** — [`Shell`], [`User`], [`Workdir`], [`Run`], [`Add`]
//! - **Runtime config** — [`Entrypoint`], [`Expose`], [`Enable`], [`Disable`],
//!   [`Hostname`], [`Locale`], [`Timezone`]
//!
//! `FROM` is captured separately on [`super::Stage`] rather than as a variant
//! here — it's structural, not a peer of the others. The kernel is sourced from
//! `FROM` in bootable builds (see the spec's L0 Introspection section); there
//! is no `KERNEL` directive. The base userland is added with `ADD --from=<ref>`
//! (there is no `ROOTFS` directive); the initramfs is generated implicitly from
//! `ENTRYPOINT` context at L3, and there is no `INITRD` directive. Boot
//! packaging (classic vs UKI) is a `LABEL org.imagilux.umf.flavor` on the
//! bootable image read by `umf compile`, not a `BOOTLOADER` directive.

use super::{Span, Spanned};
use crate::types::{
    EnvVarName, EnvVarValue, HttpsUrl, LabelKey, LabelValue, OciReference, RecipePath, SecretId,
    Username,
};

/// All current spec directives. Each variant carries its parsed arguments and source span.
///
/// See the module-level documentation for the grouping.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Directive {
    /// `LABEL <key>=<value>`.
    Label(Label),
    /// `ENV <key>=<value>`.
    Env(Env),
    /// `ARG <name>[=<default>]`.
    Arg(Arg),
    /// `SHELL <kind>`.
    Shell(Shell),
    /// `USER <name>`.
    User(User),
    /// `WORKDIR <path>`.
    Workdir(Workdir),
    /// `RUN <cmd>`.
    Run(Run),
    /// `ADD <src> <dst>`.
    Add(Add),
    /// `ENTRYPOINT <init>`.
    Entrypoint(Entrypoint),
    /// `EXPOSE <port>/<proto>`.
    Expose(Expose),
    /// `CMD <command>`.
    Cmd(Cmd),
    /// `VOLUME <path>...`.
    Volume(Volume),
    /// `STOPSIGNAL <signal>`.
    Stopsignal(Stopsignal),
}

impl Directive {
    /// Source span covering the entire directive.
    pub const fn span(&self) -> Span {
        match self {
            Self::Label(d) => d.span,
            Self::Env(d) => d.span,
            Self::Arg(d) => d.span,
            Self::Shell(d) => d.span,
            Self::User(d) => d.span,
            Self::Workdir(d) => d.span,
            Self::Run(d) => d.span,
            Self::Add(d) => d.span,
            Self::Entrypoint(d) => d.span,
            Self::Expose(d) => d.span,
            Self::Cmd(d) => d.span,
            Self::Volume(d) => d.span,
            Self::Stopsignal(d) => d.span,
        }
    }
}

// === Metadata ===

/// `LABEL <key>=<value>` — OCI manifest metadata; key/value; inheritable.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    /// Label key (left of `=`).
    pub key: Spanned<LabelKey>,
    /// Label value (right of `=`).
    pub value: Spanned<LabelValue>,
    /// Source span of the full directive.
    pub span: Span,
}

/// `ENV <key>=<value>` — runtime environment variable.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Env {
    /// Variable name.
    pub key: Spanned<EnvVarName>,
    /// Variable value.
    pub value: Spanned<EnvVarValue>,
    /// Source span of the full directive.
    pub span: Span,
}

/// `ARG <name>[=<default>]` — build-time-only variable; not kept in the image.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arg {
    /// Argument name.
    pub name: Spanned<EnvVarName>,
    /// Optional default value (absent ⇒ required to be supplied at build time).
    pub default: Option<Spanned<EnvVarValue>>,
    /// Source span of the full directive.
    pub span: Span,
}

// === Build steps ===

/// `SHELL <kind>` (keyword form) or `SHELL ["interp", "-c", …]` (Docker exec
/// form) — selects the interpreter argv for subsequent shell-form `RUN` /
/// `CMD` / `ENTRYPOINT`.
///
/// Both forms resolve to an explicit argv at parse time: the keyword `sh` /
/// `bash` / `powershell` expands to its conventional argv (`["/bin/sh", "-c"]`,
/// `["/bin/bash", "-c"]`, `["powershell", "-command"]`), and the exec form
/// carries its argv verbatim — so a regular-Dockerfile
/// `SHELL ["/bin/bash", "-euo", "pipefail", "-c"]` is preserved exactly.
/// `SHELL none` resolves to the empty argv.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    /// The interpreter argv that subsequent shell-form steps run through.
    /// Empty for `SHELL none`.
    pub argv: Vec<Spanned<String>>,
    /// Source span of the full directive.
    pub span: Span,
}

/// `USER <username>` — switch execution context.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// Username to switch to.
    pub name: Spanned<Username>,
    /// Source span of the full directive.
    pub span: Span,
}

/// `WORKDIR <path>` — set the working directory for subsequent steps.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workdir {
    /// Working directory path.
    ///
    /// May be absolute or relative (resolved against the previous WORKDIR
    /// by the builder — same rule as the equivalent recipe step). A
    /// trailing `/` is preserved verbatim.
    pub path: Spanned<RecipePath>,
    /// Source span of the full directive.
    pub span: Span,
}

/// `RUN <cmd>` (or `RUN ["a", "b", ...]`) — execute in the build environment.
///
/// In VM mode the environment is a micro-VM booted from the current layer
/// state; in container-target mode it's a container.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// The command to execute.
    pub command: RunCommand,
    /// Optional `--mount=...` options (e.g. `type=secret`).
    pub mounts: Vec<RunMount>,
    /// Source span of the full directive.
    pub span: Span,
}

/// Form of a RUN command.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunCommand {
    /// Shell form: `RUN <command-string>`.
    Shell(Spanned<String>),
    /// Exec form: `RUN ["<arg0>", "<arg1>", ...]`.
    Exec(Vec<Spanned<String>>),
}

/// A `--mount=...` option on a `RUN` directive.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunMount {
    /// Mount kind.
    pub kind: RunMountKind,
    /// Source span of the full `--mount=...` option.
    pub span: Span,
}

/// Kind of `RUN --mount`.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunMountKind {
    /// `RUN --mount=type=secret,id=<id>,target=<path>` — secret mount,
    /// scoped to this RUN, never persisted in the layer.
    Secret {
        /// Secret identifier (matches `--secret id=<id>` on the CLI).
        id: Spanned<SecretId>,
        /// Optional target path inside the container (defaults to
        /// `/run/secrets/<id>` when omitted).
        ///
        /// May be absolute or relative (relative is resolved against the
        /// build's current WORKDIR), so the same recipe-path rules as
        /// ADD/WORKDIR apply.
        target: Option<Spanned<RecipePath>>,
    },
}

/// `ADD <source> <destination>` — copy into the image.
///
/// Single source-to-destination copy primitive. Source may be a local path, an
/// HTTP/HTTPS URL (fetched and, when it sniffs as an archive, extracted), or an
/// external OCI image (`ADD <image-ref> /` or `oci://…`, pulled and unpacked).
/// The parser picks the [`AddSource`] variant from the source string's scheme
/// and shape. `--from=<stage>` ([`Add::from`]) additionally roots the source in
/// a sibling build stage.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Add {
    /// Source — discriminated by what the user wrote (path / URL / OCI ref).
    pub source: AddSource,
    /// Destination path inside the image.
    ///
    /// May be absolute or relative (relative is resolved against the
    /// current WORKDIR by the builder). A trailing `/` is preserved
    /// verbatim and forces directory semantics on the destination —
    /// `ADD foo /target/` always treats `/target` as a directory, while
    /// `ADD foo /target` may create a file `/target`.
    pub destination: Spanned<RecipePath>,
    /// Optional `--from=<stage-name>` for a multi-stage cross-stage copy. An
    /// external OCI image is *not* a `--from`; it is an [`AddSource::Oci`]
    /// source (`ADD <image-ref> /`), resolved like `FROM`.
    pub from: Option<Spanned<crate::types::StageName>>,
    /// `true` when this node was written as `COPY` rather than `ADD`. `COPY`
    /// is the Docker-compatible plain copy: it lowers through the same engine
    /// as `ADD` but is restricted to local-context and `--from=<stage>`
    /// sources — [`AddSource::Url`] and [`AddSource::Oci`] are rejected, since
    /// fetching remote blobs / pulling OCI images is `ADD`'s job, not
    /// `COPY`'s. (Neither verb auto-extracts a *local* archive in UMF, so for
    /// the sources `COPY` does accept it behaves identically to `ADD`.)
    pub plain_copy: bool,
    /// Source span of the full directive.
    pub span: Span,
}

/// Where an [`Add`] directive's source comes from, discriminated at parse time
/// by the source string's scheme and shape:
/// - `http://` / `https://` ⇒ [`Url`](Self::Url) (a remote blob).
/// - `oci://` / `https+oci://`, or a bare reference carrying a `:tag` /
///   `@digest` ⇒ [`Oci`](Self::Oci) (an external OCI image).
/// - anything else (a `./`/`/`-style path, or a bare name) ⇒ [`Path`](Self::Path)
///   (a local context path; also the form used with `--from=<stage>`).
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddSource {
    /// `ADD https://example.com/payload.tar.gz /opt/payload` — a remote blob,
    /// fetched and (when it sniffs as an archive) extracted.
    Url(Spanned<HttpsUrl>),
    /// `ADD ./local /dst`, `ADD /etc/foo /dst`, or
    /// `ADD --from=<stage> /usr/bin/myapp /dst` — a local context path (or a
    /// path inside a sibling build stage when paired with `--from`).
    Path(Spanned<String>),
    /// `ADD imagilux/rootfs:v7.0 /` or `ADD oci://<ref> /` — an external OCI
    /// image, resolved through the same `registry → cache → source` chain
    /// `FROM` uses and unpacked. The base-userland mechanism for bootable
    /// builds (the replacement for the former `ROOTFS` directive).
    Oci(Spanned<OciReference>),
}

impl AddSource {
    /// Source span covering the source argument.
    #[must_use]
    pub const fn span(&self) -> Span {
        match self {
            Self::Url(s) => s.span,
            Self::Path(s) => s.span,
            Self::Oci(s) => s.span,
        }
    }

    /// The source as a string slice, regardless of which variant.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Url(s) => s.value.as_str(),
            Self::Path(s) => s.value.as_str(),
            Self::Oci(s) => s.value.as_str(),
        }
    }
}

// === Runtime config ===

/// `ENTRYPOINT <init>` — selects PID 1.
///
/// Polymorphic per the spec: bare keywords (`systemd`, `openrc`, `none`) select
/// an init system or "no init"; a leading-`/` path or exec form runs a binary
/// as PID 1. The leading-`/` rule is what disambiguates keywords from paths.
///
/// In a bootable build (`FROM` resolves to a kernel artifact), ENTRYPOINT also
/// shapes PID 1: an init keyword (`systemd`/`openrc`) boots that init system
/// with a generated initramfs, while a binary form is the *appliance* shape —
/// the kernel jumps straight to the binary via `init=`, no initramfs. In a
/// container build, the value is the image's entrypoint.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entrypoint {
    /// PID 1 selector.
    pub init: EntrypointInit,
    /// Source span of the full directive.
    pub span: Span,
}

/// PID 1 selector for [`Entrypoint`].
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntrypointInit {
    /// `ENTRYPOINT systemd` — full systemd init.
    Systemd,
    /// `ENTRYPOINT openrc` — OpenRC init.
    OpenRc,
    /// `ENTRYPOINT /path/to/bin [args...]` — direct binary execution as PID 1.
    /// Shell form: the parser preserves the value as a single command string;
    /// the builder applies shell-style word splitting at execution time.
    Path(Spanned<String>),
    /// `ENTRYPOINT ["argv0", "argv1", ...]` — direct binary execution as PID 1.
    /// Exec form: argv vector, no shell involvement.
    Exec(Vec<Spanned<String>>),
    /// `ENTRYPOINT none` — no init declared; runtime supplies PID 1 (container target).
    None,
}

impl EntrypointInit {
    /// `true` if this entrypoint selects an init system (`systemd` or `openrc`).
    ///
    /// Used by the builder to decide whether L3 initramfs generation runs.
    pub const fn is_init_system(&self) -> bool {
        matches!(self, Self::Systemd | Self::OpenRc)
    }

    /// `true` if this entrypoint launches a binary directly (appliance or
    /// container ENTRYPOINT). Returns `false` for init systems and for `None`.
    pub const fn is_binary(&self) -> bool {
        matches!(self, Self::Path(_) | Self::Exec(_))
    }
}

/// `EXPOSE <port>/<protocol>` — emit an nftables ACCEPT rule.
///
/// Default policy is **block all** — only explicitly exposed ports are
/// reachable. EXPOSE emits real nftables rules; it is *not* a metadata-only
/// hint.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expose {
    /// Port number.
    pub port: u16,
    /// Transport protocol.
    pub protocol: ExposeProtocol,
    /// Source span of the full directive.
    pub span: Span,
}

/// Transport protocol for an EXPOSE directive.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExposeProtocol {
    /// TCP.
    Tcp,
    /// UDP.
    Udp,
}

/// `CMD <command>` — the image's default command, written to the OCI image
/// config `Cmd`. Container target only.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    /// Shell-form string or exec-form argv.
    pub command: CmdForm,
    /// Source span of the full directive.
    pub span: Span,
}

/// Shell vs exec form for [`Cmd`].
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdForm {
    /// `CMD <command>` — wrapped by the image's shell at run time.
    Shell(Spanned<String>),
    /// `CMD ["argv0", ...]` — exec form, no shell.
    Exec(Vec<Spanned<String>>),
}

/// `VOLUME <path>...` — declare mount points, written to the OCI image config
/// `Volumes`. Container target only.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// One or more mount-point paths (`VOLUME /a /b` or `VOLUME ["/a", "/b"]`).
    pub paths: Vec<Spanned<String>>,
    /// Source span of the full directive.
    pub span: Span,
}

/// `STOPSIGNAL <signal>` — the stop signal, written to the OCI image config
/// `StopSignal`. Container target only.
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stopsignal {
    /// Signal name (`SIGTERM`) or number (`15`).
    pub signal: Spanned<String>,
    /// Source span of the full directive.
    pub span: Span,
}

// `ENABLE` / `DISABLE` / `HOSTNAME` / `LOCALE` / `TIMEZONE` are not UMF
// directives (recognized at parse time as rejected directives with a migration hint).
// Service enablement is expressed as a unit via `ADD`/`COPY` or `RUN systemctl
// enable`; host/locale/timezone are first-boot concerns for cloud-init/ignition.
