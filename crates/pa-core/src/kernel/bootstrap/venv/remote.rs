//! The remote counterpart of `probe.rs`/`layout.rs`: probe an SSH host for
//! a supported Python, upload the runtime plus the session's skills, and
//! build a venv under `~/.prime/agent/remote/<key>/` there.
//!
//! The base install is keyed on its content. Each skill dir goes to
//! `<root>/skills/<id>/` (id = dir name + content hash), and a Python skill
//! is also built into `<key>/skill-sites/<id>/` on its kernel's `sys.path`,
//! so two sessions running different versions of one skill each import
//! their own. `remote_install.py` installs under flocks and only
//! garbage-collects idle, unheld dirs.
//!
//! The system `ssh` and its config are used, hardened by
//! [`crate::kernel::remote_policy`]. `PRIME_AGENT_SSH` (test seam) and
//! `PRIME_AGENT_REMOTE_INSTALL_ROOT` (absolute) override the binary and the
//! root, read from the kernel env first, then the process env.
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
const REMOTE_LAYOUT_VERSION: &str = "v3";

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

/// A remote setting from `env` (the kernel env), then the process env.
fn remote_setting(env: &HashMap<String, String>, key: &str) -> Option<String> {
    env.get(key)
        .cloned()
        .or_else(|| std::env::var(key).ok())
        .filter(|value| !value.is_empty())
}

/// The remote install root: `PRIME_AGENT_REMOTE_INSTALL_ROOT` (absolute),
/// else `~/.prime/agent/remote` (the remote expands the `~`).
fn remote_install_root(env: &HashMap<String, String>) -> anyhow::Result<String> {
    match remote_setting(env, "PRIME_AGENT_REMOTE_INSTALL_ROOT") {
        Some(root) => {
            if !root.starts_with('/') {
                bail!(
                    "PRIME_AGENT_REMOTE_INSTALL_ROOT must be an absolute remote path, got {root:?}"
                );
            }
            validate_remote_path(&root, "PRIME_AGENT_REMOTE_INSTALL_ROOT")?;
            Ok(root)
        }
        None => Ok(DEFAULT_REMOTE_INSTALL_ROOT.to_string()),
    }
}

impl RemoteHost {
    /// Validate `host` and resolve the ssh binary and install root from
    /// `env` (the kernel env), then the process env.
    pub(crate) fn new(host: &str, env: &HashMap<String, String>) -> anyhow::Result<Self> {
        let destination = SshDestination::parse(host)?;
        let ssh_program =
            remote_setting(env, "PRIME_AGENT_SSH").unwrap_or_else(|| "ssh".to_string());
        Ok(Self {
            destination,
            ssh_program: ssh_program.into(),
            install_root: remote_install_root(env)?,
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
    /// The ids of this session's skill dirs (`<root>/skills/<id>/`): the
    /// kernel holds them and pins its Python skills' sites by them.
    pub(crate) skill_ids: Vec<String>,
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
    /// Skill ids whose files are not (completely) on the remote yet.
    #[serde(default)]
    missing_stores: Vec<String>,
    /// Python skill ids whose pinned site is not built in this install yet.
    #[serde(default)]
    missing_sites: Vec<String>,
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

/// Every file of a package tree except caches, build output, hidden dirs and
/// files (`.env`, `.npmrc`, ...), and secret-named files (keys, certs: the
/// workspace snapshot's denylist). Everything collected goes to every
/// remote host a child runs on.
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
            && !name.starts_with('.')
            && !crate::workspace_snapshot::is_secret_path(&name)
            && !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("pyc"))
        {
            files.push(path);
        }
    }
    Ok(())
}

/// A skill dir's upload: its id (`<dir name>-<content hash>`) and its files
/// (archive paths under `skills/<id>/`). The id is the dir's remote name, so
/// a changed skill is a new dir and the old one stays for running kernels.
fn skill_upload(dir: &Path) -> anyhow::Result<(String, Vec<UploadFile>)> {
    let mut paths = Vec::new();
    collect_package_files(dir, &mut paths)?;
    let mut sources = Vec::with_capacity(paths.len());
    for path in paths {
        let relative = path.strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
        sources.push((relative, path));
    }
    let mut hasher = sha2::Sha256::new();
    let mut files = read_upload_files(sources, &mut hasher)?;
    let label: String = dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let label = if label.is_empty() {
        "skill".to_string()
    } else {
        label
    };
    let id = format!("{label}-{}", &format!("{:x}", hasher.finalize())[..16]);
    for file in &mut files {
        file.path = format!("skills/{id}/{}", file.path);
    }
    Ok((id, files))
}

/// Where a skill file lands on a remote host (`<root>/skills/<id>/<path>`),
/// with the same id the upload uses.
///
/// # Errors
///
/// Returns an error when the skill dir cannot be read or `skill_file` is not
/// inside it.
pub(crate) fn remote_skill_location(skill_dir: &Path, skill_file: &Path) -> anyhow::Result<String> {
    let relative = skill_file
        .strip_prefix(skill_dir)
        .with_context(|| {
            format!(
                "skill file {} is outside its dir {}",
                skill_file.display(),
                skill_dir.display()
            )
        })?
        .to_string_lossy()
        .replace('\\', "/");
    let (id, _) = skill_upload(skill_dir)?;
    Ok(format!(
        "{}/skills/{id}/{relative}",
        remote_install_root(&HashMap::new())?
    ))
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
/// skills is missing there. `skill_dirs` are the session's model-visible
/// skill dirs; the Python skills' packages (and their sibling skill
/// dependencies) are added and built into pinned sites.
#[tracing::instrument(skip_all, fields(host = %remote.label()))]
pub(crate) async fn ensure_remote_kernel_python(
    remote: &RemoteHost,
    python_skills: &[KernelPythonSkill],
    skill_dirs: &[PathBuf],
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
    let python_skills = python_skills.to_vec();
    let skill_dirs = skill_dirs.to_vec();
    let bundle_extras = extras.clone();
    // The upload: the base (runtime sources, keyed by their content, the
    // install script, and the extras) under `<base key>/`, and each skill
    // dir under `skills/<id>/` (id -> files, Python package or not).
    type SkillUploads = BTreeMap<String, (bool, Vec<UploadFile>)>;
    let (base, base_key, skills) = tokio::task::spawn_blocking(
        move || -> anyhow::Result<(Vec<UploadFile>, String, SkillUploads)> {
            let runtime = require_runtime_source_dir()?;
            let mut runtime_files = Vec::new();
            collect_package_files(&runtime.join("src"), &mut runtime_files)?;
            let mut sources = vec![(
                "runtime/pyproject.toml".to_string(),
                runtime.join("pyproject.toml"),
            )];
            for file in runtime_files {
                let relative = file.strip_prefix(&runtime)?.to_string_lossy().to_string();
                sources.push((format!("runtime/{relative}"), file));
            }
            let mut hasher = sha2::Sha256::new();
            hasher.update(REMOTE_LAYOUT_VERSION.as_bytes());
            // The install script defines how the base is built: a changed
            // script builds fresh next to the old install.
            hasher.update(INSTALL_SCRIPT.as_bytes());
            for extra in &bundle_extras {
                hasher.update(extra.as_bytes());
                hasher.update([0]);
            }
            let mut base = read_upload_files(sources, &mut hasher)?;
            let base_key = format!(
                "{REMOTE_LAYOUT_VERSION}-{}",
                &format!("{:x}", hasher.finalize())[..16]
            );
            for file in &mut base {
                file.path = format!("{base_key}/{}", file.path);
            }
            let mut skills = SkillUploads::new();
            let python_dirs: Vec<PathBuf> = normalize_python_skills(&python_skills)
                .into_iter()
                .map(|skill| PathBuf::from(skill.package_path))
                .collect();
            for (dir, python) in python_dirs
                .iter()
                .map(|dir| (dir, true))
                .chain(skill_dirs.iter().map(|dir| (dir, false)))
            {
                let (id, files) = skill_upload(dir)?;
                let entry = skills.entry(id).or_insert((false, files));
                entry.0 |= python;
            }
            Ok((base, base_key, skills))
        },
    )
    .await
    .map_err(|join| anyhow!("remote upload collection failed: {join}"))??;
    let label = remote.label();
    let skill_kinds: BTreeMap<&str, bool> = skills
        .iter()
        .map(|(id, (python, _))| (id.as_str(), *python))
        .collect();
    let skill_kinds = serde_json::to_string(&skill_kinds)?;
    report(&format!("Probing Python on {label}..."));
    let stdout = run_remote(
        remote,
        &python_command(
            PROBE_SCRIPT,
            &[&remote.install_root, &base_key, &skill_kinds],
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
        skill_ids: skills.keys().cloned().collect(),
    };
    if probe.installed && probe.missing_stores.is_empty() && probe.missing_sites.is_empty() {
        return Ok(install);
    }
    let mut files = Vec::new();
    for (id, (_, skill_files)) in skills {
        if probe.missing_stores.contains(&id) {
            files.extend(skill_files);
        }
    }
    if probe.installed {
        let mut missing: Vec<&String> = probe
            .missing_stores
            .iter()
            .chain(&probe.missing_sites)
            .collect();
        missing.sort();
        missing.dedup();
        report(&format!(
            "Installing skills on {label}: {}...",
            missing
                .iter()
                .map(|id| id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
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
                path: format!("{base_key}/bin/uv"),
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
                &skill_kinds,
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

    /// A skill dir's upload leaves out dotfiles and dot dirs, secret-named
    /// files, and caches; its id is content-addressed, and the prompt's
    /// location is the uploaded SKILL.md under that id.
    #[test]
    fn skill_uploads_skip_dotfiles_and_secrets_and_name_their_remote_location() {
        let dir = tempfile::TempDir::new().unwrap();
        let skill = dir.path().join("field-notes");
        for (path, text) in [
            ("SKILL.md", "---\nname: field-notes\n---\n"),
            ("scripts/run.sh", "echo hi\n"),
            (".env", "TOKEN=secret\n"),
            (".npmrc", "//registry/:_authToken=secret\n"),
            ("keys/deploy.pem", "-----BEGIN-----\n"),
            ("id_ed25519", "private\n"),
            (".git/config", "[core]\n"),
            ("__pycache__/x.pyc", "bytecode"),
        ] {
            let file = skill.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }
        let (id, files) = skill_upload(&skill).unwrap();
        assert!(id.starts_with("field-notes-") && id.len() == "field-notes-".len() + 16);
        let paths: Vec<String> = files.iter().map(|file| file.path.clone()).collect();
        assert_eq!(
            paths,
            [
                format!("skills/{id}/SKILL.md"),
                format!("skills/{id}/scripts/run.sh")
            ]
        );
        assert_eq!(
            remote_skill_location(&skill, &skill.join("SKILL.md")).unwrap(),
            format!("~/.prime/agent/remote/skills/{id}/SKILL.md")
        );
        // A changed skill is a new dir; the old one stays for its kernels.
        std::fs::write(skill.join("SKILL.md"), "changed").unwrap();
        assert_ne!(skill_upload(&skill).unwrap().0, id);
    }

    /// The probe touches every install and skill dir it reports present:
    /// garbage collection removes only dirs idle for a week, so nothing the
    /// probe just vouched for can vanish before its kernel holds it.
    #[test]
    fn the_probe_touches_what_it_reports_present() {
        let root = tempfile::TempDir::new().unwrap();
        let key = "v3-0000000000000000";
        let id = "notes-0123456789abcdef";
        let install = root.path().join(key);
        let store = root.path().join("skills").join(id);
        std::fs::create_dir_all(install.join("venv/bin")).unwrap();
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(install.join("venv/bin/python"), "").unwrap();
        std::fs::write(install.join(".installed"), key).unwrap();
        std::fs::write(store.join(".complete"), id).unwrap();
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(python_command(
                PROBE_SCRIPT,
                &[
                    &root.path().to_string_lossy(),
                    key,
                    &format!("{{\"{id}\": false}}"),
                ],
            ))
            .output()
            .expect("probe runs");
        assert!(output.status.success(), "{output:?}");
        let report: ProbeReport =
            last_json_line(&String::from_utf8_lossy(&output.stdout)).expect("report");
        assert!(report.installed);
        assert_eq!(
            (report.missing_stores, report.missing_sites),
            (Vec::<String>::new(), Vec::<String>::new())
        );
        for dir in [&install, &store] {
            let touched = std::fs::metadata(dir.join(".last-used"))
                .and_then(|metadata| metadata.modified())
                .expect("the probe touched .last-used");
            assert!(touched.elapsed().unwrap() < std::time::Duration::from_secs(60));
        }
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
