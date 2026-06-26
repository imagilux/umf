# Troubleshooting

Common failures and their fixes. Run `umf doctor` (or `umf doctor <recipe>`) first: it surfaces most of these as a host report before you hit them mid-build. For the full host checklist see [Prerequisites](prerequisites.md); for paths the implementation rejects by design see [Known limitations](known-limitations.md).

## A RUN step that hits the network (apt/apk/curl) hangs or fails

A `RUN` step that fetches packages runs in its own network namespace, NAT'd out through the host. When name resolution or outbound traffic fails inside that step, the cause is almost always the host egress path, not the recipe.

Check, in order:

1. **`nft` missing.** `umf doctor` prints `nft: <none on PATH>`. The masquerade rule can't be programmed without it. Install `nftables` (see [Prerequisites](prerequisites.md#container-run-step-network-egress)).
2. **`FORWARD` policy is DROP.** A default-drop netfilter `FORWARD` chain silently blocks the NAT'd packets, and UMF cannot override a host firewall policy. `sudo umf doctor` reports `FORWARD policy: DROP`. Allow the container subnet (default `10.69.0.0/16`) or the `umfv*` veth interfaces in your firewall:

    ```bash
    sudo nft add rule inet filter forward ip saddr 10.69.0.0/16 accept
    sudo nft add rule inet filter forward ip daddr 10.69.0.0/16 ct state established,related accept
    ```

    (Adapt to your firewall's table/chain names; the point is to stop the forward chain dropping the subnet.)
3. **`ip_forward` forced off.** `umf doctor` prints `net.ipv4.ip_forward: disabled`. UMF enables it per build, but a host that re-asserts `net.ipv4.ip_forward=0` via sysctl wins. Set and persist `net.ipv4.ip_forward=1`.

A plain `umf doctor` reports `FORWARD policy: unknown` because reading the ruleset needs root; re-run as `sudo umf doctor` to get the real verdict.

> Egress is **best-effort and torn down with the step**. A build whose `RUN` steps never reach the network is unaffected by all of the above.

## Rootless build fails creating a user namespace or cgroup

An unprivileged `umf build` enters one user namespace at start (you are mapped to container root) and runs the whole build inside it, placing each `RUN` step in a systemd-managed cgroup scope. Two host prerequisites must hold; `umf doctor` reports both.

**1. Unprivileged user namespaces must be permitted.** The failure looks like:

```
error: runtime backend: rootless: failed to write /proc/self/uid_map: Permission denied
... Unprivileged user namespaces appear to be restricted on this host ...
```

On Ubuntu 24.04+ this is the AppArmor policy `kernel.apparmor_restrict_unprivileged_userns=1`. Grant the `umf` binary an AppArmor profile carrying the `userns,` permission, or relax the sysctl (not persistent across reboot):

```bash
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
```

Also confirm namespaces are not disabled outright (`sysctl user.max_user_namespaces` must be greater than 0, and the kernel needs `CONFIG_USER_NS`). Running inside an outer container or a restrictive sandbox can block creation entirely: run on the host, or grant the namespace capability the outer environment withholds.

**2. Rootless cgroups need a systemd user session.** UMF places each `RUN` step in a delegated systemd scope over your user session bus, so rootless builds are **systemd-dependent**: they require a running systemd *user* instance with cgroup v2 delegation. A normal desktop or server login already has one. A minimal host, a CI runner, or a bare non-login shell may not. The failure is an `EACCES` writing `/sys/fs/cgroup/.../cgroup.subtree_control`. Enable a lingering user manager, then build from that session:

```bash
sudo loginctl enable-linger "$(id -un)"
```

> The container's id map is **single-id**: container root is your user, and every other id resolves to `nobody`. A base-image file owned by a non-root uid, or `RUN --user <nonzero>`, will not resolve to a real id in a rootless build. Run the build as root (or in a privileged environment) when faithful multi-uid ownership is required.

## Rootless build network troubleshooting

### A rootless `RUN` step can't reach the network {#rootless-build-network-troubleshooting}

A rootless `umf build` uses a userspace egress backend, not the host veth plus NAT path (which needs real root). Check which backend is active:

```bash
umf doctor     # look for the "rootless egress backend" line
```

- **Backend is `none`.** The `UMF_ROOTLESS_NET` env var may be set to `none`. Unset it, or pass `--rootless-net native` explicitly.
- **Backend is `native` but steps can't connect.** Confirm the container is attempting a public IP: the native backend denies all five host-internal address categories by default (`loopback`, `link-local`, `rfc1918`, `ula`, `cgnat`). If the destination is a public internet address and connections still fail, check that `--rootless-net-allow` or `UMF_ROOTLESS_NET_ALLOW` has not been set to an unexpected value (a malformed value fails closed).
- **Backend is `pasta` and `pasta` is missing.** `umf doctor` reports `pasta: <none on PATH>`. Install the `passt` package, or switch to the default `native` backend.
- **Userns is blocked.** The egress gateway can't start if the user namespace itself fails to create. See [Rootless build fails creating a user namespace or cgroup](#rootless-build-fails-creating-a-user-namespace-or-cgroup) above.

### Name resolution fails inside a rootless RUN step

With the `native` backend, the engine surfaces the host's upstream nameservers into the container by bind-mounting the host resolver config. This works when the host has a real upstream nameserver reachable from a non-loopback address. It does not work when the host's only configured nameserver is a loopback stub:

```
# /etc/resolv.conf or /run/systemd/resolve/resolv.conf shows only:
nameserver 127.0.0.53
```

The `native` gateway routes through a tap to the host; loopback addresses (`127.x.x.x`) are not routed through that gateway by design, so the DNS query never leaves the container. Egress to literal public IPs is unaffected.

Fixes:

1. Configure the host's stub resolver to expose real upstreams. On a systemd-resolved host, check `/run/systemd/resolve/resolv.conf` (not `/etc/resolv.conf`): if it exists and carries a non-loopback nameserver, the bind-mount will use it automatically.
2. Switch to the `pasta` backend: `--rootless-net pasta` (requires the `passt` package; weaker SSRF posture).
3. For `none` or other loopback-only scenarios, pre-pull packages into the build context and use `ADD` + a `RUN` that accesses only the local filesystem.

### An egress connection is denied with "address refused: host-internal"

```
error: native egress: connect refused: destination 192.168.1.10 is rfc1918 (host-internal)
```

The `native` backend's SSRF policy denies connections to host-internal address categories by default. This protects builds against supply-chain attacks that try to reach cloud metadata endpoints or internal services. To reach an internal package mirror or private registry:

```bash
umf build --rootless-net-allow rfc1918 --tag local/app:1.0 .
```

Comma or space-separate multiple categories: `--rootless-net-allow rfc1918,link-local`. Allowed categories: `loopback`, `link-local`, `rfc1918`, `ula`, `cgnat`. A malformed value fails closed (deny-all). The `pasta` and `none` backends do not enforce this policy.

## `umf run` on a bootable image: no UEFI firmware found

```
error: no UEFI firmware for x86_64 found at the usual host paths — install OVMF
(e.g. the `ovmf` package), or pass --firmware PATH to boot this image
```

UMF is UEFI-only and needs an OVMF/EDK II firmware blob to boot a compiled disk. `umf run` probes the well-known paths (`/usr/share/OVMF/OVMF.fd`, `/usr/share/qemu/OVMF.fd`, `/usr/share/edk2/ovmf/OVMF.fd`, `/usr/share/edk2-ovmf/OVMF.fd`, `/usr/share/ovmf/OVMF.fd`).

- Install the firmware: `sudo apt-get install -y ovmf` (Debian/Ubuntu) or `sudo dnf install -y edk2-ovmf` (Fedora).
- Or point at it explicitly: `umf run --firmware /path/to/OVMF.fd <ref>`.
- This auto-discovery (and so this error) applies to **both** backends for a bootable image, `--vmm=ch` included. The one place firmware is never discovered is a raw `--disk` boot: there `--vmm=ch` **requires** `--firmware PATH` (it cannot boot a raw disk without a firmware payload), while `qemu` falls back to its built-in firmware.

## A `RUN` in a `FROM scratch` build fails with "no such file or directory"

`FROM scratch` starts from a genuinely empty filesystem, so the first `RUN` has no shell — or any binary — to execute, and fails at exec the same way docker's does. `ADD` the tooling first (a static busybox, your compiled binary) and a subsequent `RUN` sees it through the overlay; or keep scratch stages to `ADD` + metadata, the static-appliance shape they're made for.

## `ADD <url>` fails with "fetched payload is a … archive"

```
error: ADD <url>: fetched payload is a zstd archive, which is not extracted yet —
       pre-extract it, or repackage as tar/tar.gz
```

The payload is sniffed by magic number, never by extension. tar and tar.gz extract natively; xz / bzip2 / zstd / zip do not yet — repackage as tar/tar.gz or pre-extract into the build context. A payload with no recognised archive magic is placed as a plain file at the destination, so a *misnamed* `.tar.gz` that is actually HTML lands as a file rather than failing — check what the URL actually serves. See [Known limitations](known-limitations.md#add-url-archive-coverage).

## `ADD <url>` fails with "could not be extracted as a tar archive"

```
error: ADD <url>: the fetched gzip payload could not be extracted as a tar
       archive: ... . Use a plain-file source, or repackage the content as
       tar / tar.gz.
```

The payload sniffed as **gzip** or **tar** by magic number, so UMF extracted it as a (optionally gzipped) tar, but it was not one. A lone `.gz` of a single file, or a corrupt or truncated archive, lands here. Decompress the file and `ADD` it uncompressed (it then lands as a plain file), or repackage the content as a real tar / tar.gz. See [Known limitations](known-limitations.md#add-url-archive-coverage).

## Registry push/pull is anonymous although my credential helper has the login

UMF shells out to Docker credential helpers: a per-registry `credHelpers` entry, then the global `credsStore`, by executing `docker-credential-<name> get`. When the configured helper can't deliver, UMF degrades to anonymous with one warning:

```
credential helper returned no credentials; falling back to anonymous
```

Checklist:

- the `docker-credential-<name>` binary is on `PATH` for the user running `umf` (a helper that fails to spawn logs *credential helper failed to spawn*);
- the helper actually holds a login for that registry host (`echo <host> | docker-credential-<name> get`);
- a configured helper is **authoritative** for its host — UMF deliberately does not fall back to a stale inline `auths` entry when the helper fails, same as `docker login` semantics.

Full precedence: `--username`/`--password-stdin` → `UMF_REGISTRY_USERNAME`/`UMF_REGISTRY_PASSWORD` → per-registry `credHelpers` → global `credsStore` → inline base64 `auth` entry.

## A `FROM` / base-image pull fails with "unauthenticated pull rate limit"

```
error: ... You have reached your unauthenticated pull rate limit.
https://www.docker.com/increase-rate-limit
```

Docker Hub throttles **anonymous** pulls by client IP, so a first `umf build`
whose `FROM` (or `ADD --from`) resolves to a `docker.io` image can hit this on
an otherwise clean host. It is a registry policy, not a UMF fault: UMF resolved
the reference correctly and the registry refused the pull. Authenticate the
pull (a free Docker Hub account lifts the anonymous limit substantially):

```bash
printf '%s' "$DOCKERHUB_TOKEN" | umf build --username <you> --password-stdin \
  --tag local/hello:1.0 ./hello.umf
```

or export `UMF_REGISTRY_USERNAME` / `UMF_REGISTRY_PASSWORD`, or add an inline
`auth` entry for `https://index.docker.io/v1/` to `~/.docker/config.json`. A
pull-through mirror, or hosting the base on a registry without anonymous
throttling, avoids the limit entirely.

## `umf compile` rejects a non-squashfs rootfs

```
... squashfs ROOTFS partition (ext4 / erofs are not implemented); rebuild with
the default squashfs rootfs
```

The disk projector only writes a **squashfs** root partition today. If an image carries `org.imagilux.umf.rootfs.fs=ext4` (or `erofs`), `umf compile` rejects it; rebuild with the default squashfs rootfs. See [Known limitations](known-limitations.md#ext4-erofs-root-partition-at-compile).

## `umf compile` rejects the flavor

```
error: flavor `<x>` is not supported (expected `systemd-boot` or `uki`; `grub` is reserved)
```

`org.imagilux.umf.flavor` accepts `systemd-boot` (classic) or `uki`. `grub` is reserved and not yet implemented by the projector; an absent label defaults to `systemd-boot` with a warning. For the classic flavor the bootloader `.efi` is read from **inside** the image rootfs (`/usr/lib/systemd/boot/efi/<arch>.efi`); an image that ships no bootloader is an error, so switch to `flavor=uki` or install systemd-boot into the rootfs.

## `umf run --vmm=ch` port-forward: the guest gets no address, or I can't find the netns

Cloud Hypervisor port-forwarding runs the VM in a per-VM network namespace that UMF holds open by file descriptor, **not** by name. It is intentionally anonymous, so `ip netns list` will not show it and `ip netns exec <name>` cannot enter it. To inspect a running VM's namespace, go in by the cloud-hypervisor PID instead (find it with `umf ps`):

```bash
nsenter --net=/proc/<ch-pid>/ns/net ip addr        # the bridge .1/29 + tap
nsenter --net=/proc/<ch-pid>/ns/net ss -tlnp        # what is listening
```

If the guest never gets a DHCP lease, the in-namespace DHCP daemon is the place to look. `dnsmasq` is the default and must be on `PATH`; install it, or supply your own with `--dhcp-command "<argv>"` (it starts with the bridge already up at `10.70.x.1/29` and owns its own config), or pass `--dhcp-command none` and give the guest a static address. The daemon is launched best-effort and never blocks the VM spawn, so a missing one is a warning, not a failure.

## Still stuck

`umf doctor` plus the relevant `umf <command> --help` is the authoritative, current picture of what the binary supports. If a path you need is listed under [Known limitations](known-limitations.md), it is a tracked gap rather than a misconfiguration.
