# Remote kernel probe (see remote.rs): reports the remote interpreter, its
# venv support, and the install state in one round trip. It runs under the
# remote's system python3 of any version, so it stays compatible with old
# interpreters (no f-strings, no walrus) to report an unsupported version
# instead of failing on syntax.
# It also touches the .last-used of every install and skill dir it reports
# present, so garbage collection cannot remove one between this probe and
# the kernel that holds it.
# argv: install root (may start with ~), base key, JSON {skill id: is Python}.
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


def touch(directory):
    try:
        last_used = os.path.join(directory, ".last-used")
        with open(last_used, "a"):
            pass
        os.utime(last_used)
    except OSError:
        pass


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
if installed:
    touch(target)
missing_stores = []
missing_sites = []
for skill_id in sorted(skills):
    store = os.path.join(root, "skills", skill_id)
    if read(os.path.join(store, ".complete")) == skill_id:
        touch(store)
    else:
        missing_stores.append(skill_id)
    if skills[skill_id]:
        site = os.path.join(target, "skill-sites", skill_id)
        if installed and read(os.path.join(site, ".installed")) == skill_id:
            touch(site)
        else:
            missing_sites.append(skill_id)
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
            "missing_stores": missing_stores,
            "missing_sites": missing_sites,
        }
    )
)
