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

### P0.1 — Sovereignty: ~~the source-build rung does not exist~~ · not a gap · [#30](https://github.com/imagilux/umf/issues/30)

**Closed as a documentation error, not implemented.** This item claimed a
missing third resolution rung. There is no such rung and there was never meant
to be one — the specification, `CLAUDE.md` and the resolver's own module header
all described a `registry → cache → source build` chain that misstated the
pillar, and this roadmap built a P0 on top of that description.

**What the pillar actually means**, per the maintainer: a *build* never
requires a registry. Every component — kernels, bootloaders, build
environments — is itself an artifact produced by an ordinary `umf build` from
its own recipe, and a build's output lands in the local cache. An air-gapped
operator therefore builds components in dependency order, each resolving the
previous from cache, contacting nothing. A registry is where finished artifacts
get published and where other nodes retrieve them — an accelerator and a
distribution mechanism, never a precondition.

**What UMF deliberately does not do** is fetch or build a component's *sources*
on the author's behalf. Where those live is the author's concern, not the
format's. A reference that is neither cached nor retrievable is an error, even
offline — not a trigger for an implicit build. An automatic source build would
turn a typo in a reference into a multi-hour compile, and would require UMF to
know where every component's sources are.

The implementation always matched this. `crates/umf-builder/tests/air_gapped_container_build.rs`
has asserted exactly it since before this item was written: *"an air-gapped
node, given a pre-populated layout, can build new images from the cached
components alone."* The `Provenance` enum having three variants —
`Override | Registry | Cache` — was correct, not incomplete.

Corrected in the spec's **Artifact Resolution** section (now `local cache →
registry`), both affected design pillars in `CLAUDE.md`, the resolver module
header, the `resolve_add` / `resolve_from_kernel` doc comments, and
`docs/examples.md`.

**The one real requirement this leaves** is that an unresolvable reference fail
*properly*. The exit code is already correct (`1`), but the message is not:
a registry transport failure renders as `OCI distribution: error sending
request for url (…)` with no cause, because `reqwest::Error`'s `Display` drops
its source chain. Offline, an operator gets no indication that DNS or the
network is the problem. Tracked as P1.5 below.

### P0.2 — The SSRF policy did not cover a bootable build's `RUN` · ~~partial~~ fixed

**Fixed.** A bootable build's `RUN` steps now egress through a tap in a network
namespace whose `forward` hook carries the same default-deny CIDR set a
container `RUN` obeys, installed by the same function so the two cannot drift.
The VMM's own user-mode stack is no longer used for this path: it is the VMM's,
not UMF's, so nothing could be enforced on it.

**This item was recorded wrongly twice before landing, which is worth keeping.**
It first claimed bootable `RUN` steps had no network at all — a misreading of
`net: None` in the micro-VM spec, which means "no pre-built `TapNet`", not "no
NIC". A module-survey agent had reported the real version (*"no umf-networking
egress and no SSRF policy"*) and it was refuted here on those wrong grounds.
The refutation was the error, not the finding. A refutation needs the same
standard of evidence as a finding.

**Privilege.** `CAP_NET_ADMIN` is now required for a bootable build's `RUN`
steps, by decision rather than by accident. There is deliberately no unpoliced
fallback: a build that cannot create the namespace fails with an actionable
error. A fallback would make the guarantee depend on how the build was
launched, which is the same as not having one. Bootable builds with no `RUN`
steps, and all container builds, are unaffected.

**Shape of the change:**

- `umf-networking` gained `VmNet::setup_policed_egress`, reusing the existing
  netns / veth / bridge / tap plumbing and swapping the DNAT ruleset for the
  masquerade + deny-set one. The deny set is installed through the same
  in-crate function the rootful container path calls, so a change to the
  container policy cannot silently leave the VM path on the old one.
- The QEMU backend now honours `spec.net` — it previously ignored it entirely —
  attaching `-netdev tap` and `setns`-ing the forked child into the namespace
  before exec, mirroring what Cloud Hypervisor already did.
- The generated run initramfs configures the NIC statically from a staged
  `.umf-net` rather than running DHCP, so the path needs no daemon in the
  namespace. DHCP remains the fallback for the user-mode-stack shape.
- DNS needed handling: switching off the user-mode stack removes QEMU's
  built-in resolver, so the guest borrows the host's nameservers (loopback
  stubs filtered). The resolver is **bind-mounted** over the rootfs copy rather
  than written, because a plain write would go through the 9p share and bake
  the build host's resolver into the image layer.

**Verification, and its limit.** The networking primitive is covered by a smoke
that reads the ruleset back from `nft` and asserts every denied CIDR is present
(with a guard that the policy is non-empty, so the assertion cannot pass
vacuously), plus teardown leak checks. The argv wiring and the generated init
script are unit-tested, including `sh -n` on the init.

What is still **not** covered is an end-to-end bootable `RUN` that actually
sends a packet — no CI lane runs a bootable `RUN` step at all (boot-smoke
compiles and boots a disk but executes none). That lane remains the missing
piece, and it is the one that would catch an integration-level mistake in this
change. Tracked with the other coverage gaps in P2.

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

### P1.2 — Extending a `type=bootable` image · ~~absent~~ fixed · [#28](https://github.com/imagilux/umf/issues/28)

**Fixed.** A bootable image is now a valid `FROM` in practice, not just per
`L0Kind::is_valid_from`. Its layers already carry the merged userland + kernel
tree, so the build lays that down with the same unpack loop a kernel artifact
uses — there was never a separate L2 step to skip, which is why the change is
smaller than the original note assumed.

**The real work was inheritance, not layers.** `pick_flavor` and
`pick_entrypoint` both silently default (`systemd-boot`, `systemd`), so a
recipe extending a `uki` / OpenRC base without restating either would have
quietly produced a `systemd-boot` / systemd image. Silence now means "keep what
the base declared"; anything the recipe states still wins. The rest of the
manifest (`kernel.release`, `kernel.vmlinuz`, `initramfs`, `rootfs.fs`) is
re-derived from the resulting tree, so it describes the extended image.

**One case is refused rather than guessed.** The boot-manifest `entrypoint`
label is coarse by design — every binary PID 1 records `appliance`, dropping
the argv — so an appliance's real entrypoint now comes from the standard OCI
`Entrypoint` field, which bootable builds did not previously set at all.
Setting it is independently more OCI-correct. A base built before that field
existed has nothing to recover, and extending one without restating
`ENTRYPOINT` fails with a message saying so: guessing yields a disk that boots
to a kernel panic with nothing naming the cause.

The other two halves of #28 — platform-aware introspection and not swallowing
introspect errors into "not bootable" — landed earlier in
[#44](https://github.com/imagilux/umf/pull/44).

**Verification.** Unit tests cover each inheritance path, including the two
refusal cases. The end-to-end test extends a real bootable image offline and
asserts the result is still `type=bootable` with the base's manifest — built
deliberately with a **`uki`** flavor and an **appliance** entrypoint, because a
`systemd-boot` base would make "inherited" and "silently re-defaulted"
indistinguishable and the assertion vacuous. Both inheritance paths were
confirmed to fail the test when broken.

### P1.3 — `ext4` / `erofs` root partitions · ~~absent~~ implemented

The roadmap framed this as "the projector implements only squashfs" — true, but
not the whole state. `rootfs.fs` was **builder-derived**, and the spec said in
as many words that a recipe cannot forge a derived key. The builder hardcoded
`squashfs`. So no author input, no build flag and no derivation rule could ever
have produced `erofs` or `ext4`: the value set in the published label table
described a capability with **no entry point at all**. Implementing the
projector side alone would have left it just as unreachable.

**The entry point is `umf compile --fs`, not a directive.** The root filesystem
is a property of the *disk*, not of the image — the layers are byte-identical
whichever one is written — so it belongs to projection, exactly like disk
geometry and VM-vs-bare-metal. The recipe still has no say, which keeps the
builder-derived rule intact rather than carving out a second exception beside
`flavor`. The label survives as the *default* for projection, so images built
before the flag existed project exactly as they always did.

**What had to become filesystem-agnostic.** The initramfs was the real coupling:
it baked the type into both its module set and `mount -t squashfs` at *build*
time. A projection-time choice would have produced ext4 bytes under an
initramfs still mounting squashfs — a disk that fails at switch_root and
nowhere earlier. It now reads `rootfstype=` back from `/proc/cmdline`, exactly
as it already read `root=` for the device, and carries the driver for all
three. Listing extra modules is free by the allowlist's own existing rationale.

**Writers.** squashfs stays in-process (`backhand`), so the default projection
still needs nothing installed on an air-gapped node. ext4 and erofs shell out
to `mkfs.ext4` / `mkfs.erofs` — neither has a mature pure-Rust writer — with
**no fallback**, unlike the erofs *layer cache*, which falls back because there
the result is identical either way. Both tools run unprivileged and preserve
ownership, modes and device nodes, so this adds no privilege requirement.

**Two bugs found while building it, both silent:**

- The `--fs` selection was missing from the **block-cache key**. `umf compile
  --fs ext4` after a plain compile would have been served the cached squashfs
  disk — a wrong artifact, not a missing one. The key is the image digest plus
  the variant, and a digest pins the label, so the override is the only free
  variable and is sufficient on its own.
- `image_bytes` for squashfs read `stream_position()` after the write, which is
  **96 bytes** regardless of image size: `backhand` seeks back to offset 0 to
  lay down the superblock last. Every size check against it silently passed and
  every size logged from it was wrong. Now tracked as a high-water mark.

**Verification.** The end-to-end test projects one image to each filesystem and
asserts the ROOTFS superblock and the loader entry's `rootfstype=` name the
same one — the lockstep invariant the design rests on. Confirmed to fail when
the cmdline is pinned to the default while the partition honours `--fs`. The
sparse copy, which is new logic and corrupts images silently if wrong, has four
tests, all confirmed to fail under two separate compiling sabotages.

**Boot proof — ext4 lands, erofs is `unproven` until CI runs.** `tests/boot_smoke.rs`
now projects and boots the fixture once per filesystem. It could not be run
here: the environment has no KVM, no local kernel, and the network policy
blocks both Docker Hub's CDN and Alpine's, so the fixture cannot be built. In
CI, `mkfs.ext4` is on the runner image and ext4 boots without further change;
`mkfs.erofs` is not, so erofs needs the `erofs-utils` install in
`boot-smoke.yml`. `UMF_REQUIRE_MKFS=1` turns the skip into a failure so the
lane cannot go green having booted only squashfs.

### P1.4 — Cross-architecture `RUN` execution · absent · documented

`--platform` resolves the right images for a foreign architecture but does not
emulate its `RUN` steps; the `binfmt_misc` + `qemu-user-static` path the spec
describes is not wired. Pairs naturally with the new aarch64 CI lane, which now
gives a place to prove it.

### P1.5 — A CLI error never printed why it failed · ~~partial~~ fixed · [#56](https://github.com/imagilux/umf/issues/56)

Every subcommand funnelled its result through one helper in `src/cli/mod.rs`,
and that helper was bounded on `std::fmt::Display`. A `Display` bound cannot
reach `source()` — so the cause chain was not *dropped by a bug*, it was
structurally unreachable. Whatever the top-level message happened to say was
the entire diagnostic the operator got.

Filed here originally as *"a registry transport failure does not say why"* —
the symptom that surfaced it. The root cause turned out to sit a layer above
the registry code, in the dispatcher every subcommand shares, so the fix is
broader than the title suggested.

UMF's own errors mostly survived this, because they are written to be
self-contained (`path does not exist: …`, `… is not in the local layout —
run umf pull …`). The failures that mattered were the ones whose real
explanation lives in a **foreign leaf**: `reqwest::Error`'s `Display` is
deliberately cause-less, so a registry transport failure rendered as the URL
and nothing else, while the actual reason — proxy tunnel refused, TLS
rejected, DNS failure — sat one or two `source()` hops below, visible only in
`RUST_LOG` debug output.

This is the error-reporting half of the sovereignty story in P0.1: an
unreachable source **should** fail, and the operator should be told which
part of reaching it failed.

**Fix.** Bound the helper on `std::error::Error` and walk `source()`,
printing each hop as an indented `caused by:` line:

```
error: build: OCI distribution: error sending request for url (https://…/manifests/1)
  caused by: client error (Connect)
  caused by: tunnel error: unsuccessful
```

The de-duplication matters as much as the walk. thiserror's `#[error("… {0}")]`
interpolates the inner error's `Display` into the parent, so a naive walk
echoes the same sentence at every level; a cause whose text already appears
above is skipped, as is an empty one (a `Display`-less wrapper would otherwise
emit a bare `caused by:`). The **first line is byte-identical to what it was
before**, deliberately: anything scraping `error: …` out of CI logs keeps
working, and the new detail is purely additive.

**Verification.** Five unit tests over hand-built error chains — a cause the
top line hides, a cause already stated above, interpolated levels collapsing
while a hidden leaf still shows, a source-less error, and an empty cause. All
five were confirmed to fail when the walk or the de-duplication is removed.
Exit codes are unchanged. Three further tests pin the classifier's block
boundary in both directions, and were each confirmed to fail under the
opposite mistake — one under a whole-stream scan, two under an error-line-only
read.

**Also fixed.** The three `is_pull_environmental` helpers that decide whether
a smoke lane reports a failure or skips it had the same defect, in a sharper
form: two of them were bounded on `Display` while looking exclusively for
substrings — `connection refused`, `name or service not known`, `network is
unreachable` — that only ever appear *in the chain*. They could not match
their own needles. They now walk `source()`; the CLI-level one reads the
`error:` line **plus its `caused by:` continuations**, and no further, since
`umf build` Debug-formats a whole chain into a `warn!` for each registry
candidate that fails before a later one succeeds — scanning the stream would
wave a genuine failure through as environmental.

One correction to what #55 predicted: walking the chain does **not** retire
the `reqwest`-phrasing needle added in #52. Behind an HTTP proxy the entire
chain reads `client error (Connect)` → `tunnel error: unsuccessful`, naming no
transport at all, so the top-line match is still the only thing that
classifies that case. It is now one entry among several rather than the only
one that can ever fire.

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
