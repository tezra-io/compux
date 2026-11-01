#!/usr/bin/env bash
#
# M86 L2: the Linux helper builds, passes its tests and runs on no C library but
# the C runtime. Run from the repository root on any Linux host with Docker:
#
#   fixtures/linux/spike/no_c_deps.sh OUT_DIR
#
# In a plain Ubuntu 24.04 container with only a C toolchain for linking (gcc,
# libc6-dev) and rustup's minimal stable toolchain, and no pkg-config:
#   1. the X, XKB, xdo and D-Bus development packages are shown to be absent;
#   2. `cargo build --release` and `cargo test` run;
#   3. the release binary's dynamic dependencies are only the C runtime's;
#   4. `cargo clippy --target aarch64-unknown-linux-gnu --all-targets -D warnings`
#      passes: an aarch64 build stays one command away (none is made here).
#
# The repository is copied in (tracked files and the working tree), the cargo
# caches are named volumes (compux-nocdeps-registry, compux-nocdeps-target), and
# nothing is mounted from the host. Nothing here starts a display or a bus; the
# helper's tests run no display either.
set -euo pipefail

OUT=${1:?usage: no_c_deps.sh OUT_DIR}
DOCKER=${DOCKER:-docker}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
LIMIT_S=${NO_C_DEPS_LIMIT_S:-3600}
mkdir -p "$OUT"

die() { echo "no_c_deps.sh: $*" >&2; exit 1; }

context=$(mktemp -d)
trap 'rm -rf "$context"' EXIT
(cd "$REPO" && git ls-files -coz --exclude-standard | rsync -rl --from0 --files-from=- ./ "$context/")

c=compux-no-c-deps-$$
"$DOCKER" rm -f "$c" >/dev/null 2>&1 || true
# shellcheck disable=SC2016  # the script is the container's, expanded there
"$DOCKER" create --name "$c" --init --tmpfs /tmp:rw,exec,mode=1777,size=4g \
  -v compux-nocdeps-registry:/root/.cargo/registry -v compux-nocdeps-target:/target \
  -e CARGO_TARGET_DIR=/target -w /work/native/compux ubuntu:24.04 bash -c '
    set -euo pipefail
    apt-get update -qq >/dev/null
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
      ca-certificates curl gcc libc6-dev >/dev/null
    curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain stable
    export PATH=/root/.cargo/bin:$PATH
    rustup component add clippy >/dev/null
    rustup target add aarch64-unknown-linux-gnu >/dev/null
    rustc --version
    echo "== absent: X, XKB, xdo, D-Bus development packages, pkg-config"
    present=$(dpkg-query -W -f "\${Package}\n" | grep -E "^(libxcb|libxkbcommon|libxdo|libdbus-1-dev|libx11-dev|pkg-config|pkgconf)" || true)
    if [ -n "$present" ]; then echo "present: $present"; exit 1; fi
    echo "none of them installed"
    echo "== build";  cargo build --release -q
    echo "== test"
    if ! cargo test -q >/tmp/test.log 2>&1; then
      grep -m 60 -E "^test result|FAILED|panicked|^error" /tmp/test.log || true
      echo NO_C_DEPS_TESTS_FAILED
      exit 1
    fi
    grep -E "^test result" /tmp/test.log
    echo "== linked"
    ldd /target/release/compux | tee /tmp/ldd.txt
    beyond=$(grep -vE "linux-vdso|libc\.so|libm\.so|libgcc_s\.so|ld-linux|libpthread\.so|libdl\.so|librt\.so" /tmp/ldd.txt || true)
    if [ -n "$beyond" ]; then
      echo "the helper links past the C runtime: $beyond"
      exit 1
    fi
    echo "only the C runtime"
    echo "== clippy aarch64-unknown-linux-gnu"
    cargo clippy -q --target aarch64-unknown-linux-gnu --all-targets -- -D warnings
    echo NO_C_DEPS_OK' >/dev/null
"$DOCKER" cp "$context/." "$c:/work"
rc=0
timeout -k 10 "$LIMIT_S" "$DOCKER" start -a "$c" 2>&1 | tee "$OUT/no-c-deps.log" || rc=$?
"$DOCKER" rm -f "$c" >/dev/null
[ "$rc" -eq 0 ] || die "failed (see $OUT/no-c-deps.log)"
grep -q NO_C_DEPS_OK "$OUT/no-c-deps.log" || die "did not finish (see $OUT/no-c-deps.log)"
echo "NO_C_DEPS_DONE $OUT"
