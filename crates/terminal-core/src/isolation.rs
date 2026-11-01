//! Process ownership outside the launching terminal's cgroup. No installed units or root
//! privileges: the user's systemd manager owns transient services and session scopes.
//! Service environments travel in a sealed, anonymous memory file, never in unit properties,
//! command lines or files on disk. The early exec bootstrap restores the exact environment.
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::time::Duration;

use anyhow::Context;

pub const ENV: &str = "MUSTER_ISOLATION";
const BOOTSTRAP: &str = "--muster-service-exec";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode { Auto, Required, Off }

impl Mode {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::parse(std::env::var(ENV).ok().as_deref())
    }
    fn parse(value: Option<&str>) -> anyhow::Result<Self> {
        match value {
            None | Some("") | Some("auto") => Ok(Self::Auto),
            Some("required") => Ok(Self::Required),
            Some("off") => Ok(Self::Off),
            _ => anyhow::bail!("{ENV} must be auto, required or off"),
        }
    }
}

/// Both binaries call this before creating any threads, parsing normal options, or logging.
/// A successful bootstrap execs and never returns. Stdin becomes /dev/null for the real daemon.
pub fn service_exec() -> Option<anyhow::Result<()>> {
    if std::env::args_os().nth(1).as_deref() != Some(OsStr::new(BOOTSTRAP)) { return None }
    Some(exec_from_memory())
}

fn exec_from_memory() -> anyhow::Result<()> {
    use std::io::Read;
    use std::os::unix::{ffi::OsStringExt, process::CommandExt};
    // Bound parsing even if this internal entry point was called by hand.
    let mut bytes = Vec::new();
    std::io::stdin().take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= 4 * 1024 * 1024, "service environment is too large");
    let environment: Vec<(Vec<u8>, Vec<u8>)> = serde_json::from_slice(&bytes)
        .context("invalid service environment")?;
    let environment = environment.into_iter().map(|(k, v)| (OsString::from_vec(k), OsString::from_vec(v)));
    let error = Command::new(std::env::current_exe()?)
        .args(std::env::args_os().skip(2)).env_clear().envs(environment)
        .stdin(std::process::Stdio::null()).exec();
    Err(error).context("executing isolated service")
}

/// A service is not this process's Unix child, but has the same startup/exit observations.
/// Dropping this handle never stops it; the service outlives the launching CLI/backend.
pub enum Process {
    Child(Child),
    #[cfg(target_os = "linux")]
    Service(linux::Service),
}

impl Process {
    pub fn id(&self) -> u32 {
        match self {
            Self::Child(child) => child.id(),
            #[cfg(target_os = "linux")]
            Self::Service(service) => service.pid,
        }
    }
    pub fn try_wait(&mut self) -> anyhow::Result<Option<ExitStatus>> {
        match self {
            Self::Child(child) => Ok(child.try_wait()?),
            #[cfg(target_os = "linux")]
            Self::Service(service) => service.try_wait(),
        }
    }
    pub fn reap_later(self) {
        if let Self::Child(mut child) = self {
            std::thread::spawn(move || { let _ = child.wait(); });
        }
    }
}

/// `command` must target a binary implementing `service_exec`, inherit its environment, and
/// need only the standard fds (stdin null, stdout/stderr `log`). Its pre_exec is used only by
/// the direct fallback. No fallback is allowed after a manager accepted a service request.
pub fn spawn_service(command: &mut Command, log: &File, role: &str) -> anyhow::Result<Process> {
    let mode = Mode::from_env()?;
    #[cfg(target_os = "linux")]
    if let Some(manager) = linux::Manager::connect(mode)? {
        return manager.spawn(command, log, role).map(Process::Service);
    }
    #[cfg(not(target_os = "linux"))]
    anyhow::ensure!(mode != Mode::Required, "systemd process isolation requires Linux");
    let _ = (log, role);
    if mode == Mode::Auto {
        eprintln!("muster: systemd user manager unavailable; process isolation is off");
    }
    Ok(Process::Child(command.spawn()?))
}

/// Move only this process before it can fork/exec its workload; its PID, PTY and process group
/// remain unchanged. Lifecycle helpers use a separate slice so stopping a backend service
/// cannot also kill the command that is replacing it.
pub fn enter_scope(role: &str, slice: &str) -> anyhow::Result<()> {
    let mode = Mode::from_env()?;
    #[cfg(target_os = "linux")]
    if let Some(manager) = linux::Manager::connect(mode)? { return manager.scope(role, slice) }
    #[cfg(not(target_os = "linux"))]
    anyhow::ensure!(mode != Mode::Required, "systemd process isolation requires Linux");
    let _ = (role, slice);
    Ok(())
}

/// Check actual service creation before a reload stops a working backend. The probe has no
/// application state and exits immediately; it also verifies fd forwarding and bootstrap.
pub fn check_service_support(executable: &Path) -> anyhow::Result<()> {
    let log = std::fs::OpenOptions::new().write(true).open("/dev/null")?;
    let mut cmd = Command::new(executable);
    cmd.arg("--version").stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?).stderr(log.try_clone()?);
    let mut process = spawn_service(&mut cmd, &log, "probe")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = process.try_wait()? {
            anyhow::ensure!(status.success(), "isolated service probe exited with {status}; nothing was stopped");
            return Ok(());
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "isolated service probe timed out; nothing was stopped");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{Seek, Write};
    use std::os::{fd::{AsFd, AsRawFd, FromRawFd}, unix::{ffi::OsStrExt, process::ExitStatusExt}};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;
    use zbus::{blocking::{Connection, Proxy}, zvariant::{Fd, OwnedObjectPath, Value}};

    const DEST: &str = "org.freedesktop.systemd1";
    const ROOT: &str = "/org/freedesktop/systemd1";
    const MANAGER: &str = "org.freedesktop.systemd1.Manager";
    const SERVICE: &str = "org.freedesktop.systemd1.Service";
    const SCOPE: &str = "org.freedesktop.systemd1.Scope";
    type Property<'a> = (&'a str, Value<'a>);

    pub struct Manager { connection: Connection }

    fn proxy<'a>(connection: &Connection, path: &'a str, interface: &'a str) -> zbus::Result<Proxy<'a>> {
        zbus::blocking::proxy::Builder::new(connection).destination(DEST)?.path(path)?.interface(interface)?
            .cache_properties(zbus::proxy::CacheProperties::No).build()
    }

    pub fn unit_name(role: &str, suffix: &str) -> String {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let tick = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        format!("muster-{role}-{}-{tick:x}-{}.{suffix}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
    }

    impl Manager {
        pub fn connect(mode: Mode) -> anyhow::Result<Option<Self>> {
            if mode == Mode::Off { return Ok(None) }
            let connect = || -> anyhow::Result<Self> {
                anyhow::ensure!(Path::new("/sys/fs/cgroup/cgroup.controllers").exists(), "cgroup v2 is unavailable");
                let connection = zbus::blocking::connection::Builder::session()?
                    .method_timeout(Duration::from_secs(5)).build()?;
                let _: String = proxy(&connection, ROOT, MANAGER)?.get_property("Version")?;
                Ok(Self { connection })
            };
            match connect() {
                Ok(manager) => Ok(Some(manager)),
                Err(_) if mode == Mode::Auto => Ok(None),
                Err(error) => Err(error.context("systemd user process isolation is required but unavailable")),
            }
        }

        fn start(&self, name: &str, properties: &[Property<'_>]) -> anyhow::Result<OwnedObjectPath> {
            let empty: Vec<(&str, Vec<Property<'_>>)> = Vec::new();
            let _: OwnedObjectPath = proxy(&self.connection, ROOT, MANAGER)?.call("StartTransientUnit", &(name, "fail", properties, empty))
                .with_context(|| format!("creating isolated unit {name}"))?;
            Ok(proxy(&self.connection, ROOT, MANAGER)?.call("GetUnit", &(name,))?)
        }

        pub fn spawn(self, command: &Command, log: &File, role: &str) -> anyhow::Result<Service> {
            let name = unit_name(role, "service");
            let program = command.get_program().to_str().context("service executable path must be UTF-8")?;
            anyhow::ensure!(Path::new(program).is_absolute(), "service executable path must be absolute");
            let cwd = command.get_current_dir().map(Path::to_path_buf).unwrap_or(std::env::current_dir()?);
            let cwd = cwd.to_str().context("service working directory must be UTF-8")?;
            let mut argv = vec![program.to_string(), BOOTSTRAP.to_string()];
            for arg in command.get_args() {
                argv.push(arg.to_str().context("service argument must be UTF-8")?.to_string());
            }
            let environment = environment_file(command)?;
            let exec = vec![(program, argv, vec!["no-env-expand"])];
            let properties = vec![
                ("Description", Value::from(format!("Muster {role}"))),
                ("Slice", Value::from("muster-core.slice")),
                ("Type", Value::from("exec")),
                ("ExecStartEx", Value::from(exec)),
                ("WorkingDirectory", Value::from(cwd)),
                ("StandardInputFileDescriptor", Value::from(Fd::from(environment.as_fd()))),
                ("StandardOutputFileDescriptor", Value::from(Fd::from(log.as_fd()))),
                ("StandardErrorFileDescriptor", Value::from(Fd::from(log.as_fd()))),
                ("MemoryAccounting", Value::from(true)),
                // The CLI/host owns restart policy. A failed host must never silently duplicate PTYs.
                ("Restart", Value::from("no")),
                ("CollectMode", Value::from("inactive-or-failed")),
                // Hold exit status until this handle goes away, including very fast failures.
                ("AddRef", Value::from(true)),
            ];
            let path = self.start(&name, &properties)?;
            let mut service = Service { manager: self, path, pid: 0 };
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                service.pid = proxy(&service.manager.connection, &service.path, SERVICE)?.get_property("ExecMainPID")?;
                if service.pid != 0 { return Ok(service) }
                if let Some(status) = service.try_wait()? {
                    anyhow::bail!("isolated {role} could not start ({status})");
                }
                anyhow::ensure!(Instant::now() < deadline, "isolated {role} did not start within 10s");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        pub fn scope(self, role: &str, slice: &str) -> anyhow::Result<()> {
            let name = unit_name(role, "scope");
            let properties = vec![
                ("Description", Value::from(format!("Muster {role}"))),
                ("Slice", Value::from(slice)),
                ("PIDs", Value::from(vec![std::process::id()])),
                ("MemoryAccounting", Value::from(true)),
                ("CollectMode", Value::from("inactive-or-failed")),
                ("AddRef", Value::from(true)),
            ];
            let path = self.start(&name, &properties)?;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let group: String = proxy(&self.connection, &path, SCOPE)?.get_property("ControlGroup")?;
                let actual = std::fs::read_to_string("/proc/self/cgroup")?;
                if !group.is_empty() && actual.lines().any(|line| line.strip_prefix("0::") == Some(group.as_str())) {
                    return Ok(());
                }
                anyhow::ensure!(Instant::now() < deadline, "systemd did not move this process into {name}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    /// No environment values appear in the unit or its argv, including credentials.
    fn environment_file(command: &Command) -> anyhow::Result<File> {
        let mut env: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
        for (key, value) in command.get_envs() {
            match value { Some(value) => { env.insert(key.into(), value.into()); }, None => { env.remove(key); } }
        }
        // Once launched in a service, a missing manager must not put new sessions back in
        // the host's group. Explicit off never reaches this path.
        env.insert(ENV.into(), "required".into());
        let values: Vec<(&[u8], &[u8])> = env.iter().map(|(k, v)| (k.as_bytes(), v.as_bytes())).collect();
        // SAFETY: static NUL-terminated name; the successful descriptor is uniquely owned.
        let fd = unsafe { libc::memfd_create(c"muster-service-environment".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
        if fd < 0 { return Err(std::io::Error::last_os_error().into()) }
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(&serde_json::to_vec(&values)?)?;
        file.rewind()?;
        let seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
        // SAFETY: fcntl acts on the live descriptor owned by `file`.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(file)
    }

    pub struct Service { manager: Manager, path: OwnedObjectPath, pub pid: u32 }

    impl Service {
        pub fn try_wait(&self) -> anyhow::Result<Option<ExitStatus>> {
            let proxy = proxy(&self.manager.connection, &self.path, SERVICE)?;
            let code: i32 = proxy.get_property("ExecMainCode")?;
            if code == 0 { return Ok(None) }
            let status: i32 = proxy.get_property("ExecMainStatus")?;
            let raw = match code {
                libc::CLD_EXITED => status << 8,
                libc::CLD_KILLED => status,
                libc::CLD_DUMPED => status | 0x80,
                _ => anyhow::bail!("unexpected service exit code {code}"),
            };
            Ok(Some(ExitStatus::from_raw(raw)))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::Read;
        #[test]
        fn environment_is_sealed_and_preserves_values_without_shell_expansion() {
            let mut cmd = Command::new("/bin/true");
            cmd.env("MUSTER_TEST_SECRET", "secret\n$HOME %n \\\"quoted");
            cmd.env_remove("PATH");
            let mut file = environment_file(&cmd).unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            let values: Vec<(Vec<u8>, Vec<u8>)> = serde_json::from_slice(&bytes).unwrap();
            assert!(values.iter().any(|(k, v)| k == b"MUSTER_TEST_SECRET" && v == b"secret\n$HOME %n \\\"quoted"));
            assert!(!values.iter().any(|(k, _)| k == b"PATH"));
            assert!(file.write_all(b"overwrite").is_err());
        }
        #[test]
        fn transient_names_are_unique_without_paths_or_environment_values() {
            let first = unit_name("backend", "service");
            assert!(first.starts_with("muster-backend-"));
            assert!(first.ends_with(".service"));
            assert_ne!(first, unit_name("backend", "service"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mode_requires_an_explicit_valid_choice() {
        assert_eq!(Mode::parse(None).unwrap(), Mode::Auto);
        assert_eq!(Mode::parse(Some("off")).unwrap(), Mode::Off);
        assert_eq!(Mode::parse(Some("required")).unwrap(), Mode::Required);
        assert!(Mode::parse(Some("maybe")).is_err());
    }
}
