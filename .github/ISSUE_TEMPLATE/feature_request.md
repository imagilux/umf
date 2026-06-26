---
name: Feature request
about: Propose a change to the UMF spec or the reference implementation
title: ""
labels: enhancement
assignees: ""
---

## Problem / motivation

What are you trying to do, and what makes it hard or impossible today? Describe
the use case, not just the proposed solution.

## Proposed change

What you would like to see. If it touches the DSL or directive semantics, sketch
the recipe:

```dockerfile
FROM scratch
# proposed directive / behaviour
```

## Scope

Which part does this affect (check what applies)?

- [ ] Specification (`docs/`, normative wording)
- [ ] Reference implementation (`crates/` / `src/`, behaviour)
- [ ] CLI surface / flags
- [ ] Docs / examples only

## Alternatives considered

Other approaches you weighed, and why this one. UMF deliberately rejects some
complexity (LVM/RAID-era disk shaping, per-target switches, Docker assumptions
that mislead); note if your request brushes against those.

## Additional context

Links, prior art, or anything else.
