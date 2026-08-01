# repo-link developer workflow. Every recipe is idempotent — re-running
# `just install` after `just install` ends in the same state without errors,
# and `just uninstall` is safe whether or not the daemon was ever installed.

# Path to the freshly-built CLI. Use this everywhere instead of `rl` on
# $PATH so the recipes work even when an older `rl` binary lives ahead of
# `~/.local/bin/` on PATH.
rl := "./target/release/rl"

set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command"]

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
[unix]
install:
    cargo build --release
    mkdir -p ~/.local/bin
    ln -sf "$(pwd)/target/release/rl"  ~/.local/bin/rl
    ln -sf "$(pwd)/target/release/rld" ~/.local/bin/rld
    {{rl}} daemon install

# Build and copy Windows executables into ~/.local/bin.
[windows]
install:
    cargo build --release
    New-Item -ItemType Directory -Force -ErrorAction Stop (Join-Path $env:USERPROFILE ".local\bin") | Out-Null
    $legacy = @((Join-Path $env:USERPROFILE ".local\bin\rl"), (Join-Path $env:USERPROFILE ".local\bin\rld")); $legacy | Where-Object { Test-Path -LiteralPath $_ } | Remove-Item -Force -ErrorAction Stop
    Copy-Item -Force -ErrorAction Stop ".\target\release\rl.exe" (Join-Path $env:USERPROFILE ".local\bin\rl.exe")
    Copy-Item -Force -ErrorAction Stop ".\target\release\rld.exe" (Join-Path $env:USERPROFILE ".local\bin\rld.exe")

# uninstall — `rl daemon uninstall` itself reports `manifest_existed: false`
# on a clean checkout and exits 0, but `{{rl}}` points at the freshly-built
# binary, which may not exist on a clean tree. Fall back to the symlinked
# `~/.local/bin/rl` (left behind by a previous `just install`), and if
# neither is present, skip the daemon step — the symlink cleanup below is
# still useful.

# Unload the unit, delete the manifest, remove the ~/.local/bin symlinks.
[unix]
uninstall:
    if [ -x {{rl}} ]; then {{rl}} daemon uninstall; \
    elif [ -x ~/.local/bin/rl ]; then ~/.local/bin/rl daemon uninstall; \
    else echo "rl not built and not on PATH; skipping daemon uninstall"; fi
    rm -f ~/.local/bin/rl ~/.local/bin/rld

# Remove Windows executables and legacy extensionless installs.
[windows]
uninstall:
    $installed = @((Join-Path $env:USERPROFILE ".local\bin\rl.exe"), (Join-Path $env:USERPROFILE ".local\bin\rld.exe"), (Join-Path $env:USERPROFILE ".local\bin\rl"), (Join-Path $env:USERPROFILE ".local\bin\rld")); $installed | Where-Object { Test-Path -LiteralPath $_ } | Remove-Item -Force -ErrorAction Stop

# daemon-restart — `stop` can legitimately fail when the unit was never
# installed; `|| true` keeps the recipe useful mid-recovery so `start`
# always runs.

# Toggle the persistent unit off then on.
[unix]
daemon-restart:
    {{rl}} daemon stop  || true
    {{rl}} daemon start

# logs — convenience alias for the first-class CLI command.

# Tail the daemon log file.
logs:
    {{rl}} daemon logs --follow

# dev — foreground debug daemon for iteration: faster tick, pretty logs,
# prune enabled so the grace counter exercises locally.

# Run rld in the foreground with dev-friendly flags.
dev:
    cargo build
    ./target/debug/rld --interval-secs 10 --prune --log-format pretty
