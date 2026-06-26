# Git hooks

Repo-tracked hooks that keep secrets and PII out of history. They are **opt-in**
per clone (git never auto-runs hooks from a checkout):

```bash
git config core.hooksPath .githooks
```

## `pre-commit`

Runs [`scan-secrets.sh`](./scan-secrets.sh) over your **staged** changes and
blocks the commit if it finds a personal webmail address, private key,
GitHub/AWS/Slack token, JWT, or similar before it can land in history.

Suppress a reviewed false positive by adding the path to
[`.secrets-allowlist`](../.secrets-allowlist), putting `pragma: allowlist secret`
on the line, or committing with `git commit --no-verify`.
