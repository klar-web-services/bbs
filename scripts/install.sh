#!/bin/sh
set -eu

BBS_REPOSITORY=${BBS_REPOSITORY:-klar-web-services/bbs}
BBS_INSTALL_DIR=${BBS_INSTALL_DIR:-"$HOME/.local/bin"}
BBS_VERSION=${BBS_VERSION:-}

need() { command -v "$1" >/dev/null 2>&1 || { echo "bbs installer requires $1" >&2; exit 1; }; }
need curl

case "$(uname -s)" in
  Linux) bbs_os=unknown-linux-gnu ;;
  Darwin) bbs_os=apple-darwin ;;
  *) echo "unsupported operating system: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64) bbs_arch=x86_64 ;;
  arm64|aarch64) bbs_arch=aarch64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

if [ -z "$BBS_VERSION" ]; then
  BBS_VERSION=$(curl -fsSL "https://api.github.com/repos/$BBS_REPOSITORY/releases/latest" | sed -n 's/.*"tag_name":[[:space:]]*"v\{0,1\}\([^"]*\)".*/\1/p' | head -n 1)
fi
[ -n "$BBS_VERSION" ] || { echo "could not determine latest bbs version" >&2; exit 1; }

bbs_asset="bbs-$bbs_arch-$bbs_os.tar.gz"
bbs_base="https://github.com/$BBS_REPOSITORY/releases/download/v$BBS_VERSION"
bbs_tmp=$(mktemp -d "${TMPDIR:-/tmp}/bbs-install.XXXXXX")
trap 'rm -r "$bbs_tmp"' EXIT INT TERM

curl -fsSL "$bbs_base/$bbs_asset" -o "$bbs_tmp/$bbs_asset"
curl -fsSL "$bbs_base/checksums.txt" -o "$bbs_tmp/checksums.txt"
bbs_expected=$(sed -n "s/[[:space:]]\+$bbs_asset$//p" "$bbs_tmp/checksums.txt" | head -n 1)
[ -n "$bbs_expected" ] || { echo "release checksum is missing for $bbs_asset" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then bbs_actual=$(sha256sum "$bbs_tmp/$bbs_asset" | awk '{print $1}');
elif command -v shasum >/dev/null 2>&1; then bbs_actual=$(shasum -a 256 "$bbs_tmp/$bbs_asset" | awk '{print $1}');
else echo "bbs installer requires sha256sum or shasum" >&2; exit 1; fi
[ "$bbs_actual" = "$bbs_expected" ] || { echo "checksum verification failed" >&2; exit 1; }

mkdir -p "$BBS_INSTALL_DIR"
tar -xzf "$bbs_tmp/$bbs_asset" -C "$bbs_tmp"
install -m 0755 "$bbs_tmp/bbs" "$BBS_INSTALL_DIR/bbs"
echo "Installed bbs $BBS_VERSION to $BBS_INSTALL_DIR/bbs"
case ":$PATH:" in *":$BBS_INSTALL_DIR:"*) ;; *) echo "Add $BBS_INSTALL_DIR to PATH." ;; esac
