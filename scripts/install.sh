#!/bin/sh
# Install ThinkWatch Core on a Linux server, as a systemd service.
#
#   curl -fsSL https://raw.githubusercontent.com/ThinkWatchProject/ThinkWatch-Core/main/scripts/install.sh | sudo sh
#   ... | sudo sh -s -- --version 0.47.0
#
# What it does, and the rest of the setup: docs/server.md.
#
# Running it again is safe. The binary and the unit are replaced; the
# configuration, /etc/thinkwatch/env and the data are never touched. The
# service is not started or restarted: that is left to the person running it.
#
# Options:
#   --version X.Y.Z   install this release instead of the latest
#   --archive FILE    install from a downloaded twcore-<target>.tar.gz; its
#                     FILE.sha256 is checked when it is next to it
#
# POSIX sh on purpose: it runs before anything else is installed.

set -eu

REPO="ThinkWatchProject/ThinkWatch-Core"
BIN_DIR="/usr/local/bin"
USER_NAME="thinkwatch"
DATA_DIR="/var/lib/thinkwatch"
ETC_DIR="/etc/thinkwatch"
UNIT="/etc/systemd/system/twcore.service"

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

VERSION=""
ARCHIVE=""
while [ $# -gt 0 ]; do
    case "$1" in
        --version) [ $# -ge 2 ] || die "--version needs a value"; VERSION="${2#v}"; shift 2 ;;
        --version=*) VERSION="${1#--version=}"; VERSION="${VERSION#v}"; shift ;;
        --archive) [ $# -ge 2 ] || die "--archive needs a file"; ARCHIVE="$2"; shift 2 ;;
        --archive=*) ARCHIVE="${1#--archive=}"; shift ;;
        -h|--help)
            say "usage: install.sh [--version X.Y.Z] [--archive twcore-<target>.tar.gz]"
            say "Installs twcore into $BIN_DIR and a systemd unit. Guide: docs/server.md"
            exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
done

# ── Where we are ──────────────────────────────────────────────────────

[ "$(uname -s)" = Linux ] || die "this script installs the Linux server build. On macOS and Windows, core comes with the desktop app."
[ "$(id -u)" -eq 0 ] || die "run it as root (with sudo): it installs into $BIN_DIR and creates a system user."

case "$(uname -m)" in
    x86_64|amd64) TARGET="x86_64-unknown-linux-gnu" ;;
    aarch64|arm64) TARGET="aarch64-unknown-linux-gnu" ;;
    *) die "there is no build for $(uname -m); builds exist for x86_64 and aarch64." ;;
esac
ASSET="twcore-$TARGET.tar.gz"

if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    die "neither sha256sum nor shasum is installed, so the download cannot be verified."
fi

fetch() { # URL FILE
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 3 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        die "neither curl nor wget is installed."
    fi
}

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

# ── Get the archive and check it ──────────────────────────────────────

if [ -n "$ARCHIVE" ]; then
    [ -f "$ARCHIVE" ] || die "$ARCHIVE does not exist."
    cp "$ARCHIVE" "$TMP/$ASSET"
    if [ -f "$ARCHIVE.sha256" ]; then
        cp "$ARCHIVE.sha256" "$TMP/$ASSET.sha256"
    else
        say "note: no $ARCHIVE.sha256 next to the archive; it is installed unverified."
    fi
else
    if [ -n "$VERSION" ]; then
        BASE="https://github.com/$REPO/releases/download/v$VERSION"
    else
        BASE="https://github.com/$REPO/releases/latest/download"
    fi
    say "downloading $ASSET (${VERSION:-latest release})"
    fetch "$BASE/$ASSET" "$TMP/$ASSET" \
        || die "could not download $BASE/$ASSET. Check the version, and that this machine reaches github.com."
    fetch "$BASE/$ASSET.sha256" "$TMP/$ASSET.sha256" \
        || die "could not download $BASE/$ASSET.sha256."
fi

if [ -f "$TMP/$ASSET.sha256" ]; then
    WANT=$(cut -d' ' -f1 "$TMP/$ASSET.sha256")
    GOT=$(sha256 "$TMP/$ASSET")
    [ "$WANT" = "$GOT" ] || die "the SHA-256 of $ASSET is $GOT, and the release says $WANT. Nothing was installed."
    say "SHA-256 checked: $GOT"
fi

tar -xzf "$TMP/$ASSET" -C "$TMP" || die "$ASSET could not be unpacked."
SRC="$TMP/twcore-$TARGET"
[ -f "$SRC/twcore" ] || die "$ASSET does not contain twcore-$TARGET/twcore."

# Does it run here? A glibc older than the build's shows up now rather than
# as a service that never starts.
NEW_VERSION=$("$SRC/twcore" --version 2>&1) \
    || die "the downloaded twcore does not run on this machine: $NEW_VERSION (it needs glibc 2.35 or newer)."

# ── Install ───────────────────────────────────────────────────────────

# Next to the old one and renamed over it: a running service keeps its
# binary, and there is never a half-written file at the real path.
install -d -m 0755 "$BIN_DIR"
install -m 0755 "$SRC/twcore" "$BIN_DIR/.twcore.new"
mv -f "$BIN_DIR/.twcore.new" "$BIN_DIR/twcore"
say "installed $BIN_DIR/twcore ($NEW_VERSION)"

if ! id "$USER_NAME" >/dev/null 2>&1; then
    NOLOGIN=$(command -v nologin 2>/dev/null || echo /bin/false)
    useradd --system --user-group --home-dir "$DATA_DIR" --no-create-home \
        --shell "$NOLOGIN" --comment "ThinkWatch Core" "$USER_NAME" \
        || die "could not create the user $USER_NAME."
    say "created the system user $USER_NAME"
fi
install -d -m 0700 -o "$USER_NAME" -g "$USER_NAME" "$DATA_DIR"

install -d -m 0755 "$ETC_DIR"
if [ ! -e "$ETC_DIR/env" ]; then
    {
        say "# Environment of the twcore service, one NAME=value per line."
        say "# \${NAME} in $DATA_DIR/config.yaml reads it. Restart the service after a change."
        say "#ANTHROPIC_API_KEY=sk-ant-..."
        say "#HTTPS_PROXY=http://proxy.example.com:3128"
    } > "$ETC_DIR/env"
    chown "root:$USER_NAME" "$ETC_DIR/env"
    chmod 0640 "$ETC_DIR/env"
    say "created $ETC_DIR/env"
fi

HAS_SYSTEMD=0
if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then
    HAS_SYSTEMD=1
fi
if [ -f "$SRC/twcore.service" ]; then
    install -d -m 0755 "$(dirname "$UNIT")"
    install -m 0644 "$SRC/twcore.service" "$UNIT"
    say "installed $UNIT"
    if [ "$HAS_SYSTEMD" = 1 ]; then
        systemctl daemon-reload
    fi
fi

as_service_user() {
    if command -v runuser >/dev/null 2>&1; then
        runuser -u "$USER_NAME" -- env THINKWATCH_HOME="$DATA_DIR" "$@"
    else
        # 单引号是有意的：$0、$@ 由 su 起的那个 shell 展开
        # shellcheck disable=SC2016
        su -s /bin/sh "$USER_NAME" -c 'THINKWATCH_HOME="$0" exec "$@"' "$DATA_DIR" "$@"
    fi
}

if [ ! -e "$DATA_DIR/config.yaml" ]; then
    say ""
    as_service_user "$BIN_DIR/twcore" init
fi

# ── What next ─────────────────────────────────────────────────────────

RUNNING=0
if [ "$HAS_SYSTEMD" = 1 ] && systemctl is-active --quiet twcore 2>/dev/null; then
    RUNNING=1
fi

say ""
if [ "$RUNNING" = 1 ]; then
    say "The service is running the previous version. To switch to $NEW_VERSION:"
    say "  sudo systemctl restart twcore"
else
    say "Next:"
    say "  1. Edit $DATA_DIR/config.yaml (sudoedit works): set listen.gateway.bind to all,"
    say "     enable listen.control.remote, and list your networks in both allow_from."
    say "     Keys for \${NAME} go in $ETC_DIR/env."
    say "  2. Check it:   sudo -u $USER_NAME THINKWATCH_HOME=$DATA_DIR twcore check"
    if [ "$HAS_SYSTEMD" = 1 ]; then
        say "  3. Start it:   sudo systemctl enable --now twcore"
    else
        say "  3. This machine is not running systemd; start it with:"
        say "       sudo -u $USER_NAME THINKWATCH_HOME=$DATA_DIR twcore serve"
    fi
    say "  4. Get the key for the desktop app:"
    say "       sudo -u $USER_NAME THINKWATCH_HOME=$DATA_DIR twcore control-key"
fi
say ""
say "Guide: https://github.com/$REPO/blob/main/docs/server.md"
