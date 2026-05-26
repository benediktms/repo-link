# repo-link developer workflow. Every recipe is idempotent — re-running
# `just install` after `just install` ends in the same state without errors,
# and `just uninstall` is safe whether or not the daemon was ever installed.

# Path to the freshly-built CLI. Use this everywhere instead of `rl` on
# $PATH so the recipes work even when an older `rl` binary lives ahead of
# `~/.local/bin/` on PATH.
rl := "./target/release/rl"

default: list

list:
    @just --list

# install — idempotency notes:
#   - `cargo build --release` is a no-op when the build is up to date.
#   - `mkdir -p` no-ops when the directory exists.
#   - `ln -sf` overwrites any existing target (file / broken link / stale
#     symlink pointing elsewhere).
#   - `rl daemon install` follows the documented contract: read-then-write
#     the manifest, then bootout-then-bootstrap (macOS) / daemon-reload +
#     enable --now (Linux), tolerating "not loaded" as success.

# install — uses the just-built binary explicitly (via the `rl` variable)
# so the `daemon install` call doesn't resolve to a stale `rl` on PATH that
# predates this feature.

# Build, symlink into ~/.local/bin, and load the daemon unit.
install:
    cargo build --release
    mkdir -p ~/.local/bin
    ln -sf "$(pwd)/target/release/rl"  ~/.local/bin/rl
    ln -sf "$(pwd)/target/release/rld" ~/.local/bin/rld
    {{rl}} daemon install

# uninstall — `rl daemon uninstall` itself reports `manifest_existed: false`
# on a clean checkout and exits 0, so no `|| true` guard is needed.

# Unload the unit, delete the manifest, remove the ~/.local/bin symlinks.
uninstall:
    {{rl}} daemon uninstall
    rm -f ~/.local/bin/rl ~/.local/bin/rld

# daemon-restart — `stop` can legitimately fail when the unit was never
# installed; `|| true` keeps the recipe useful mid-recovery so `start`
# always runs.

# Toggle the persistent unit off then on.
daemon-restart:
    {{rl}} daemon stop  || true
    {{rl}} daemon start

# daemon-logs — `status` includes `log_path` in its JSON so the file
# location stays in one source of truth (no second hardcode here).

# Tail the daemon log file.
daemon-logs:
    tail -F "$({{rl}} daemon status | jq -r '.log_path')"

# dev — foreground debug daemon for iteration: faster tick, pretty logs,
# prune enabled so the grace counter exercises locally.

# Run rld in the foreground with dev-friendly flags.
dev:
    cargo build
    ./target/debug/rld --interval-secs 10 --prune --log-format pretty
