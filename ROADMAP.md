# UMF roadmap

Maintainer-facing work queue for the reference implementation, ordered by
priority. This is the companion to [`docs/known-limitations.md`](docs/known-limitations.md):
that document is user-facing and answers *"what will I hit, and what do I do
about it"*; this one answers *"what do we fix next, and why that first"*.

Status is deliberate vocabulary, used the same way throughout:

| term | meaning |
| --- | --- |
| **absent** | promised by the spec, the design pillars or the docs; no implementation exists |
| **broken** | implemented, but demonstrably produces a wrong result |
| **unproven** | implemented and plausibly correct, but nothing in CI demonstrates it — a self-skipping test counts as unproven, never as done |
| **partial** | implemented with a real, bounded limitation |

A claim only appears here once it has been **reproduced against the built
binary or the source**. Leads that have not been through that are tracked
separately in [#40](https://github.com/imagilux/umf/issues/40) and are not
roadmap items until verified — several plausible-sounding ones have already
turned out to be wrong.

---

## P0 — the product does not do what it says

These are the items where the specification, the design pillars or the CLI's
own help text describe behaviour the binary does not deliver. They rank above
everything else because each one makes a written promise false.

### P0.1 — Sovereignty: the source-build rung does not exist · absent · [#30](https://github.com/imagilux/umf/issues/30)

Design pillar 3 states that *any artifact is buildable from source on an
air-gapped node; registries and caches accelerate but are never required*. The
resolver's own module header advertises `registry → local cache → source build`.

There is no source-build rung. `Provenance` has exactly three variants —
`Override`, `Registry`, `Cache` (`crates/umf-builder/src/resolver/mod.rs:53`) —
and a tree-wide search for `SourceBuild`, `source_build` or `build_missing`
returns nothing. An internal comment already contradicts the module header,
describing the ladder as `override → cache → registry`.

Everything else on this page is a bounded gap inside a component. This is a
pillar the product does not stand on, and it is the item that decides whether
UMF is what it claims to be.

**Blocked on design input**, not on effort. Three questions have to be answered
before any code is worth writing:

1. Where is a component's source recipe declared — a label on the artifact, a
   convention in the registry namespace, or an operator-supplied manifest?
2. How is recursion bounded? A kernel artifact is itself a UMF build that
   `FROM`s a kernel-build-env, which is itself a UMF build. Depth limit, cycle
   detection, or both?
3. Opt-in or automatic? An automatic source build turns a typo in a reference
   into a multi-hour compile; an explicit `--build-missing` keeps the failure
   fast but means the air-gapped case needs a flag.

### P0.2 — The SSRF policy does not cover a bootable build's `RUN` · partial · unfiled

**This item was previously recorded wrongly, and the correction matters more
than the original claim.** It said bootable `RUN` steps had no network at all,
reading `net: None` in the micro-VM spec as "no NIC". That field means only "no
pre-built `TapNet`", which is the Cloud Hypervisor port-forward path; its own
doc says so. The QEMU backend attaches `-netdev user,id=net0` plus a
`virtio-net-pci` **unconditionally** (verified by dumping the generated
argv for a micro-VM spec), and the generated run initramfs brings `eth0` up and
runs `udhcpc`. Bootable `RUN` steps have had working network access all along.

A module-survey agent reported the real version of this — *"bootable-build
micro-VM RUN steps get no umf-networking egress and no SSRF policy"* — and it
was refuted here on the wrong grounds. That refutation was the error.

**The actual gap.** Bootable `RUN` egress is not policed. Both container paths
enforce the default-deny SSRF set — the rootful path turns the policy into a
`forward`-hook drop set (`crates/umf-networking/src/lib.rs:231`), the rootless
path checks each connect in the smoltcp gateway — while the micro-VM reaches
the network through the VMM's own user-mode stack, which UMF does not program.
So a bootable `RUN` can reach host services, the cloud-metadata endpoint and
the local network, and `--rootless-net-allow` has no effect on it.

**Done so far:** the specification's closing claim (*"outbound egress from a RUN
step is namespaced, and host-internal destinations are denied by default"*) was
false for this shape and now scopes itself to container builds; the limitation
is documented; and `umf build` warns once per run when it applies, so an
operator is not silently unprotected.

**Enforcement is still open**, and deliberately not bundled with the above. The
design is worked out:

- The micro-VM needs a netns + tap whose `forward` hook carries the same
  `deny_cidrs` set `ContainerNet` already builds from the policy, plus a
  masquerade for outbound. `VmNet` has the netns/veth/bridge/tap half but does
  DNAT only — no masquerade, no deny chain — so this is a new composition of
  existing pieces rather than new primitives.
- The QEMU backend must honour `spec.net`, which it currently ignores entirely
  (Cloud Hypervisor already honours it: tap + `setns` before exec).
- There is a privilege trade-off to settle first. A tap needs `CAP_NET_ADMIN`,
  which a bootable build does not otherwise require — KVM needs group access to
  `/dev/kvm`, not root. Enforcing unconditionally would regress unprivileged
  bootable builds; falling back to the unpoliced stack makes the guarantee
  privilege-dependent, which is what the container path already does but should
  be a conscious choice rather than an accident.

**Two things block starting it**, and both are worth stating plainly:

1. The privilege question above is a policy call, not an implementation detail.
2. **Nothing can verify it.** No CI lane runs a bootable `RUN` step at all — the
   boot-smoke lane compiles and boots a disk but executes no build-time `RUN` —
   so a tap + nft egress path would ship untested. That is precisely how the
   VmNet smoke came to be broken and invisible (#32). A lane that exercises a
   bootable `RUN` should land before, or with, the enforcement.

### P0.3 — Parser rejects and silently mangles ordinary Docker-compatible input · ~~broken~~ fixed

The DSL is the product's front door, and four defects sat in it.

**Fixed.** All four were reproduced against the built binary, fixed, and each
is now covered by a regression test verified to fail without its fix.

| input | was | now |
| --- | --- | --- |
| `ENV USER=app` / `ARG USER=app` | **rejected** — *"expected key after ENV"* | accepted |
| `ADD --chown=1000:1000 ./f /f` | parsed, flag **silently dropped** | refused with a hint |
| `ADD --chmod=755 ./f /f` | parsed, flag **silently dropped** | refused with a hint |
| `RUN curl http://x/a#frag` | became `curl http://x/a` | preserved |
| `RUN --network=none echo hi` | command became `--network=none echo hi` | refused with a hint |

The keyword case affected `ENV` and `ARG`. An earlier draft of this roadmap
also listed `LABEL`; that was wrong. `LABEL USER=x` is refused by the OCI
label-key grammar, which requires a lowercase first character — `LABEL user=x`
has always worked, and the refusal is correct.

Two design notes worth keeping:

- `#` is now literal only inside a **shell payload** (`RUN` / `CMD` /
  `ENTRYPOINT` shell form); a trailing comment on a structured directive is
  still stripped. Dockerfile has no inline comments at all, so this is
  deliberately more permissive than Docker while losing nothing: an unquoted
  `#` in a payload already opens a comment *to the shell*, so passing it
  through changes nothing for `RUN foo # note` and preserves fragments and
  colour literals.
- Unknown long options are now **refused** rather than dropped for
  forward-compatibility. Silent forward-compat is the wrong trade for a flag
  that changes the produced image: refusing is recoverable, shipping the wrong
  ownership is not.

---

## P1 — correctness and resource defects

### P1.1 — Every VM spawn leaks a tempdir · ~~broken~~ fixed

**Fixed.** Both backends detached their scratch directories with
`TempDir::keep()` and nothing ever removed them — the QMP/API socket's parent
in each, plus the writable UEFI VARS copy under QEMU. The in-code justification
was that the OS reclaims them at process exit, which is not true of the system
temp directory. A bootable build spawns one micro-VM per `RUN` step, so the
leak grew with recipe length.

`VmHandle` now **owns** the `TempDir` values instead, so `TempDir`'s own `Drop`
performs the cleanup. That was preferred over the `Drop for VmHandle` the old
comment anticipated: a hand-written `remove_dir_all` is a second place for the
path logic to be wrong, whereas ownership makes the cleanup structural. The
field is private, so nothing outside the crate can detach it and reintroduce
the leak.

Verified two ways: a unit test that fails if the directories survive the
handle, and a before/after count across the CLI suite showing no directories
left behind.

### P1.2 — Extending a `type=bootable` image · absent · [#28](https://github.com/imagilux/umf/issues/28)

The spec says a bootable image is a valid `FROM` and that extending it keeps it
bootable. `L0Kind::is_valid_from` agrees; the build pipeline rejects it
explicitly. The shape detection was fixed so the rejection is now loud and
accurate rather than silently producing a broken container — but the capability
itself is still unbuilt.

Making it work means more than accepting the base: L2 would otherwise reinstall
a kernel the base already carries, and the base's boot-manifest labels
(`flavor`, `entrypoint`, `kernel.*`) are not inherited.

### P1.3 — `ext4` / `erofs` root partitions · absent · documented

The boot-manifest label table lists `squashfs`, `erofs` and `ext4` as the
`rootfs.fs` value set. The projector implements only squashfs and refuses the
other two (`crates/umf-compile/src/image.rs:117`). Either implement them or
narrow the spec's value set.

### P1.4 — Cross-architecture `RUN` execution · absent · documented

`--platform` resolves the right images for a foreign architecture but does not
emulate its `RUN` steps; the `binfmt_misc` + `qemu-user-static` path the spec
describes is not wired. Pairs naturally with the new aarch64 CI lane, which now
gives a place to prove it.

---

## P2 — verification debt

Work that does not change behaviour but changes how much of the behaviour is
believable.

### P2.1 — Triage the unverified lead backlog · [#40](https://github.com/imagilux/umf/issues/40)

A module survey produced ~70 candidate findings across six crates. Its
adversarial-verification stage did not complete, so **none of them are
confirmed**. Spot-checking the six highest-severity claims by hand refuted five
of them — including two that read as serious bugs and one that had already been
fixed. Treat the list as leads, and promote an item to this roadmap only after
reproducing it.

Refuted so far, recorded so they are not re-raised: `prune_erofs_cache` evicting
live entries (a guard test covers exactly that case); Ctrl-C on `umf run --vmm`
SIGKILLing the guest (fixed in #47); quoted `#` truncating a `RUN` payload;
backslashes being a lexical error.

One entry moved the other way. *"Bootable-build micro-VM RUN steps get no
umf-networking egress and no SSRF policy"* was refuted here on the grounds that
the micro-VM had no NIC to police. That reasoning was wrong — it does have one —
and the finding itself was correct. It is now P0.2. The lesson generalises: a
refutation needs the same standard of evidence as a finding, and a plausible
mechanism is not evidence.

### P2.2 — Close the remaining CI coverage gaps · [#31](https://github.com/imagilux/umf/issues/31) [#32](https://github.com/imagilux/umf/issues/32) [#33](https://github.com/imagilux/umf/issues/33) [#34](https://github.com/imagilux/umf/issues/34)

The workflow changes for all four are written and validated but not yet on
`main`: they need a push from an account with the `workflow` scope. Until they
land, nothing executes on aarch64, two integration tests still assert nothing in
any lane, and advisories are still only evaluated when someone happens to push.

That last one is not theoretical. Two advisories have now landed on a green
`main` between pushes — RUSTSEC-2026-0258 (h2) and RUSTSEC-2026-0285 (rustls) —
each discovered by an unrelated PR rather than by a lane that was watching.

### P2.3 — Test coverage where it is thinnest · [#39](https://github.com/imagilux/umf/issues/39)

Coverage is inverted with respect to risk. `umf-engine` and `umf-networking` do
the most privileged, least reversible work and carry the fewest tests per
thousand lines (14 and 12, against 46 for `umf-parser`). That is also where the
real bugs have actually been found.

---

## P3 — documentation truth

### P3.1 — Docs drift · [#36](https://github.com/imagilux/umf/issues/36)

Six verified inaccuracies are already tracked. Two more found since:

- `.claude/CLAUDE.md` describes `umf-networking` as reaching the CLI
  transitively through `umf-engine`, and omits it from `umf-builder` entirely.
  Both depend on it **directly** (`crates/umf-builder/src/engine_build/fetch.rs:45`).
- `umf sbom --help` still reads *"Attach (and, later, generate)"*, though
  `umf sbom generate` is implemented and wired.

Both are small, but architecture docs that misstate the dependency graph are
the kind of thing a new contributor trusts.

---

## Recently closed

| item | closed by |
| --- | --- |
| RUSTSEC-2026-0285 (rustls TLS 1.3 handshake) | [#49](https://github.com/imagilux/umf/pull/49) |
| libseccomp tarball shipped unverified into every musl binary | [#35](https://github.com/imagilux/umf/issues/35) → #49 |
| VmNet smoke asserting against the wrong namespace | #49 |
| Initramfs unable to find a non-virtio root device | [#48](https://github.com/imagilux/umf/pull/48) |
| Ctrl-C on `umf run` leaking netns / tap / nft | [#47](https://github.com/imagilux/umf/pull/47) |
| `LABEL` / `ENV` dropped from bootable images | [#46](https://github.com/imagilux/umf/pull/46) |
| `RUN` exec form silently degraded to shell form | [#45](https://github.com/imagilux/umf/pull/45) |
| `FROM` shape detection not platform-aware | [#44](https://github.com/imagilux/umf/pull/44) |
| Layer codec accept-sets drifted between reader and writer | [#43](https://github.com/imagilux/umf/pull/43) |
| `ADD <url>` bypassing the SSRF egress policy on redirect | [#42](https://github.com/imagilux/umf/pull/42) |
| OCI whiteout emission | [#41](https://github.com/imagilux/umf/pull/41) |
