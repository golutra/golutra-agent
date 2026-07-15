//! 平台 sandbox 规划器。调用方只负责执行返回的 launch plan。

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackendKind {
    MacOsSeatbelt,
    LinuxBubblewrap,
    ProcessOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxRequest {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
    pub scratch_dir: PathBuf,
    pub workspace_access: WorkspaceAccess,
    pub allow_network: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxLaunch {
    pub backend: SandboxBackendKind,
    pub os_enforced: bool,
    pub program: OsString,
    pub args: Vec<OsString>,
    pub environment: BTreeMap<OsString, OsString>,
}

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("sandbox working directory is invalid: {0}")]
    InvalidWorkingDirectory(String),
    #[error("sandbox scratch directory is invalid: {0}")]
    InvalidScratchDirectory(String),
    #[error("sandbox path cannot be represented safely: {0}")]
    UnsafePath(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemSandbox {
    backend: SandboxBackendKind,
    launcher: Option<PathBuf>,
}

impl SystemSandbox {
    #[must_use]
    pub fn process_only() -> Self {
        Self {
            backend: SandboxBackendKind::ProcessOnly,
            launcher: None,
        }
    }

    #[must_use]
    pub fn detect() -> Self {
        #[cfg(target_os = "macos")]
        if let Some(launcher) = executable_at("/usr/bin/sandbox-exec") {
            return Self {
                backend: SandboxBackendKind::MacOsSeatbelt,
                launcher: Some(launcher),
            };
        }

        #[cfg(target_os = "linux")]
        if let Some(launcher) = find_on_path("bwrap") {
            return Self {
                backend: SandboxBackendKind::LinuxBubblewrap,
                launcher: Some(launcher),
            };
        }

        Self {
            backend: SandboxBackendKind::ProcessOnly,
            launcher: None,
        }
    }

    #[must_use]
    pub fn backend(&self) -> SandboxBackendKind {
        self.backend
    }

    #[must_use]
    pub fn os_enforced(&self) -> bool {
        self.backend != SandboxBackendKind::ProcessOnly
    }

    pub fn plan(&self, request: &SandboxRequest) -> Result<SandboxLaunch, SandboxError> {
        let mut request = request.clone();
        request.cwd = canonical_directory(&request.cwd, true)?;
        request.workspace_root = canonical_directory(&request.workspace_root, true)?;
        request.scratch_dir = canonical_directory(&request.scratch_dir, false)?;
        let environment = sanitized_environment(&request.scratch_dir);
        match self.backend {
            SandboxBackendKind::MacOsSeatbelt => self.plan_macos(&request, environment),
            SandboxBackendKind::LinuxBubblewrap => self.plan_linux(&request, environment),
            SandboxBackendKind::ProcessOnly => Ok(SandboxLaunch {
                backend: self.backend,
                os_enforced: false,
                program: request.program.clone(),
                args: request.args.clone(),
                environment,
            }),
        }
    }

    fn plan_macos(
        &self,
        request: &SandboxRequest,
        environment: BTreeMap<OsString, OsString>,
    ) -> Result<SandboxLaunch, SandboxError> {
        let profile = macos_profile(request)?;
        let mut args = vec![
            OsString::from("-p"),
            OsString::from(profile),
            request.program.clone(),
        ];
        args.extend(request.args.clone());
        Ok(SandboxLaunch {
            backend: self.backend,
            os_enforced: true,
            program: self
                .launcher
                .clone()
                .expect("detected launcher")
                .into_os_string(),
            args,
            environment,
        })
    }

    fn plan_linux(
        &self,
        request: &SandboxRequest,
        environment: BTreeMap<OsString, OsString>,
    ) -> Result<SandboxLaunch, SandboxError> {
        let read_roots = readable_roots(&request.cwd)
            .into_iter()
            .filter(|root| root != &request.cwd && root != &request.workspace_root)
            .collect::<Vec<_>>();
        let mut args = vec![
            "--die-with-parent".into(),
            "--new-session".into(),
            "--unshare-pid".into(),
            "--unshare-ipc".into(),
            "--unshare-uts".into(),
            "--unshare-cgroup-try".into(),
            "--tmpfs".into(),
            "/".into(),
            "--proc".into(),
            "/proc".into(),
            "--dev".into(),
            "/dev".into(),
        ];
        if !request.allow_network {
            args.push("--unshare-net".into());
        }

        let mut created = BTreeSet::new();
        for root in read_roots {
            add_parent_directories(&mut args, &mut created, &root);
            args.push("--ro-bind".into());
            args.push(root.clone().into_os_string());
            args.push(root.into_os_string());
        }
        if request.cwd != request.workspace_root {
            add_parent_directories(&mut args, &mut created, &request.cwd);
            args.push("--ro-bind".into());
            args.push(request.cwd.clone().into_os_string());
            args.push(request.cwd.clone().into_os_string());
        }
        add_parent_directories(&mut args, &mut created, &request.workspace_root);
        args.push(
            match request.workspace_access {
                WorkspaceAccess::ReadOnly => "--ro-bind",
                WorkspaceAccess::ReadWrite => "--bind",
            }
            .into(),
        );
        args.push(request.workspace_root.clone().into_os_string());
        args.push(request.workspace_root.clone().into_os_string());
        add_parent_directories(&mut args, &mut created, &request.scratch_dir);
        args.push("--bind".into());
        args.push(request.scratch_dir.clone().into_os_string());
        args.push(request.scratch_dir.clone().into_os_string());
        args.push("--chdir".into());
        args.push(request.cwd.clone().into_os_string());
        for (key, value) in &environment {
            args.push("--setenv".into());
            args.push(key.clone());
            args.push(value.clone());
        }
        args.push("--".into());
        args.push(request.program.clone());
        args.extend(request.args.clone());

        Ok(SandboxLaunch {
            backend: self.backend,
            os_enforced: true,
            program: self
                .launcher
                .clone()
                .expect("detected launcher")
                .into_os_string(),
            args,
            environment: BTreeMap::new(),
        })
    }
}

impl Default for SystemSandbox {
    fn default() -> Self {
        Self::detect()
    }
}

fn canonical_directory(path: &Path, working_directory: bool) -> Result<PathBuf, SandboxError> {
    let canonical = path
        .is_absolute()
        .then(|| std::fs::canonicalize(path).ok())
        .flatten()
        .filter(|path| path.is_dir());
    if let Some(canonical) = canonical {
        return Ok(canonical);
    }
    let rendered = path.display().to_string();
    if working_directory {
        Err(SandboxError::InvalidWorkingDirectory(rendered))
    } else {
        Err(SandboxError::InvalidScratchDirectory(rendered))
    }
}

fn sanitized_environment(scratch_dir: &Path) -> BTreeMap<OsString, OsString> {
    let mut values = env::vars_os()
        .filter(|(key, _)| environment_key_allowed(key))
        .collect::<BTreeMap<_, _>>();
    values.insert(OsString::from("TMPDIR"), scratch_dir.as_os_str().to_owned());
    values.insert(OsString::from("TMP"), scratch_dir.as_os_str().to_owned());
    values.insert(OsString::from("TEMP"), scratch_dir.as_os_str().to_owned());
    values
}

fn environment_key_allowed(key: &OsStr) -> bool {
    let key = key.to_string_lossy().to_ascii_uppercase();
    if [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "CREDENTIAL",
        "AUTHORIZATION",
    ]
    .iter()
    .any(|fragment| key.contains(fragment))
    {
        return false;
    }
    matches!(
        key.as_str(),
        "PATH"
            | "HOME"
            | "USER"
            | "LOGNAME"
            | "SHELL"
            | "LANG"
            | "LC_ALL"
            | "LC_CTYPE"
            | "TERM"
            | "COLORTERM"
            | "NO_COLOR"
            | "CARGO_HOME"
            | "RUSTUP_HOME"
            | "RUSTC"
            | "RUSTDOC"
            | "CC"
            | "CXX"
            | "AR"
            | "SDKROOT"
            | "MACOSX_DEPLOYMENT_TARGET"
    )
}

fn readable_roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = [
        "/bin",
        "/sbin",
        "/usr",
        "/System",
        "/Library",
        "/Applications",
        "/opt",
        "/etc",
        "/private/etc",
        "/private/var/db/timezone",
        "/lib",
        "/lib64",
        "/nix",
    ]
    .into_iter()
    .map(PathBuf::from)
    .filter(|path| path.exists())
    .collect::<BTreeSet<_>>();
    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        for relative in [".cargo", ".rustup", ".local", ".bun", "Library/Developer"] {
            let path = home.join(relative);
            if path.exists() {
                roots.insert(path);
            }
        }
        if let Some(path) = env::var_os("PATH") {
            for entry in env::split_paths(&path).filter(|path| path.starts_with(&home)) {
                if entry.exists() {
                    roots.insert(entry);
                }
            }
        }
    }
    roots.remove(cwd);
    roots.into_iter().collect()
}

fn macos_profile(request: &SandboxRequest) -> Result<String, SandboxError> {
    let read_roots = readable_roots(&request.cwd)
        .into_iter()
        .chain(std::iter::once(request.cwd.clone()))
        .chain(std::iter::once(request.workspace_root.clone()))
        .chain(std::iter::once(request.scratch_dir.clone()))
        .map(|path| sandbox_path(&path))
        .collect::<Result<Vec<_>, _>>()?;
    let workspace = sandbox_path(&request.workspace_root)?;
    let scratch = sandbox_path(&request.scratch_dir)?;
    let read_rules = read_roots
        .iter()
        .map(|path| format!("(subpath \"{path}\")"))
        .collect::<Vec<_>>()
        .join(" ");
    let network_rule = if request.allow_network {
        "(allow network*)"
    } else {
        ""
    };
    let workspace_write = match request.workspace_access {
        WorkspaceAccess::ReadOnly => String::new(),
        WorkspaceAccess::ReadWrite => format!("(subpath \"{workspace}\")"),
    };
    Ok(format!(
        "(version 1)\n(deny default)\n(import \"system.sb\")\n(allow process*)\n(allow file-read-metadata)\n(allow file-read* {read_rules})\n(allow file-write* {workspace_write} (subpath \"{scratch}\") (literal \"/dev/null\") (literal \"/dev/urandom\"))\n{network_rule}\n"
    ))
}

fn sandbox_path(path: &Path) -> Result<String, SandboxError> {
    let value = path.to_string_lossy();
    if value.contains(['\n', '\r', '"', '\\']) {
        return Err(SandboxError::UnsafePath(value.into_owned()));
    }
    Ok(value.into_owned())
}

fn add_parent_directories(args: &mut Vec<OsString>, created: &mut BTreeSet<PathBuf>, path: &Path) {
    let mut parents = path.ancestors().skip(1).collect::<Vec<_>>();
    parents.reverse();
    for parent in parents
        .into_iter()
        .filter(|parent| *parent != Path::new("/"))
    {
        let parent = parent.to_path_buf();
        if created.insert(parent.clone()) {
            args.push("--dir".into());
            args.push(parent.into_os_string());
        }
    }
}

#[cfg(target_os = "macos")]
fn executable_at(path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    path.is_file().then_some(path)
}

#[cfg(target_os = "linux")]
fn find_on_path(program: &str) -> Option<PathBuf> {
    env::var_os("PATH")
        .into_iter()
        .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Output};

    use super::*;

    fn execute(plan: SandboxLaunch, cwd: &Path) -> Output {
        let mut command = Command::new(plan.program);
        command
            .args(plan.args)
            .current_dir(cwd)
            .env_clear()
            .envs(plan.environment)
            .output()
            .expect("sandboxed command")
    }

    #[test]
    fn sensitive_environment_names_are_never_forwarded() {
        assert!(!environment_key_allowed(OsStr::new("OPENAI_API_KEY")));
        assert!(!environment_key_allowed(OsStr::new("GITHUB_TOKEN")));
        assert!(!environment_key_allowed(OsStr::new("DATABASE_PASSWORD")));
        assert!(environment_key_allowed(OsStr::new("PATH")));
        assert!(environment_key_allowed(OsStr::new("RUSTUP_HOME")));
    }

    #[test]
    fn process_only_plan_is_explicit_and_sanitized() {
        let cwd = tempfile::tempdir().expect("cwd");
        let scratch = tempfile::tempdir().expect("scratch");
        let sandbox = SystemSandbox {
            backend: SandboxBackendKind::ProcessOnly,
            launcher: None,
        };
        let plan = sandbox
            .plan(&SandboxRequest {
                program: "echo".into(),
                args: vec!["ok".into()],
                cwd: cwd.path().to_path_buf(),
                workspace_root: cwd.path().to_path_buf(),
                scratch_dir: scratch.path().to_path_buf(),
                workspace_access: WorkspaceAccess::ReadOnly,
                allow_network: false,
            })
            .expect("plan");

        assert_eq!(plan.backend, SandboxBackendKind::ProcessOnly);
        assert!(!plan.os_enforced);
        assert_eq!(plan.program, OsString::from("echo"));
        assert_eq!(
            plan.environment.get(OsStr::new("TMPDIR")),
            Some(
                &scratch
                    .path()
                    .canonicalize()
                    .expect("canonical scratch")
                    .into_os_string()
            )
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_seatbelt_enforces_workspace_and_network_boundaries() {
        use std::{
            fs,
            io::Write,
            net::TcpListener,
            thread,
            time::{Duration, Instant},
        };

        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let scratch = tempfile::tempdir().expect("scratch");
        let outside_file = outside.path().join("outside.txt");
        fs::write(&outside_file, "outside-secret").expect("outside fixture");
        let sandbox = SystemSandbox::detect();
        assert_eq!(sandbox.backend(), SandboxBackendKind::MacOsSeatbelt);

        let read_outside = sandbox
            .plan(&SandboxRequest {
                program: "/bin/cat".into(),
                args: vec![outside_file.as_os_str().to_owned()],
                cwd: workspace.path().to_path_buf(),
                workspace_root: workspace.path().to_path_buf(),
                scratch_dir: scratch.path().to_path_buf(),
                workspace_access: WorkspaceAccess::ReadWrite,
                allow_network: false,
            })
            .expect("outside read plan");
        let output = execute(read_outside, workspace.path());
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("outside-secret"));

        let inside_file = workspace.path().join("inside.txt");
        let write_inside = sandbox
            .plan(&SandboxRequest {
                program: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    format!("printf inside > '{}'", inside_file.display()).into(),
                ],
                cwd: workspace.path().to_path_buf(),
                workspace_root: workspace.path().to_path_buf(),
                scratch_dir: scratch.path().to_path_buf(),
                workspace_access: WorkspaceAccess::ReadWrite,
                allow_network: false,
            })
            .expect("inside write plan");
        assert!(execute(write_inside, workspace.path()).status.success());
        assert_eq!(
            fs::read_to_string(&inside_file).expect("inside output"),
            "inside"
        );

        let read_only_file = workspace.path().join("read-only.txt");
        let write_read_only = sandbox
            .plan(&SandboxRequest {
                program: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    format!("printf denied > '{}'", read_only_file.display()).into(),
                ],
                cwd: workspace.path().to_path_buf(),
                workspace_root: workspace.path().to_path_buf(),
                scratch_dir: scratch.path().to_path_buf(),
                workspace_access: WorkspaceAccess::ReadOnly,
                allow_network: false,
            })
            .expect("read-only write plan");
        assert!(!execute(write_read_only, workspace.path()).status.success());
        assert!(!read_only_file.exists());

        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let address = listener.local_addr().expect("listener address");
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        );
                        return true;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return false;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return false,
                }
            }
        });
        let network = sandbox
            .plan(&SandboxRequest {
                program: "/usr/bin/curl".into(),
                args: vec![
                    "--silent".into(),
                    "--show-error".into(),
                    "--max-time".into(),
                    "2".into(),
                    format!("http://{address}").into(),
                ],
                cwd: workspace.path().to_path_buf(),
                workspace_root: workspace.path().to_path_buf(),
                scratch_dir: scratch.path().to_path_buf(),
                workspace_access: WorkspaceAccess::ReadOnly,
                allow_network: false,
            })
            .expect("network plan");
        assert!(!execute(network, workspace.path()).status.success());
        assert!(!server.join().expect("server thread"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_bubblewrap_plan_isolated_mounts_and_network() {
        let workspace = tempfile::tempdir().expect("workspace");
        let scratch = tempfile::tempdir().expect("scratch");
        let sandbox = SystemSandbox {
            backend: SandboxBackendKind::LinuxBubblewrap,
            launcher: Some(PathBuf::from("/usr/bin/bwrap")),
        };
        let plan = sandbox
            .plan(&SandboxRequest {
                program: "/bin/echo".into(),
                args: vec!["ok".into()],
                cwd: workspace.path().to_path_buf(),
                workspace_root: workspace.path().to_path_buf(),
                scratch_dir: scratch.path().to_path_buf(),
                workspace_access: WorkspaceAccess::ReadWrite,
                allow_network: false,
            })
            .expect("bubblewrap plan");
        let args = plan
            .args
            .iter()
            .map(|value| value.to_string_lossy())
            .collect::<Vec<_>>();

        assert!(plan.os_enforced);
        assert!(args.iter().any(|value| value == "--unshare-net"));
        assert!(args.iter().any(|value| value == "--bind"));
        assert!(args.iter().any(|value| value == "--ro-bind"));
        assert!(args.iter().any(|value| value == "--chdir"));
        assert!(args.iter().any(|value| value == "--"));
    }
}
