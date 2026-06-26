# Security Policy

UMF builds and runs privileged container and VM workloads, so we take security
seriously and welcome coordinated disclosure.

## Reporting a vulnerability

Please report security issues **privately**, not through a public issue.

1. Preferred: open a [GitHub private security advisory](https://github.com/imagilux/umf/security/advisories/new)
   on this repository (GitHub keeps it confidential until disclosed).
2. Alternatively, email **contact@bitswalk.com** with a clear description, the
   affected version (`umf --version`), and reproduction steps. PGP-encrypted
   mail is welcome; ask for a key if you want one.

Please include enough detail to reproduce (a minimal `.umf` recipe, the exact
command line, host OS / kernel, and the observed versus expected behaviour). We
aim to acknowledge a report within a few business days and to keep you updated
as we investigate. Do not disclose publicly until a fix is available and we have
agreed on a coordinated date.

There is no bug-bounty program: UMF is a single-author open-source project under
the Imagilux umbrella. Credit is given in the release notes for any reported
issue, unless you ask to remain anonymous.

## Supported versions

UMF ships from a single linear `main`; only the latest tagged release receives
fixes. There is no long-term-support branch. Spec and binary version
independently (the spec is tagged `spec-vX.Y[.Z]`, the binary `vX.Y.Z`); security
fixes target the implementation and go out in the next binary release.

| Component             | Supported            |
| --------------------- | -------------------- |
| Latest `vX.Y.Z` (bin) | yes                  |
| Older binary releases | no (upgrade)         |

## Security model

UMF is daemonless and runs the build pipeline in-process. A build executes
arbitrary author-supplied `RUN` steps, so those steps are treated as
**semi-trusted**: contained, but assumed to be capable of trying to break out.
The containment is layered and applied to every container `RUN` step the engine
launches (`crates/umf-engine`):

- **User namespace + sub-id mapping**: rootless builds run in a user namespace
  with the host uid/gid mapped onto an unprivileged sub-id range, so "root"
  inside the step is unprivileged on the host.
- **Capability drop**: the bounding/effective/permitted/inheritable/ambient
  sets are limited to the conventional container-runtime default
  (CHOWN/DAC/FOWNER/FSETID/KILL/MKNOD/NET_BIND/NET_RAW/SETUID/SETGID/SETPCAP/
  SETFCAP/SYS_CHROOT/AUDIT). There is no `CAP_SYS_ADMIN`, no `CAP_SYS_MODULE`,
  no `CAP_SYS_PTRACE`.
- **`no_new_privileges`**: set on every step, so a step can never gain
  privileges via setuid binaries or file capabilities.
- **Masked and read-only paths**: sensitive `/proc` and `/sys` entries are
  masked, and the standard read-only paths are enforced, using the
  runtime-spec defaults; each step also gets a fresh `sysfs`.
- **Default-deny seccomp**: every `RUN` step is filtered by the well-tested
  containerd/Docker default profile (`SCMP_ACT_ERRNO` default action plus a
  curated allowlist), blocking the dangerous syscall tail (`kexec_load`, `bpf`,
  unguarded `mount`, arbitrary `ptrace`, ...). The profile is vendored verbatim
  from upstream rather than hand-written.
- **Cgroup namespace + pid cap**: each step gets its own cgroup-hierarchy root
  and a cgroup `pids` limit, bounding fork-bomb and runaway-process behaviour.
- **Per-step network namespace**: egress (when enabled) is a dedicated veth
  pair behind a host NAT masquerade (`crates/umf-networking`), not host
  networking.

**Build secrets** supplied via `RUN --mount=type=secret,id=<id>,target=<path>`
are scoped to a single `RUN`, are never written into an image layer, and never
contaminate the cache key by *value*: the layer cache hashes only the referenced
secret **IDs**, so rotating a secret's contents still cache-hits while the secret
material itself never lands on disk in the image. Secrets must never be passed
via `ARG` or `ENV`.

`EXPOSE` is not metadata: it emits real nftables rules with a **default-deny**
posture, so only explicitly exposed ports are reachable.

Disk projection (`umf compile`) reads boot files from inside the image rootfs
with symlink containment, so a crafted image cannot make the host read files
outside the image tree.

### Hardening posture

The containment above reflects the 2026-05-31 audit-hardening pass: a full
code-audit remediation that re-enabled and tightened the per-`RUN` sandbox
(masked / read-only paths, fresh sysfs, cgroup namespace, a cgroup pids cap,
sub-id range mapping, and re-enabled default-deny seccomp). New `RUN`-step
sandbox controls should be added in `crates/umf-engine` (see `bundle.rs` and
`seccomp.rs`) and must not regress any of the invariants above.

### What is out of scope

UMF assumes a trusted host and operator. Running `umf` itself, the contents of
the OCI layout cache, registry credentials in `~/.docker/config.json`, and the
host kernel / hypervisor are the operator's trust boundary, not UMF's. A
malicious *host* is out of scope. The VM backends (QEMU, Cloud Hypervisor) carry
their own security boundaries; UMF drives them but does not re-implement their
isolation.
