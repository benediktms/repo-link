//! Shared manifest writing for the macOS plist, the Linux systemd unit, and
//! the Windows task XML. The idempotency contract — "running install twice
//! writes the file once" — lives in [`write_if_changed`].

use std::path::Path;

/// Compare the desired bytes to what's on disk; write atomically only when
/// they differ. Returns `true` iff the file was (re)written.
///
/// Always creates the parent directory if missing so first-run installs on
/// a fresh box don't need a separate `mkdir -p`. The same-directory rename
/// in [`write_atomic`] guarantees readers never see a half-written manifest.
pub(super) fn write_if_changed(path: &Path, desired: &str) -> std::io::Result<bool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Distinguish "file doesn't exist" (write it) from "can't read the file"
    // (something is wrong with the FS / perms — surface it rather than
    // pressing on into a write that would fail with a less informative error).
    match std::fs::read(path) {
        Ok(current) if current == desired.as_bytes() => return Ok(false),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    write_atomic(path, desired.as_bytes())?;
    Ok(true)
}

/// Both manifest formats are XML — the launchd plist and the Task Scheduler
/// task definition — and both interpolate user-derived paths, so a home
/// directory containing `&` would otherwise produce a manifest the platform
/// tool refuses to parse.
pub(super) fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Manifest paths get handed to `launchctl` / `schtasks` as argv, which is
/// `&str`-shaped — so a non-UTF-8 path has to fail loudly here rather than be
/// lossily mangled into a path the platform tool can't find.
pub(super) fn path_to_string(p: &Path) -> anyhow::Result<String> {
    p.to_str()
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {p:?}"))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let file_name = path.file_name().and_then(|s| s.to_str()).ok_or_else(|| {
        std::io::Error::other(format!("manifest path has no file name: {path:?}"))
    })?;
    let tmp = path.with_file_name(format!(".{file_name}.tmp"));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    // fsync the bytes before rename so a crash mid-write can't surface a
    // truncated file to the next reader.
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
