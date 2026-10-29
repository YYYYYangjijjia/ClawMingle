#!/usr/bin/env python3
"""Prepare a private, pinned Node/Playwright runtime. Never installs OS packages.

Only --inherit-env names explicitly selected by the operator reach network clients.
stdout contains one runtime JSON object; diagnostics never contain child output.
"""
import argparse
import base64
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import posixpath
import selectors
import shutil
import ssl
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

NODE = "24.20.0"
PLAYWRIGHT = "1.63.0"
CHROMIUM = "153.0.8010.12"
REVISION = "1243"
GIB = 1024 ** 3
LIMIT = GIB
OUTPUT_LIMIT = 65536
NODE_HASHES = {
    "x64": "2f2c0da162318f0de47665410c7c8c2ed3d36c8f3105de4bbc61176c70a7cbf2",
    "arm64": "5f4ddab610c1ab2016b3c227cebdbf6d9495161487e4739c7b90090595f465f7",
}
INTEGRITIES = {
    "playwright": "sha512-+7ziBLidS4NaNCdt57SUDT+wYmmd5fmiQejUic/kb+YsYSCPyOOE9sebzMjNmQrsnNpDJqd4WHvV/8lfKfUDUg==",
    "playwright-core": "sha512-rYCsBF/M5HjUch52bbtVONEFjv6Xu8sm8h72dNlR5bzIE1fvC/bxgspzkjSfU+MweEMmPM8KJebG6nnyxo5mCg==",
}
NETWORK_ENV = {
    "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY",
    "http_proxy", "https_proxy", "all_proxy", "no_proxy",
    "SSL_CERT_FILE", "SSL_CERT_DIR", "NODE_EXTRA_CA_CERTS",
}
LIBRARIES = (
    "libnss3.so", "libnspr4.so", "libatk-1.0.so.0", "libatk-bridge-2.0.so.0",
    "libcups.so.2", "libdrm.so.2", "libdbus-1.so.3", "libX11.so.6",
    "libXcomposite.so.1", "libXdamage.so.1", "libXext.so.6", "libXfixes.so.3",
    "libXrandr.so.2", "libgbm.so.1", "libxcb.so.1", "libxkbcommon.so.0",
    "libasound.so.2", "libglib-2.0.so.0", "libgobject-2.0.so.0",
    "libpango-1.0.so.0", "libcairo.so.2", "libfontconfig.so.1", "libfreetype.so.6",
)


class Error(Exception):
    pass


def require(condition, message):
    if not condition:
        raise Error(message)


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def trusted(path):
    """Accept resolved paths only through root/current-user non-writable ancestors."""
    require(path.is_absolute() and ".." not in path.parts, "paths must be absolute and normalized")
    resolved = path.resolve(strict=True)
    for parent in [resolved, *resolved.parents]:
        info = parent.lstat()
        require(info.st_uid in (0, os.getuid()) and not info.st_mode & 0o022,
                "runtime paths need trusted owners and no group/other write permission")
    return resolved


def private_dir(path):
    if not path.exists() and not path.is_symlink():
        trusted(path.parent)
        path.mkdir(mode=0o700)
    info = path.lstat()
    require(stat.S_ISDIR(info.st_mode) and not path.is_symlink()
            and info.st_uid == os.getuid() and not info.st_mode & 0o077,
            "runtime root/directories must be real current-user directories with mode 0700")
    trusted(path)
    return path


def platform_arch():
    require(sys.version_info >= (3, 10), "Python 3.10 or newer is required")
    require(sys.platform == "linux", "native runtime bootstrap requires Linux")
    arch = {"x86_64": "x64", "aarch64": "arm64"}.get(platform.machine())
    require(arch is not None, "supported architectures are Linux x86_64 and aarch64")
    return arch


def minimal_env(stage, inherited):
    require(set(inherited) <= NETWORK_ENV, "unsupported --inherit-env name; only proxy/CA variables are allowed")
    env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8"}
    env.update({name: os.environ[name] for name in inherited if name in os.environ})
    if stage is not None:
        env.update({
            "PATH": str(stage / "node/bin") + ":/usr/bin:/bin",
            "TMPDIR": str(stage / "tmp"),
            "HOME": str(stage / "home"),
            "XDG_CONFIG_HOME": str(stage / "home/.config"),
            "XDG_CACHE_HOME": str(stage / "home/.cache"),
            "npm_config_cache": str(stage / "npm-cache"),
            "npm_config_update_notifier": "false",
            "PLAYWRIGHT_BROWSERS_PATH": str(stage / "browsers"),
            "PLAYWRIGHT_SKIP_BROWSER_GC": "1",
            "PLAYWRIGHT_DOWNLOAD_CONNECTION_TIMEOUT": "30000",
        })
    return env


def tree_bytes(root):
    seen, size = set(), 0
    for directory, dirs, files in os.walk(root, followlinks=False):
        for name in dirs + files:
            info = (Path(directory) / name).lstat()
            key = (info.st_dev, info.st_ino)
            if key not in seen:
                size += info.st_size
                seen.add(key)
    return size


class Budget:
    def __init__(self, root, stage=None):
        self.root, self.stage = root, stage

    def check(self, before=False):
        total, _, free = shutil.disk_usage(self.root)
        reserve = max(15 * GIB, (total + 4) // 5)
        require(free >= reserve + (LIMIT if before else 0),
                "insufficient disk: preserve max(15 GiB, 20% filesystem capacity) plus installation budget")
        require(tree_bytes(self.root) < LIMIT, "runtime tools directory reached the 1 GiB limit; existing directories preserved")


def run(args, cwd, env, budget, label, timeout=120):
    """Drain bounded output and enforce deadline/disk budget.

    Inherit the Rust wrapper's process group so its cancellation reaches every
    npm/Playwright descendant. The wrapper also reaps that group on failure.
    """
    budget.check()
    output = bytearray()
    started, next_check = time.monotonic(), 0.0
    process = subprocess.Popen([str(a) for a in args], cwd=cwd, env=env,
                               stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT)
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            while selector.get_map() or process.poll() is None:
                now = time.monotonic()
                require(now - started < timeout, label + " timed out")
                if now >= next_check:
                    budget.check()
                    next_check = now + 0.5
                for key, _ in selector.select(timeout=0.2):
                    chunk = os.read(key.fileobj.fileno(), 8192)
                    if not chunk:
                        selector.unregister(key.fileobj)
                    else:
                        output.extend(chunk)
                        require(len(output) <= OUTPUT_LIMIT, label + " exceeded diagnostic output limit")
            remaining = max(0.1, timeout - (time.monotonic() - started))
            try:
                code = process.wait(timeout=remaining)
            except subprocess.TimeoutExpired:
                raise Error(label + " timed out") from None
        hint = "check prerequisite libraries, public registry access, and selected proxy/CA settings"
        if label == "Bubblewrap namespace prerequisite check":
            hint = "Bubblewrap must be allowed to create user, PID, and network namespaces; configure this separately"
        elif label == "library check":
            hint = "Node 24 requires glibc >= 2.28 and libstdc++ with GLIBCXX_3.4.25; Chromium also needs distribution browser libraries"
        require(code == 0, label + " failed (exit " + str(code) + "); " + hint)
        budget.check()
        return output.decode("utf-8", errors="replace")
    finally:
        with contextlib.suppress(ProcessLookupError):
            process.kill()
        process.wait()
        process.stdout.close()


def opener_for(env):
    proxies = {}
    for scheme in ("http", "https", "all", "no"):
        value = env.get(scheme + "_proxy") or env.get(scheme.upper() + "_PROXY")
        if value:
            proxies[scheme] = value
    # Do not let unselected SSL_CERT_* variables silently change trust roots.
    defaults = ssl.get_default_verify_paths()
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    cafile = env.get("SSL_CERT_FILE") or defaults.openssl_cafile
    capath = env.get("SSL_CERT_DIR") or defaults.openssl_capath
    cafile = cafile if cafile and Path(cafile).is_file() else None
    capath = capath if capath and Path(capath).is_dir() else None
    require(cafile or capath or env.get("NODE_EXTRA_CA_CERTS"),
            "no system CA certificates; install ca-certificates or explicitly select a trusted CA file")
    if cafile or capath:
        context.load_verify_locations(cafile=cafile, capath=capath)
    if env.get("NODE_EXTRA_CA_CERTS"):
        context.load_verify_locations(cafile=env["NODE_EXTRA_CA_CERTS"])
    return urllib.request.build_opener(urllib.request.ProxyHandler(proxies),
                                      urllib.request.HTTPSHandler(context=context))


def download(url, destination, maximum, env, budget):
    started, received = time.monotonic(), 0
    try:
        with opener_for(env).open(url, timeout=20) as response, destination.open("xb") as output:
            require(urllib.parse.urlparse(response.url).scheme == "https", "download redirected away from HTTPS")
            while True:
                require(time.monotonic() - started < 300, "Node download timed out")
                chunk = response.read(1024 * 1024)
                if not chunk:
                    break
                received += len(chunk)
                require(received <= maximum, "Node download exceeded size limit")
                output.write(chunk)
                budget.check()
    except (urllib.error.URLError, TimeoutError, ssl.SSLError):
        raise Error("Node download failed; check nodejs.org access and selected proxy/CA settings") from None


def extract_node(archive, destination, arch, budget):
    prefix = "node-v" + NODE + "-linux-" + arch
    count, expanded, links = 0, 0, []
    with tarfile.open(archive, "r:xz") as source:
        for member in source:
            count += 1
            require(count <= 30000, "Node archive has too many entries")
            path = PurePosixPath(member.name)
            require(path.parts and not path.is_absolute() and ".." not in path.parts and path.parts[0] == prefix,
                    "unsafe Node archive path")
            relative = PurePosixPath(*path.parts[1:])
            selected = str(relative) == "bin/node" or str(relative).startswith("lib/node_modules/npm/")
            if not selected:
                continue
            if ".bin" in relative.parts:
                continue  # npm-cli is invoked directly; bundled bin shims are unused.
            if member.issym():
                link = PurePosixPath(member.linkname)
                normalized = PurePosixPath(posixpath.normpath(str(relative.parent / link)))
                require(not link.is_absolute() and str(normalized).startswith("lib/node_modules/npm/")
                        and ".." not in normalized.parts, "unsafe Node npm symlink")
                links.append((destination / str(relative), destination / str(normalized)))
                continue
            require(member.isfile() or member.isdir(), "unsupported link/device in Node npm archive")
            target = destination / str(relative)
            target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            if member.isdir():
                target.mkdir(exist_ok=True, mode=0o700)
                continue
            expanded += member.size
            require(expanded <= 256 * 1024 * 1024, "Node extraction exceeded size limit")
            with source.extractfile(member) as stream, target.open("xb") as output:
                shutil.copyfileobj(stream, output, length=1024 * 1024)
            target.chmod(0o700 if member.mode & 0o111 else 0o600)
            budget.check()
    # Materialize internal file aliases as copies. Never leave tar symlinks that
    # could redirect later extraction or escape a read-only runtime mount.
    while links:
        pending = []
        for target, original in links:
            if not original.is_file():
                pending.append((target, original))
                continue
            expanded += original.stat().st_size
            require(expanded <= 256 * 1024 * 1024, "Node extraction exceeded size limit")
            target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            with original.open("rb") as source, target.open("xb") as output:
                shutil.copyfileobj(source, output, length=1024 * 1024)
            target.chmod(stat.S_IMODE(original.stat().st_mode))
            budget.check()
        require(len(pending) < len(links), "Node npm symlink target is missing, cyclic, or not a regular file")
        links = pending
    require((destination / "bin/node").is_file()
            and (destination / "lib/node_modules/npm/bin/npm-cli.js").is_file(),
            "Node archive is missing bundled Node/npm")


def validate_lock(lock):
    require(isinstance(lock, dict), "invalid npm lock metadata")
    require(lock.get("lockfileVersion") == 3, "npm must produce package-lock version 3")
    packages = lock.get("packages", {})
    require(isinstance(packages, dict), "invalid npm lock packages")
    expected = {"", *["node_modules/" + name for name in INTEGRITIES]}
    require(set(packages) == expected, "unexpected npm dependency in runtime lock")
    for name, integrity in INTEGRITIES.items():
        entry = packages["node_modules/" + name]
        require(isinstance(entry, dict), "invalid npm dependency metadata")
        resolved = "https://registry.npmjs.org/" + name + "/-/" + name + "-" + PLAYWRIGHT + ".tgz"
        require(entry.get("version") == PLAYWRIGHT and entry.get("resolved") == resolved
                and entry.get("integrity") == integrity and not entry.get("link"),
                "npm lock dependency version, registry URL, or SRI differs from approved pin")
        require(len(base64.b64decode(integrity[7:], validate=True)) == 64, "invalid npm SRI")


def load_json(path):
    require(path.stat().st_size <= 4 * 1024 * 1024, "runtime metadata exceeds limit")
    try:
        value = json.loads(path.read_text())
        require(isinstance(value, dict), "runtime JSON metadata must be an object")
        return value
    except (ValueError, UnicodeError):
        raise Error("invalid runtime JSON metadata") from None


def inventory(root):
    result, hashed = {}, {}
    for directory, dirs, files in os.walk(root, followlinks=False):
        for name in sorted(dirs + files):
            path = Path(directory) / name
            relative = path.relative_to(root).as_posix()
            if relative == "manifest.json":
                continue
            info = path.lstat()
            require(not info.st_mode & 0o022 and info.st_uid == os.getuid(), "untrusted runtime file permissions or owner")
            if stat.S_ISDIR(info.st_mode):
                result[relative] = {"kind": "directory", "mode": stat.S_IMODE(info.st_mode)}
            elif stat.S_ISREG(info.st_mode):
                key = (info.st_dev, info.st_ino)
                if key not in hashed:
                    hashed[key] = digest(path)
                result[relative] = {"kind": "file", "mode": stat.S_IMODE(info.st_mode),
                                    "size": info.st_size, "sha256": hashed[key]}
            else:
                raise Error("runtime contains a symlink or special file")
    return result


def shell_paths(stage, arch):
    suffix = "linux64" if arch == "x64" else "linux-arm64"
    directory = stage / "browsers" / ("chromium_headless_shell-" + REVISION) / ("chrome-headless-shell-" + suffix)
    return directory / "chrome-headless-shell", directory / "chrome"


def check_elf(path):
    require(path.is_file() and not path.is_symlink(), "required native executable is missing or is a symlink")
    with path.open("rb") as stream:
        require(stream.read(4) == b"\x7fELF", "native executable must be ELF")
    require(os.access(path, os.X_OK), "native executable lacks execute permission")


def check_ldd(path, env, budget):
    output = run(["/usr/bin/ldd", path], path.parent, env, budget, "library check", timeout=15)
    missing = []
    for line in output.splitlines():
        if "not found" in line:
            name = line.strip().split()[0]
            if all(c.isalnum() or c in "._+-" for c in name):
                missing.append(name)
    require(not missing, "missing system libraries: " + ", ".join(sorted(set(missing)))
            + "; install the matching distribution packages separately")


def prerequisites(bubblewrap, worker, env, budget):
    bubblewrap, worker = trusted(bubblewrap), trusted(worker)
    check_elf(bubblewrap)
    require(worker.is_file() and worker.stat().st_size <= 65536, "--worker must be the existing bundled browser worker (at most 64 KiB)")
    require(Path("/usr/bin/ldd").is_file(), "missing prerequisite /usr/bin/ldd (libc-bin)")
    # The library check runs in a child with an explicit environment, never loading
    # arbitrary caller LD_PRELOAD or Python customization into the check process.
    probe = "import ctypes,sys\nmissing=[]\nfor name in sys.argv[1:]:\n try: ctypes.CDLL(name)\n except OSError: missing.append(name)\nprint(', '.join(missing))"
    missing = run([sys.executable, "-I", "-c", probe, *LIBRARIES], worker.parent, env, budget,
                  "system library prerequisite check", timeout=20).strip()
    require(not missing, "missing browser libraries: " + missing
            + "; install matching distribution packages separately (Ubuntu 24.04 uses t64 variants)")
    command = [bubblewrap, "--unshare-all", "--die-with-parent", "--new-session", "--ro-bind", "/usr", "/usr"]
    for name in ("bin", "lib", "lib64"):
        if Path("/" + name).exists():
            command.extend(["--ro-bind", "/" + name, "/" + name])
    command.extend(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--", "/usr/bin/true"])
    run(command, worker.parent, env, budget, "Bubblewrap namespace prerequisite check", timeout=15)
    return bubblewrap, worker


def validate_runtime(release, arch, env, budget):
    node = release / "node/bin/node"
    shell, chrome = shell_paths(release, arch)
    for binary in (node, shell, chrome):
        check_elf(binary)
    require(os.path.samefile(shell, chrome), "Chromium chrome alias must be a same-directory hard link")
    for binary in [node, chrome, *sorted(chrome.parent.glob("*.so*"))]:
        check_ldd(binary, env, budget)
    require(run([node, "--version"], release, env, budget, "Node version check").strip() == "v" + NODE,
            "installed Node version does not match pin")
    require(CHROMIUM in run([chrome, "--version"], release, env, budget, "Chromium version check"),
            "installed Chromium version does not match pin")
    for name in INTEGRITIES:
        package = release / "npm/node_modules" / name
        require(load_json(package / "package.json").get("version") == PLAYWRIGHT,
                "installed Playwright version does not match pin")
    cli = release / "npm/node_modules/playwright-core/cli.js"
    run([node, cli, "mcp", "--help"], release, env, budget, "bundled official MCP availability check")
    validate_lock(load_json(release / "npm/package-lock.json"))


def install(stage, arch, env, budget):
    for name in ("tmp", "node", "npm", "npm-cache", "browsers", "home", "home/.config", "home/.cache"):
        (stage / name).mkdir(mode=0o700)
    filename = "node-v" + NODE + "-linux-" + arch + ".tar.xz"
    base = "https://nodejs.org/dist/v" + NODE + "/"
    sums, archive = stage / "SHASUMS256.txt", stage / filename
    download(base + "SHASUMS256.txt", sums, 65536, env, budget)
    entries = [line.split() for line in sums.read_text().splitlines()]
    require([parts[0] for parts in entries if len(parts) == 2 and parts[1] == filename] == [NODE_HASHES[arch]],
            "Node official checksum differs from approved pin")
    download(base + filename, archive, 64 * 1024 * 1024, env, budget)
    require(digest(archive) == NODE_HASHES[arch], "Node archive checksum mismatch")
    extract_node(archive, stage / "node", arch, budget)
    archive.unlink()
    sums.unlink()
    node = stage / "node/bin/node"
    check_ldd(node, env, budget)
    npm = stage / "node/lib/node_modules/npm/bin/npm-cli.js"
    manifest = {"name": "clawmingle-native-runtime", "version": "1.0.0", "private": True,
                "dependencies": {name: PLAYWRIGHT for name in INTEGRITIES}}
    (stage / "npm/package.json").write_text(json.dumps(manifest) + "\n")
    # npm rejects loading the same filename as both user and global config.
    # Distinct empty private files also prevent inherited operator npm settings.
    user_config, global_config = stage / "npm-user.conf", stage / "npm-global.conf"
    user_config.touch(mode=0o600, exist_ok=False)
    global_config.touch(mode=0o600, exist_ok=False)
    flags = ["--ignore-scripts", "--no-audit", "--no-fund", "--bin-links=false",
             "--userconfig=" + str(user_config), "--globalconfig=" + str(global_config),
             "--registry=https://registry.npmjs.org/"]
    run([node, npm, "install", "--package-lock-only", *flags], stage / "npm", env, budget,
        "npm lock generation", timeout=180)
    validate_lock(load_json(stage / "npm/package-lock.json"))
    run([node, npm, "ci", "--omit=dev", *flags], stage / "npm", env, budget,
        "npm integrity-checked install", timeout=300)
    cli = stage / "npm/node_modules/playwright-core/cli.js"
    run([node, cli, "install", "--only-shell", "chromium", "--no-remove", "--no-progress"],
        stage / "npm", env, budget, "official Chromium headless-shell installation", timeout=600)
    shell, chrome = shell_paths(stage, arch)
    check_elf(shell)
    os.link(shell, chrome)
    validate_runtime(stage, arch, env, budget)
    # These are exclusively owned transient paths created above. No shared cache.
    for name in ("tmp", "npm-cache"):
        shutil.rmtree(stage / name)
    # Playwright's GC references contain the staging pathname. This private cache
    # is never shared or garbage-collected; the reference is unnecessary after move.
    links = stage / "browsers/.links"
    if links.exists():
        require(links.is_dir() and not links.is_symlink(), "unexpected browser cache metadata")
        shutil.rmtree(links)


@contextlib.contextmanager
def locked(path):
    descriptor = os.open(path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        info = os.fstat(descriptor)
        require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
                and not info.st_mode & 0o077 and info.st_nlink == 1, "unsafe runtime lock file")
        deadline = time.monotonic() + 60
        while True:
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                require(time.monotonic() < deadline, "another native runtime bootstrap holds the lock")
                time.sleep(0.1)
        yield
    finally:
        os.close(descriptor)


def publish_worker(tools, source, budget):
    """Publish a small immutable worker independently of package release pins."""
    data = source.read_bytes()
    require(len(data) <= 65536, "browser worker exceeds 64 KiB")
    checksum = hashlib.sha256(data).hexdigest()
    target = tools / ("worker-" + checksum + ".mjs")
    if target.exists() or target.is_symlink():
        info = target.lstat()
        require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
                and stat.S_IMODE(info.st_mode) == 0o600 and digest(target) == checksum,
                "existing content-addressed worker failed integrity check; preserved")
        return target
    budget.check()
    require(tree_bytes(tools) + len(data) + 4096 < LIMIT, "runtime tools directory lacks worker budget")
    descriptor, filename = tempfile.mkstemp(prefix=".worker-", dir=tools)
    temporary = Path(filename)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        # Atomic no-replace publication; an unexpected existing target survives.
        os.link(temporary, target)
        budget.check()
    finally:
        temporary.unlink()
    return target


def prepare(root, bubblewrap, worker, inherited=()):
    arch = platform_arch()
    require(root.is_absolute() and ".." not in root.parts, "--root must be an absolute normalized private path")
    private_dir(root)
    tools = root
    budget = Budget(tools)
    env = minimal_env(None, inherited)
    bubblewrap, worker = prerequisites(bubblewrap, worker, env, budget)
    release = tools / ("node-" + NODE + "-playwright-" + PLAYWRIGHT + "-linux-" + arch + "-v1")
    with locked(tools / ".bootstrap.lock"):
        if release.exists() or release.is_symlink():
            require(release.is_dir() and not release.is_symlink(), "existing runtime release is not a real directory; preserved")
            path = release / "manifest.json"
            require(path.is_file() and not path.is_symlink(), "existing incomplete runtime release preserved; choose another private root or investigate it")
            recorded = load_json(path)
            require(recorded.get("version") == 1 and recorded.get("node") == NODE
                    and recorded.get("playwright") == PLAYWRIGHT and recorded.get("arch") == arch,
                    "existing runtime manifest does not match approved pins; preserved")
            require(inventory(release) == recorded.get("files"), "existing runtime integrity check failed; preserved")
            require(tree_bytes(release) < LIMIT, "existing runtime exceeds the 1 GiB limit")
            validate_runtime(release, arch, minimal_env(release, ()), budget)
        else:
            budget.check(before=True)
            stage = Path(tempfile.mkdtemp(prefix="." + release.name + ".", dir=tools))
            identity = stage.stat().st_ino
            try:
                stage_budget = Budget(tools, stage)
                install(stage, arch, minimal_env(stage, inherited), stage_budget)
                recorded = {"version": 1, "node": NODE, "playwright": PLAYWRIGHT,
                            "arch": arch, "chromium": CHROMIUM, "files": inventory(stage)}
                metadata = stage / "manifest.json"
                with metadata.open("x") as output:
                    json.dump(recorded, output, sort_keys=True)
                    output.write("\n")
                    output.flush()
                    os.fsync(output.fileno())
                metadata.chmod(0o600)
                stage_budget.check()
                require(not release.exists(), "runtime release appeared concurrently; preserved")
                stage.rename(release)
                directory = os.open(tools, os.O_RDONLY | os.O_DIRECTORY)
                try:
                    os.fsync(directory)
                finally:
                    os.close(directory)
            finally:
                if stage.exists() and not stage.is_symlink() and stage.stat().st_ino == identity:
                    shutil.rmtree(stage)
        persistent_worker = publish_worker(tools, worker, budget)
    return {"bubblewrap": str(bubblewrap), "node": str(release / "node/bin/node"), "node_modules": str(release / "npm/node_modules"),
            "chromium": str(shell_paths(release, arch)[1]), "worker": str(persistent_worker)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--bubblewrap", type=Path, required=True)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--inherit-env", action="append", default=[], choices=sorted(NETWORK_ENV))
    args = parser.parse_args()
    os.umask(0o077)
    try:
        print(json.dumps(prepare(args.root, args.bubblewrap, args.worker, args.inherit_env), sort_keys=True))
        return 0
    except Error as error:
        print("native runtime: " + str(error), file=sys.stderr)
    except (OSError, ValueError, tarfile.TarError):
        print("native runtime: local filesystem, archive, or configuration check failed; existing releases preserved", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
