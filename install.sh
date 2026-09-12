#!/bin/sh
#
# Install the `mono` binary from a GitHub release.
#
#     curl -fsSL https://github.com/Tomperez98/mono/releases/latest/download/install.sh | sh
#
# Pass options after `sh -s --` when the script arrives on stdin:
#
#     curl -fsSL .../install.sh | sh -s -- --version v0.1.5
#     curl -fsSL .../install.sh | sh -s -- --prefix /usr/local
#
# The release copy carries its version and archive digest. The download is
# checked before it is unpacked, and only the `mono` binary is copied out. Mono keeps
# no state of its own, so undoing this is `rm` on the installed path.

set -eu

REPOSITORY="Tomperez98/mono"
BINARY="mono"

#region published defaults
# `xtask release-docs` replaces this region in the release asset, pinning it to
# that release and carrying the digests for its archives. The checked-in source
# remains unpinned and requires --version or MONO_VERSION.
default_version=""
default_checksums=""
#endregion

die() {
  printf 'install.sh: %s\n' "$1" >&2
  exit 1
}

note() {
  printf 'install.sh: %s\n' "$1" >&2
}

usage() {
  cat <<'EOF'
Install the mono binary from a GitHub release.

Usage: install.sh [options]

Options:
  --version <VERSION>  Install a specific release, for example v0.1.5 or 0.1.5.
                       Default: the release this script was published with.
  --prefix <DIR>       Install to <DIR>/bin.
  -h, --help           Print this help.

Environment:
  MONO_VERSION         Same as --version; useful when piping the script.
  MONO_INSTALL_DIR     Install directory, overridden by --prefix.

The script needs curl and one of sha256sum, shasum, or openssl.
EOF
}

# Map this machine onto one of the canonical release targets.
detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Linux)
      case "$arch" in
        x86_64 | amd64) printf 'x86_64-unknown-linux-gnu' ;;
        *) die "no prebuilt mono for Linux $arch" ;;
      esac
      ;;
    Darwin)
      case "$arch" in
        arm64 | aarch64) printf 'aarch64-apple-darwin' ;;
        x86_64) printf 'x86_64-apple-darwin' ;;
        *) die "no prebuilt mono for macOS $arch" ;;
      esac
      ;;
    MINGW* | MSYS* | CYGWIN*)
      die "this is the POSIX installer; on Windows run install.ps1 in PowerShell"
      ;;
    *)
      die "no prebuilt mono for $os; see https://github.com/${REPOSITORY}/releases"
      ;;
  esac
}

# Accept both `0.1.5` and `v0.1.5`; the release tag carries the prefix.
normalize_tag() {
  case "$1" in
    v*) printf '%s' "$1" ;;
    *) printf 'v%s' "$1" ;;
  esac
}

# The SHA-256 of one file, printed alone. `sha256sum` is GNU, `shasum` ships
# with macOS, and `openssl` is the fallback when neither is installed.
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{ print $1 }'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$1" | awk '{ print $NF }'
  else
    die "no sha256sum, shasum, or openssl available to verify the download"
  fi
}

# The digest a published copy of this script baked in for `target`, or nothing
# when this copy has no defaults or the caller asked for another release. The
# value is a space-separated list of `target=digest` pairs, so the unquoted
# expansion below is the word splitting this needs.
baked_checksum() {
  for entry in $default_checksums; do
    case "$entry" in
      "$1"=*) printf '%s\n' "${entry#*=}"; return 0 ;;
    esac
  done
  return 0
}

version=""
prefix=""
while [ $# -gt 0 ]; do
  case "$1" in
    -h | --help)
      usage
      exit 0
      ;;
    --version)
      [ $# -ge 2 ] || die "--version requires a value"
      version="$2"
      shift 2
      ;;
    --version=*)
      version="${1#*=}"
      shift
      ;;
    --prefix)
      [ $# -ge 2 ] || die "--prefix requires a value"
      prefix="$2"
      shift 2
      ;;
    --prefix=*)
      prefix="${1#*=}"
      shift
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

if [ -z "$prefix" ]; then
  prefix="${MONO_INSTALL_DIR:-}"
fi
if [ -z "$prefix" ]; then
  [ -n "${HOME:-}" ] || die "HOME is not set; pass --prefix or MONO_INSTALL_DIR"
  prefix="${HOME}/.local"
fi
command -v curl >/dev/null 2>&1 || die "curl is required"

target="$(detect_target)"

if [ -z "$version" ]; then
  version="${MONO_VERSION:-}"
fi
if [ -z "$version" ]; then
  version="$default_version"
fi
if [ -z "$version" ]; then
  die "this installer is not pinned; use a release asset or pass --version"
fi
tag="$(normalize_tag "$version")"

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT

base="https://github.com/${REPOSITORY}/releases/download/${tag}"
archive="${BINARY}-${tag}-${target}.tar.gz"

note "downloading ${archive}"
curl -fsSL --proto '=https' --tlsv1.2 -o "${tmp}/${archive}" "${base}/${archive}" ||
  die "could not download ${base}/${archive}"

# A published copy already carries this release's digest, so it needs neither
# the API nor a second download. Any other release fetches SHA256SUMS.
expected=""
if [ -n "$default_version" ] && [ "$tag" = "$default_version" ]; then
  expected="$(baked_checksum "$target")"
fi
if [ -n "$expected" ]; then
  note "verifying ${archive} against the checksum built into this release's installer"
else
  curl -fsSL --proto '=https' --tlsv1.2 -o "${tmp}/SHA256SUMS" "${base}/SHA256SUMS" ||
    die "could not download ${base}/SHA256SUMS"
  expected="$(awk -v name="${archive}" '$2 == name { print $1; exit }' "${tmp}/SHA256SUMS")"
fi
[ -n "$expected" ] || die "SHA256SUMS has no entry for ${archive}"
actual="$(sha256_of "${tmp}/${archive}")"
[ "$expected" = "$actual" ] ||
  die "checksum mismatch for ${archive}: expected ${expected}, got ${actual}"

tar -xzf "${tmp}/${archive}" -C "${tmp}" "${BINARY}" ||
  die "could not extract ${BINARY} from ${archive}"

mkdir -p "${prefix}/bin"
if command -v install >/dev/null 2>&1; then
  install -m 0755 "${tmp}/${BINARY}" "${prefix}/bin/${BINARY}"
else
  cp "${tmp}/${BINARY}" "${prefix}/bin/${BINARY}"
  chmod 0755 "${prefix}/bin/${BINARY}"
fi

installed="${prefix}/bin/${BINARY}"
case ":${PATH:-}:" in
  *":${prefix}/bin:"*) ;;
  *)
    note "add ${prefix}/bin to PATH, for example:"
    note "  export PATH=\"${prefix}/bin:\$PATH\""
    ;;
esac

if reported="$("${installed}" --version 2>/dev/null)"; then
  note "installed ${reported} to ${installed}"
else
  note "installed ${installed}"
fi
note "remove it with: rm '${installed}'"
