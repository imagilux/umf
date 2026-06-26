# Contributing to UMF

Thanks for your interest in UMF. This repository holds two tightly-coupled
things in one git history: the **UMF specification** (MkDocs source under
`docs/`, published at <https://umf.imagilux.org/>) and the **reference
implementation** (a Rust Cargo workspace). They are versioned independently but
ship together.

This guide is the short version of the contributor-facing conventions. For
security reporting, see [`SECURITY.md`](SECURITY.md).

## Workflow: pull requests only

`main` is a continuous-development branch and is protected. **Never push or
commit directly to `main`.** Every change goes through a pull request:

1. Branch off `main` with a descriptive name (e.g. `fix/lexer-crlf`,
   `feat/compile-uki`).
2. Make your change, keep it focused, and add a release note (see below) when it
   warrants one.
3. Open a PR against `main`. CI must be green and the PR reviewed before merge.

When a PR closes an issue, double-check the issue number with `gh issue view N`
before writing `Closes #N` (closing the wrong issue is easy and annoying to
undo).

## Commit messages: Conventional Commits

Use [Conventional Commits](https://www.conventionalcommits.org/) for the subject
line. Common types in this repo: `feat`, `fix`, `refactor`, `chore`, `docs`,
`security`, optionally scoped by crate or area, e.g.:

```
feat(compile): project UKI images to a single-file ESP
fix(parser): handle CRLF line continuations
security(engine): re-enable seccomp on RUN steps
```

Use `!` after the type/scope (or a `BREAKING CHANGE:` footer) for breaking
changes. Keep the body short and explain the *why*.

## Release notes: reno

Binary-release notes are managed with [`reno`](https://docs.openstack.org/reno/)
(a `uv`-managed dev dependency). Any change worth a line in the next release adds
**one** YAML note under `releasenotes/notes/`, committed *with* the change:

```bash
uv run reno new <slug>     # creates releasenotes/notes/<slug>-<hash>.yaml
uv run reno report         # render accumulated notes, grouped by tag
```

A note is RST-bodied YAML using reno's standard sections (`prelude`, `security`,
`features`, `fixes`, `upgrade`, `deprecations`, `other`). Pure-internal
refactors with no user-visible effect can skip the note; user-facing changes,
fixes, and especially security or upgrade-impacting changes should always carry
one.

## Rust: the quality gate

The implementation is a Cargo workspace (edition 2024, stable toolchain pinned
in `rust-toolchain.toml`; rustup auto-installs on first use). Before you push,
all three of these must pass:

```bash
cargo fmt --all                                          # format (always)
cargo clippy --workspace --all-targets -- -D warnings    # lint, warnings denied
cargo test --workspace                                   # all tests
```

`clippy` runs with `-D warnings`, so a warning fails CI. Never finish red. When
your change is scoped to one crate you can iterate with
`cargo clippy -p <crate> --all-targets -- -D warnings` and `cargo test -p
<crate>`, but the full workspace run is what gates merge.

Shared dependencies are declared once under `[workspace.dependencies]` in the
root `Cargo.toml` and opted into per crate with `dep = { workspace = true }`;
lint policy lives in `[workspace.lints]`. Don't pin a dependency in two places.
`Cargo.lock` is committed (this repo ships a binary; reproducibility wins).

## Docs and the spec site

The MkDocs site is **uv-native**. Never reach for `pip`, `venv`, or
`requirements.txt`:

```bash
uv sync                # install from pyproject.toml + uv.lock
uv run mkdocs serve     # live preview at http://127.0.0.1:8000
uv run mkdocs build     # static build (CI / sanity check)
```

`uv.lock` is committed; `.venv/` and `site/` are gitignored. Edit
`docs/specification.md` for normative spec wording, `docs/index.md` for the
pitch/overview, `docs/compatibility.md` for the directive matrix,
`docs/examples.md` for recipes, and `docs/quickstart.md` / `docs/cli.md` /
`docs/architecture.md` for the tool docs. The spec is normative; the
implementation is canonical. Touch `crates/` and `src/` for behaviour, the docs
for wording.

A small house-style note for prose: avoid em dashes (they read as AI-generated);
prefer commas, colons, parentheses, or two sentences.

## Workspace layout

The CLI binary (`umf`) lives at the repo root (`src/`); the libraries live under
`crates/` with the `umf-` prefix (the prefix avoids shadowing `std::core` in
clap's derive macro and is a precondition for publishing on crates.io). The
dependency graph is a strict tree, no cycles:

- `umf-core`: shared types, errors, AST, the `org.imagilux.umf.*` label
  namespace. No IO. Depended on by everything.
- `umf-parser`: `&str` to AST (lexer + grammar + diagnostics). Depends only on
  `umf-core`.
- `umf-oci`: OCI primitives (manifest/config/layer emission, registry client,
  layout cache, layer materialization, archive import/export).
- `umf-networking`: NAT'd egress for container `RUN` steps (veth via
  `rtnetlink` + host `nft` masquerade).
- `umf-engine`: in-process container build + run (youki `libcontainer` +
  overlayfs), with the per-`RUN` sandbox and egress wiring.
- `umf-vmm`: VMM control layer, a `VmRuntime` trait with QEMU (QMP) and Cloud
  Hypervisor (REST) backends.
- `umf-builder`: AST to OCI image (FROM resolution + introspection, the
  container-vs-bootable decision; RUN backends, secrets, EXPOSE to nftables).
- `umf-compile`: projects a `type=bootable` OCI image into a GPT/ESP/UEFI disk
  (squashfs root, systemd-boot or UKI).

The AST lives in `umf-core`, not `umf-parser`, deliberately: it keeps the
builder/oci/engine/vmm/compile crates free of any parser dependency. See
[`docs/architecture.md`](docs/architecture.md) for the full picture.

## License

By contributing you agree that your contributions are licensed under the
project's [Apache-2.0](LICENSE) license.
