#!/usr/bin/env bash
# Scan added content for secrets / PII before it ever lands in history.
#
# Invoked by the local pre-commit hook (.githooks/pre-commit). Exits non-zero
# if any pattern matches, printing the offending file + lines.
#
# Modes:
#   --staged            scan staged additions            (pre-commit hook)
#   --range BASE HEAD   scan additions in BASE...HEAD     (a commit range)
#   --files FILE...     scan whole files
#
# Suppressing a reviewed false positive:
#   * add the path (a shell glob, repo-relative) to .secrets-allowlist, or
#   * put `pragma: allowlist secret` on the offending line, or
#   * commit with `git commit --no-verify`.
set -uo pipefail

ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
ALLOWFILE="$ROOT/.secrets-allowlist"

mode="${1:---staged}"; shift || true
base=""; head=""
case "$mode" in
  --staged) mapfile -t FILES < <(git diff --cached --name-only --diff-filter=ACM) ;;
  --range)  base="${1:?BASE required}"; head="${2:?HEAD required}"
            mapfile -t FILES < <(git diff --name-only --diff-filter=ACM "$base...$head") ;;
  --files)  FILES=("$@") ;;
  *) echo "usage: scan-secrets.sh [--staged | --range BASE HEAD | --files FILE...]" >&2; exit 2 ;;
esac

is_allowed() { # repo-relative path -> 0 when allowlisted
  [ -f "$ALLOWFILE" ] || return 1
  local pat
  while IFS= read -r pat; do
    case "$pat" in ''|\#*) continue ;; esac
    # shellcheck disable=SC2254
    case "$1" in $pat) return 0 ;; esac
  done < "$ALLOWFILE"
  return 1
}

get_content() { # path -> added/added-or-whole content for the active mode
  case "$mode" in
    --staged) git diff --cached --no-color --unified=0 -- "$1" 2>/dev/null | grep -E '^\+[^+]' || true ;;
    --range)  git diff --no-color --unified=0 "$base...$head" -- "$1" 2>/dev/null | grep -E '^\+[^+]' || true ;;
    --files)  cat -- "$1" 2>/dev/null || true ;;
  esac
}

# label||extended-regex
RULES=(
  'personal webmail address||[A-Za-z0-9._%+-]+@(gmail|hotmail|outlook|live|yahoo|ymail|proton|protonmail|icloud|aol)\.[a-z]{2,}'
  'private key material||-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----'
  'GitHub token||(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36}'
  'GitHub fine-grained PAT||github_pat_[A-Za-z0-9_]{40,}'
  'AWS access key id||AKIA[0-9A-Z]{16}'
  'Slack token||xox[abprs]-[0-9A-Za-z-]{10,}'
  'JWT / signed token||eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}'
)

fail=0
for f in "${FILES[@]:-}"; do
  [ -n "$f" ] || continue
  is_allowed "$f" && continue
  c="$(get_content "$f")"
  [ -n "$c" ] || continue
  c="$(printf '%s\n' "$c" | grep -v 'pragma: allowlist secret' || true)"
  for rule in "${RULES[@]}"; do
    label="${rule%%||*}"; rx="${rule##*||}"
    hits="$(printf '%s\n' "$c" | grep -EinI -e "$rx" || true)"
    if [ -n "$hits" ]; then
      printf '\033[31m✗ %s — %s\033[0m\n' "$f" "$label" >&2
      printf '%s\n' "$hits" | sed 's/^/    /' >&2
      fail=1
    fi
  done
done

if [ "$fail" -ne 0 ]; then
  printf '\033[31mscan-secrets: findings above. Remove them, allowlist the path in .secrets-allowlist, or use `git commit --no-verify`.\033[0m\n' >&2
  exit 1
fi
