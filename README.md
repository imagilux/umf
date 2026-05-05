# UMF — Universal Machine Format

A Dockerfile-inspired DSL for building universal bootable artifacts — VM disk images, bootc images, unikernels, and OCI containers — through a single declarative spec.

**Status**: Draft v0.1.

> **Scope**: this repository is documentation-only. It hosts the UMF specification and nothing else. Runtime, parser, and builder implementations live in separate repositories.

## Spec

The full specification lives in [`docs/index.md`](docs/index.md) and is published via [MkDocs Material](https://squidfunk.github.io/mkdocs-material/).

## Local preview

```bash
uv sync
uv run mkdocs serve
```

Then open <http://127.0.0.1:8000>.

## Build static site

```bash
uv run mkdocs build
```

Output lands in `site/`.

## Adding dependencies

```bash
uv add <package>     # adds to pyproject.toml + updates uv.lock
```

## Author

Gaël THEROND / Imagilux
