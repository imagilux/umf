# Changelog

UMF does not keep a hand-edited changelog. Release notes are generated from
per-change [`reno`](https://docs.openstack.org/reno/) notes that live in
`releasenotes/notes/` and are aggregated by git tag.

To render the accumulated notes grouped by release:

```bash
uv run reno report
```

Notes committed after the last `vX.Y.Z` tag belong to the next release; tagging
freezes them into that version.

## Where to find releases

- **Binary releases** (the `umf` CLI) are published as
  [GitHub Releases](https://github.com/imagilux/umf/releases) on `vX.Y.Z` tags,
  with the reno-rendered notes attached.
- **Specification revisions** are published to the versioned docs site at
  <https://umf.imagilux.org/> on `spec-vX.Y[.Z]` tags (the two release tracks
  are intentionally decoupled).

## Adding to the changelog

Don't edit this file. When you make a change worth a release line, add a reno
note with it (see [`CONTRIBUTING.md`](CONTRIBUTING.md)):

```bash
uv run reno new <slug>
```
