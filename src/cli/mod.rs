//! `umf` CLI surface — argument parsing ([`Cli`] / [`Command`]) and the
//! top-level dispatch ([`run()`]). Each subcommand's implementation lives
//! in its own submodule; this module owns only the clap definitions,
//! the shared `*Format` value-enums, and the wiring that routes a
//! parsed command to its handler.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

mod archive;
mod attest;
mod bench;
mod build;
mod compile;
mod debug;
mod doctor;
mod images;
mod index;
mod inspect;
mod parse;
mod process;
mod ps;
mod registry;
mod run;
mod sbom;
mod sign;
mod tracing;
mod util;

use tracing::{TraceFormat, setup_tracing};

/// Universal Machine Format command-line interface.
#[derive(Debug, Parser)]
#[command(name = "umf", version, about, long_about = None)]
struct Cli {
    /// Trace output format. `text` (default) keeps today's log shape;
    /// `json` emits one structured JSON object per span event for
    /// piping into `jq` / Loki / Honeycomb; `pretty` emits the
    /// tracing-subscriber tree-shaped human format.
    #[arg(long, global = true, value_enum, value_name = "FORMAT")]
    trace_format: Option<TraceFormat>,
    /// Where trace output goes. `stderr` (default) and `stdout` are
    /// special tokens; anything else is treated as a file path the
    /// run will overwrite + append to.
    #[arg(long, global = true, value_name = "DEST")]
    trace_output: Option<String>,
    /// Tracing level filter — sugar over `RUST_LOG`. Accepts
    /// `trace|debug|info|warn|error`. `RUST_LOG`, when set, wins —
    /// this flag is the fallback when the env var is absent.
    #[arg(long, global = true, value_name = "LEVEL")]
    trace_level: Option<String>,
    /// Override the OCI image-layout directory. Defaults to
    /// `$XDG_CACHE_HOME/umf/oci-layout` or `~/.cache/umf/oci-layout`.
    ///
    /// Applies to every subcommand that reads or writes the local
    /// layout (build / run / images / push / pull / save / load /
    /// inspect / debug). `bench` honours it as the parent dir for
    /// its per-bench tempdir; `parse` and `doctor` don't touch the
    /// layout and ignore the flag.
    #[arg(long, global = true, value_name = "PATH")]
    layout_dir: Option<PathBuf>,
    /// Rootless egress backend for container `build`/`run` RUN steps:
    /// `native` (default; in-process userspace stack, no external binary),
    /// `none` (loopback only, no egress), or `pasta` (userspace egress via the
    /// external `passt`/`pasta` helper). Takes precedence over
    /// `UMF_ROOTLESS_NET`. Ignored for a root build (which uses the host veth +
    /// NAT path).
    #[arg(long = "rootless-net", global = true, value_name = "MODE")]
    rootless_net: Option<String>,
    /// Re-allow host-internal address categories for rootless egress, as a
    /// comma-separated list of `loopback`, `link-local`, `rfc1918`, `ula`,
    /// `cgnat`. The default denies all of them (so a RUN step can't reach the
    /// host's loopback, cloud metadata, LAN, etc.); list the ones to permit
    /// (e.g. `rfc1918` to reach a private mirror). Takes precedence over
    /// `UMF_ROOTLESS_NET_ALLOW`. Enforced by the `native` backend.
    #[arg(long = "rootless-net-allow", global = true, value_name = "CATS")]
    rootless_net_allow: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
// `Build` carries a dozen optional fields (container + bootable target). The
// resulting variant size dwarfs `Parse` / `Inspect`, but the enum lives for
// a fraction of a second per process — boxing would just add ceremony.
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Parse a UMF source file and print the AST.
    Parse {
        /// Recipe file, or a directory holding a Containerfile/Dockerfile
        /// (defaults to the current directory). The `.umf` extension is a
        /// convention, not a requirement — any filename works.
        path: Option<PathBuf>,
        /// Explicit recipe path, bypassing directory discovery. Docker's
        /// `-f`: point at a recipe of any name anywhere.
        #[arg(short = 'f', long = "file", value_name = "PATH")]
        file: Option<PathBuf>,
        /// Output format for the parsed AST.
        ///
        /// `table` (default): human-readable column summary, one row per
        /// directive. `json`: machine-readable AST including spans, for
        /// tooling. `debug`: Rust `Debug` representation — verbose, mostly
        /// useful when debugging the parser itself.
        #[arg(long, value_enum, default_value_t = ParseFormat::Table)]
        format: ParseFormat,
    },
    /// Build a UMF source file into a plain OCI image.
    ///
    /// The shape is inferred from `FROM`: a kernel artifact makes a bootable
    /// image (`type=bootable`, projected to a disk later by `umf compile`); a
    /// base image or `scratch` makes a container. Either way the output is an
    /// OCI image — there is no disk-output path.
    Build {
        /// Recipe file, or a directory holding a Containerfile/Dockerfile
        /// (defaults to the current directory). The `.umf` extension is a
        /// convention, not a requirement — any filename works. The
        /// directory containing the recipe is the build context.
        path: Option<PathBuf>,
        /// Explicit recipe path, bypassing directory discovery. Docker's
        /// `-f`: point at a recipe of any name; the positional then names
        /// the build context directory.
        #[arg(short = 'f', long = "file", value_name = "PATH")]
        file: Option<PathBuf>,
        /// Reference under which the built image is registered (e.g.
        /// `registry.example.com/repo:tag`). Required for every build —
        /// container and bootable both emit a plain layered OCI image.
        #[arg(long)]
        tag: Option<String>,
        /// Container target: target platform in `os/arch` form (e.g.
        /// `linux/amd64`). Cross-arch container builds are tracked as a
        /// follow-up; today this flag is honoured only for bootable-target
        /// preflight (`qemu-system-<arch>` detection).
        #[arg(long)]
        platform: Option<String>,
        /// Compression codec for every layer this build packages: `gzip`
        /// (default — universally compatible) or `zstd` (OCI 1.1 media
        /// type; smaller, faster decode). Step-cache entries are keyed per
        /// codec, so switching repackages rather than reusing layers.
        #[arg(long, value_enum, value_name = "CODEC", default_value_t = CompressionArg::Gzip)]
        compression: CompressionArg,
        /// Build-time secret in `id=<id>,src=<path>` or
        /// `id=<id>,env=<NAME>` form, matching BuildKit. Repeatable.
        /// Secrets are bind-mounted at `/run/secrets/<id>` (or the
        /// directive's explicit `target=`) for the duration of any
        /// RUN step that references them via `RUN --mount=type=secret,
        /// id=<id>`. Never enters a layer.
        #[arg(long, value_name = "SPEC")]
        secret: Vec<String>,
        /// `--build-arg NAME=VALUE`: override an `ARG`'s declared default
        /// during `${VAR}` / `$VAR` substitution. Repeatable.
        /// The value drives the build and the layer-cache keys but never
        /// enters the image history or config.
        #[arg(long = "build-arg", value_name = "NAME=VALUE")]
        build_arg: Vec<String>,
        /// Container target: push the built image to the registry implied
        /// by `--tag`.
        #[arg(long)]
        push: bool,
        /// Container target: allow plain-HTTP push (e.g. for a local
        /// `registry:2` running on `localhost:<port>`). Off by default.
        #[arg(long)]
        insecure_registry: bool,
        /// Registry username. When omitted, UMF looks up credentials in
        /// `UMF_REGISTRY_USERNAME`/`UMF_REGISTRY_PASSWORD` env vars and then
        /// in `~/.docker/config.json`. Combine with `--password-stdin` to
        /// supply the password without leaking it through shell history.
        #[arg(long, value_name = "NAME")]
        username: Option<String>,
        /// Read the registry password from stdin. Intended to keep secrets
        /// out of process listings and shell history. Use with `--username`;
        /// ignored otherwise.
        #[arg(long)]
        password_stdin: bool,
        /// Bootable build: persist the unpacked staging tree at this path after
        /// the build finishes (otherwise it's dropped with the tempdir).
        /// Debugging aid.
        #[arg(long, value_name = "PATH")]
        staging_keep: Option<PathBuf>,
        /// Per-build metrics report rendering. `text` (default) prints
        /// a column-aligned "Build summary" at end of the build;
        /// `json` writes structured JSON suitable for CI consumption;
        /// `none` suppresses the summary entirely.
        #[arg(long, value_enum, value_name = "FORMAT", default_value_t = MetricsFormat::Text)]
        metrics: MetricsFormat,
        /// Write the metrics report to a file (in addition to
        /// printing). Use with any `--metrics` mode except `none`.
        #[arg(long, value_name = "PATH")]
        metrics_output: Option<PathBuf>,
    },
    /// Project a bootable-OS image into a local disk image (block).
    ///
    /// `build` produces a plain OCI image; `compile` links it into a bootable
    /// disk by reading the `org.imagilux.umf.*` boot manifest. The block is
    /// local-only: written to `-o PATH`, or a content-addressed sidecar in the
    /// layout's block cache — never an OCI artifact, never pushed.
    Compile {
        /// Reference of a bootable-OS image (`type=bootable`) resident in the
        /// local layout. `umf build` or `umf pull` it first.
        reference: String,
        /// Write the raw disk image to this path. When omitted, the block is
        /// written to a content-addressed sidecar in the layout block cache.
        #[arg(short = 'o', long = "output", value_name = "PATH")]
        output: Option<PathBuf>,
        /// Total disk image size in bytes. Default 2 GiB (sparse).
        #[arg(long, value_name = "BYTES")]
        disk_size: Option<u64>,
        /// EFI System Partition size in bytes. Default 500 MiB (per spec).
        #[arg(long, value_name = "BYTES")]
        esp_size: Option<u64>,
    },
    /// Execute a previously-built image.
    ///
    /// The target is detected from the image's `org.imagilux.umf.type` label: a
    /// container runs via the linked-in libcontainer runtime (no host container
    /// CLI needed); a bootable image (`type=bootable`) is auto-compiled to a
    /// disk (cached) and booted in a VM with OVMF auto-supplied — no `--disk`
    /// needed. A raw disk can be booted directly with `--vmm` + `--disk`.
    Run {
        /// OCI reference (`registry/repo:tag` or `registry/repo@digest`).
        /// Looked up in the on-disk layout first; pulled from the
        /// registry implied by the ref if absent.
        reference: String,
        /// Allocate a pseudo-TTY for the container — same shape as
        /// `podman run -i`. Required when running an interactive shell
        /// or any program that checks `isatty(0)`.
        #[arg(short, long)]
        interactive: bool,
        /// Set an environment variable inside the container in
        /// `KEY=VALUE` form. Merged with the image's `Env`; on key
        /// collision, the CLI value wins. Repeatable.
        #[arg(short = 'e', long = "env", value_name = "KEY=VAL")]
        env_overrides: Vec<String>,
        /// Override the image's `ENTRYPOINT`. Passed verbatim as the
        /// first argv element; the image's `CMD` is dropped (mirrors
        /// the standard `--entrypoint` convention).
        #[arg(long, value_name = "CMD")]
        entrypoint: Option<String>,
        /// Preserve the on-disk OCI bundle prepared for this run after
        /// the container exits. Path is printed to stderr on completion.
        /// Use for debugging the bundle contents libcontainer saw.
        #[arg(long)]
        keep_bundle: bool,
        /// Container target: allow plain-HTTP pull (e.g. for a local
        /// `registry:2` on `localhost:<port>`). Off by default.
        #[arg(long)]
        insecure_registry: bool,
        /// Registry username. When omitted, UMF looks up credentials
        /// in `UMF_REGISTRY_USERNAME`/`UMF_REGISTRY_PASSWORD` env vars
        /// and then in `~/.docker/config.json`. Combine with
        /// `--password-stdin`.
        #[arg(long, value_name = "NAME")]
        username: Option<String>,
        /// Read the registry password from stdin. Use with
        /// `--username`; ignored otherwise.
        #[arg(long)]
        password_stdin: bool,
        /// Override the image's `CMD` and arguments. Everything after
        /// the reference (or after a literal `--`) is collected as
        /// argv after the entrypoint. Use `--` if the args start with
        /// a dash. Container target only.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
        /// VM boot: select the VMM backend. `qemu` (default, universal,
        /// mature) or `ch` (Cloud Hypervisor — Rust-native, faster boot,
        /// requires `--firmware PATH`). A bootable image is auto-compiled
        /// then booted; pair with `--disk` to boot a raw disk directly.
        #[arg(long, value_name = "BACKEND")]
        vmm: Option<String>,
        /// VM boot: path to a raw disk image to boot (e.g. one written
        /// by `umf compile` / `umf save --type=block`). Pair this flag with
        /// `--vmm=qemu` to launch a previously-projected disk.
        #[arg(long, value_name = "PATH")]
        disk: Option<PathBuf>,
        /// VM boot: optional OVMF / EDK II firmware override.
        /// When absent the backend uses its default UEFI firmware
        /// resolution (`-bios` lookup for QEMU).
        #[arg(long, value_name = "PATH")]
        firmware: Option<PathBuf>,
        /// VM boot: guest RAM in MiB. Default 1024.
        #[arg(long, value_name = "MIB")]
        memory: Option<u32>,
        /// VM boot: guest vCPU count. Default 2.
        #[arg(long, value_name = "N")]
        cpus: Option<u32>,
        /// VM boot: `[bind:]host:guest[/proto]` port forward (e.g. `8080:80`,
        /// `8080:80/udp`, or `127.0.0.1:8080:80` to bind one host address
        /// instead of all interfaces). Repeatable.
        #[arg(short = 'p', long = "port-forward", value_name = "[BIND:]HOST:GUEST")]
        port_forwards: Vec<String>,
        /// VM boot (`--vmm ch` port-forwarding): the DHCP daemon run inside the
        /// VM's network namespace to lease the guest. Default `dnsmasq`; `none`
        /// launches nothing (run your own DHCP in the namespace, or use a static
        /// guest address); any other value is a command (whitespace-split argv)
        /// launched in the namespace, e.g.
        /// `--dhcp-command "kea-dhcp4 -c /etc/kea.conf"`.
        #[arg(long = "dhcp-command", value_name = "ARGV")]
        dhcp_command: Option<String>,
        /// VM boot: render qemu's display in a graphical window
        /// instead of `-display none -serial mon:stdio`. Off by default;
        /// the headless mode is what powers automation / CI use.
        #[arg(long)]
        graphic: bool,
    },
    /// Manage images in the on-disk OCI layout (list, remove, prune).
    ///
    /// With no action flag (the default), lists the cached refs in a table with
    /// REF / TYPE / SIZE / DIGEST columns. `--remove <REF>...` drops one or more
    /// refs; `--prune` GCs blobs that aren't reachable from any surviving
    /// manifest (combine with `--remove` to delete + GC in one shot).
    /// `--format=json` emits structured output suitable for tooling.
    ///
    /// (The action lives here rather than under a separate `rmi` subcommand
    /// because removing images is an image-management operation — every
    /// `images` action acts on the same layout.)
    Images {
        /// Output format. `table` (default) is human-readable;
        /// `json` dumps the full per-ref entry shape. Honoured by
        /// list and (in a smaller way) by remove + prune reports.
        #[arg(long, value_enum, default_value_t = ImagesFormat::Table)]
        format: ImagesFormat,
        /// Explicit "list the cached refs" action. Equivalent to
        /// passing no action flag at all — exposed so scripted
        /// invocations can be unambiguous about their intent.
        #[arg(long, conflicts_with = "remove")]
        list: bool,
        /// One or more refs to remove from the layout. Repeatable
        /// or space-separated (e.g. `--remove ref1 ref2 ref3`).
        /// Blobs aren't touched unless `--prune` is also passed.
        #[arg(long, value_name = "REF", num_args = 1..)]
        remove: Vec<String>,
        /// Walk the index and delete blobs no longer reachable from
        /// any surviving manifest. Pair with `--remove` to remove +
        /// GC in one shot; on its own, just GCs the layout.
        #[arg(long)]
        prune: bool,
    },
    /// Push an existing image from the layout to its registry.
    ///
    /// The registry is the one implied by the image's ref. Same credential /
    /// TLS resolution as `umf build --push`.
    Push {
        /// OCI reference already present in the layout.
        reference: String,
        /// Allow plain-HTTP push (e.g. for a local `registry:2`).
        #[arg(long)]
        insecure_registry: bool,
        /// Registry username. When omitted, falls back to env vars +
        /// `~/.docker/config.json`.
        #[arg(long, value_name = "NAME")]
        username: Option<String>,
        /// Read registry password from stdin (use with `--username`).
        #[arg(long)]
        password_stdin: bool,
    },
    /// Pull an image into the layout without building it.
    ///
    /// Pulls from the registry implied by its ref. Useful to pre-warm a `FROM`
    /// base for offline builds.
    Pull {
        /// OCI reference to pull.
        reference: String,
        /// Allow plain-HTTP pull (e.g. for a local `registry:2`).
        #[arg(long)]
        insecure_registry: bool,
        /// Registry username.
        #[arg(long, value_name = "NAME")]
        username: Option<String>,
        /// Read registry password from stdin (use with `--username`).
        #[arg(long)]
        password_stdin: bool,
    },
    /// Compose a multi-arch image index from already-built per-arch images.
    ///
    /// Build each arch first (`umf build --platform=linux/<arch> --tag <ref>`),
    /// then stitch them: `umf index --tag <multi-ref> <amd64-ref> <arm64-ref>`.
    /// The result is one `application/vnd.oci.image.index.v1+json` registered
    /// under `--tag`; each child's `platform` is read from its own OCI config.
    /// `--push` uploads it (and every child tree) to the registry implied by
    /// `--tag`. Consume it with `umf inspect --platform=…` (per-arch select)
    /// or as a `FROM` base.
    Index {
        /// Reference the composed index is registered under (e.g.
        /// `registry.example.com/repo:tag`).
        #[arg(long)]
        tag: String,
        /// Per-arch child image refs, already present in the local layout.
        /// At least one; typically one per architecture.
        #[arg(required = true)]
        children: Vec<String>,
        /// Push the composed index to the registry implied by `--tag` after
        /// writing it locally (mirrors `umf build --push`).
        #[arg(long)]
        push: bool,
        /// Allow plain-HTTP push (e.g. for a local `registry:2`).
        #[arg(long)]
        insecure_registry: bool,
        /// Registry username for `--push`. Same resolution chain as
        /// `umf push` / `umf build --push`.
        #[arg(long, value_name = "NAME")]
        username: Option<String>,
        /// Read the registry password from stdin (use with `--username`).
        #[arg(long)]
        password_stdin: bool,
    },
    /// Attach (and, later, generate) SBOMs as OCI 1.1 referrer artifacts of an image.
    ///
    /// The `subject` is the target manifest; referrers are cosign-/oras-
    /// compatible. List them back with a referrers-aware client (`oras
    /// discover`, `cosign tree`).
    Sbom {
        #[command(subcommand)]
        action: SbomAction,
    },
    /// Sign an image with a static key (cosign-compatible signature referrer).
    ///
    /// Attaches the signature as an OCI 1.1 referrer; `cosign verify --key`
    /// reads it back.
    Sign {
        /// Image to sign. Must be present in the local layout.
        reference: String,
        /// Signing-key spec, same grammar as `umf build --secret`:
        /// `id=<id>,src=<key.pem>` or `id=<id>,env=<NAME>`. PKCS#8 PEM.
        #[arg(long)]
        key: String,
        /// Key algorithm. Auto-detected from the key when omitted.
        #[arg(long, value_enum)]
        key_type: Option<SignKeyType>,
        /// Push the signature referrer to the registry implied by <reference>
        /// (the subject image must already be pushed).
        #[arg(long)]
        push: bool,
        /// Allow plain-HTTP push (e.g. for a local `registry:2`).
        #[arg(long)]
        insecure_registry: bool,
        /// Registry username for `--push`.
        #[arg(long, value_name = "NAME")]
        username: Option<String>,
        /// Read the registry password from stdin (use with `--username`).
        #[arg(long)]
        password_stdin: bool,
    },
    /// Attest an image: a signed in-toto/SLSA DSSE predicate as an OCI 1.1 referrer.
    ///
    /// Wraps a predicate in a signed in-toto/SLSA DSSE envelope and attaches it
    /// as an OCI 1.1 referrer; `cosign verify-attestation --key` reads it back.
    Attest {
        /// Image to attest. Must be present in the local layout.
        reference: String,
        /// Predicate document (JSON) to wrap in the in-toto statement.
        #[arg(long, value_name = "FILE")]
        predicate: PathBuf,
        /// Predicate type: a cosign shorthand (`slsaprovenance`, `spdx`,
        /// `cyclonedx`, `vuln`, `link`) or a full URI.
        #[arg(long = "type", value_name = "TYPE", default_value = "slsaprovenance")]
        predicate_type: String,
        /// Signing-key spec, same grammar as `umf build --secret`:
        /// `id=<id>,src=<key.pem>` or `id=<id>,env=<NAME>`. PKCS#8 PEM.
        #[arg(long)]
        key: String,
        /// Key algorithm. Auto-detected from the key when omitted.
        #[arg(long, value_enum)]
        key_type: Option<SignKeyType>,
        /// Push the attestation referrer (the subject image must already be
        /// pushed).
        #[arg(long)]
        push: bool,
        /// Allow plain-HTTP push (e.g. for a local `registry:2`).
        #[arg(long)]
        insecure_registry: bool,
        /// Registry username for `--push`.
        #[arg(long, value_name = "NAME")]
        username: Option<String>,
        /// Read the registry password from stdin (use with `--username`).
        #[arg(long)]
        password_stdin: bool,
    },
    /// Save layout content to a file (an OCI archive, or a bootable disk image).
    ///
    /// Default (`--type=oci-archive`): one or more refs to an OCI Image Layout
    /// tarball, round-trip compatible with `skopeo copy oci-archive:<file>` and
    /// `docker save`. With `--type=block`: extract a bootable image's compiled
    /// disk from the block cache as a raw, `dd`-able image (the image must have
    /// been `umf compile`d / `umf run`).
    Save {
        /// One or more refs to save.
        #[arg(required = true)]
        references: Vec<String>,
        /// Output path. For `--type=oci-archive`, the tarball (`-` for stdout);
        /// for `--type=block`, the raw disk image.
        #[arg(short, long, value_name = "PATH")]
        output: PathBuf,
        /// Export format: `oci-archive` (default — a `skopeo` / `docker save`
        /// compatible tarball) or `block` (extract a bootable image's compiled
        /// disk from the block cache as a raw, `dd`-able image; the image must
        /// have been `umf compile`d / `umf run` first).
        #[arg(
            long = "type",
            value_name = "FORMAT",
            default_value = "oci-archive",
            value_parser = ["oci-archive", "block"]
        )]
        format: String,
    },
    /// Load an OCI Image Layout tarball into the local layout.
    ///
    /// Use with `umf save`'s output or any `oci-archive:` produced by skopeo /
    /// docker save.
    Load {
        /// Input tarball path. Use `-` for stdin.
        #[arg(short, long, value_name = "PATH")]
        input: PathBuf,
        /// When a ref in the archive already exists in the layout,
        /// replace it. Without `--overwrite` the load errors fast
        /// and the layout is left untouched.
        #[arg(long)]
        overwrite: bool,
    },
    /// Step through a build directive-by-directive (interactive debugger).
    ///
    /// Today supports container builds via `umf debug build <recipe>`. The build
    /// pauses before each RUN/ADD/metadata directive and offers a small REPL
    /// with continue / step / inspect / breakpoint / quit. Use it to walk
    /// through a recipe and see what each directive does without rebuilding from
    /// scratch every time.
    Debug {
        #[command(subcommand)]
        what: DebugTarget,
    },
    /// Benchmark a recipe's build for timing and cache determinism.
    ///
    /// Runs the build N times under controlled conditions (1 cold-cache + N
    /// warm-cache by default) and reports median / p99 / min / max wall-clock
    /// timing along with cache-determinism flags (layer count + total bytes
    /// should be invariant across warm runs of a deterministic recipe).
    Bench {
        /// Recipe file, or a directory holding a Containerfile/Dockerfile
        /// (defaults to the current directory). The `.umf` extension is a
        /// convention, not a requirement — any filename works.
        path: Option<PathBuf>,
        /// Explicit recipe path, bypassing directory discovery (Docker's `-f`).
        #[arg(short = 'f', long = "file", value_name = "PATH")]
        file: Option<PathBuf>,
        /// Number of warm-cache measurement runs. Default 5.
        #[arg(long, default_value_t = 5)]
        runs: usize,
        /// Pre-measurement warmup runs (excluded from aggregate stats).
        #[arg(long, default_value_t = 0)]
        warmup: usize,
        /// Skip the warm runs entirely — only measure one cold-cache
        /// build.
        #[arg(long)]
        cold_only: bool,
        /// Output format. `text` (default) prints a human-readable
        /// table; `json` dumps the structured `BenchReport` for CI
        /// regression tracking.
        #[arg(long, value_enum, default_value_t = BenchFormat::Text)]
        format: BenchFormat,
        /// Container target: tag the benched image lands under. Used
        /// only to give the underlying `umf build` invocations a
        /// concrete `--tag`; the registry is never contacted.
        #[arg(long, value_name = "REF", default_value = "umf-bench/local:latest")]
        tag: String,
    },
    /// Inspect an OCI artifact's UMF labels, runtime config, layers, and history.
    ///
    /// Pulls the image from the registry implied by the reference if it isn't
    /// already in the local layout.
    Inspect {
        /// OCI reference (`registry/repo:tag` or `registry/repo@digest`).
        reference: String,
        /// Output format. `table` (default) is the human-readable
        /// summary; `json` dumps the full structured data the
        /// table is built from (target type, runtime config, layers,
        /// history, labels, manifest + config digests).
        #[arg(long, value_enum, default_value_t = InspectFormat::Table)]
        format: InspectFormat,
        /// In the table view, also enumerate blob digests + sizes per
        /// layer. No-op for `--format=json` (which always includes
        /// blob info).
        #[arg(long)]
        show_blobs: bool,
        /// `os/arch` (e.g. `linux/arm64`). When the reference resolves to a
        /// multi-arch image index, selects which per-arch child to report;
        /// defaults to the host arch. Ignored for a single-arch image.
        #[arg(long, value_name = "OS/ARCH")]
        platform: Option<String>,
        /// Allow plain-HTTP pull (e.g. for a local `registry:2` on
        /// `localhost:<port>`).
        #[arg(long)]
        insecure_registry: bool,
        /// Registry username for the pull-on-miss path. Same
        /// resolution chain as `umf build --push` / `umf run`.
        #[arg(long, value_name = "NAME")]
        username: Option<String>,
        /// Read the registry password from stdin. Use with `--username`.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Manage the registries searched for unqualified references.
    ///
    /// Bare references like `alpine:3.23` resolve against Docker Hub by
    /// default. Registries added here are tried in order (then `docker.io`)
    /// when resolving an unqualified `FROM` / `ADD` / `pull`; fully-qualified
    /// references are unaffected. Stored at `$XDG_CONFIG_HOME/umf/registries.toml`.
    Registry {
        #[command(subcommand)]
        action: RegistryAction,
    },
    /// Report which host runtimes UMF needs and what's actually installed.
    ///
    /// Pass a recipe (or a directory holding a Containerfile/Dockerfile) to
    /// scope the report to a specific build; without one, the full set of known
    /// runtimes is listed.
    Doctor {
        /// Optional recipe (file or directory) whose requirements should
        /// be checked. Any filename works — `.umf` is convention only.
        path: Option<PathBuf>,
        /// Output format: `table` (default, sectioned human report) or
        /// `json` (structured, for tooling).
        #[arg(long, value_enum, default_value_t = DoctorFormat::Table)]
        format: DoctorFormat,
    },
    /// List umf-managed processes (the builds and runs umf has launched).
    ///
    /// Read from the on-disk registry under `$XDG_STATE_HOME/umf/processes/`.
    /// umf is daemonless, so each `umf build` / `umf run` records itself;
    /// finished processes remain as history (like `docker ps -a`).
    Ps {
        /// Output format: `pretty` (table, default), `plain` (tab-separated,
        /// scriptable), or `json` (the raw records).
        #[arg(
            short = 'o',
            long = "output",
            visible_alias = "o",
            value_enum,
            default_value_t = ps::PsOutput::Pretty
        )]
        output: ps::PsOutput,
        /// Sort by a column — `id|name|process|type|status|release|started`,
        /// optionally suffixed `:asc` / `:desc` (e.g. `--sort status:desc`).
        /// A bare `asc` / `desc` sorts by start time. Default: newest first.
        #[arg(
            short = 's',
            long = "sort",
            visible_alias = "s",
            value_name = "KEY[:DIR]"
        )]
        sort: Option<String>,
        /// Filter: comma-separated `KEY=VALUE` criteria, all ANDed
        /// (repeatable). Keys: id, name, process, type, status, release.
        /// A `VALUE` of `all` or `*` matches anything. Example:
        /// `--filter STATUS=exited,TYPE=build`.
        #[arg(
            short = 'f',
            long = "filter",
            visible_alias = "f",
            value_name = "KEY=VALUE,..."
        )]
        filter: Vec<String>,
        /// Remove finished (exited / failed) records, honouring `--filter`
        /// (e.g. `--prune -f STATUS=failed`). A still-running process is
        /// never removed. For trimming history on heavy-usage machines.
        #[arg(long = "prune")]
        prune: bool,
    },
}

/// AST output format for `umf parse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ParseFormat {
    /// Human-readable table summary (default).
    Table,
    /// JSON representation (machine-friendly; for tooling and IDEs).
    Json,
    /// Rust `Debug` representation (verbose; mostly for debugging the parser).
    Debug,
}

/// Output format for `umf inspect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum InspectFormat {
    /// Human-readable table summary (default).
    Table,
    /// Structured JSON for tooling — the full L0 profile, runtime
    /// config, layers, history, labels.
    Json,
}

/// Output format for the `umf doctor` host-readiness report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum DoctorFormat {
    /// Sectioned, human-readable table (default).
    Table,
    /// Structured JSON for tooling.
    Json,
}

/// Output format for the `umf build` metrics report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum MetricsFormat {
    /// Column-aligned "Build summary" table (default). Printed to
    /// stderr at the end of the build.
    Text,
    /// Structured JSON. Pipe-friendly for CI.
    Json,
    /// Suppress the summary entirely.
    None,
}

/// Layer compression codec selectable on `umf build` (`--compression`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum CompressionArg {
    /// gzip-compressed tar layers (default) — accepted by every registry
    /// and runtime.
    Gzip,
    /// zstd-compressed tar layers (the OCI 1.1 media type) — smaller and
    /// faster to decode, but consumers must understand
    /// `application/vnd.oci.image.layer.v1.tar+zstd`.
    Zstd,
}

impl From<CompressionArg> for umf_oci::image::LayerCompression {
    fn from(arg: CompressionArg) -> Self {
        match arg {
            CompressionArg::Gzip => Self::Gzip,
            CompressionArg::Zstd => Self::Zstd,
        }
    }
}

/// Output format for `umf bench`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum BenchFormat {
    /// Human-readable bench report (default).
    Text,
    /// Structured JSON `BenchReport` for CI / regression-tracking.
    Json,
}

/// Output format for `umf images`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ImagesFormat {
    /// Column-aligned table (default).
    Table,
    /// Structured JSON for tooling.
    Json,
}

/// What `umf debug` is debugging. Today only `build` is supported;
/// future targets (run-time, image inspection, etc.) plug in here
/// without breaking the top-level `umf debug` shape.
#[derive(Debug, Subcommand)]
enum DebugTarget {
    /// Step through a container build directive-by-directive.
    Build {
        /// Recipe file, or a directory holding a Containerfile/Dockerfile
        /// (defaults to the current directory). The `.umf` extension is a
        /// convention, not a requirement — any filename works.
        path: Option<PathBuf>,
        /// Explicit recipe path, bypassing directory discovery (Docker's `-f`).
        #[arg(short = 'f', long = "file", value_name = "PATH")]
        file: Option<PathBuf>,
        /// Container target: tag the debugged image will land under.
        #[arg(long, default_value = "umf-debug/local:latest")]
        tag: String,
        /// Compression codec for packaged layers (`gzip` default, `zstd`
        /// opt-in) — same semantics as `umf build --compression`.
        #[arg(long, value_enum, value_name = "CODEC", default_value_t = CompressionArg::Gzip)]
        compression: CompressionArg,
        /// Preset breakpoints, comma-separated 1-based step indices
        /// (e.g. `--break-on=3,5`). When set, `c` (continue) runs
        /// until the next breakpoint instead of to the end.
        #[arg(long, value_name = "INDEX[,INDEX...]")]
        break_on: Option<String>,
    },
}

/// Actions for `umf registry` — manage the unqualified-search registry list.
#[derive(Debug, Subcommand)]
pub(crate) enum RegistryAction {
    /// Add a registry to the unqualified-search list (idempotent).
    Add {
        /// Registry host, e.g. `registry.example.com` or `ghcr.io`.
        registry: String,
    },
    /// Remove a registry from the search list.
    Remove {
        /// Registry host to remove.
        registry: String,
    },
    /// List the configured search registries, in precedence order.
    List,
}

/// SBOM document format for `umf sbom` (`--format`); auto-detected from the
/// document body when omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub(crate) enum SbomFormat {
    /// SPDX JSON (`application/spdx+json`).
    #[default]
    Spdx,
    /// CycloneDX JSON (`application/vnd.cyclonedx+json`).
    Cyclonedx,
}

/// Signing-key algorithm for `umf sign` (`--key-type`); auto-detected from the
/// key when omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SignKeyType {
    /// ECDSA over NIST P-256 / SHA-256 (cosign's default).
    EcdsaP256,
    /// Ed25519.
    Ed25519,
}

/// `umf sbom` actions. Today `attach`; `generate` (per-distro package scan)
/// plugs in here next.
#[derive(Debug, Subcommand)]
enum SbomAction {
    /// Attach an existing SPDX or CycloneDX SBOM document to an image as an
    /// OCI referrer artifact.
    Attach {
        /// Image whose manifest the SBOM refers to (the `subject`). Must
        /// already be present in the local layout (`umf build` / `umf pull`).
        reference: String,
        /// Path to the SBOM document (SPDX or CycloneDX JSON).
        #[arg(long, value_name = "FILE")]
        sbom: PathBuf,
        /// SBOM format. Auto-detected from the document when omitted.
        #[arg(long, value_enum)]
        format: Option<SbomFormat>,
        /// Push the referrer to the registry implied by <reference> after
        /// attaching it locally (the subject image must already be pushed).
        #[arg(long)]
        push: bool,
        /// Allow plain-HTTP push (e.g. for a local `registry:2`).
        #[arg(long)]
        insecure_registry: bool,
        /// Registry username for `--push`. Same resolution chain as `umf push`.
        #[arg(long, value_name = "NAME")]
        username: Option<String>,
        /// Read the registry password from stdin (use with `--username`).
        #[arg(long)]
        password_stdin: bool,
    },
    /// Scan an image's installed packages and emit an SBOM (SPDX or CycloneDX).
    ///
    /// Write it to a file / stdout and/or attach it as a referrer.
    Generate {
        /// Image to scan. Must be present in the local layout.
        reference: String,
        /// Output document format.
        #[arg(long, value_enum, default_value_t)]
        format: SbomFormat,
        /// Write the document to this path (`-` for stdout). Defaults to
        /// stdout when neither `--output` nor `--attach` is given.
        #[arg(long, short = 'o', value_name = "FILE")]
        output: Option<PathBuf>,
        /// Attach the generated document to the image as a referrer artifact.
        #[arg(long)]
        attach: bool,
        /// Push the attached referrer (requires `--attach`; the subject image
        /// must already be in the registry).
        #[arg(long, requires = "attach")]
        push: bool,
        /// Allow plain-HTTP push (e.g. for a local `registry:2`).
        #[arg(long)]
        insecure_registry: bool,
        /// Registry username for `--push`.
        #[arg(long, value_name = "NAME")]
        username: Option<String>,
        /// Read the registry password from stdin (use with `--username`).
        #[arg(long)]
        password_stdin: bool,
    },
}

/// Map a subcommand handler's `Result<(), E>` to a process exit code: success,
/// or print `error: <e>` to stderr and fail. Collapses the per-arm boilerplate
/// in [`run()`]'s dispatch.
fn finish<E: std::fmt::Display>(result: Result<(), E>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Parse the CLI, install tracing, and dispatch the chosen subcommand.
/// Returns the process exit code.
/// Whether a subcommand drives the container build engine in-process and so
/// must run inside our rootless user namespace (entered before any Tokio
/// runtime is built). The VM run path (`--vmm`/`--disk`) and metadata-only
/// subcommands stay in the host namespace; `bench` spawns child `umf`
/// processes that each enter it themselves.
fn command_needs_build_userns(command: &Command) -> bool {
    match command {
        Command::Build { .. } | Command::Debug { .. } => true,
        Command::Run { vmm, disk, .. } => vmm.is_none() && disk.is_none(),
        _ => false,
    }
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();

    if let Err(err) = setup_tracing(
        cli.trace_format.unwrap_or(TraceFormat::Text),
        cli.trace_output.as_deref(),
        cli.trace_level.as_deref(),
    ) {
        eprintln!("error: tracing setup failed: {err}");
        return ExitCode::FAILURE;
    }

    // Move the global `--layout-dir` out of `cli` so each arm can hand it
    // to its handler without re-borrowing `cli`. Partial move is fine —
    // we still consume `cli.command` below.
    let layout_dir_global = cli.layout_dir;
    let layout_dir_override = layout_dir_global.as_deref();

    // Rootless: enter our single build user namespace now — while the process
    // is still single-threaded, before any subcommand builds a Tokio runtime
    // (`unshare(CLONE_NEWUSER)` requires a single-threaded process). Only the
    // in-process container-engine paths attempt it; the VM path and
    // metadata-only subcommands stay in the host namespace. A no-op for real
    // root. See `umf_engine::rootless`.
    //
    // Best-effort: a user namespace is the capability a rootless *container*
    // build/run needs, not a precondition for every `umf build`. A host that
    // forbids unprivileged user namespaces (e.g. Ubuntu's
    // `apparmor_restrict_unprivileged_userns`) must still run bootable builds
    // (RUN executes in a micro-VM) and commands that never start a container
    // (recipe discovery, `--tag`/validation errors, metadata). So we don't fail
    // here — if a rootless container RUN genuinely needs the namespace, youki
    // surfaces the failure at that point. The diagnostic is logged at debug to
    // keep it off the default-level output the CLI contract is asserted against.
    if command_needs_build_userns(&cli.command) {
        if let Err(err) = umf_engine::enter_rootless_userns() {
            // `::tracing` — the local `mod tracing` (CLI tracing setup) shadows
            // the crate name within this module.
            ::tracing::debug!(error = %err, "rootless user namespace unavailable; continuing without it");
        }
    }

    // `--rootless-net` selects the rootless egress backend, overriding
    // `UMF_ROOTLESS_NET`. Resolved before dispatch so the engine sees it; a bad
    // value is a clear up-front error.
    if let Some(spec) = cli.rootless_net.as_deref()
        && let Err(msg) = umf_engine::rootless::set_egress_mode_from_arg(spec)
    {
        eprintln!("error: invalid --rootless-net: {msg}");
        return ExitCode::FAILURE;
    }

    // `--rootless-net-allow` re-allows host-internal address categories for the
    // rootless egress SSRF policy, overriding `UMF_ROOTLESS_NET_ALLOW`.
    if let Some(spec) = cli.rootless_net_allow.as_deref()
        && let Err(msg) = umf_engine::rootless::set_egress_policy_from_arg(spec)
    {
        eprintln!("error: invalid --rootless-net-allow: {msg}");
        return ExitCode::FAILURE;
    }

    match cli.command {
        Command::Parse { path, file, format } => {
            parse::run_parse(path.as_deref(), file.as_deref(), format)
        }
        Command::Build {
            path,
            file,
            tag,
            platform,
            secret,
            build_arg,
            push,
            insecure_registry,
            username,
            password_stdin,
            staging_keep,
            metrics,
            metrics_output,
            compression,
        } => finish(build::run_build(build::BuildArgs {
            path: path.as_deref(),
            file: file.as_deref(),
            tag: tag.as_deref(),
            platform,
            compression: compression.into(),
            secret_specs: &secret,
            build_arg_specs: &build_arg,
            push,
            layout_dir_override,
            insecure_registry,
            username: username.as_deref(),
            password_stdin,
            staging_keep: staging_keep.as_deref(),
            metrics,
            metrics_output: metrics_output.as_deref(),
        })),
        Command::Compile {
            reference,
            output,
            disk_size,
            esp_size,
        } => finish(compile::run_compile(compile::CompileArgs {
            reference: &reference,
            output: output.as_deref(),
            disk_size,
            esp_size,
            layout_dir_override,
        })),
        Command::Run {
            reference,
            interactive,
            env_overrides,
            entrypoint,
            keep_bundle,
            insecure_registry,
            username,
            password_stdin,
            cmd,
            vmm,
            disk,
            firmware,
            memory,
            cpus,
            port_forwards,
            dhcp_command,
            graphic,
        } => match run::run_run(run::RunArgs {
            reference: &reference,
            interactive,
            env_overrides: &env_overrides,
            entrypoint: entrypoint.as_deref(),
            keep_bundle,
            layout_dir_override,
            insecure_registry,
            username: username.as_deref(),
            password_stdin,
            cmd: &cmd,
            vmm: vmm.as_deref(),
            disk: disk.as_deref(),
            firmware: firmware.as_deref(),
            memory,
            cpus,
            port_forwards: &port_forwards,
            dhcp_command: dhcp_command.as_deref(),
            graphic,
        }) {
            Ok(exit_code) => ExitCode::from(u8::try_from(exit_code.clamp(0, 255)).unwrap_or(0)),
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        },
        Command::Images {
            format,
            list,
            remove,
            prune,
        } => finish(images::run_images(images::ImagesArgs {
            layout_dir_override,
            format,
            explicit_list: list,
            remove: &remove,
            prune,
        })),
        Command::Push {
            reference,
            insecure_registry,
            username,
            password_stdin,
        } => finish(images::run_push_subcommand(
            &reference,
            insecure_registry,
            username.as_deref(),
            password_stdin,
            layout_dir_override,
        )),
        Command::Pull {
            reference,
            insecure_registry,
            username,
            password_stdin,
        } => finish(images::run_pull_subcommand(
            &reference,
            insecure_registry,
            username.as_deref(),
            password_stdin,
            layout_dir_override,
        )),
        Command::Index {
            tag,
            children,
            push,
            insecure_registry,
            username,
            password_stdin,
        } => finish(index::run_index(index::IndexArgs {
            tag: &tag,
            children: &children,
            push,
            layout_dir_override,
            insecure_registry,
            username: username.as_deref(),
            password_stdin,
        })),
        Command::Sbom { action } => match action {
            SbomAction::Attach {
                reference,
                sbom,
                format,
                push,
                insecure_registry,
                username,
                password_stdin,
            } => finish(sbom::run_attach(sbom::AttachArgs {
                reference: &reference,
                sbom: &sbom,
                format,
                push,
                layout_dir_override,
                insecure_registry,
                username: username.as_deref(),
                password_stdin,
            })),
            SbomAction::Generate {
                reference,
                format,
                output,
                attach,
                push,
                insecure_registry,
                username,
                password_stdin,
            } => finish(sbom::run_generate(sbom::GenerateArgs {
                reference: &reference,
                format,
                output: output.as_deref(),
                attach,
                push,
                layout_dir_override,
                insecure_registry,
                username: username.as_deref(),
                password_stdin,
            })),
        },
        Command::Sign {
            reference,
            key,
            key_type,
            push,
            insecure_registry,
            username,
            password_stdin,
        } => finish(sign::run_sign(sign::SignArgs {
            reference: &reference,
            key: &key,
            key_type,
            push,
            layout_dir_override,
            insecure_registry,
            username: username.as_deref(),
            password_stdin,
        })),
        Command::Attest {
            reference,
            predicate,
            predicate_type,
            key,
            key_type,
            push,
            insecure_registry,
            username,
            password_stdin,
        } => finish(attest::run_attest(attest::AttestArgs {
            reference: &reference,
            predicate: &predicate,
            predicate_type: &predicate_type,
            key: &key,
            key_type,
            push,
            layout_dir_override,
            insecure_registry,
            username: username.as_deref(),
            password_stdin,
        })),
        Command::Save {
            references,
            output,
            format,
        } => finish(archive::run_save(
            &references,
            &output,
            format == "block",
            layout_dir_override,
        )),
        Command::Load { input, overwrite } => {
            finish(archive::run_load(&input, overwrite, layout_dir_override))
        }
        Command::Debug { what } => match what {
            DebugTarget::Build {
                path,
                file,
                tag,
                compression,
                break_on,
            } => finish(debug::run_debug_build(debug::DebugBuildArgs {
                path: path.as_deref(),
                file: file.as_deref(),
                tag: &tag,
                compression: compression.into(),
                layout_dir_override,
                break_on: break_on.as_deref(),
            })),
        },
        Command::Bench {
            path,
            file,
            runs,
            warmup,
            cold_only,
            format,
            tag,
        } => finish(bench::run_bench(bench::BenchArgs {
            path: path.as_deref(),
            file: file.as_deref(),
            runs,
            warmup,
            cold_only,
            format,
            layout_dir_override,
            tag: &tag,
        })),
        Command::Inspect {
            reference,
            format,
            show_blobs,
            platform,
            insecure_registry,
            username,
            password_stdin,
        } => finish(inspect::run_inspect(inspect::InspectArgs {
            reference: &reference,
            format,
            show_blobs,
            platform: platform.as_deref(),
            layout_dir_override,
            insecure_registry,
            username: username.as_deref(),
            password_stdin,
        })),
        Command::Registry { action } => finish(registry::run_registry(&action)),
        Command::Doctor { path, format } => doctor::run_doctor(path.as_deref(), format),
        Command::Ps {
            output,
            sort,
            filter,
            prune,
        } => ps::run_ps(output, sort.as_deref(), &filter, prune),
    }
}
