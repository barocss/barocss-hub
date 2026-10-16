#!/bin/sh
# Issue #57: a shipped barocss-hub must not contain any HUB_TEST_* hook.
# Usage: scripts/check-no-test-hooks.sh [BINARY...]
# With no argument, builds the release binary exactly as a dist build does
# (no `test-hooks` feature, own target dir) and checks it. Exits 1 if any
# given binary contains the byte string HUB_TEST_.
set -eu
if [ "$#" -eq 0 ]; then
    root=$(cd "$(dirname "$0")/.." && pwd)
    target="$root/target/dist-check"
    cargo build --release --locked -q -p barocss-hub-cli --bin barocss-hub \
        --manifest-path "$root/Cargo.toml" --target-dir "$target"
    set -- "$target/release/barocss-hub"
fi
status=0
for bin in "$@"; do
    if LC_ALL=C grep -a -c 'HUB_TEST_' "$bin" >/dev/null; then
        echo "FAIL: $bin contains HUB_TEST_ hooks:" >&2
        LC_ALL=C grep -a -o 'HUB_TEST_[A-Z_]*' "$bin" | sort -u >&2
        status=1
    else
        echo "ok: $bin has no HUB_TEST_ strings"
    fi
done
exit $status
