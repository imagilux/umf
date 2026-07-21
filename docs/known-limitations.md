# Known limitations

The paths the reference implementation (`umf` **v0.0.1**) does not yet support, and the cases where the [Specification](specification.md) describes something the implementation currently rejects. Each entry is a real, reachable error (not a hypothetical), so you can recognise it and reach for the documented workaround. The DSL is pre-1.0; see [Stability & versioning](specification.md#stability-versioning).

For the failure messages and fixes, see [Troubleshooting](troubleshooting.md). For host setup, [Prerequisites](prerequisites.md).

## Where the spec advertises more than the implementation accepts

A few of these are gaps between the **normative spec** (which describes the intended DSL) and the **current binary**. They are called out inline below with a *Spec vs. impl* note. The spec is aspirational where marked; the binary is canonical for what runs today.

## Build inputs

### `ADD <url>` archive coverage

`ADD <url>` fetches the payload (size-capped, re-fetched every build with the layer cache keyed on the payload digest) and extracts archives sniffed as **tar** or **tar.gz**; anything unrecognised, **`.zip` included** (it sniffs as an opaque payload), is placed as a plain file rather than extracted. Payloads sniffing as **xz / bzip2 / zstd / squashfs** are rejected with *fetched payload is a … archive, which is not extracted yet*.

- **Spec vs. impl.** The [ADD](specification.md#add) directive lists `.tar.xz` and `.zip` among the recognised archives; the engine extracts tar and tar.gz today.
- **A gzip source must wrap a tar.** Because the payload is fingerprinted by magic number, a gzip stream is always treated as a gzipped *tar*: a lone `.gz` of a single file, or a corrupt archive, fails the build with *could not be extracted as a tar archive* rather than landing as a plain file. Decompress it and `ADD` the result, or serve the content uncompressed.
- **Workaround.** Repackage the resource as tar/tar.gz, or pre-extract it into the build context.

### SBOM package scanning

`umf sbom generate` reads installed packages from the **dpkg**, **apk**, **pacman**, and **sqlite rpm** databases. The legacy Berkeley-DB rpm database (`/var/lib/rpm/Packages`, on RHEL/CentOS 7-8 and derivatives) is not read; on those images the generated SBOM lists no rpm packages. Use a base on the sqlite rpmdb (Fedora 33+, RHEL/derivatives 9+, openSUSE Leap 15.4+), or attach an externally-generated SBOM with `umf sbom attach`.

### Bootable build step ordering

Multi-stage bootable builds are supported: the **final** stage's `FROM` decides the shape (a kernel artifact makes the build bootable), earlier stages build as ordinary container producers, and the final stage consumes them with cross-stage `ADD --from=<stage>`. Only the final stage may be bootable — an earlier stage whose `FROM` resolves to a kernel (nested-bootable) is rejected.

Two ordering notes carry over from the single-stage path. `SHELL` / `USER` / `WORKDIR` apply to subsequent `RUN` steps by wrapping each command host-side (`WORKDIR` → `cd`, `SHELL` → the interpreter argv, `USER` → `runuser`); a recipe that sets none of them runs byte-identically to before. Local, URL, and cross-stage `ADD`s are applied to the rootfs **before** the `RUN` steps rather than strictly interleaved in source order, so a `RUN` that precedes an `ADD` in the recipe still sees that `ADD`'s files. Author recipes `ADD`-before-`RUN` (the conventional order) and this is invisible.

### EXPOSE firewall enforcement

The spec describes [EXPOSE](specification.md#expose) as emitting an actual default-deny nftables ruleset, not just metadata. That enforcement applies only to **init-system bootable images** (`ENTRYPOINT systemd` / `openrc`), where the generated `nftables` service is enabled so the ruleset loads at boot. Two shapes do not get it:

- **Container builds** record the exposed ports as ordinary OCI image-config metadata only (`exposed_ports`); no nftables ruleset is programmed, and the container runtime governs reachability.
- **Appliance bootable images** (a binary-path `ENTRYPOINT`, no init system) write `/etc/nftables.conf` but have no init to enable the `nftables` service, so the ruleset is present but not auto-loaded.

So treat EXPOSE's default-deny as a guarantee of init-system bootable images; for the other shapes, enforce reachability with your runtime or an explicit boot-time hook.

## Rootless builds

A rootless (non-root) `umf build` runs inside a single user namespace UMF enters at startup. Four constraints apply; see [Prerequisites](prerequisites.md) and [Troubleshooting](troubleshooting.md).

- **systemd-dependent cgroups.** `RUN` steps are placed in a cgroup scope created by your user's systemd over its session bus (cgroup v2 delegation). A host with no systemd user session (a minimal image, some CI runners, a bare non-login shell) cannot place the step, and the build fails creating the cgroup. There is no rootless fs-cgroup fallback: youki's fs manager would write the root-owned `cgroup.subtree_control`. Build as root, or enable a lingering user session (`loginctl enable-linger`).
- **Subordinate-id delegation is required.** The rootless namespace maps container root to your user plus your delegated `/etc/subuid`/`/etc/subgid` range (applied by the `newuidmap`/`newgidmap` setuid helpers), so non-root image files and `RUN --user` resolve to real ids. This makes `uidmap` + a subuid/subgid grant a hard requirement for a rootless container build; without them the build fails with an actionable error rather than silently producing `nobody`-owned output. See [Prerequisites](prerequisites.md#rootless-builds). A user with multiple `/etc/subuid` lines gets their first allocation.
- **Rootless egress via userspace backends.** Rootless `RUN` steps use a userspace egress backend selected with `--rootless-net`. The default is `native` (in-process smoltcp gateway, no external binary). `pasta` is an opt-in alternative requiring the `passt` package. `none` gives loopback only. The host veth plus NAT masquerade path is not used for rootless builds (it needs real root). Two caveats apply: (1) `pasta` has a weaker SSRF posture (coarse `--no-map-gw` guard only, no per-category deny); (2) name resolution under `native` fails when the host's only nameserver is a loopback stub (for example, systemd-resolved `127.0.0.53` with no `/run/systemd/resolve/resolv.conf` carrying real upstreams), because loopback traffic is not routed through the gateway. Egress to literal IPs is unaffected.
- **Ubuntu unprivileged-userns restriction.** On Ubuntu 24.04+, `kernel.apparmor_restrict_unprivileged_userns=1` blocks namespace creation for the unconfined binary until you grant an AppArmor profile with the `userns,` permission or relax the sysctl.

## Disk projection (`umf compile`)

### ext4 / erofs root partition at compile

`umf compile` writes a **squashfs** root partition only. An image whose boot manifest sets `org.imagilux.umf.rootfs.fs=ext4` or `=erofs` is rejected: *ext4 / erofs are not implemented; rebuild with the default squashfs rootfs*.

- **Spec vs. impl.** The [boot-manifest labels](specification.md#boot-manifest-labels) table lists `squashfs` / `erofs` / `ext4` as the `rootfs.fs` value set; only `squashfs` is implemented by the projector.

### `grub` flavor

`org.imagilux.umf.flavor` accepts `systemd-boot` (classic) and `uki`. `grub` is **reserved**: `umf compile` rejects it (*`grub` is reserved*). An absent flavor defaults to `systemd-boot` with a warning; an unrecognised value is an error. Classic-flavor projection reads the bootloader `.efi` from inside the image rootfs (no host fallback), so a classic image shipping no bootloader is an error: switch to `flavor=uki` or install systemd-boot into the userland.

### Real-kernel boot validation

Real-kernel boot is validated end to end under **QEMU/KVM**: a CI boot-smoke test builds a minimal kernel + busybox image, `umf compile`s it to a GPT/ESP/squashfs UKI disk, and boots it to a userspace marker on the serial console. Bare-metal-specific hardware is not separately tested in CI; the projected UEFI disk is byte-identical whether it boots under a VMM or on hardware, so the QEMU/KVM validation exercises the same boot contract.

## VM runtime (`umf run`)

### Cloud Hypervisor: firmware auto-discovery and DHCP-based port-forwarding

The `--vmm=ch` (Cloud Hypervisor) backend now does direct-kernel boot and host port-forwarding, with two caveats versus the default QEMU backend:

- **Firmware is mandatory for disk boot, with no auto-discovery.** Cloud Hypervisor cannot boot a raw disk without a payload, so `umf run --vmm=ch --disk` requires `--firmware PATH` (it does not auto-discover OVMF the way QEMU does). Direct-kernel boot needs no firmware.
- **Port-forwarding is host-side and needs the guest to take a DHCP lease.** Unlike QEMU's user-mode `hostfwd`, `-p/--port-forward --vmm=ch` sets up a per-VM network namespace + tap + nft DNAT, with a detached `dnsmasq` leasing the guest its address (gateway via the host veth). The guest must run a DHCP client, or you run your own DHCP in that namespace; `umf doctor` reports whether `dnsmasq` is present, and `nft` + `ip` are required.

Use `--vmm=qemu` (the default) for auto-discovered firmware, or a guest image that does not configure its NIC.

## Cross-architecture

`--platform=<os>/<arch>` selects the architecture for **component resolution** (base images, kernels) and for the bootable preflight (`qemu-system-<arch>` detection). Cross-arch **container `RUN` execution** (via `binfmt_misc` + qemu-user-static, as the spec's [Cross-Architecture Builds](specification.md#cross-architecture-builds) describes) is a follow-up: a `--platform` that differs from the host arch resolves the right images but does not yet emulate foreign-arch `RUN` steps. Same-arch builds are unaffected.

## OCI layer encoding

`umf build` emits gzip-compressed tar layers (`application/vnd.oci.image.layer.v1.tar+gzip`) by default and zstd (`…tar+zstd`, the OCI 1.1 media type) with `--compression zstd`. On the **read** side UMF transparently applies gzip-, zstd-, and uncompressed-tar layers; the build-staging unpacker is narrower and accepts gzip or plain tar only (a zstd/xz/bzip2-compressed staging archive is rejected: *staging unpacks gzip or plain tar only*). gzip layers interoperate with any OCI registry and with `docker` / `podman` / `skopeo`; zstd layers need OCI-1.1-aware consumers (current containerd / podman / skopeo read them, older Docker daemons do not).
