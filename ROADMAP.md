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

**Boot proof — squashfs and ext4 both reach userspace; erofs is `unproven`.**
`tests/boot_smoke.rs` projects and boots the fixture once per filesystem. It
could not be run locally (no KVM, no local kernel, and the network policy
blocks both Docker Hub's CDN and Alpine's), so it was proved in CI:

```
boot-smoke OK (squashfs): observed userspace marker "UMF-BOOT-OK-7f3a2c9d"
boot-smoke OK (ext4):     observed userspace marker "UMF-BOOT-OK-7f3a2c9d"
SKIP erofs boot: `mkfs.erofs` absent
```

erofs stays unproven until `erofs-utils` is installed in `boot-smoke.yml`;
`UMF_REQUIRE_MKFS=1` then turns that skip into a failure.

**The boot lane earned its cost three times over.** Every in-process test
passed while the disk did not boot, and each failure was a distinct thing no
build-time check could have seen:

1. **Link-time deps.** `ext4.ko` was embedded without `jbd2` / `mbcache` /
   `crc16`. It loads, every `jbd2_*` symbol resolves to nothing, the mount
   returns `EINVAL` and PID 1 dies at switch_root. The flat allowlist was not
   wrong before — `squashfs` needs no other module, so it looked correct right
   up until a second filesystem existed. Fixed by resolving `modules.dep`.
2. **A fix that silently did nothing.** That resolution keyed on paths, but
   `make-boot-fixture.sh` gunzips every `.ko.gz` without re-running `depmod`,
   so `modules.dep` still named `jbd2.ko.gz` while the file was `jbd2.ko`.
   Zero matches, zero dependencies, silent fallback to the flat list — the
   identical panic, with nothing to show the new code had run. Fixed by
   keying on the module stem, which also survives any repacking of a kernel
   artifact rather than just our own fixture.
3. **A dependency no graph can express.** `mkfs.ext4` enables `metadata_csum`
   by default, so mounting asks the *crypto API* for a `crc32c` shash by
   algorithm name. That is not a symbol reference, so it appears nowhere in
   `modules.dep` and no amount of correct resolution finds it.

The lesson worth keeping: "the driver is present" is not "the filesystem
mounts". Only a boot distinguishes them.

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

### Triaged so far

**Confirmed and fixed — symlink containment missing in `umf-builder`.** The
lead was accurate. `initrd.rs` read `bin/busybox` from the staging rootfs with
`is_file()` + `fs::read`, both symlink-following, on a tree materialized from
an untrusted image. Reproduced: a staging tree whose `bin/busybox` is a symlink
to a file outside the root produces an initramfs **containing that file's
bytes**, which then ships on the ESP of the projected disk. Fixed with
`contained_read`, the helper the SBOM scan already used.

Two things the reproduction taught that reasoning alone would not have:

- The module walk (`collect_modules_for`) is **not** vulnerable — `walkdir`
  does not follow symlinks, so a planted `.ko` is skipped by `is_file()`.
  Pinned with a guard test that fails under `follow_links(true)`, because that
  safety is incidental rather than stated.
- The first version of the leak assertion searched the **gzipped** image, so it
  could never match. It reported the escape was accepted while silently failing
  to prove the leak. Decompressing first is what turned it into evidence.

**Confirmed, needs a decision — `VOLUME` / `STOPSIGNAL` rejected though the
spec calls them inert.** `bootable/validate.rs` returns `ContainerOnlyDirective`
for `CMD`, `VOLUME` and `STOPSIGNAL`. The spec disagrees on two of the three:

| directive | spec | implementation |
| --- | --- | --- |
| `CMD` | "**rejected** at build start" — but conditioned on *"whose `ENTRYPOINT` is an init system"* (`specification.md:504`) | rejected for **every** bootable build, including an appliance, where `specification.md:502` says `CMD` supplies the binary's default arguments |
| `VOLUME` | "**Inert** for the bootable target" (`:544`) | rejected |
| `STOPSIGNAL` | "Advisory metadata … not something baked into a bootable image" (`:560`) | rejected |

So the lead's framing — *"the spec describes them as inert/advisory"* — is
right for `VOLUME` and `STOPSIGNAL` and wrong for `CMD`, which the spec does
say is rejected. The residual `CMD` gap is narrower: the implementation
over-rejects relative to the condition the spec states.

Not fixed here, because the fix direction is a design call rather than a
defect: rejecting early tells an author their directive does nothing, while the
spec's "inert" promises it is merely ignored. Whichever wins, the other text
has to change.

**Confirmed and fixed — `BootloaderUnavailable` advertised two remedies that do
not exist.** It read *"ship one in the image, install systemd-boot on the host,
or pass `--bootloader-path`"*. `resolve_bootloader`'s own doc comment says there
is **no host fallback** (the disk must be reproducible from the image alone) and
that the override argument is *"a library test seam; there is no CLI flag for
it"*. So an operator hitting this error did two things that could not help, then
searched `--help` for a flag that was never there. Now names the two real ways
out. Same class as the `umf build -t` fix in P3.1: user-facing text describing a
CLI that does not exist.

**Confirmed and fixed — `ukify` was an undocumented hard host dependency.**
`flavor=uki` shells out to `ukify` with no in-process fallback, and it appeared
**zero** times in `prerequisites.md`, the page an operator reads before
building. Now listed, with its `systemd-boot-efi` companion.

**Confirmed and fixed — cross-arch UKI rejection was undocumented.**
`umf compile` refuses a UKI for a non-host architecture, because `ukify` takes
systemd's EFI stub from the host and a foreign-arch UKI would be unbootable.
`known-limitations.md` covered cross-arch *`RUN`* but not this. Now documented,
quoting the real error text — my first draft paraphrased it and got the wording
wrong, which is the same defect class the entry is about.

**Confirmed — `--secret` rejected on bootable builds, against an explicit spec
promise.** The spec says the secret is mounted *"inside the `RUN` step's
container **or VM**"*, and its worked example is `sbsign` — Secure Boot
signing, a bootable-only workflow. The micro-VM `RUN` backend has no secret
mount, so the promised VM half is unimplemented.

The implementation gap is left open: wiring a tmpfs secret into the micro-VM
backend cannot be verified in an environment without KVM, and this session has
already shown what shipping an unverified boot-path change costs. What *is*
fixed is the error text, which said `--secret` was "only meaningful for
container builds" — telling authors their intent was wrong about a capability
the spec had promised them. It now names the path as unimplemented and points
at `known-limitations.md`, where the gap and its container-build-then-`ADD`
workaround are now recorded.

**Confirmed — the `EXPOSE` appliance limitation was inverted, in three places.**
`specification.md`, `known-limitations.md` and `.claude/CLAUDE.md` all said an
appliance bootable image *writes* `/etc/nftables.conf` and simply has no init to
load it. The build is in fact **rejected** (`ExposeUnenforceable`), before
anything is written. The `nft`-binary precondition — an init-system image whose
userland ships no `nft` is refused too — was documented nowhere, though the
error names it.

Worth noting what this was *not*: the code was correct and already had tests for
both refusals. Only the prose had drifted, in the direction that matters most —
telling an operator a build succeeds with a caveat when it actually fails. The
appliance test now also asserts nothing is written, which is the specific claim
the docs got wrong.

**Confirmed, count right and name wrong — undocumented `UMF_*` variables.**
Seven operator-facing variables had no documentation: `UMF_LAYER_CACHE`,
`UMF_OVERLAY_BACKEND`, `UMF_MAX_UNCOMPRESSED_LAYER_BYTES`,
`UMF_REGISTRY_TIMEOUT`, `UMF_RUN_CPUS`, `UMF_RUN_MEMORY_MIB`,
`UMF_RUN_TIMEOUT_SECS`. The lead's "~7" was right, but it named
`UMF_LAYER_STRATEGY`, which does not exist anywhere in the tree — the real one
is `UMF_LAYER_CACHE`. A sweep also has to discard the `UMF_*` names that are
shell variables inside the *generated* init script (`UMF_MODS`, `UMF_IP`, …)
and the CI gates (`UMF_REQUIRE_PRIVILEGED`, `UMF_REQUIRE_MKFS`), none of which
belong in operator documentation.

**Confirmed — `umf-networking` described as rootful-NAT-only in three places.**
`README.md`, `CONTRIBUTING.md` and `architecture.md` all documented one surface
where there are three. The omission that matters is the **connect-time SSRF
policy**: a security control denying host-internal destinations by default,
which a reader of those pages would not have known existed.

**Confirmed — the `.githooks/` secret-scanning hook was undiscoverable.** A
tracked `pre-commit` hook blocks a commit carrying a webmail address, private
key, cloud token or JWT. Git does not run hooks from a checkout, so it needs
`git config core.hooksPath .githooks` per clone, and `CONTRIBUTING.md` never
mentioned it. A secret that reaches history has to be rotated, not reverted,
so the cost of not knowing is asymmetric.

**Confirmed — the `compatibility.md` legend described markers the table no
longer uses** (`✓`, italics). The only `✓` left in the file was in the legend
explaining it.

**Refuted — the stale `grammar.rs` comment.** The lead says
`umf-parser/src/grammar.rs:16-18` claims `RUN --mount` options are "dropped on
the floor". That phrase appears nowhere in `umf-parser`; lines 14-20 are about
exec-vs-shell array form, and `RunMount` is parsed throughout. Either it was
already fixed or the reference was wrong. Recorded so it is not re-raised.

**Confirmed — a bootable build emits one squashed layer.** `bootable/mod.rs`
builds a single `LayerSource` from the whole staging tree and passes `&[layer]`
to `emit_image`, while the spec's L4+ rule promises one content-addressed layer
per diff-producing directive, reused on input-hash match. The container path
does assemble a chain (`assemble_layer_chain`), so this is bootable-only.
Documented rather than implemented: per-directive layering for the micro-VM
`RUN` path is architectural work, and it cannot be exercised here without KVM.

**Confirmed — `umf doctor` sample output was obsolete.** `quickstart.md`
carried a flat `name: value` listing under a *"Detected runtimes on this host"*
heading. The real report is two aligned tables (**Container build & RUN**,
**VM / bootable**) with name / purpose / path / version / status columns —
nothing about the documented shape survives. `cli.md` named a
*"Container RUN-step network egress"* section that no longer exists, and
`prerequisites.md` / `troubleshooting.md` quoted values in the old
`key: value` rendering. All corrected against real output.

**Confirmed but unfixable from this session — the subprocess-guardrail comment.**
`rust.yml:56-58` describes *"the temporary allowlist (round-trip tests scheduled
for removal)"*. `scripts/check-subprocess-calls.sh` has no round-trip-test
allowlist and nothing marked temporary; its allowlist is the per-backend spawn
helpers (`backends/qemu/spawn.rs`, `backends/cloud_hypervisor/spawn.rs`), which
are permanent by design. The fix is one comment in a workflow file, which this
session's OAuth token cannot push — it belongs with the other blocked CI work.

**Confirmed — erofs, the default layer strategy, is never exercised in CI.**
No lane installs `erofs-utils`, so `umf_oci::erofs::encoder_available()` is
false everywhere, `want_erofs` never holds, and every lane silently takes the
`Merge` fallback. No unit test drives `LayerStrategy::Erofs` either. The
fallback is deliberate and correct — erofs is an acceleration, so any failure
degrades to the merged unpack — but that is exactly what makes the gap
invisible: the lanes are green *because* the code under test never ran.

**Confirmed — the UKI unit tests always self-skip.** `boot-smoke.yml` installs
`ukify` but runs only `cargo test --test boot_smoke`; `rust.yml` runs
`cargo test --workspace`, which contains the UKI tests, but installs no
`ukify`. So the lane with the tool does not run the tests and the lane with the
tests has no tool. Same shape as the `mkfs` skip this session already fixed,
and the repo already has the remedy pattern (`UMF_REQUIRE_PRIVILEGED`).

Both fixes are apt packages in workflow files, which this session's token
cannot push. They are folded into the blocked CI patch alongside the `mkfs`
tooling so one `git am` closes all three.

**Confirmed — `v0.0.1` was tagged with zero reno notes.** `git ls-tree -r
v0.0.1 releasenotes/notes/` returns nothing, behind roughly fifteen
substantive changes including a behaviour change. Not fixable retroactively —
a published tag is immutable — so `CONTRIBUTING.md` now carries a pre-tag check
that lists notes added since the last tag.

### The remaining CI-coverage leads

All four confirmed; all four fix in workflow files, so they are recorded here
rather than attempted.

- **No lane runs a real `cloud-hypervisor` binary.** The only mention across
  every workflow is a *comment* inside `rust.yml`'s subprocess-guardrail step.
  So the CH backend — its REST control surface and the `VmNet` netns + tap +
  nft DNAT port-forwarding — is exercised only against unit tests, never the
  real VMM. Of the untested paths this is the largest: it is a whole second
  backend.
- **The root `Containerfile` and `scripts/install.sh` are never exercised.**
  Both are user-facing distribution paths. `release.yml` names `install.sh`
  only in comments about which tag becomes "Latest".
- **`boot-smoke` pulls its fixture from Docker Hub** (`alpine:3.21`, via
  `ALPINE_TAG`). That is the rate-limit exposure other lanes moved away from —
  and this session hit a harder version of it: a network policy that blocked
  Docker Hub's CDN outright made the fixture unbuildable, which is what stopped
  the ext4 boot from being proved locally.
- **The registry client is only tested against UMF's own in-process server,
  which implements no auth.** Nothing in `crates/umf-oci/src/registry/` tests
  touches `credsStore`, `credHelpers` or `~/.docker/config.json`, so the
  documented precedence chain (flags → env → docker config → helpers) has never
  been exercised against a real `401` challenge.

One of these nearly went the other way. Grepping the workflows for
`Containerfile` matches `rootless.yml` several times — but those are *recipe
fixtures the lane writes*, named `Containerfile` because that is what
`umf build` discovers, not the repository's own `Containerfile`. A keyword hit
is not evidence about what a lane does.

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

**Two security bugs found here, both failing open.** A sweep of these two crates
produced unverified leads (most of its verification stage died on a session
limit); two were checked by hand and both were real.

- **`UMF_ROOTLESS_NET` failed open on a typo.** It resolved through
  `EgressMode::from_env`, which maps an unparseable value to the default — and
  the default is `native`, *full egress*. An operator hardening a build with
  `UMF_ROOTLESS_NET=none` who typed `nonee` got the egress they were switching
  off, silently, with `umf doctor` reporting `native … ok`. The
  `--rootless-net` flag rejected the identical string, so one door failed
  closed and the other open. Fixed by giving the environment form the flag's
  parser.
- **LSM confinement was dropped from every RUN step.**
  `apply_run_spec_to_bundle` rebuilds `process` from `ProcessBuilder::default()`
  and copies fields back one by one; it copied seven and missed
  `apparmorProfile` and `selinuxLabel`, which `build_runtime_spec` sets
  immediately before from `UMF_APPARMOR_PROFILE` / `UMF_SELINUX_LABEL`. Every
  RUN step ran unconfined while the operator believed otherwise — and the loss
  is unobservable on a host with no LSM loaded, which is most CI.

Both have tests confirmed to fail when the fix is reverted. Neither was a crash
or a wrong answer: both were a security control quietly not applying, which is
the shape thin coverage is least able to catch.

**Coverage added for the gaps #39 and the sweep named** — every new test
confirmed to fail under a targeted sabotage of the code it covers, and in three
cases confirmed *not* to fail under a sabotage the property survives:

- **Secret resolution** (`resolve_secrets`): env-sourced secrets are written
  owner-only and removed when the build ends; missing sources are errors naming
  the id. #39's other two invariants — secret bytes out of the layer and out of
  the cache key — turned out to be tested already, in `run.rs` and
  `cache/tests.rs`. The explicit `0600` chmod is currently redundant (`tempfile`
  already creates files `0600`), so the test pins the *property* rather than
  that line: deleting it does not fail the test, loosening it to `0644` does.
- **UDP SSRF deny**: a datagram to loopback through the policed gateway never
  reaches a real loopback server. Removing the policy check makes it arrive.
- **`owned_netns.rs`**, previously untested: a pre-existing file and a dangling
  symlink at the pin path are both refused, and the guard removes its pin on
  drop. `O_EXCL` and `O_NOFOLLOW` each independently block the symlink attack —
  removing either alone leaves the test green, removing both fails it.
- **The zip-bomb ceiling** is cumulative: three entries each under the cap but
  over it together are refused. Dropping `remaining -= copied` — the one line
  carrying the budget between entries — fails exactly the multi-entry cases and
  nothing else.
- **boot-smoke** now fails rather than skips when `UMF_BOOT_SMOKE=1` is set and
  a prerequisite is missing. Reproduced first: with qemu hidden, the old code
  printed `SKIP` and the required check *passed*.

---

## P3 — documentation truth

### P3.1 — Docs drift · ~~open~~ fixed · [#36](https://github.com/imagilux/umf/issues/36)

Eight claims, each re-verified against the current tree rather than taken from
the issue — which was written at `8b39f60`, and two of its items had moved.

**Fixed:** the `ip` runtime requirement (`rtnetlink` replaced the `iproute2`
shell-outs, so `umf doctor` needs `nft` only); `--build-arg`, `doctor --format`
and the `-p [BIND:]HOST:GUEST` form, all absent from a `cli.md` that claims to
document every flag; `umf build -t`, used in `examples.md` but undefined, so
the documented invocation died on `unexpected argument '-t' found`;
`docs/examples.md` and `docs/examples/README.md` both rendering to
`examples/index.html`, one silently winning; a missing `site_url`, which left
`sitemap.xml` with no absolute URLs; and `umf sbom --help` still calling
`generate` future work after it shipped.

**Two items had changed since the issue was filed**, which is the argument for
re-verifying rather than working from the list:

- The workflow drift **inverted**. `rootless.yml` and `release-validate.yml`
  are now documented — but `audit.yml` is documented and does not exist. It was
  added on `6b4e673`, a branch that never merged: the CI-hardening work blocked
  on the `workflow` OAuth scope. The docs describe CI that the push wall kept
  out of the tree.
- *"`.claude/CLAUDE.md` describes `umf-networking` as reaching the CLI
  transitively"* was **already fixed**; both the `umf-builder` and `umf` bullets
  now say "directly". Left alone.

**One sub-claim was wrong.** The issue reports `umf build -t` in `README.md`;
the `-t` there is on `docker build -t umf:latest .`, which is Docker's own flag
and correct. Only `examples.md` was broken.

`bench/` is removed from the layout tree rather than created: it is in
`.gitignore`, so documenting it as repo structure describes something no clone
ever has.

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
