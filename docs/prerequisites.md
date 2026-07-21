# Prerequisites

What the host needs for each kind of UMF build, and how to satisfy it. `umf doctor` is the source of truth: it reports every item below and, given a recipe, scopes the verdict to that one build. Run it first; this page explains each line and how to fix a MISSING one.

```bash
umf doctor       # host-wide report
umf doctor .     # scope to the build in this directory
```

## Build-time (always linked in)

One dependency is needed to **build the `umf` binary itself**, and it is the only hard requirement for container builds:

| Item | Why | Debian / Ubuntu | Fedora |
|------|-----|------------------|--------|
| **libseccomp** (library + headers) | The container engine links `libcontainer` with its `libseccomp` feature, so every `RUN` step runs under the default deny-by-default seccomp filter. The dev package is needed at compile time. | `sudo apt-get install -y libseccomp-dev` | `sudo dnf install -y libseccomp-devel` |

Once built, **container builds and runs need nothing else external**: the build/run engine (youki's `libcontainer` + overlayfs) is linked into the binary. No `docker`, no `podman`, no daemon. `umf doctor` shows this as `container runtime: linked-in`.

The `seccomp:` line in `umf doctor` confirms the vendored default profile loaded. If it reads `UNAVAILABLE`, the binary's embedded profile is corrupt (rebuild from a clean checkout).

## Container RUN-step network egress

A `RUN` step that reaches the network (`apt-get`, `apk add`, `git clone`, `curl`) runs in its own network namespace and is NAT'd out through the host. `umf doctor` reports a **Container RUN-step network egress** section for this. A build whose `RUN` steps never touch the network needs none of it. (That section also prints a `dnsmasq` line, which belongs to the cloud-hypervisor VM port-forward path below, not to container egress.)

| Item | Why | Debian / Ubuntu | Fedora |
|------|-----|------------------|--------|
| **nftables** (`nft` on `PATH`) | UMF programs the host-side NAT masquerade rule with `nft`. Without it the rule cannot be installed and egress fails. | `sudo apt-get install -y nftables` | `sudo dnf install -y nftables` |
| **`net.ipv4.ip_forward`** | The host must forward the veth subnet's packets. UMF enables it per build, but a host that **forces it off** via sysctl will block egress. | `sudo sysctl -w net.ipv4.ip_forward=1` (persist in `/etc/sysctl.d/`) | same |
| **netfilter `FORWARD` policy** | A default-drop `FORWARD` chain silently blocks NAT'd egress, and UMF cannot override a host firewall policy. | allow the container subnet (default `10.69.0.0/16`) or the `umfv*` veths in your firewall | same |

`umf doctor` can only read the `FORWARD` policy as root: a plain run prints `FORWARD policy: unknown`, while `sudo umf doctor` resolves it to `accept` or `DROP`. See [Troubleshooting → apt/apk in a RUN step hangs or fails](troubleshooting.md#a-run-step-that-hits-the-network-aptapkcurl-hangs-or-fails) when egress misbehaves.

## Rootless builds

An unprivileged `umf build` (and container `umf run`) runs entirely inside one user namespace UMF enters at startup. `umf doctor` reports a **Rootless builds** section. Building as root needs none of this.

| Item | Why | Debian / Ubuntu | Fedora |
|------|-----|------------------|--------|
| **Unprivileged user namespaces** | UMF maps your user to container root in a namespace it creates; without it a rootless build cannot start. On Ubuntu 24.04+ the AppArmor policy `kernel.apparmor_restrict_unprivileged_userns=1` blocks it for unconfined binaries. | grant `umf` an AppArmor profile with `userns,`, or `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`; check `user.max_user_namespaces` > 0 | check `user.max_user_namespaces` > 0 (unrestricted by default) |
| **`uidmap` + subid delegation** | UMF maps the container's ids onto your delegated `/etc/subuid` + `/etc/subgid` range using the setuid `newuidmap`/`newgidmap` helpers. This is what makes `apt`/`dnf` (which drop to a sandbox user via `setgroups`), base-image files owned by non-root users, and `RUN --user <nonzero>` resolve to real ids. Without it a rootless **container** build fails with an actionable error (bootable and rootful builds don't need it). | `sudo apt-get install -y uidmap`, then `sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 "$(id -un)"` | `sudo dnf install -y shadow-utils` (usually preinstalled); ensure `/etc/subuid`+`/etc/subgid` have a range (default on most installs) |
| **systemd user session** (cgroup v2 delegation) | Rootless cgroup placement is **systemd-dependent**: UMF asks your user systemd, over its session bus, to create a delegated scope per `RUN` step. A login session already has one; a minimal host, a CI runner, or a non-login shell may not. | `sudo loginctl enable-linger "$(id -un)"`, or build from a login session | same |
| **Linux ≥ 5.11** | The rootfs overlay is mounted in-process with the kernel overlay driver inside the user namespace, support for which (unprivileged overlay) landed in 5.11. | kernel 5.11+ (Ubuntu 22.04+ qualifies) | kernel 5.11+ |

Rootless `RUN` steps run under a full subordinate-id map: container root is your user, and the rest of the container id space maps onto your delegated `/etc/subuid`/`/etc/subgid` range, so image files owned by non-root users and `RUN --user` resolve to real ids. For network egress, see the next section.

## Rootless RUN-step network egress

Rootless `RUN` steps reach the network through a userspace egress backend selected by `--rootless-net` (or `UMF_ROOTLESS_NET`; the flag wins). Three modes:

| Mode | What it does | Extra host requirement |
|------|-------------|------------------------|
| `native` **(default)** | In-process userspace TCP/IP stack (smoltcp `any_ip` transparent gateway over a tap in the netns UMF owns). Re-originates the container's connections from ordinary host sockets. No external binary, works air-gapped. SSRF-safe by default (see below). | None |
| `pasta` | Userspace egress via the external `passt`/`pasta` helper. | `passt` package (`sudo apt-get install -y passt` / `sudo dnf install -y passt`) |
| `none` | Loopback only: no outbound traffic. | None |

`umf doctor` reports the selected backend, and whether `pasta` is on `PATH`.

**SSRF policy (native backend).** By default, `native` denies all five host-internal address categories at connect time on the literal destination IP (immune to DNS-rebinding): `loopback` (including `0.0.0.0`), `link-local` (including the `169.254.169.254` cloud-metadata IP), `rfc1918`, `ula`, and `cgnat`. Public destinations are always allowed. To reach an internal host, re-allow specific categories:

```bash
umf build --rootless-net-allow rfc1918 --tag local/app:1.0 .
```

`pasta` only applies its coarser `--no-map-gw` guard and does not enforce the full five-category policy.

**DNS under native.** The engine bind-mounts the host's upstream resolver list into the container (preferring `/run/systemd/resolve/resolv.conf`), and the gateway relays UDP transparently. One caveat: if the host's only configured nameserver is a loopback stub (for example, systemd-resolved advertising only `127.0.0.53` with no `/run/systemd/resolve/resolv.conf` carrying real upstreams), name resolution inside the container will fail because loopback is not routed through the gateway. Egress to literal IPs is unaffected. Switch to `--rootless-net pasta`, or configure the host resolver to expose a non-loopback upstream, if you hit this. See also [Known limitations](known-limitations.md#rootless-builds) and [Troubleshooting](troubleshooting.md#rootless-build-network-troubleshooting).

## Bootable builds and VM runs

A **bootable** build (`FROM` a kernel artifact) runs its `RUN` steps in a micro-VM, and `umf run` boots a bootable image under a VMM. These need host virtualization:

| Item | Why | Debian / Ubuntu | Fedora |
|------|-----|------------------|--------|
| **QEMU** (`qemu-system-<arch>`) | Default VMM backend for `umf run` and the per-`RUN` micro-VMs. `umf doctor` reports its path or `<none on PATH>`. | `sudo apt-get install -y qemu-system-x86` | `sudo dnf install -y qemu-system-x86` |
| **KVM** (`/dev/kvm`, group access) | Hardware acceleration. Without it QEMU falls back to TCG (much slower); the build still works. `umf doctor` reports `accessible`, `present, no permission`, or `absent`. | add your user to the `kvm` group: `sudo usermod -aG kvm $USER`, then re-login | same |
| **OVMF / EDK II firmware** | UMF is UEFI-only; booting a compiled disk needs an OVMF firmware blob. `umf run` auto-discovers it at the usual host paths (`/usr/share/OVMF/OVMF.fd`, `/usr/share/qemu/OVMF.fd`, `/usr/share/edk2/ovmf/OVMF.fd`, `/usr/share/edk2-ovmf/OVMF.fd`, `/usr/share/ovmf/OVMF.fd`), or pass `--firmware PATH`. | `sudo apt-get install -y ovmf` | `sudo dnf install -y edk2-ovmf` |

> **Real-kernel boot is validated under QEMU/KVM**: a CI boot-smoke test builds, compiles, and boots a minimal image to a userspace marker. Bare-metal-specific hardware is not separately tested; the projected UEFI disk is byte-identical either way. See [Known limitations](known-limitations.md).

The **Cloud Hypervisor** backend (`--vmm=ch`) is an alternative to QEMU. For a raw `--disk` boot it requires an explicit `--firmware` path (the raw-disk path discovers no firmware); a bootable image run auto-discovers OVMF the same way QEMU does. Its `-p/--port-forward` is wired host-side (a per-VM netns + tap + nft DNAT, pure-Rust with no `iproute2`), so it additionally needs **`nft`** on `PATH`, plus **`dnsmasq`** for the default in-namespace DHCP (not required if you pass `--dhcp-command` to run your own DHCP daemon, or `--dhcp-command none`); `umf doctor` reports them. See [Known limitations](known-limitations.md).

## Optional LSM confinement (AppArmor / SELinux)

Not required. Container RUN steps are already contained by seccomp, a dropped
capability set, a user namespace, and masked/read-only paths; an LSM profile is
an extra defence-in-depth layer, applied only when the host provides one and
skipped cleanly otherwise (so absence never breaks a build).

- **AppArmor**: load the shipped profile once, and UMF applies it to every RUN
  step automatically:

  ```bash
  sudo apparmor_parser -r -W crates/umf-engine/resources/apparmor/umf-default
  ```

  Point UMF at a different loaded profile with `UMF_APPARMOR_PROFILE=<name>`, or
  set it to `unconfined` to disable. UMF does not load the profile itself
  (loading needs init-namespace privilege).
- **SELinux**: on an enforcing host, supply the process/mount labels with
  `UMF_SELINUX_LABEL=<context>` and `UMF_SELINUX_MOUNT_LABEL=<context>`; they are
  ignored on a non-enforcing host.

## Optional acceleration

Not required for any build; it only makes **warm** rebuilds faster.

| Item | Why | Debian / Ubuntu | Fedora |
|------|-----|------------------|--------|
| **`mkfs.erofs`** (from `erofs-utils`) | When present, UMF encodes cached lower layers as erofs images for a faster warm-rebuild overlay. Absent, a pure-Rust unpack is used instead, so it is never required and `umf doctor` does not gate on it. | `sudo apt-get install -y erofs-utils` | `sudo dnf install -y erofs-utils` |

## At a glance

| You want to… | You need |
|--------------|----------|
| Build the `umf` binary | `libseccomp-dev` |
| Build/run **containers** | nothing beyond the binary (egress packages only if `RUN` hits the network) |
| Build/run **containers rootless** (non-root) | unprivileged user namespaces enabled + `uidmap` + `/etc/subuid`,`subgid` delegation + a systemd user session |
| Rootless `RUN` steps that fetch packages (default `native` backend) | nothing extra |
| Rootless `RUN` steps with the `pasta` backend | `passt` package + `--rootless-net pasta` |
| Privileged `RUN` steps that fetch packages | `nftables` + forwarding allowed |
| Build/run **bootable** images | QEMU, KVM group membership, OVMF |

When a build fails on one of these, [Troubleshooting](troubleshooting.md) has the specific remedy.
