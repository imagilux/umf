#!/bin/sh
# UMF installer: download a release binary, verify it, drop it on your PATH.
#
#   curl -fsSL https://raw.githubusercontent.com/imagilux/umf/main/scripts/install.sh | sh
#
# What it does:
#   1. Detects your OS and CPU architecture.
#   2. Picks the matching release tarball from GitHub Releases
#      (Linux x86_64/aarch64, gnu or musl; musl is auto-selected on a
#      musl host, e.g. Alpine).
#   3. Verifies the download's SHA-256 against the release SHA256SUMS.
#   4. Installs `umf` to /usr/local/bin (root) or ~/.local/bin (non-root).
#
# It is idempotent: re-running re-installs the requested version in place.
# On success it prints a single confirmation line and nothing else.
#
# Environment overrides (all optional):
#   UMF_VERSION       release tag to install (default: latest, e.g. v0.0.1)
#   UMF_LIBC          gnu | musl                (default: auto-detected)
#   UMF_INSTALL_DIR   directory to install into (default: see above)
#
# POSIX sh, no bashisms. Apache-2.0, (c) Gaël THEROND / Imagilux.

set -eu

REPO="imagilux/umf"
BIN="umf"

# --- output helpers --------------------------------------------------------
# Status/errors go to stderr so stdout stays clean for the success line.
say() { printf '%s\n' "$*" >&2; }
err() { printf 'umf-install: %s\n' "$*" >&2; }
die() {
	err "$*"
	exit 1
}

need() {
	command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

# --- platform detection ----------------------------------------------------
detect_os() {
	os=$(uname -s 2>/dev/null || echo unknown)
	case "$os" in
	Linux) echo linux ;;
	*)
		die "unsupported OS: $os. UMF ships Linux binaries only; \
build from source instead: https://github.com/$REPO#reference-implementation"
		;;
	esac
}

detect_arch() {
	arch=$(uname -m 2>/dev/null || echo unknown)
	case "$arch" in
	x86_64 | amd64) echo x86_64 ;;
	aarch64 | arm64) echo aarch64 ;;
	*)
		die "unsupported architecture: $arch. UMF releases cover \
x86_64 and aarch64; build from source for others: https://github.com/$REPO"
		;;
	esac
}

# gnu vs musl: prefer an explicit override, otherwise sniff the C library.
# A musl host (Alpine, distroless) reports "musl" from `ldd --version`; the
# musl build is statically linked, so it is also the safe fallback when we
# can't tell glibc apart.
detect_libc() {
	if [ -n "${UMF_LIBC:-}" ]; then
		case "$UMF_LIBC" in
		gnu | musl) echo "$UMF_LIBC" ;;
		*) die "UMF_LIBC must be 'gnu' or 'musl', got: $UMF_LIBC" ;;
		esac
		return
	fi
	if ldd --version 2>&1 | grep -qi musl; then
		echo musl
	else
		echo gnu
	fi
}

# --- install destination ---------------------------------------------------
# Root installs system-wide; an unprivileged user installs under ~/.local/bin.
# An explicit UMF_INSTALL_DIR always wins.
choose_dir() {
	if [ -n "${UMF_INSTALL_DIR:-}" ]; then
		echo "$UMF_INSTALL_DIR"
	elif [ "$(id -u)" = "0" ]; then
		echo /usr/local/bin
	else
		echo "${HOME:?HOME is unset; set UMF_INSTALL_DIR}/.local/bin"
	fi
}

# --- latest version --------------------------------------------------------
# The asset filename embeds the version, so even for "latest" we must resolve
# the concrete tag. Read it from the GitHub API redirect for /releases/latest.
resolve_version() {
	if [ -n "${UMF_VERSION:-}" ]; then
		echo "$UMF_VERSION"
		return
	fi
	# Follow the redirect /releases/latest -> /releases/tag/<tag> and read
	# the final tag out of the Location header. No jq dependency.
	tag=$(
		curl -fsSI "https://github.com/$REPO/releases/latest" 2>/dev/null |
			tr -d '\r' |
			sed -n 's#^[Ll]ocation:.*/tag/\(.*\)$#\1#p' |
			tail -n 1
	)
	[ -n "$tag" ] || die "could not determine the latest release tag; \
set UMF_VERSION explicitly (e.g. UMF_VERSION=v0.0.1)"
	echo "$tag"
}

# --- checksum verification -------------------------------------------------
# Verify <file> against SHA256SUMS, which lists "<hash>  <bare-filename>".
# Prefer sha256sum; fall back to shasum -a 256 (BSD/macOS userland).
verify_sha() {
	_file=$1
	_sums=$2
	_name=$(basename "$_file")
	if command -v sha256sum >/dev/null 2>&1; then
		# --ignore-missing: SHA256SUMS lists every target; we only fetched one.
		( cd "$(dirname "$_file")" && grep " ${_name}\$" "$_sums" | sha256sum -c --strict - ) \
			>/dev/null 2>&1 || die "checksum verification failed for $_name"
	elif command -v shasum >/dev/null 2>&1; then
		_want=$(grep " ${_name}\$" "$_sums" | awk '{print $1}')
		[ -n "$_want" ] || die "no checksum entry for $_name in SHA256SUMS"
		_have=$(shasum -a 256 "$_file" | awk '{print $1}')
		[ "$_want" = "$_have" ] || die "checksum verification failed for $_name"
	else
		die "need sha256sum or shasum to verify the download"
	fi
}

main() {
	need curl
	need tar
	need install
	need uname

	os=$(detect_os)
	arch=$(detect_arch)
	libc=$(detect_libc)
	version=$(resolve_version)
	dir=$(choose_dir)

	target="${arch}-unknown-${os}-${libc}"
	stem="${BIN}-${version#v}-${target}"
	tarball="${stem}.tar.gz"
	base="https://github.com/$REPO/releases/download/${version}"

	say "Installing ${BIN} ${version} (${target}) into ${dir}"

	tmp=$(mktemp -d "${TMPDIR:-/tmp}/umf-install.XXXXXX") ||
		die "could not create a temporary directory"
	trap 'rm -rf "$tmp"' EXIT INT TERM

	curl -fsSL "${base}/${tarball}" -o "${tmp}/${tarball}" ||
		die "download failed: ${base}/${tarball} (does ${version} ship ${target}?)"
	curl -fsSL "${base}/SHA256SUMS" -o "${tmp}/SHA256SUMS" ||
		die "download failed: ${base}/SHA256SUMS"

	verify_sha "${tmp}/${tarball}" "${tmp}/SHA256SUMS"

	# The archive extracts to a directory <stem>/ holding umf, README, LICENSE.
	tar -xzf "${tmp}/${tarball}" -C "$tmp" ||
		die "could not extract ${tarball}"
	binsrc="${tmp}/${stem}/${BIN}"
	[ -f "$binsrc" ] || die "archive did not contain ${BIN} where expected (${stem}/${BIN})"

	mkdir -p "$dir" || die "could not create install directory: $dir"
	# install(1) is atomic, sets mode 0755, and replaces any existing binary.
	install -m 0755 "$binsrc" "${dir}/${BIN}" ||
		die "could not install to ${dir}/${BIN} (permission denied? try sudo, or set UMF_INSTALL_DIR)"

	say "Installed ${BIN} to ${dir}/${BIN}"

	# Nudge the user if the install dir isn't already on PATH.
	case ":${PATH}:" in
	*":${dir}:"*) : ;;
	*) say "Note: ${dir} is not on your PATH. Add it, e.g.: export PATH=\"${dir}:\$PATH\"" ;;
	esac

	# The single line on stdout: a machine-readable confirmation.
	printf '%s\n' "${dir}/${BIN}"
}

main "$@"
