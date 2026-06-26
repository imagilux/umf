<!--
Thanks for contributing to UMF! Keep PRs focused. See CONTRIBUTING.md for the
full workflow.
-->

## Summary

What does this PR change, and why?

Closes #<!-- issue number; verify with `gh issue view N` before pasting -->

## Type of change

- [ ] Bug fix
- [ ] New feature
- [ ] Breaking change (note `!` / `BREAKING CHANGE:` in the commit)
- [ ] Refactor / internal
- [ ] Docs / spec

## Checklist

- [ ] Subject line follows [Conventional Commits](https://www.conventionalcommits.org/)
      (e.g. `fix(parser): ...`).
- [ ] Added a [reno](https://docs.openstack.org/reno/) release note when the
      change is user-facing (`uv run reno new <slug>`), or this is a
      pure-internal change that needs none.
- [ ] `cargo fmt --all` is clean.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] `cargo test --workspace` passes (tests green, not red).
- [ ] Docs touched? `uv run mkdocs build` succeeds.

## Notes for reviewers

Anything that needs extra attention (security-sensitive paths, boot/compile
behaviour, cache-key impact, ...).
