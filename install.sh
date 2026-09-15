#!/bin/sh
# Oro installer. Detects your OS/arch, downloads the matching release archive,
# verifies its SHA256, and installs the `oro` binary. POSIX sh; works with curl
# or wget; no bash features.
#
#   curl -fsSL https://raw.githubusercontent.com/qrazil/oro/main/install.sh | sh
#
# Environment overrides:
#   ORO_VERSION=v0.2.0        pin a specific release (default: latest)
#   ORO_INSTALL_DIR=/opt/bin  install location (default: ~/.local/bin)
#   ORO_BASE_URL=...          where to fetch the archive and SHA256SUMS from,
#                             instead of this release's GitHub download URL. A
#                             mirror, or `file:///path` to a local directory —
#                             which is how the installer is tested against a
#                             locally-built archive without a release existing.
#
# ─────────────────────────────────────────────────────────────────────────────
# The published repo. Change these three if the project moves.
GITHUB_OWNER="qrazil"
GITHUB_REPO="oro"
BIN_NAME="oro"
# ─────────────────────────────────────────────────────────────────────────────

set -eu

# --- output helpers ----------------------------------------------------------
info() { printf '%s\n' "$*" >&2; }
err() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

# --- http: prefer curl, fall back to wget ------------------------------------
# All downloads are of PUBLIC release assets, so there is no authentication
# anywhere here — no token, no Authorization header. Do not add one.
#
# download <url> <path>  -> body to a file
# latest_tag             -> the newest release's tag, via the /releases/latest
#                           redirect (unauthenticated, and not subject to the
#                           GitHub API's 60-req/hour-per-IP limit that hitting
#                           /repos/.../releases/latest would be).
if command -v curl >/dev/null 2>&1; then
    download() { curl -fsSL "$1" -o "$2"; }
    latest_tag() {
        # Follow the redirect and read the tag off the final URL.
        eff=$(curl -fsSL -o /dev/null -w '%{url_effective}' \
            "https://github.com/${GITHUB_OWNER}/${GITHUB_REPO}/releases/latest") || return 1
        case "$eff" in */tag/*) printf '%s' "${eff##*/tag/}" ;; *) return 1 ;; esac
    }
elif command -v wget >/dev/null 2>&1; then
    download() { wget -qO "$2" "$1"; }
    latest_tag() {
        # wget cannot print the effective URL, so read the first redirect's
        # Location header (`-S` prints headers to stderr; `--max-redirect=0`
        # stops before following it).
        loc=$(wget -S --max-redirect=0 -O /dev/null \
            "https://github.com/${GITHUB_OWNER}/${GITHUB_REPO}/releases/latest" 2>&1 \
            | awk 'tolower($1) == "location:" { print $2 }' | tr -d '\r' | head -n 1)
        case "$loc" in */tag/*) printf '%s' "${loc##*/tag/}" ;; *) return 1 ;; esac
    }
else
    err "need curl or wget to download Oro"
fi

# --- map uname to a Rust target triple ---------------------------------------
detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Linux) plat="unknown-linux-musl" ;;
        Darwin) plat="apple-darwin" ;;
        *) err "unsupported OS '$os' — Oro ships prebuilt binaries for Linux and macOS only" ;;
    esac
    case "$arch" in
        x86_64 | amd64) cpu="x86_64" ;;
        aarch64 | arm64) cpu="aarch64" ;;
        *) err "unsupported architecture '$arch' — Oro ships x86_64 and aarch64/arm64 builds" ;;
    esac
    printf '%s-%s' "$cpu" "$plat"
}

# --- resolve the release tag -------------------------------------------------
resolve_version() {
    if [ -n "${ORO_VERSION:-}" ]; then
        printf '%s' "$ORO_VERSION"
        return
    fi
    tag=$(latest_tag) || tag=""
    # Validate it looks like a version tag, so a redirect that went somewhere
    # unexpected (a rate-limit page, a moved repo) is a clear error rather than a
    # garbage version that 404s the download a step later.
    case "$tag" in
        v[0-9]* | [0-9]*) printf '%s' "$tag" ;;
        *) err "could not resolve the latest release of ${GITHUB_OWNER}/${GITHUB_REPO} — pin one instead, e.g. ORO_VERSION=v0.2.0" ;;
    esac
}

# --- sha256 of a file (portable) ---------------------------------------------
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        err "need sha256sum or shasum to verify the download"
    fi
}

# --- choose an install directory ---------------------------------------------
# Prefer a user-writable dir (no sudo); fall back to /usr/local/bin with sudo.
choose_install_dir() {
    if [ -n "${ORO_INSTALL_DIR:-}" ]; then
        printf '%s' "$ORO_INSTALL_DIR"
        return
    fi
    printf '%s' "${HOME}/.local/bin"
}

main() {
    target=$(detect_target)
    version=$(resolve_version)
    info "Installing ${BIN_NAME} ${version} for ${target}"

    base="${ORO_BASE_URL:-https://github.com/${GITHUB_OWNER}/${GITHUB_REPO}/releases/download/${version}}"
    archive="${BIN_NAME}-${version}-${target}.tar.gz"

    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t oro)
    # Clean up on any exit.
    trap 'rm -rf "$tmp"' EXIT INT TERM

    info "Downloading ${archive}"
    download "${base}/${archive}" "${tmp}/${archive}"
    download "${base}/SHA256SUMS" "${tmp}/SHA256SUMS"

    # Verify the archive against the published SHA256SUMS (never skipped).
    expected=$(grep " ${archive}\$" "${tmp}/SHA256SUMS" | awk '{print $1}' | head -n 1)
    [ -n "$expected" ] || err "no checksum for ${archive} in SHA256SUMS"
    actual=$(sha256_of "${tmp}/${archive}")
    if [ "$expected" != "$actual" ]; then
        err "checksum mismatch for ${archive}
  expected: ${expected}
  actual:   ${actual}
Refusing to install a file that does not match the published checksum."
    fi
    info "Checksum OK"

    # Unpack. The archive holds a single directory containing the binary.
    ( cd "$tmp" && tar -xzf "$archive" )
    binpath=$(find "$tmp" -type f -name "$BIN_NAME" ! -name '*.tar.gz' | head -n 1)
    [ -n "$binpath" ] || err "archive did not contain a '${BIN_NAME}' binary"
    chmod +x "$binpath"

    # Install, preferring a no-sudo location.
    dir=$(choose_install_dir)
    mkdir -p "$dir" 2>/dev/null || true
    if [ -d "$dir" ] && [ -w "$dir" ]; then
        cp "$binpath" "${dir}/${BIN_NAME}"
    elif [ -z "${ORO_INSTALL_DIR:-}" ]; then
        # Couldn't write ~/.local/bin; fall back to a system dir with sudo.
        dir="/usr/local/bin"
        info "~/.local/bin is not writable; installing to ${dir} (may prompt for sudo)"
        if command -v sudo >/dev/null 2>&1; then
            sudo mkdir -p "$dir"
            sudo cp "$binpath" "${dir}/${BIN_NAME}"
        else
            err "cannot write to ${dir} and sudo is unavailable — set ORO_INSTALL_DIR to a writable directory"
        fi
    else
        err "install directory '${dir}' is not writable"
    fi

    installed="${dir}/${BIN_NAME}"
    info "Installed ${installed}"

    # Verify it runs before declaring success.
    if ! "$installed" --version >/dev/null 2>&1; then
        err "the installed binary did not run (${installed} --version failed)"
    fi
    info "Verified: $("$installed" --version)"

    # Tell the user how to put it on PATH — do not edit their rc files.
    case ":${PATH}:" in
        *":${dir}:"*) : ;;
        *)
            info ""
            info "${dir} is not on your PATH. Add it with:"
            info ""
            info "    export PATH=\"${dir}:\$PATH\""
            info ""
            info "(add that line to your shell's startup file to make it permanent)"
            ;;
    esac

    info "Done. Run '${BIN_NAME} --version' to check."
}

main
