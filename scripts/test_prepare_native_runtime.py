#!/usr/bin/env python3
"""Offline fixtures only: no real package downloads, Node, browser, or services."""
import io
import json
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest import mock

import prepare_native_runtime as runtime


def write(path, content=b"fixture", mode=0o600):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    path.write_bytes(content)
    path.chmod(mode)


def lock_fixture():
    return {"lockfileVersion": 3, "packages": {"": {}, **{
        "node_modules/" + name: {
            "version": runtime.PLAYWRIGHT,
            "resolved": "https://registry.npmjs.org/" + name + "/-/" + name + "-" + runtime.PLAYWRIGHT + ".tgz",
            "integrity": integrity,
        } for name, integrity in runtime.INTEGRITIES.items()}}}


class RuntimeTests(unittest.TestCase):
    def setUp(self):
        previous_umask = os.umask(0o077)
        self.addCleanup(os.umask, previous_umask)
        self.temp = tempfile.TemporaryDirectory(prefix="cm-runtime-tests-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "tools"
        self.root.mkdir(mode=0o700)
        self.worker = Path(self.temp.name) / "worker.mjs"
        write(self.worker, b"fixture worker")

    def fixture_install(self, stage, arch, env, budget):
        write(stage / "node/bin/node", b"\x7fELFnode", 0o700)
        write(stage / "npm/package-lock.json", json.dumps(lock_fixture()).encode())
        for name in runtime.INTEGRITIES:
            write(stage / "npm/node_modules" / name / "package.json",
                  json.dumps({"version": runtime.PLAYWRIGHT}).encode())
        write(stage / "npm/node_modules/playwright-core/cli.js")
        shell, chrome = runtime.shell_paths(stage, arch)
        write(shell, b"\x7fELFchrome", 0o700)
        os.link(shell, chrome)

    def prepare_patches(self):
        stack = self.enterContext(__import__("contextlib").ExitStack())
        stack.enter_context(mock.patch.object(runtime, "trusted", side_effect=lambda p: p.resolve()))
        stack.enter_context(mock.patch.object(runtime, "platform_arch", return_value="x64"))
        stack.enter_context(mock.patch.object(runtime.Budget, "check"))
        stack.enter_context(mock.patch.object(runtime, "prerequisites", side_effect=lambda b, w, e, d: (b, w)))
        stack.enter_context(mock.patch.object(runtime, "validate_runtime"))
        return stack

    def test_atomic_install_and_offline_reuse(self):
        self.prepare_patches()
        with mock.patch.object(runtime, "install", side_effect=self.fixture_install) as install:
            result = runtime.prepare(self.root, Path("/usr/bin/bwrap"), self.worker)
            self.assertEqual(set(result), {"bubblewrap", "node", "node_modules", "chromium", "worker"})
            self.assertEqual(install.call_count, 1)
        chrome = Path(result["chromium"])
        self.assertTrue(os.path.samefile(chrome, chrome.with_name("chrome-headless-shell")))
        with mock.patch.object(runtime, "install", side_effect=AssertionError("network reuse")), \
                mock.patch.object(runtime, "download", side_effect=AssertionError("network reuse")):
            self.assertEqual(runtime.prepare(self.root, Path("/usr/bin/bwrap"), self.worker), result)

    def test_tampered_release_is_preserved_and_not_repaired(self):
        self.prepare_patches()
        with mock.patch.object(runtime, "install", side_effect=self.fixture_install):
            result = runtime.prepare(self.root, Path("/usr/bin/bwrap"), self.worker)
        Path(result["node"]).write_bytes(b"changed")
        with mock.patch.object(runtime, "install") as install:
            with self.assertRaisesRegex(runtime.Error, "integrity check failed"):
                runtime.prepare(self.root, Path("/usr/bin/bwrap"), self.worker)
            install.assert_not_called()
        self.assertEqual(Path(result["node"]).read_bytes(), b"changed")

    def test_exact_tools_root_and_persistent_worker_across_helper_locations(self):
        self.prepare_patches()
        with mock.patch.object(runtime, "install", side_effect=self.fixture_install):
            result = runtime.prepare(self.root, Path("/usr/bin/bwrap"), self.worker)
        persistent = Path(result["worker"])
        self.assertEqual(persistent.parent, self.root)
        self.assertTrue(persistent.name.startswith("worker-"))
        self.assertFalse((self.root / "runtime/tools").exists())
        content = self.worker.read_bytes()
        self.worker.unlink()
        self.assertEqual(persistent.read_bytes(), content)
        replacement = Path(self.temp.name) / "another-helper/worker.mjs"
        write(replacement, content)
        with mock.patch.object(runtime, "install", side_effect=AssertionError("unexpected reinstall")):
            self.assertEqual(runtime.prepare(self.root, Path("/usr/bin/bwrap"), replacement), result)
            write(replacement, b"updated worker")
            updated = runtime.prepare(self.root, Path("/usr/bin/bwrap"), replacement)
        self.assertNotEqual(updated["worker"], result["worker"])
        self.assertEqual(updated["node"], result["node"])
        self.assertEqual(Path(updated["worker"]).read_bytes(), b"updated worker")
        self.assertEqual(persistent.read_bytes(), content)

    def test_internal_npm_file_symlinks_become_safe_copies(self):
        prefix = "node-v24.20.0-linux-x64/"
        archive = self.make_tar([
            (prefix + "bin/node", b"\x7fELFnode", tarfile.REGTYPE),
            (prefix + "lib/node_modules/npm/bin/npm-cli.js", b"npm", tarfile.REGTYPE),
            (prefix + "lib/node_modules/npm/alias.js", "bin/npm-cli.js", tarfile.SYMTYPE),
        ])
        dest = self.root / "out"
        runtime.extract_node(archive, dest, "x64", mock.Mock())
        alias = dest / "lib/node_modules/npm/alias.js"
        self.assertFalse(alias.is_symlink())
        self.assertEqual(alias.read_bytes(), b"npm")

    def test_missing_system_ca_is_a_concise_prerequisite_error(self):
        defaults = mock.Mock(openssl_cafile=None, openssl_capath=None)
        with mock.patch.object(runtime.ssl, "get_default_verify_paths", return_value=defaults):
            with self.assertRaisesRegex(runtime.Error, "no system CA certificates"):
                runtime.opener_for({})

    def test_unknown_incomplete_release_is_preserved(self):
        self.prepare_patches()
        release = self.root / ("node-24.20.0-playwright-1.63.0-linux-x64-v1")
        write(release / "operator-file")
        with self.assertRaisesRegex(runtime.Error, "incomplete runtime release preserved"):
            runtime.prepare(self.root, Path("/usr/bin/bwrap"), self.worker)
        self.assertTrue((release / "operator-file").exists())

    def test_only_own_staging_is_cleaned_on_failure(self):
        self.prepare_patches()
        tools = self.root
        unknown = tools / ".earlier-incomplete"
        write(unknown / "keep")
        def fail(stage, *args):
            write(stage / "partial")
            raise runtime.Error("fixture failure")
        with mock.patch.object(runtime, "install", side_effect=fail):
            with self.assertRaisesRegex(runtime.Error, "fixture failure"):
                runtime.prepare(self.root, Path("/usr/bin/bwrap"), self.worker)
        self.assertTrue((unknown / "keep").exists())
        self.assertEqual(sorted(p.name for p in tools.iterdir()), [".bootstrap.lock", ".earlier-incomplete"])

    def test_runtime_lock_rejects_symlink(self):
        target = self.root / "target"
        write(target)
        link = self.root / "lock"
        link.symlink_to(target)
        with self.assertRaises(OSError):
            with runtime.locked(link):
                self.fail("followed lock symlink")

    def test_lock_validates_registry_integrity_and_dependency_set(self):
        runtime.validate_lock(lock_fixture())
        for field, value in (("resolved", "https://example.com/package.tgz"),
                             ("integrity", "sha512-" + "A" * 88),
                             ("version", "1.63.0-alpha")):
            lock = lock_fixture()
            lock["packages"]["node_modules/playwright"][field] = value
            with self.assertRaisesRegex(runtime.Error, "approved pin"):
                runtime.validate_lock(lock)
        lock = lock_fixture()
        lock["packages"]["node_modules/extra"] = {}
        with self.assertRaisesRegex(runtime.Error, "unexpected npm dependency"):
            runtime.validate_lock(lock)

    def make_tar(self, entries):
        archive = self.root / "node.tar.xz"
        with tarfile.open(archive, "w:xz") as output:
            for name, content, kind in entries:
                member = tarfile.TarInfo(name)
                member.mode = 0o755
                member.type = kind
                if kind == tarfile.REGTYPE:
                    member.size = len(content)
                    output.addfile(member, io.BytesIO(content))
                else:
                    member.linkname = str(content)
                    output.addfile(member)
        return archive

    def test_extracts_only_node_and_bundled_npm(self):
        prefix = "node-v24.20.0-linux-x64/"
        archive = self.make_tar([
            (prefix + "bin/node", b"\x7fELFnode", tarfile.REGTYPE),
            (prefix + "lib/node_modules/npm/bin/npm-cli.js", b"npm", tarfile.REGTYPE),
            (prefix + "include/node/unused.h", b"omit", tarfile.REGTYPE),
        ])
        dest = self.root / "out"
        runtime.extract_node(archive, dest, "x64", mock.Mock())
        self.assertTrue((dest / "bin/node").exists())
        self.assertFalse((dest / "include").exists())
        self.assertEqual((dest / "bin/node").stat().st_mode & 0o777, 0o700)

    def test_archive_path_traversal_rejected(self):
        archive = self.make_tar([("node-v24.20.0-linux-x64/../../escape", b"bad", tarfile.REGTYPE)])
        with self.assertRaisesRegex(runtime.Error, "unsafe Node archive path"):
            runtime.extract_node(archive, self.root / "out", "x64", mock.Mock())
        self.assertFalse((self.root / "escape").exists())

    def test_archive_external_symlink_rejected(self):
        archive = self.make_tar([("node-v24.20.0-linux-x64/lib/node_modules/npm/link", "/etc/passwd", tarfile.SYMTYPE)])
        with self.assertRaisesRegex(runtime.Error, "unsafe Node npm symlink"):
            runtime.extract_node(archive, self.root / "out", "x64", mock.Mock())

    def test_environment_inherits_only_explicit_proxy_ca_selection(self):
        with mock.patch.dict(os.environ, {"HTTPS_PROXY": "https://proxy.example", "NODE_OPTIONS": "unsafe",
                                         "OPENAI_API_KEY": "EXAMPLE_API_KEY", "HOME": "/fixture/home"}):
            clean = runtime.minimal_env(self.root, ())
            selected = runtime.minimal_env(self.root, ("HTTPS_PROXY",))
        self.assertNotIn("HTTPS_PROXY", clean)
        self.assertEqual(selected["HTTPS_PROXY"], "https://proxy.example")
        self.assertEqual(selected["HOME"], str(self.root / "home"))
        for key in ("NODE_OPTIONS", "OPENAI_API_KEY"):
            self.assertNotIn(key, selected)
        with self.assertRaises(runtime.Error):
            runtime.minimal_env(None, ("NODE_OPTIONS",))

    def test_disk_reserve_and_stage_budget(self):
        with mock.patch.object(runtime.shutil, "disk_usage", return_value=(100 * runtime.GIB, 80 * runtime.GIB, 20 * runtime.GIB)):
            runtime.Budget(self.root).check()
            with self.assertRaisesRegex(runtime.Error, "insufficient disk"):
                runtime.Budget(self.root).check(before=True)
        with mock.patch.object(runtime.shutil, "disk_usage", return_value=(100 * runtime.GIB, 0, 100 * runtime.GIB)), \
                mock.patch.object(runtime, "tree_bytes", return_value=runtime.GIB):
            with self.assertRaisesRegex(runtime.Error, "1 GiB"):
                runtime.Budget(self.root, self.root).check()

    def test_inventory_rejects_external_links_and_counts_hardlink_once(self):
        write(self.root / "data", b"12345")
        os.link(self.root / "data", self.root / "alias")
        self.assertEqual(runtime.tree_bytes(self.root), 5)
        self.assertEqual(runtime.inventory(self.root)["data"]["sha256"], runtime.inventory(self.root)["alias"]["sha256"])
        (self.root / "escape").symlink_to("/etc/passwd")
        with self.assertRaises(runtime.Error):
            runtime.inventory(self.root)

    def test_missing_ldd_library_has_exact_hint(self):
        with mock.patch.object(runtime, "run", return_value="libnss3.so => not found\nlibc.so.6 => /lib/libc.so.6\n"):
            with self.assertRaisesRegex(runtime.Error, "missing system libraries: libnss3.so"):
                runtime.check_ldd(self.root / "chrome", {}, mock.Mock())

    def test_checksum_failure_stops_before_extraction(self):
        def fake_download(url, path, maximum, env, budget):
            if url.endswith("SHASUMS256.txt"):
                path.write_text(runtime.NODE_HASHES["x64"] + "  node-v24.20.0-linux-x64.tar.xz\n")
            else:
                path.write_bytes(b"wrong archive")
        with mock.patch.object(runtime, "download", side_effect=fake_download), \
                mock.patch.object(runtime, "extract_node") as extract:
            with self.assertRaisesRegex(runtime.Error, "checksum mismatch"):
                runtime.install(self.root, "x64", {}, mock.Mock())
            extract.assert_not_called()

    def test_subprocess_timeout_keeps_parent_process_group_and_kills_child(self):
        process = mock.Mock()
        selector = mock.MagicMock()
        selector.__enter__.return_value = selector
        selector.get_map.return_value = {1: True}
        with mock.patch.object(runtime.subprocess, "Popen", return_value=process) as popen, \
                mock.patch.object(runtime.selectors, "DefaultSelector", return_value=selector), \
                mock.patch.object(runtime.time, "monotonic", side_effect=[0, 10]):
            with self.assertRaisesRegex(runtime.Error, "timed out"):
                runtime.run(["fixture"], self.root, {}, mock.Mock(), "fixture stage", timeout=1)
        self.assertNotIn("start_new_session", popen.call_args.kwargs)
        self.assertNotIn("process_group", popen.call_args.kwargs)
        process.kill.assert_called_once()
        process.wait.assert_called_once()

    def test_install_uses_only_shell_and_creates_real_hardlink(self):
        def fake_download(url, path, maximum, env, budget):
            if url.endswith("SHASUMS256.txt"):
                path.write_text(runtime.NODE_HASHES["x64"] + "  node-v24.20.0-linux-x64.tar.xz\n")
            else:
                path.write_bytes(b"fixture archive")
        def fake_extract(archive, dest, arch, budget):
            write(dest / "bin/node", b"\x7fELFfixture", 0o700)
            write(dest / "lib/node_modules/npm/bin/npm-cli.js")
        def fake_run(args, cwd, env, budget, label, timeout=120):
            if "--package-lock-only" in args:
                write(cwd / "package-lock.json", json.dumps(lock_fixture()).encode())
            if "--only-shell" in args:
                shell, _ = runtime.shell_paths(self.root, "x64")
                write(shell, b"\x7fELFfixture", 0o700)
            return ""
        with mock.patch.object(runtime, "download", side_effect=fake_download), \
                mock.patch.object(runtime, "digest", return_value=runtime.NODE_HASHES["x64"]), \
                mock.patch.object(runtime, "extract_node", side_effect=fake_extract), \
                mock.patch.object(runtime, "run", side_effect=fake_run) as run, \
                mock.patch.object(runtime, "validate_runtime"):
            runtime.install(self.root, "x64", {}, mock.Mock())
        shell, chrome = runtime.shell_paths(self.root, "x64")
        self.assertTrue(os.path.samefile(shell, chrome))
        self.assertFalse(chrome.is_symlink())
        calls = [call.args[0] for call in run.call_args_list]
        self.assertEqual(sum("--only-shell" in args and "chromium" in args for args in calls), 1)
        self.assertTrue(all("--ignore-scripts" in args for args in calls if "ci" in args or "--package-lock-only" in args))
        for args in calls:
            if "ci" in args or "--package-lock-only" in args:
                user = next(str(arg).split("=", 1)[1] for arg in args if str(arg).startswith("--userconfig="))
                system = next(str(arg).split("=", 1)[1] for arg in args if str(arg).startswith("--globalconfig="))
                self.assertNotEqual(user, system)
                self.assertEqual(Path(user).read_bytes(), b"")
                self.assertEqual(Path(system).read_bytes(), b"")
        self.assertFalse((self.root / "npm-cache").exists())


if __name__ == "__main__":
    unittest.main()
