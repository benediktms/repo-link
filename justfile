# repo-link developer workflow. Every recipe is idempotent — re-running
# `just install` after `just install` ends in the same state without errors,
# and `just uninstall` is safe whether or not the daemon was ever installed.

# Path to the freshly-built CLI. Use this everywhere instead of `rl` on
# $PATH so the recipes work even when an older `rl` binary lives ahead of
# `~/.local/bin/` on PATH.
rl := "./target/release/rl"

set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command"]

default:
    @just --list

# ── Checks ────────────────────────────────────────────────────────────────

# check is the only moon recipe: it is the CI contract, so it takes no
# narrowing arguments. lint and test call cargo directly because moon hashes
# the workspace sources and not the argument — a cached moon run would answer
# for the wrong crate.

# Run fmt, lint, and test exactly as CI runs them
[group('checks')]
check:
    moon run root:fmt root:lint root:test

alias c := check

# Auto-format all code
[group('checks')]
fmt:
    cargo fmt --all

alias f := fmt

# Lint, warnings denied (e.g. just l -p domain-task)
[group('checks')]
lint *args="--workspace":
    cargo clippy {{args}} --all-targets --no-deps -- -D warnings

alias l := lint

# Run tests (e.g. just t -p domain-task, just t --no-fail-fast)
[group('checks')]
test *args="--workspace --all-targets":
    cargo test {{args}}

alias t := test

# ── Install ───────────────────────────────────────────────────────────────

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
[group('install')]
[unix]
install:
    cargo build --release
    mkdir -p ~/.local/bin
    ln -sf "{{justfile_directory()}}/target/release/rl"  ~/.local/bin/rl
    ln -sf "{{justfile_directory()}}/target/release/rld" ~/.local/bin/rld
    {{rl}} daemon install

# Build, copy Windows executables into ~/.local/bin, and register the task.
# The `daemon stop` must precede the copy: Windows refuses to overwrite a
# running executable image, so a re-install over a live daemon would fail on
# `Copy-Item` before `daemon install` ever ran. `|| true` has no PowerShell
# equivalent here, so the call is guarded on the installed `rld.exe` existing
# instead — that is the image the task runs and the one the copy overwrites,
# and there is nothing to stop before the first install. The stop is issued
# through the freshly built `rl.exe`, which the `cargo build` above guarantees.
[group('install')]
[windows]
install:
    cargo build --release
    New-Item -ItemType Directory -Force -ErrorAction Stop (Join-Path $env:USERPROFILE ".local\bin") | Out-Null
    $installed = Join-Path $env:USERPROFILE ".local\bin\rld.exe"; if (Test-Path -LiteralPath $installed) { .\target\release\rl.exe daemon stop }
    $legacy = @((Join-Path $env:USERPROFILE ".local\bin\rl"), (Join-Path $env:USERPROFILE ".local\bin\rld")); $legacy | Where-Object { Test-Path -LiteralPath $_ } | Remove-Item -Force -ErrorAction Stop
    Copy-Item -Force -ErrorAction Stop ".\target\release\rl.exe" (Join-Path $env:USERPROFILE ".local\bin\rl.exe")
    Copy-Item -Force -ErrorAction Stop ".\target\release\rld.exe" (Join-Path $env:USERPROFILE ".local\bin\rld.exe")
    .\target\release\rl.exe daemon install

# uninstall — `rl daemon uninstall` itself reports `manifest_existed: false`
# on a clean checkout and exits 0, but `{{rl}}` points at the freshly-built
# binary, which may not exist on a clean tree. Fall back to the symlinked
# `~/.local/bin/rl` (left behind by a previous `just install`), and if
# neither is present, skip the daemon step — the symlink cleanup below is
# still useful.

# Unload the unit, delete the manifest, remove the ~/.local/bin symlinks.
[group('install')]
[unix]
uninstall:
    if [ -x {{rl}} ]; then {{rl}} daemon uninstall; \
    elif [ -x ~/.local/bin/rl ]; then ~/.local/bin/rl daemon uninstall; \
    else echo "rl not built and not on PATH; skipping daemon uninstall"; fi
    rm -f ~/.local/bin/rl ~/.local/bin/rld

# Unregister the task, then remove Windows executables and legacy
# extensionless installs. The daemon step must precede the deletions: it
# terminates the running `rld.exe`, which would otherwise hold a lock on its
# own file.
[group('install')]
[windows]
uninstall:
    $rl = @(".\target\release\rl.exe", (Join-Path $env:USERPROFILE ".local\bin\rl.exe")) | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1; if ($rl) { & $rl daemon uninstall } else { Write-Host "rl not built and not installed; skipping daemon uninstall" }
    $installed = @((Join-Path $env:USERPROFILE ".local\bin\rl.exe"), (Join-Path $env:USERPROFILE ".local\bin\rld.exe"), (Join-Path $env:USERPROFILE ".local\bin\rl"), (Join-Path $env:USERPROFILE ".local\bin\rld")); $installed | Where-Object { Test-Path -LiteralPath $_ } | Remove-Item -Force -ErrorAction Stop

# ── Daemon ────────────────────────────────────────────────────────────────

# daemon-restart — `stop` can legitimately fail when the unit was never
# installed; `|| true` keeps the recipe useful mid-recovery so `start`
# always runs.

# Toggle the persistent unit off then on.
[group('daemon')]
[unix]
daemon-restart:
    {{rl}} daemon stop  || true
    {{rl}} daemon start

# No `|| true` counterpart: `rl daemon stop` on Windows already treats an
# unregistered task as a no-op (`tolerate_missing_task` covers both `schtasks
# /End` and `/Change /DISABLE`), so there is nothing for it to swallow.
[group('daemon')]
[windows]
daemon-restart:
    .\target\release\rl.exe daemon stop
    .\target\release\rl.exe daemon start

# logs — convenience alias for the first-class CLI command.

# Tail the daemon log file.
[group('daemon')]
logs:
    {{rl}} daemon logs --follow

# dev — foreground debug daemon for iteration: faster tick, pretty logs,
# prune enabled so the grace counter exercises locally.

# Run rld in the foreground with dev-friendly flags.
[group('daemon')]
dev:
    cargo build
    ./target/debug/rld --interval-secs 10 --prune --log-format pretty
