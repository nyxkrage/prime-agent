//! The remote kernel environment concern: the remote counterpart of
//! `probe.rs`/`layout.rs`. It probes an SSH host for a supported Python,
//! uploads the bundled runtime (plus the session's Python skills), and
//! builds a venv under `~/.prime/agent/remote/<version>/` on the remote.
//! A version stamp there makes later spawns skip the upload.
//!
//! The system `ssh` binary and config are used, so keys, agents,
//! `ProxyJump`, and `ControlMaster` work unchanged. `PRIME_AGENT_SSH`
//! overrides the binary (a test seam: a shim that runs commands locally),
//! and `PRIME_AGENT_REMOTE_INSTALL_ROOT` overrides the remote install root.

use std::io::Write as _;
use std::process::Stdio;

use anyhow::{anyhow, bail, Context};
use sha2::Digest;
use tokio::io::AsyncWriteExt;

use super::{require_runtime_source_dir, Path, PathBuf, STATE_SNAPSHOT_REQUIREMENT};
use crate::kernel::bootstrap::{
    default_rlm_extra_uv_args, KernelBootstrapProgressHandler, KernelPythonSkill,
};

/// Lowest remote Python the runtime supports (`requires-python` in
/// `prime-agent-runtime/pyproject.toml`).
const MIN_REMOTE_PYTHON: (u64, u64) = (3, 11);
const DEFAULT_REMOTE_INSTALL_ROOT: &str = "~/.prime/agent/remote";

/// Probes the remote interpreter and the install stamp in one round trip.
/// argv: install root (may start with `~`), version key.
const PROBE_SCRIPT: &str = r#"
import importlib.util, json, os, platform, shutil, sys
root = os.path.expanduser(sys.argv[1])
key = sys.argv[2]
target = os.path.join(root, key)
python = os.path.join(target, "venv", "bin", "python")
stamp = os.path.join(target, ".installed")
installed = False
if os.path.isfile(python) and os.path.isfile(stamp):
    with open(stamp) as handle:
        installed = handle.read().strip() == key
def has(module):
    return importlib.util.find_spec(module) is not None
print(json.dumps({
    "version": list(sys.version_info[:3]),
    "executable": sys.executable,
    "venv": has("venv") and has("ensurepip"),
    "uv": shutil.which("uv"),
    "system": platform.system(),
    "machine": platform.machine(),
    "root": root,
    "dir": target,
    "installed": installed,
}))
"#;

/// Unpacks the uploaded archive (stdin) and builds the venv under an
/// exclusive lock dir, so concurrent spawns on one host install once.
/// argv: install root, version key, JSON list of skill dir names, JSON list
/// of extra packages.
const INSTALL_SCRIPT: &str = r#"
import io, json, os, shutil, subprocess, sys, tarfile, time
root = os.path.expanduser(sys.argv[1])
key = sys.argv[2]
skills = json.loads(sys.argv[3])
extras = json.loads(sys.argv[4])
target = os.path.join(root, key)
venv = os.path.join(target, "venv")
python = os.path.join(venv, "bin", "python")
stamp = os.path.join(target, ".installed")
archive = sys.stdin.buffer.read()
os.makedirs(root, exist_ok=True)
lock = target + ".lock"
deadline = time.time() + 900
while True:
    try:
        os.mkdir(lock)
        break
    except FileExistsError:
        try:
            if time.time() - os.stat(lock).st_mtime > 900:
                os.rmdir(lock)
                continue
        except FileNotFoundError:
            continue
        if time.time() > deadline:
            sys.exit("timed out waiting for the remote install lock " + lock)
        time.sleep(0.5)
def run(command):
    code = subprocess.run(command, stdin=subprocess.DEVNULL, stdout=sys.stderr.fileno()).returncode
    if code != 0:
        sys.exit(" ".join(command) + " failed with exit code " + str(code))
try:
    current = False
    if os.path.isfile(python) and os.path.isfile(stamp):
        with open(stamp) as handle:
            current = handle.read().strip() == key
    if current:
        print(json.dumps({"installed": False}))
    else:
        shutil.rmtree(target, ignore_errors=True)
        os.makedirs(target)
        with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as bundle:
            if hasattr(tarfile, "data_filter"):
                bundle.extractall(target, filter="data")
            else:
                bundle.extractall(target)
        uploaded_uv = os.path.join(target, "bin", "uv")
        uv = shutil.which("uv") or (uploaded_uv if os.path.isfile(uploaded_uv) else None)
        packages = [os.path.join(target, "runtime")]
        packages += [os.path.join(target, "skills", name) for name in skills]
        packages += extras
        if uv:
            run([uv, "venv", "--quiet", "--python", sys.executable, venv])
            run([uv, "pip", "install", "--quiet", "--python", python] + packages)
        else:
            run([sys.executable, "-m", "venv", venv])
            run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check"] + packages)
        with open(stamp, "w") as handle:
            handle.write(key)
        print(json.dumps({"installed": True}))
finally:
    os.rmdir(lock)
"#;

/// A validated SSH destination: `ssh` receives `destination` verbatim and
/// `-p port` when the spawn named `host:port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshDestination {
    destination: String,
    port: Option<u16>,
}

impl SshDestination {
    /// Validate a spawn `host`. Rejects values `ssh` would read as an option
    /// or that carry whitespace, and splits a trailing `:port`.
    pub(crate) fn parse(host: &str) -> anyhow::Result<Self> {
        let host = host.trim();
        if host.is_empty() {
            bail!("rlm.spawn host must be a non-empty SSH destination");
        }
        if host.starts_with('-') {
            bail!("rlm.spawn host must not start with '-': {host}");
        }
        if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
            bail!("rlm.spawn host must not contain whitespace: {host:?}");
        }
        if host.starts_with("ssh://") {
            return Ok(Self {
                destination: host.to_string(),
                port: None,
            });
        }
        if let Some((name, port)) = host.rsplit_once(':') {
            let port = port
                .parse::<u16>()
                .ok()
                .filter(|_| !name.is_empty() && !name.contains(':'))
                .ok_or_else(|| anyhow!("rlm.spawn host has an invalid port: {host}"))?;
            return Ok(Self {
                destination: name.to_string(),
                port: Some(port),
            });
        }
        Ok(Self {
            destination: host.to_string(),
            port: None,
        })
    }

    /// The destination for user-facing messages.
    pub(crate) fn label(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}", self.destination),
            None => self.destination.clone(),
        }
    }

    /// `ssh <options> -- <destination> <remote_command>`, with stdio piped.
    /// `BatchMode` makes a missing key fail instead of prompting, and the
    /// keepalives turn a dropped connection into an exit instead of a hang.
    pub(crate) fn command(&self, remote_command: &str) -> tokio::process::Command {
        let program = std::env::var_os("PRIME_AGENT_SSH")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "ssh".into());
        let mut command = tokio::process::Command::new(program);
        command.args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=20",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
        ]);
        if let Some(port) = self.port {
            command.arg("-p").arg(port.to_string());
        }
        command
            .arg("--")
            .arg(&self.destination)
            .arg(remote_command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        crate::platform::process::set_no_window(command.as_std_mut());
        command
    }
}

/// Quote one word for the remote POSIX shell.
pub(crate) fn sh_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// A provisioned remote kernel environment (remote absolute paths).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteKernelInstall {
    /// The venv interpreter that runs `python -m rlm.repl`.
    pub(crate) python: String,
    /// The install root (`~/.prime/agent/remote`, expanded).
    pub(crate) root: String,
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
}

/// One file of the upload archive: its archive path, bytes, and mode.
struct UploadFile {
    path: String,
    bytes: Vec<u8>,
    mode: u32,
}

/// The upload payload: runtime sources and skill packages, plus the
/// content key that names the remote install dir.
struct UploadBundle {
    files: Vec<UploadFile>,
    skill_dirs: Vec<String>,
    key: String,
}

/// Collect the runtime sources and the skill packages. The key hashes
/// every uploaded byte, so any runtime or skill change installs fresh
/// next to the old version.
fn collect_upload_bundle(python_skills: &[KernelPythonSkill]) -> anyhow::Result<UploadBundle> {
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
    let mut skill_dirs = Vec::new();
    for skill in python_skills {
        let dir_name = skill.import_name.clone();
        if skill_dirs.contains(&dir_name) {
            continue;
        }
        let mut files = Vec::new();
        collect_package_files(&skill.package_path, &mut files)?;
        for file in files {
            let relative = file
                .strip_prefix(&skill.package_path)?
                .to_string_lossy()
                .to_string();
            sources.push((format!("skills/{dir_name}/{relative}"), file));
        }
        skill_dirs.push(dir_name);
    }
    sources.sort();
    let mut hasher = sha2::Sha256::new();
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
    let digest = format!("{:x}", hasher.finalize());
    Ok(UploadBundle {
        files,
        skill_dirs,
        key: format!("v1-{}", &digest[..16]),
    })
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

fn gzip_tar(files: &[UploadFile]) -> anyhow::Result<Vec<u8>> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    for file in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(file.bytes.len() as u64);
        header.set_mode(file.mode);
        header.set_cksum();
        archive.append_data(&mut header, &file.path, file.bytes.as_slice())?;
    }
    let mut encoder = archive.into_inner()?;
    encoder.flush()?;
    Ok(encoder.finish()?)
}

/// Whether this machine's `uv` binary runs on the probed remote.
fn local_platform_matches(system: &str, machine: &str) -> bool {
    let os = match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        other => other,
    };
    let arch_matches = match std::env::consts::ARCH {
        "x86_64" => matches!(machine, "x86_64" | "amd64" | "AMD64"),
        "aarch64" => matches!(machine, "aarch64" | "arm64"),
        other => other == machine,
    };
    os == system && arch_matches
}

/// Run one remote command to completion, feeding `stdin`; the error names
/// the host and carries the stderr tail.
async fn run_remote(
    destination: &SshDestination,
    remote_command: &str,
    stdin: &[u8],
    what: &str,
) -> anyhow::Result<String> {
    let mut child = destination
        .command(remote_command)
        .spawn()
        .with_context(|| format!("failed to start ssh for {}", destination.label()))?;
    let mut pipe = child.stdin.take().context("ssh stdin is missing")?;
    let input = stdin.to_vec();
    let writer = tokio::spawn(async move {
        let written = pipe.write_all(&input).await;
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
        destination.label(),
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

/// Probe the remote, then upload and install the runtime unless this
/// version is already installed there.
pub(crate) async fn ensure_remote_kernel_python(
    destination: &SshDestination,
    python_skills: &[KernelPythonSkill],
    on_progress: Option<&KernelBootstrapProgressHandler>,
) -> anyhow::Result<RemoteKernelInstall> {
    let report = |message: &str| {
        if let Some(progress) = on_progress {
            progress(message);
        }
    };
    let skills = python_skills.to_vec();
    let bundle = tokio::task::spawn_blocking(move || collect_upload_bundle(&skills))
        .await
        .map_err(|join| anyhow!("remote upload collection failed: {join}"))??;
    let root = std::env::var("PRIME_AGENT_REMOTE_INSTALL_ROOT")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_REMOTE_INSTALL_ROOT.to_string());
    let label = destination.label();
    report(&format!("Probing Python on {label}..."));
    let probe_command = format!(
        "python3 -c {} {} {}",
        sh_quote(PROBE_SCRIPT),
        sh_quote(&root),
        sh_quote(&bundle.key)
    );
    let stdout = run_remote(destination, &probe_command, &[], "Python probe")
        .await
        .map_err(|error| {
            anyhow!("{error:#}\nThe remote host needs python3 (3.11 or newer) on its PATH.")
        })?;
    let probe: ProbeReport = stdout
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str(line.trim()).ok())
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
    };
    if probe.installed {
        return Ok(install);
    }
    let mut files = bundle.files;
    if probe.uv.is_none() && !probe.venv {
        // No remote uv and no working `python -m venv`: ship this machine's
        // uv when it can run there.
        let local_uv = super::ensure_uv()
            .ok()
            .filter(|_| local_platform_matches(&probe.system, &probe.machine));
        let Some(local_uv) = local_uv else {
            bail!(
                "Remote host {label} has neither uv nor a working `python3 -m venv` (ensurepip is missing), and this machine's uv cannot run there ({} {}). Install uv or the python3-venv package on the remote.",
                probe.system,
                probe.machine
            );
        };
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
    let archive = tokio::task::spawn_blocking(move || gzip_tar(&files))
        .await
        .map_err(|join| anyhow!("remote upload archive failed: {join}"))??;
    // The snapshot serializer plus the extras the system prompt promises,
    // exactly like the local kernel venv.
    let mut extra_packages = vec![STATE_SNAPSHOT_REQUIREMENT];
    extra_packages.extend(default_rlm_extra_uv_args());
    let install_command = format!(
        "python3 -c {} {} {} {} {}",
        sh_quote(INSTALL_SCRIPT),
        sh_quote(&root),
        sh_quote(&bundle.key),
        sh_quote(&serde_json::to_string(&bundle.skill_dirs)?),
        sh_quote(&serde_json::to_string(&extra_packages)?),
    );
    run_remote(destination, &install_command, &archive, "Runtime install").await?;
    Ok(install)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destinations_parse_ports_and_reject_option_injection() {
        assert_eq!(
            SshDestination::parse("user@box").unwrap(),
            SshDestination {
                destination: "user@box".to_string(),
                port: None
            }
        );
        assert_eq!(
            SshDestination::parse("user@box:2222").unwrap(),
            SshDestination {
                destination: "user@box".to_string(),
                port: Some(2222)
            }
        );
        assert_eq!(
            SshDestination::parse("ssh://box:22").unwrap(),
            SshDestination {
                destination: "ssh://box:22".to_string(),
                port: None
            }
        );
        for bad in ["", "-oProxyCommand=x", "a b", "box:notaport", "a:b:22"] {
            assert!(
                SshDestination::parse(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn shell_quoting_survives_single_quotes() {
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }
}
