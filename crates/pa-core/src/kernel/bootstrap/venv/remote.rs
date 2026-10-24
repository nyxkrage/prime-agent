//! The remote kernel environment concern: the remote counterpart of
//! `probe.rs`/`layout.rs`. It probes an SSH host for a supported Python,
//! uploads the bundled runtime (plus the session's Python skills), and
//! builds a venv under `~/.prime/agent/remote/<key>/` on the remote.
//!
//! The base install (runtime + extras) is keyed on its content, and each
//! skill is stamped by its own content hash, so a skill change reinstalls
//! only that skill. Installs run under an flock on the remote
//! (`remote_install.py`), and unused old installs this tool created are
//! garbage-collected there.
//!
//! The system `ssh` binary and config are used (keys, agents, `ProxyJump`,
//! an existing control master), hardened by the policy in
//! [`crate::kernel::remote_policy`]. `PRIME_AGENT_SSH` overrides the binary
//! (a test seam: a shim that runs commands locally), and
//! `PRIME_AGENT_REMOTE_INSTALL_ROOT` (absolute) overrides the remote
//! install root; both are read from the kernel env first, then the process
//! env.
//!
//! Every remote command is shell-agnostic (csh/tcsh login shells parse it
//! too): Python scripts travel as one-line `exec(base64...)` programs and
//! the kernel launch is a single `exec /bin/sh -c '...'`.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::io::Write as _;
use std::process::Stdio;

use anyhow::{anyhow, bail, Context};
use base64::Engine as _;
use sha2::Digest;
use tokio::io::AsyncWriteExt;

use super::normalize_python_skills;
use super::{require_runtime_source_dir, Path, PathBuf, STATE_SNAPSHOT_REQUIREMENT};
use crate::kernel::bootstrap::{
    default_rlm_extra_uv_args, KernelBootstrapProgressHandler, KernelPythonSkill,
};
use crate::kernel::remote_policy::{
    ssh_process_env, validate_remote_path, SSH_HARDENING_OPTIONS, SSH_HOST_CHARS,
};

/// Lowest remote Python the runtime supports (`requires-python` in
/// `prime-agent-runtime/pyproject.toml`).
const MIN_REMOTE_PYTHON: (u64, u64) = (3, 11);
const DEFAULT_REMOTE_INSTALL_ROOT: &str = "~/.prime/agent/remote";
/// Bumped when the remote layout or install script changes incompatibly:
/// part of the base key, so a bump installs fresh next to the old layout.
const REMOTE_LAYOUT_VERSION: &str = "v2";

const PROBE_SCRIPT: &str = include_str!("remote_probe.py");
const INSTALL_SCRIPT: &str = include_str!("remote_install.py");

/// A validated SSH destination: `ssh` receives `destination` verbatim and
/// `-p port` when the spawn named `host:port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshDestination {
    destination: String,
    port: Option<u16>,
}

impl SshDestination {
    /// Validate a spawn `host` against [`SSH_HOST_CHARS`]: `[user@]host`,
    /// `[user@]host:port`, `[user@][v6addr]` / `[user@][v6addr]:port`, or
    /// `ssh://...`. Nothing may start with `-`.
    pub(crate) fn parse(host: &str) -> anyhow::Result<Self> {
        if host.is_empty() {
            bail!("rlm.spawn host must be a non-empty SSH destination");
        }
        if let Some(bad) = host
            .strip_prefix("ssh://")
            .unwrap_or(host)
            .chars()
            .find(|c| !SSH_HOST_CHARS.contains(*c))
        {
            bail!(
                "rlm.spawn host {host:?} contains {bad:?}; an SSH destination may only use letters, digits, and . _ @ [ ] : -"
            );
        }
        if let Some(rest) = host.strip_prefix("ssh://") {
            let host_part = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
            if rest.starts_with('-') || host_part.is_empty() || host_part.starts_with('-') {
                bail!("rlm.spawn host has an invalid ssh:// destination: {host}");
            }
            return Ok(Self {
                destination: host.to_string(),
                port: None,
            });
        }
        let (user, rest) = match host.split_once('@') {
            Some((user, rest)) => {
                if user.is_empty() || user.starts_with('-') || rest.contains('@') {
                    bail!("rlm.spawn host has an invalid user: {host}");
                }
                (Some(user), rest)
            }
            None => (None, host),
        };
        let invalid = || anyhow!("rlm.spawn host is not a valid SSH destination: {host}");
        let (name, port) = if let Some(bracketed) = rest.strip_prefix('[') {
            // `[v6addr]` or `[v6addr]:port`: ssh takes the bare address.
            let (address, after) = bracketed.split_once(']').ok_or_else(invalid)?;
            if address.is_empty()
                || !address
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
            {
                return Err(invalid());
            }
            let port = match after {
                "" => None,
                _ => Some(after.strip_prefix(':').ok_or_else(invalid)?),
            };
            (address, port)
        } else {
            if rest.contains('[') || rest.contains(']') {
                return Err(invalid());
            }
            match rest.split_once(':') {
                Some((_, port)) if port.contains(':') => bail!(
                    "rlm.spawn host {host:?} has more than one ':'; write an IPv6 address in brackets, e.g. [::1]:22"
                ),
                Some((name, port)) => (name, Some(port)),
                None => (rest, None),
            }
        };
        if name.is_empty() || name.starts_with('-') {
            return Err(invalid());
        }
        let port = port
            .map(|port| {
                port.parse::<u16>()
                    .ok()
                    .filter(|port| *port > 0)
                    .ok_or_else(|| anyhow!("rlm.spawn host has an invalid port: {host}"))
            })
            .transpose()?;
        Ok(Self {
            destination: match user {
                Some(user) => format!("{user}@{name}"),
                None => name.to_string(),
            },
            port,
        })
    }

    /// The destination for user-facing messages.
    pub(crate) fn label(&self) -> String {
        match self.port {
            Some(port) if self.destination.contains(':') => {
                let (user, address) = match self.destination.rsplit_once('@') {
                    Some((user, address)) => (format!("{user}@"), address),
                    None => (String::new(), self.destination.as_str()),
                };
                format!("{user}[{address}]:{port}")
            }
            Some(port) => format!("{}:{port}", self.destination),
            None => self.destination.clone(),
        }
    }
}

/// One SSH remote: the validated destination plus the `ssh` binary and the
/// remote install root it is reached with.
#[derive(Debug, Clone)]
pub(crate) struct RemoteHost {
    destination: SshDestination,
    ssh_program: OsString,
    install_root: String,
}

impl RemoteHost {
    /// Validate `host` and resolve the ssh binary and install root from
    /// `env` (the kernel env), then the process env.
    pub(crate) fn new(host: &str, env: &HashMap<String, String>) -> anyhow::Result<Self> {
        let destination = SshDestination::parse(host)?;
        let setting = |key: &str| {
            env.get(key)
                .cloned()
                .or_else(|| std::env::var(key).ok())
                .filter(|value| !value.is_empty())
        };
        let ssh_program = setting("PRIME_AGENT_SSH").unwrap_or_else(|| "ssh".to_string());
        let install_root = match setting("PRIME_AGENT_REMOTE_INSTALL_ROOT") {
            Some(root) => {
                if !root.starts_with('/') {
                    bail!("PRIME_AGENT_REMOTE_INSTALL_ROOT must be an absolute remote path, got {root:?}");
                }
                validate_remote_path(&root, "PRIME_AGENT_REMOTE_INSTALL_ROOT")?;
                root
            }
            None => DEFAULT_REMOTE_INSTALL_ROOT.to_string(),
        };
        Ok(Self {
            destination,
            ssh_program: ssh_program.into(),
            install_root,
        })
    }

    /// The destination for user-facing messages.
    pub(crate) fn label(&self) -> String {
        self.destination.label()
    }

    /// `ssh -T <hardening options> [-p port] -- <destination> <command>`,
    /// stdio piped, env cleared to the ssh allowlist.
    pub(crate) fn command(&self, remote_command: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.ssh_program);
        command.arg("-T");
        for option in SSH_HARDENING_OPTIONS {
            command.arg("-o").arg(option);
        }
        if let Some(port) = self.destination.port {
            command.arg("-p").arg(port.to_string());
        }
        command
            .arg("--")
            .arg(&self.destination.destination)
            .arg(remote_command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .envs(ssh_process_env(std::env::vars()))
            .kill_on_drop(true);
        crate::platform::process::set_no_window(command.as_std_mut());
        command
    }
}

/// Quote one word for the remote POSIX shell (csh/tcsh read the same
/// quoting for the newline-free words this module sends).
pub(crate) fn sh_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// A shell-agnostic `python3 -c` invocation of `script` with `args`: the
/// script travels as one base64 line, so no login shell sees its newlines.
fn python_command(script: &str, args: &[&str]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(script);
    let mut command = format!(
        "python3 -c 'import base64;exec(compile(base64.b64decode(\"{encoded}\"),\"<prime-agent-remote>\",\"exec\"))'"
    );
    for arg in args {
        command.push(' ');
        command.push_str(&sh_quote(arg));
    }
    command
}

/// A provisioned remote kernel environment (remote absolute paths).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteKernelInstall {
    /// The venv interpreter that runs `python -m rlm.repl`.
    pub(crate) python: String,
    /// The install root (`~/.prime/agent/remote`, expanded).
    pub(crate) root: String,
    /// This install's dir (`<root>/<key>`).
    pub(crate) dir: String,
}

#[derive(Debug, serde::Deserialize)]
struct ProbeReport {
    version: Vec<u64>,
    executable: String,
    venv: bool,
    uv: Option<String>,
    system: String,
    machine: String,
    root: String,
    dir: String,
    installed: bool,
    stale_skills: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct InstallReport {
    installed_base: bool,
    failed_skills: Vec<FailedSkill>,
}

#[derive(Debug, serde::Deserialize)]
struct FailedSkill {
    name: String,
    error: String,
}

/// One file of the upload archive: its archive path, bytes, and mode.
struct UploadFile {
    path: String,
    bytes: Vec<u8>,
    mode: u32,
}

/// The upload payload: the base (runtime sources) keyed by its content plus
/// the extras, and each skill keyed by its own content.
struct UploadBundle {
    base: Vec<UploadFile>,
    base_key: String,
    /// Skill dir name -> (content hash, files under `skills/<name>-<hash>/`).
    skills: BTreeMap<String, (String, Vec<UploadFile>)>,
}

/// Read `sources` (archive path, local file) into upload files and hash
/// them: the hash covers every path and byte.
fn read_upload_files(
    mut sources: Vec<(String, PathBuf)>,
    hasher: &mut sha2::Sha256,
) -> anyhow::Result<Vec<UploadFile>> {
    sources.sort();
    let mut files = Vec::with_capacity(sources.len());
    for (path, source) in sources {
        let bytes = std::fs::read(&source)
            .with_context(|| format!("read {} for the remote upload", source.display()))?;
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(&bytes);
        hasher.update([0]);
        files.push(UploadFile {
            path,
            bytes,
            mode: 0o644,
        });
    }
    Ok(files)
}

/// Every file of a package tree except caches, build output, and hidden dirs.
fn collect_package_files(dir: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            let skipped = name.starts_with('.')
                || name == "__pycache__"
                || name == "node_modules"
                || name == "build"
                || name == "dist"
                || name.ends_with(".egg-info");
            if !skipped {
                collect_package_files(&path, files)?;
            }
        } else if file_type.is_file()
            && !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("pyc"))
        {
            files.push(path);
        }
    }
    Ok(())
}

/// Collect the runtime sources and each skill package (with its sibling
/// skill dependencies, like the local venv) and key them.
fn collect_upload_bundle(
    python_skills: &[KernelPythonSkill],
    extras: &[&str],
) -> anyhow::Result<UploadBundle> {
    let runtime = require_runtime_source_dir()?;
    let mut sources: Vec<(String, PathBuf)> = vec![(
        "runtime/pyproject.toml".to_string(),
        runtime.join("pyproject.toml"),
    )];
    let mut runtime_files = Vec::new();
    collect_package_files(&runtime.join("src"), &mut runtime_files)?;
    for file in runtime_files {
        let relative = file.strip_prefix(&runtime)?.to_string_lossy().to_string();
        sources.push((format!("runtime/{relative}"), file));
    }
    let mut hasher = sha2::Sha256::new();
    hasher.update(REMOTE_LAYOUT_VERSION.as_bytes());
    // The install script defines how the base is built: a changed script
    // builds fresh next to the old install.
    hasher.update(INSTALL_SCRIPT.as_bytes());
    for extra in extras {
        hasher.update(extra.as_bytes());
        hasher.update([0]);
    }
    let base = read_upload_files(sources, &mut hasher)?;
    let base_key = format!("v2-{}", &format!("{:x}", hasher.finalize())[..16]);

    let mut skills = BTreeMap::new();
    for skill in normalize_python_skills(python_skills) {
        let name = skill.import_name.clone();
        if skills.contains_key(&name) {
            continue;
        }
        let package = PathBuf::from(&skill.package_path);
        let mut files = Vec::new();
        collect_package_files(&package, &mut files)?;
        let mut sources = Vec::with_capacity(files.len());
        for file in files {
            let relative = file.strip_prefix(&package)?.to_string_lossy().to_string();
            sources.push((relative, file));
        }
        let mut hasher = sha2::Sha256::new();
        let mut files = read_upload_files(sources, &mut hasher)?;
        let hash = format!("{:x}", hasher.finalize())[..16].to_string();
        for file in &mut files {
            file.path = format!("skills/{name}-{hash}/{}", file.path);
        }
        skills.insert(name, (hash, files));
    }
    Ok(UploadBundle {
        base,
        base_key,
        skills,
    })
}

/// Run one remote command to completion, feeding `stdin`; the error names
/// the host and carries the stderr tail.
async fn run_remote(
    remote: &RemoteHost,
    remote_command: &str,
    stdin: Vec<u8>,
    what: &str,
) -> anyhow::Result<String> {
    let mut child = remote
        .command(remote_command)
        .spawn()
        .with_context(|| format!("failed to start ssh for {}", remote.label()))?;
    let mut pipe = child.stdin.take().context("ssh stdin is missing")?;
    let writer = tokio::spawn(async move {
        let written = pipe.write_all(&stdin).await;
        drop(pipe);
        written
    });
    let output = child.wait_with_output().await?;
    // A remote that exits early closes the pipe; its exit status is the cause.
    let _ = writer.await;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let chars: Vec<char> = stderr.trim().chars().collect();
    let tail: String = chars[chars.len().saturating_sub(2048)..].iter().collect();
    bail!(
        "{what} on remote host {} failed (exit code {}): {}",
        remote.label(),
        output
            .status
            .code()
            .map_or_else(|| "none".to_string(), |code| code.to_string()),
        if tail.is_empty() {
            "(no stderr)"
        } else {
            &tail
        }
    )
}

/// The last JSON line of a remote command's stdout (shell rc output may
/// precede it).
fn last_json_line<T: serde::de::DeserializeOwned>(stdout: &str) -> Option<T> {
    stdout
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str(line.trim()).ok())
}

/// Probe the remote, then upload and install whatever of the base and the
/// skills is missing or stale there.
#[tracing::instrument(skip_all, fields(host = %remote.label()))]
pub(crate) async fn ensure_remote_kernel_python(
    remote: &RemoteHost,
    python_skills: &[KernelPythonSkill],
    on_progress: Option<&KernelBootstrapProgressHandler>,
) -> anyhow::Result<RemoteKernelInstall> {
    let report = |message: &str| {
        if let Some(progress) = on_progress {
            progress(message);
        }
    };
    // The snapshot serializer plus the extras the system prompt promises,
    // exactly like the local kernel venv.
    let mut extras = vec![STATE_SNAPSHOT_REQUIREMENT];
    extras.extend(default_rlm_extra_uv_args());
    let skills = python_skills.to_vec();
    let bundle_extras = extras.clone();
    let bundle =
        tokio::task::spawn_blocking(move || collect_upload_bundle(&skills, &bundle_extras))
            .await
            .map_err(|join| anyhow!("remote upload collection failed: {join}"))??;
    let label = remote.label();
    let skill_hashes: BTreeMap<&str, &str> = bundle
        .skills
        .iter()
        .map(|(name, (hash, _))| (name.as_str(), hash.as_str()))
        .collect();
    report(&format!("Probing Python on {label}..."));
    let stdout = run_remote(
        remote,
        &python_command(
            PROBE_SCRIPT,
            &[
                &remote.install_root,
                &bundle.base_key,
                &serde_json::to_string(&skill_hashes)?,
            ],
        ),
        Vec::new(),
        "Python probe",
    )
    .await
    .map_err(|error| {
        anyhow!("{error:#}\nThe remote host needs python3 (3.11 or newer) on its PATH.")
    })?;
    let probe: ProbeReport = last_json_line(&stdout)
        .ok_or_else(|| anyhow!("Python probe on remote host {label} printed no report"))?;
    let (major, minor) = (
        probe.version.first().copied().unwrap_or(0),
        probe.version.get(1).copied().unwrap_or(0),
    );
    if (major, minor) < MIN_REMOTE_PYTHON {
        bail!(
            "Remote host {label} has Python {} at {}; the Prime Agent kernel needs Python {}.{} or newer as python3.",
            probe
                .version
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join("."),
            probe.executable,
            MIN_REMOTE_PYTHON.0,
            MIN_REMOTE_PYTHON.1
        );
    }
    let install = RemoteKernelInstall {
        python: format!("{}/venv/bin/python", probe.dir),
        root: probe.root.clone(),
        dir: probe.dir.clone(),
    };
    if probe.installed && probe.stale_skills.is_empty() {
        return Ok(install);
    }
    let mut files = Vec::new();
    let mut to_install: BTreeMap<String, String> = BTreeMap::new();
    let UploadBundle {
        base,
        base_key,
        skills,
    } = bundle;
    for (name, (hash, skill_files)) in skills {
        if !probe.installed || probe.stale_skills.contains(&name) {
            files.extend(skill_files);
            to_install.insert(name, hash);
        }
    }
    if probe.installed {
        report(&format!(
            "Installing Python skills on {label}: {}...",
            to_install.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    } else {
        files.extend(base);
        if probe.uv.is_none() && !probe.venv {
            // No remote uv and no working `python3 -m venv`: ship this
            // machine's uv when it can run there.
            let local_uv = super::ensure_uv().map_err(|error| {
                anyhow!(
                    "Remote host {label} has neither uv nor a working `python3 -m venv` (ensurepip is missing), and this machine has no uv to ship there: {error:#}. Install uv or the python3-venv package on the remote."
                )
            })?;
            let local_os = match std::env::consts::OS {
                "linux" => "Linux",
                "macos" => "Darwin",
                other => other,
            };
            let arch_matches = match std::env::consts::ARCH {
                "x86_64" => matches!(probe.machine.as_str(), "x86_64" | "amd64" | "AMD64"),
                "aarch64" => matches!(probe.machine.as_str(), "aarch64" | "arm64"),
                other => other == probe.machine,
            };
            if local_os != probe.system || !arch_matches {
                bail!(
                    "Remote host {label} has neither uv nor a working `python3 -m venv` (ensurepip is missing), and this machine's uv cannot run there ({} {}). Install uv or the python3-venv package on the remote.",
                    probe.system,
                    probe.machine
                );
            }
            files.push(UploadFile {
                path: "bin/uv".to_string(),
                bytes: std::fs::read(&local_uv).with_context(|| format!("read {local_uv}"))?,
                mode: 0o755,
            });
        }
        report(&format!(
            "Installing the Prime Agent runtime on {label} ({})...",
            probe.dir
        ));
    }
    let archive = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        for file in &files {
            let mut header = tar::Header::new_gnu();
            header.set_size(file.bytes.len() as u64);
            header.set_mode(file.mode);
            header.set_cksum();
            archive.append_data(&mut header, &file.path, file.bytes.as_slice())?;
        }
        let mut encoder = archive.into_inner()?;
        encoder.flush()?;
        Ok(encoder.finish()?)
    })
    .await
    .map_err(|join| anyhow!("remote upload archive failed: {join}"))??;
    let stdout = run_remote(
        remote,
        &python_command(
            INSTALL_SCRIPT,
            &[
                &remote.install_root,
                &base_key,
                &serde_json::to_string(&to_install)?,
                &serde_json::to_string(&extras)?,
                if probe.installed { "0" } else { "1" },
            ],
        ),
        archive,
        "Runtime install",
    )
    .await?;
    let outcome: InstallReport = last_json_line(&stdout)
        .ok_or_else(|| anyhow!("Runtime install on remote host {label} printed no report"))?;
    tracing::info!(installed_base = outcome.installed_base, "remote install");
    // A failing skill warns and the kernel still starts; its import then
    // fails in the kernel bootstrap, which reports it as unavailable.
    for failed in outcome.failed_skills {
        tracing::warn!(skill = %failed.name, error = %failed.error, "remote skill install failed");
        report(&format!(
            "Python skill {} failed to install on {label}: {}",
            failed.name, failed.error
        ));
    }
    Ok(install)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(host: &str) -> (String, Option<u16>) {
        let parsed = SshDestination::parse(host).unwrap();
        (parsed.destination, parsed.port)
    }

    #[test]
    fn destinations_parse_users_ports_ipv6_and_ssh_urls() {
        assert_eq!(parsed("user@box"), ("user@box".to_string(), None));
        assert_eq!(
            parsed("user@box:2222"),
            ("user@box".to_string(), Some(2222))
        );
        assert_eq!(parsed("gpu-box.lan"), ("gpu-box.lan".to_string(), None));
        assert_eq!(parsed("[::1]"), ("::1".to_string(), None));
        assert_eq!(
            parsed("root@[fe80::1]:22"),
            ("root@fe80::1".to_string(), Some(22))
        );
        assert_eq!(parsed("ssh://box:22"), ("ssh://box:22".to_string(), None));
        assert_eq!(
            SshDestination::parse("root@[fe80::1]:22").unwrap().label(),
            "root@[fe80::1]:22"
        );
    }

    #[test]
    fn destinations_outside_the_allowlist_are_refused() {
        for bad in [
            "",
            "-oProxyCommand=x",
            "user@-oProxyCommand=x",
            "box$(touch${IFS}/tmp/pwned)",
            "box`id`",
            "box;id",
            "a b",
            " box",
            "box\n",
            "%h",
            "box:notaport",
            "box:0",
            "a:b:22",
            "::1",
            "[::1",
            "[box]",
            "[::1]x",
            "@box",
            "a@b@c",
            "ssh://-oProxyCommand=x",
            "ssh://",
        ] {
            assert!(
                SshDestination::parse(bad).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    /// Debian/Ubuntu without python3-venv ship an ensurepip stub with no
    /// bundled pip: the probe must report no working venv there, so the
    /// install falls back to uv.
    #[test]
    fn the_probe_rejects_an_ensurepip_stub_without_pip() {
        let stub = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(stub.path().join("ensurepip")).unwrap();
        std::fs::write(
            stub.path().join("ensurepip/__init__.py"),
            "def version():\n    return None\n",
        )
        .unwrap();
        let root = tempfile::TempDir::new().unwrap();
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(python_command(
                PROBE_SCRIPT,
                &[&root.path().to_string_lossy(), "v2-0000000000000000", "{}"],
            ))
            .env("PYTHONPATH", stub.path())
            .output()
            .expect("probe runs");
        assert!(output.status.success(), "{output:?}");
        let report: ProbeReport =
            last_json_line(&String::from_utf8_lossy(&output.stdout)).expect("report");
        assert!(!report.venv, "an ensurepip stub is not a working venv");
        assert!(!report.installed);
    }

    #[test]
    fn shell_quoting_survives_single_quotes() {
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn python_commands_are_one_line() {
        let command = python_command("import sys\nprint(sys.argv[1:])\n", &["a b", "it's"]);
        assert!(!command.contains('\n'), "{command}");
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .output()
            .expect("sh runs");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "['a b', \"it's\"]"
        );
    }
}
