//! Application-owned executable snapshots. Terminals use one stable command path;
//! replacing/removing a checkout cannot strand their hooks or `$MUSTER_BIN`.
//! The host executable is only a bootstrap fallback. Publishing a release never
//! signals, replaces, or renegotiates the process that already owns the PTYs.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use anyhow::Context;
use sha2::{Digest, Sha256};

const APP: &str = "muster";
pub const HOST: &str = "muster-terminal-host";

pub struct Release {
    directory: PathBuf,
    command: PathBuf,
    app_hash: String,
    host_hash: String,
}

impl Release {
    pub fn command(&self) -> PathBuf {
        self.command.clone()
    }

    pub fn host(&self) -> PathBuf {
        self.directory.join(HOST)
    }

    /// SHA-256 of the application executable this release holds (the running backend's).
    pub fn app_hash(&self) -> &str {
        &self.app_hash
    }

    /// SHA-256 of the terminal host this release holds.
    pub fn host_hash(&self) -> &str {
        &self.host_hash
    }

    /// The release's own copy of the application: checked, read-only, and never replaced by a
    /// rebuild. A backend started from it runs this release (its host is the copy beside it).
    pub fn app(&self) -> PathBuf {
        self.directory.join(APP)
    }

    pub fn activate(&self) -> anyhow::Result<()> {
        let bin = self.command.parent().context("helper directory")?;
        crate::paths::ensure_private_dir(bin)?;
        let temporary = RemoveOnDrop(bin.join(format!(".muster-{}", crate::paths::random_token())));
        symlink(self.directory.join(APP), &temporary.0)?;
        fs::rename(&temporary.0, &self.command).context("publishing stable Muster command")?;
        File::open(bin)?.sync_all()?;
        Ok(())
    }
}

/// Copy complete binaries before publishing any path. They are separate files,
/// never hard links to a build output that an updater could truncate in place.
/// A backend-only distribution may reuse the previously snapshotted host binary.
#[cfg(test)]
pub fn prepare(data: &Path, executable: &Path) -> anyhow::Result<Release> {
    prepare_running(data, executable, executable)
}

/// `prepare` for the running backend: the application snapshot is copied from `running`
/// (`/proc/self/exe`: what actually executes, even if `launch` was replaced since it was
/// started), the host from beside `launch`, the path it was started from.
pub fn prepare_running(data: &Path, launch: &Path, running: &Path) -> anyhow::Result<Release> {
    crate::paths::ensure_private_dir(data)?;
    let data = fs::canonicalize(data)?;
    let command = data.join("bin").join(APP);
    let sibling_host = launch.with_file_name(HOST);
    let host = match fs::metadata(&sibling_host) {
        Ok(_) => sibling_host,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::canonicalize(&command)
                .context("terminal host executable is missing; the first installation requires cargo build --workspace")?
                .with_file_name(HOST)
        }
        Err(e) => return Err(e.into()),
    };
    let releases = data.join("application-releases");
    crate::paths::ensure_private_dir(&releases)?;
    let stage = RemoveOnDrop(releases.join(format!(".stage-{}", crate::paths::random_token())));
    fs::create_dir(&stage.0)?;
    fs::set_permissions(&stage.0, fs::Permissions::from_mode(0o700))?;
    let app_hash = snapshot(running, &stage.0.join(APP))?;
    let host_hash = snapshot(&host, &stage.0.join(HOST))?;
    File::open(&stage.0)?.sync_all()?;
    let directory = releases.join(format!("{app_hash}-{host_hash}"));
    if directory.exists() {
        // A previous startup may have published the same release. Do not trust
        // its name alone, and never silently overwrite a corrupted executable.
        let meta = fs::symlink_metadata(&directory)?;
        anyhow::ensure!(meta.is_dir(), "executable release is not a directory");
        verify(&directory.join(APP), &app_hash)?;
        verify(&directory.join(HOST), &host_hash)?;
    } else {
        fs::rename(&stage.0, &directory)?;
        File::open(&releases)?.sync_all()?;
    }
    Ok(Release { directory, command, app_hash, host_hash })
}

/// SHA-256 of a file's content, as release names use it.
pub fn hash_file(path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    hash_and_copy(&mut file, &mut std::io::sink())
}

fn executable_file(path: &Path) -> anyhow::Result<File> {
    let file =
        File::open(path).with_context(|| format!("opening executable {}", path.display()))?;
    let meta = file.metadata()?;
    anyhow::ensure!(
        meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        "not an executable file: {}",
        path.display()
    );
    Ok(file)
}

fn snapshot(source: &Path, target: &Path) -> anyhow::Result<String> {
    let mut source = executable_file(source)?;
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(target)?;
    let hash = hash_and_copy(&mut source, &mut target)?;
    target.set_permissions(fs::Permissions::from_mode(0o500))?;
    target.sync_all()?;
    Ok(hash)
}

fn verify(path: &Path, expected: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        fs::symlink_metadata(path)?.is_file(),
        "executable snapshot is not a regular file"
    );
    let mut file = executable_file(path)?;
    let hash = hash_and_copy(&mut file, &mut std::io::sink())?;
    anyhow::ensure!(
        hash == expected,
        "executable snapshot is corrupt: {}",
        path.display()
    );
    Ok(())
}

fn hash_and_copy(source: &mut impl Read, target: &mut impl Write) -> anyhow::Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0; 128 * 1024];
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        target.write_all(&buffer[..count])?;
    }
    Ok(format!("{:x}", hash.finalize()))
}

struct RemoveOnDrop(PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.0).is_ok_and(|m| m.is_dir()) {
            let _ = fs::remove_dir_all(&self.0);
        } else {
            let _ = fs::remove_file(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn binary(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn stable_command_survives_deleted_builds_and_activates_only_complete_releases() {
        let root = RemoveOnDrop(std::env::temp_dir().join(format!(
            "muster-executables-{}",
            crate::paths::random_token()
        )));
        fs::create_dir(&root.0).unwrap();
        let data = root.0.join("data");
        let build = root.0.join("build");
        fs::create_dir(&build).unwrap();
        binary(&build.join(APP), b"first application");
        binary(&build.join(HOST), b"stable core");
        let first = prepare(&data, &build.join(APP)).unwrap();
        assert!(!first.command().exists(), "preparation does not activate");
        first.activate().unwrap();
        assert_ne!(
            fs::metadata(first.command()).unwrap().ino(),
            fs::metadata(build.join(APP)).unwrap().ino()
        );
        let repeated = prepare(&data, &build.join(APP)).unwrap();
        assert_eq!(repeated.directory, first.directory);

        binary(&build.join(APP), b"second application");
        assert_eq!(fs::read(first.command()).unwrap(), b"first application");
        fs::remove_file(build.join(HOST)).unwrap();
        let second = prepare(&data, &build.join(APP)).unwrap();
        assert_eq!(
            fs::read(second.host()).unwrap(),
            b"stable core",
            "backend-only build reuses host"
        );
        assert_eq!(fs::read(first.command()).unwrap(), b"first application");
        second.activate().unwrap();
        assert_eq!(first.command(), second.command());
        fs::remove_dir_all(&build).unwrap();
        assert_eq!(fs::read(first.command()).unwrap(), b"second application");
        assert_eq!(
            fs::read(first.directory.join(APP)).unwrap(),
            b"first application"
        );
        // Re-execution from the installed helper is also a valid backend launch.
        assert_eq!(
            prepare(&data, &fs::canonicalize(first.command()).unwrap())
                .unwrap()
                .directory,
            second.directory
        );

        fs::create_dir(&build).unwrap();
        binary(&build.join(APP), b"bad release");
        fs::write(build.join(HOST), b"not executable").unwrap();
        assert!(prepare(&data, &build.join(APP)).is_err());
        assert_eq!(fs::read(first.command()).unwrap(), b"second application");
        assert!(
            fs::read_dir(data.join("application-releases"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with('.'))
        );
    }

    #[test]
    fn the_running_build_is_snapshotted_not_the_launch_path() {
        let root = RemoveOnDrop(std::env::temp_dir().join(format!(
            "muster-executables-{}",
            crate::paths::random_token()
        )));
        fs::create_dir(&root.0).unwrap();
        binary(&root.0.join(APP), b"rebuilt since launch");
        binary(&root.0.join(HOST), b"host");
        binary(&root.0.join("running"), b"what executes");
        let release = prepare_running(&root.0.join("data"), &root.0.join(APP), &root.0.join("running")).unwrap();
        assert_eq!(fs::read(release.directory.join(APP)).unwrap(), b"what executes");
        assert_eq!(release.app_hash(), hash_file(&root.0.join("running")).unwrap());
        assert_eq!(fs::read(release.host()).unwrap(), b"host", "the host comes from beside the launch path");
    }

    #[test]
    fn corrupted_snapshots_are_rejected_without_switching_the_command() {
        let root = RemoveOnDrop(std::env::temp_dir().join(format!(
            "muster-executables-{}",
            crate::paths::random_token()
        )));
        fs::create_dir(&root.0).unwrap();
        binary(&root.0.join(APP), b"application");
        binary(&root.0.join(HOST), b"host");
        let data = root.0.join("data");
        let release = prepare(&data, &root.0.join(APP)).unwrap();
        fs::set_permissions(release.host(), fs::Permissions::from_mode(0o700)).unwrap();
        binary(&release.host(), b"damaged");
        assert!(prepare(&data, &root.0.join(APP)).is_err());
        assert!(!release.command().exists());
    }
}
