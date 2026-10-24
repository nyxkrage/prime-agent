# Remote kernel probe (see remote.rs): reports the remote interpreter, its
# venv support, and the install state in one round trip. It runs under the
# remote's system python3 of any version, so it stays compatible with old
# interpreters (no f-strings, no walrus) to report an unsupported version
# instead of failing on syntax.
# argv: install root (may start with ~), base key, JSON {skill dir: content hash}.
import json
import os
import platform
import shutil
import sys

root = os.path.expanduser(sys.argv[1])
key = sys.argv[2]
skills = json.loads(sys.argv[3])
target = os.path.join(root, key)
python = os.path.join(target, "venv", "bin", "python")


def read(path):
    try:
        with open(path) as handle:
            return handle.read().strip()
    except (OSError, UnicodeDecodeError):
        return None


def venv_works():
    # `python3 -m venv` needs a working ensurepip: Debian/Ubuntu ship an
    # ensurepip stub without the bundled pip wheel unless python3-venv is
    # installed, so finding the module is not enough.
    try:
        import ensurepip
        import venv  # noqa: F401

        return bool(ensurepip.version())
    except Exception:
        return False


installed = os.path.isfile(python) and read(os.path.join(target, ".installed")) == key
stale = sorted(
    name
    for name, digest in skills.items()
    if not installed or read(os.path.join(target, "skills", name + ".installed")) != digest
)
print(
    json.dumps(
        {
            "version": list(sys.version_info[:3]),
            "executable": sys.executable,
            "venv": venv_works(),
            "uv": shutil.which("uv"),
            "system": platform.system(),
            "machine": platform.machine(),
            "root": root,
            "dir": target,
            "installed": installed,
            "stale_skills": stale,
        }
    )
)
