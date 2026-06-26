---
name: Bug report
about: Report something in the UMF CLI or build pipeline that is broken
title: ""
labels: bug
assignees: ""
---

<!--
Do NOT report security vulnerabilities here. See SECURITY.md for the private
disclosure path (GitHub security advisory or email).
-->

## What happened

A clear description of the bug and what you expected instead.

## Reproduction

Steps to reproduce, ideally with a minimal `.umf` recipe and the exact command:

```dockerfile
# minimal.umf
FROM scratch
ADD alpine:3.21 /
RUN ...
```

```bash
umf build --tag local/repro:1.0 ./minimal.umf
```

## Output / logs

Paste the relevant output. For more detail, re-run with tracing:

```bash
umf --trace-level=debug build ...
```

<details>
<summary>Logs</summary>

```
(paste here)
```

</details>

## Environment

- `umf --version`:
- OS / kernel (`uname -a`):
- For VM/compile issues, VMM and version (QEMU / Cloud Hypervisor):
- Anything else relevant (registry, rootless vs rootful, arch / `--platform`):

## Additional context

Anything else that might help track this down.
