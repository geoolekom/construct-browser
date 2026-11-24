#!/usr/bin/env bash
# Build pgvector against the PostgreSQL release the agent bundles, from pinned,
# checksum-verified sources. The agent embeds the files it writes into OUT_DIR:
#   vector.so (Linux) or vector.dylib (macOS), vector.control, vector.sql, vector.pgversion
# Targets: x86_64-unknown-linux-gnu (needs curl, tar, make, gcc, sha256sum) and
# aarch64-apple-darwin (needs curl, tar, shasum and the Xcode command line tools for make and clang).
#
# usage: scripts/build-pgvector.sh <out-dir> [<target>]
#        scripts/build-pgvector.sh --download-postgresql <out-dir> [<target>]
# The target defaults to the machine the script runs on.
# The pins are in this script (pgvector release and checksums) and in .cargo/config.toml
# (PostgreSQL release); a new PostgreSQL release needs its checksum added here.
set -euo pipefail

download_only=false
if [ "${1:-}" = --download-postgresql ]; then
  download_only=true
  shift
fi

# The PostgreSQL release is the one pinned in .cargo/config.toml (POSTGRESQL_VERSION), which is
# also the one the agent embeds. Cargo passes it in; a manual run reads it from that file.
root=$(cd "$(dirname "$0")/.." && pwd)
PG_VERSION=${POSTGRESQL_VERSION:-}
if [ -z "$PG_VERSION" ]; then
  PG_VERSION=$(sed -n 's/^POSTGRESQL_VERSION *= *"\(.*\)"/\1/p' "$root/.cargo/config.toml")
fi
PG_VERSION=${PG_VERSION#=}

out=${1:?usage: build-pgvector.sh <out-dir> [<target>]}
target=${2:-}
if [ -z "$target" ]; then
  case "$(uname -s)/$(uname -m)" in
    Linux/x86_64) target=x86_64-unknown-linux-gnu ;;
    Darwin/arm64) target=aarch64-apple-darwin ;;
    *) echo "pgvector is not built for $(uname -s) $(uname -m)" >&2; exit 1 ;;
  esac
fi
case "$target" in
  x86_64-unknown-linux-gnu) library=vector.so ;;
  aarch64-apple-darwin) library=vector.dylib ;;
  *) echo "pgvector is not built for target '$target'" >&2; exit 1 ;;
esac

# sha256 of the PostgreSQL archive of that release for each target; a new release needs its
# checksums added here.
case "$PG_VERSION/$target" in
  16.15.1/x86_64-unknown-linux-gnu) PG_SHA256="0c42e7c05f0cb596b82ac07990c96fc261ec2e8106055d557c76442d712dbf74" ;;
  16.15.1/aarch64-apple-darwin) PG_SHA256="0a70b8a3bbb991b5671dc7678ba9a8cf1a389f2fc595ac4834fb1e29e757b89e" ;;
  *) echo "no checksum for PostgreSQL '$PG_VERSION' on '$target' in $0" >&2; exit 1 ;;
esac
PG_URL="https://github.com/theseus-rs/postgresql-binaries/releases/download/${PG_VERSION}/postgresql-${PG_VERSION}-${target}.tar.gz"

PGVECTOR_VERSION="0.8.7"
PGVECTOR_SHA256="cac0b10c360f05b2d521200105ba3697e773d4cd3731f5a915a7e37ebe0bea85"
PGVECTOR_URL="https://github.com/pgvector/pgvector/archive/refs/tags/v${PGVECTOR_VERSION}.tar.gz"

mkdir -p "$out"
out=$(cd "$out" && pwd)

# GNU sha256sum on Linux; macOS has shasum (its own sha256sum, if any, takes other options).
if sha256sum --version >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
else
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
fi

fetch() { # url sha256 file
  # A copy from an earlier run is reused when it still matches the checksum.
  if [ -f "$out/$3" ] && [ "$(sha256 "$out/$3")" = "$2" ]; then
    return
  fi
  local download
  download=$(mktemp "$out/$3.XXXXXX")
  local got
  if ! curl -fsSL --retry 3 -o "$download" "$1"; then
    rm -f "$download"
    return 1
  fi
  got=$(sha256 "$download")
  if [ "$got" != "$2" ]; then
    echo "$3: sha256 is $got, expected $2" >&2
    rm -f "$download"
    return 1
  fi
  mv -f "$download" "$out/$3"
}

if [ "$download_only" = true ]; then
  fetch "$PG_URL" "$PG_SHA256" "postgresql-${PG_VERSION}-${target}.tar.gz"
  exit 0
fi

work="$out/work"
rm -rf "$work"
mkdir -p "$work"

# The archive is per target, so a copy for another target is not mistaken for this one.
fetch "$PG_URL" "$PG_SHA256" "postgresql-${target}.tar.gz"
fetch "$PGVECTOR_URL" "$PGVECTOR_SHA256" pgvector.tar.gz

mkdir "$work/pg" "$work/src"
tar -xzf "$out/postgresql-${target}.tar.gz" -C "$work/pg" --strip-components=1
tar -xzf "$out/pgvector.tar.gz" -C "$work/src" --strip-components=1

# OPTFLAGS="" drops -march=native: the build must run on every machine of the target.
make_args=(PG_CONFIG="$work/pg/bin/pg_config" OPTFLAGS="")
if [ "$target" = aarch64-apple-darwin ]; then
  # The archive's build settings name the SDK and the LLVM clang of the machine that built
  # PostgreSQL. Use the SDK of the installed command line tools, and skip the LLVM bitcode
  # (it is only for JIT inlining, which the agent does not install). The library needs no
  # newer macOS than the PostgreSQL it is loaded into.
  make_args+=(PG_SYSROOT="$(xcrun --show-sdk-path)" with_llvm=no)
  MACOSX_DEPLOYMENT_TARGET=$(vtool -show-build "$work/pg/bin/postgres" | awk '$1 == "minos" { print $2; exit }')
  export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:?no minimum macOS in the PostgreSQL binary}
fi
make -C "$work/src" "${make_args[@]}" >"$work/make.log" 2>&1 \
  || { cat "$work/make.log" >&2; exit 1; }

cp "$work/src/$library" "$out/$library"
cp "$work/src/vector.control" "$out/vector.control"
cp "$work/src/sql/vector--${PGVECTOR_VERSION}.sql" "$out/vector.sql"
# The agent refuses to install the library into another PostgreSQL release than this one.
printf '%s' "$PG_VERSION" >"$out/vector.pgversion"
rm -rf "$work"
echo "built pgvector ${PGVECTOR_VERSION} for PostgreSQL ${PG_VERSION} on ${target} in $out"
