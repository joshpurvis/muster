//! Private host files, independent of application filesystem locations.
use std::path::Path;

/// Create a directory (and parents) with mode 0700.
/// An existing directory is tightened to 0700 too (it holds tokens and terminal logs).
pub fn ensure_private_dir(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(p)?;
    let meta = std::fs::metadata(p)?;
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// chmod 0600, ignoring a missing file.
pub fn make_private_file(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
}

/// Write a file atomically (tmp + rename) with mode 0600.
pub fn write_private(p: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = p.parent() {
        ensure_private_dir(dir)?;
    }
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, p)
}

