#!/usr/bin/env python3
"""Exercise commit/push boundaries in disposable repositories, without network."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import check_sensitive_data as guard


SCRIPT = Path(__file__).with_name("check_sensitive_data.py").resolve()
HOOKS = SCRIPT.parent.parent / ".githooks"
TOKEN = "sk-" + "synthetic" + "x" * 32
SERVER = ".".join(("10", "23", "45", "67"))


def assignment(field, value):
    return (field + ' = "' + value + '"\n').encode()


class ContentTests(unittest.TestCase):
    def test_credential_literals_in_common_formats(self):
        for field in ("api_key", "JEV_API_KEY", "QQ_APP_SECRET", "app_secret", "clientSecret", "password", "refresh_token"):
            for separator in (" = ", ": "):
                raw = (field + separator + '"' + "sensitive-fixture" + '"').encode()
                self.assertIn((1, "credential-value"), guard.scan_content(raw))
        raw = ("export " + "QQ_API_KEY" + "=" + "sensitive-fixture").encode()
        self.assertIn((1, "credential-value"), guard.scan_content(raw))

    def test_placeholders_and_source_references_are_allowed(self):
        for value in ("YOUR_APP_SECRET", "your_app_secret", "${QQ_APP_SECRET}", "<API_KEY>"):
            self.assertEqual([], guard.scan_content(assignment("app_secret", value)))
        self.assertEqual([], guard.scan_content(b"pub app_secret: String,\napi_key: self.api_key"))

    def test_provider_tokens_are_found_without_field_names(self):
        self.assertIn((1, "provider-token"), guard.scan_content(TOKEN.encode()))

    def test_private_keys_and_usable_connections_are_rejected(self):
        values = (
            ("-----BEGIN " + "OPENSSH PRIVATE KEY-----", "private-key"),
            ("https://" + "person:synthetic-password@example.com", "connection-credentials"),
            ("vless" + "://" + "synthetic-connection", "vpn-connection"),
            ("Bearer " + "x" * 24, "auth-header"),
        )
        for value, rule in values:
            self.assertIn((1, rule), guard.scan_content(value.encode()))

    def test_real_network_addresses_are_rejected_including_private_ipv6(self):
        values = [SERVER, ":".join(("fd12", "", "1234")),
                  ":".join(("", "", "ffff", SERVER)), ":".join(("fe80", "", "1%eth0"))]
        for value in values:
            with self.subTest(address_type="synthetic IP"):
                self.assertIn((1, "server-ip"), guard.scan_content(("[" + value + "]:8080").encode()))

    def test_loopback_and_documentation_addresses_are_allowed(self):
        for value in ("127.0.0.1", "0.0.0.0", "192.0.2.1", "203.0.113.1", "::1", "2001:db8::1"):
            self.assertEqual([], guard.scan_content(value.encode()))

    def test_uuid_and_local_forbidden_value(self):
        value = "-".join(("a" * 8, "b" * 4, "c" * 4, "d" * 4, "e" * 12))
        self.assertIn((1, "uuid"), guard.scan_content(value.encode()))
        self.assertIn((2, "local-forbidden-value"), guard.scan_content(b"header\nprivate-host", ["private-host"]))

    def test_sensitive_paths_and_template_exceptions(self):
        for path in ("codexclaw.toml", "nested/auth.json", ".local/notes.md", "data/state.json",
                     "credentials.json", "config/local/settings.toml", "keys/account.pem",
                     "qq.env", "config/model.env", "clawmingle.toml", "nested/social.toml"):
            self.assertTrue(guard.private_path(path), path)
        for path in ("config/codexclaw.example.toml", "config/codex_models.toml", ".env.example",
                     ".agents/skills/example/SKILL.md", "src/config.rs", "Cargo.lock", "config/clawmingle.example.toml"):
            self.assertFalse(guard.private_path(path), path)


class GitBoundaryTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.env = os.environ.copy()
        # Do not inherit an alternate index/worktree or the operator's identity.
        for key in list(self.env):
            if key.startswith("GIT_"):
                del self.env[key]
        self.env.update({"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"})
        self.git("init", "--quiet")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")

    def tearDown(self):
        self.directory.cleanup()

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, env=self.env, stderr=subprocess.DEVNULL)

    def scan(self, *args, input_text=None):
        return subprocess.run([sys.executable, "-B", str(SCRIPT), *args], cwd=self.root, env=self.env,
                              input=input_text, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def commit(self, message="fixture"):
        self.git("commit", "--quiet", "-m", message)

    def test_scans_index_even_when_worktree_was_sanitized(self):
        path = self.root / "notes.md"
        path.write_text(TOKEN)
        self.git("add", "notes.md")
        path.write_text("safe worktree")
        result = self.scan("--staged")
        self.assertEqual(1, result.returncode)
        self.assertNotIn(TOKEN, result.stdout + result.stderr)
        self.assertEqual(0, self.scan("--tracked").returncode)

    def test_force_adding_ignored_private_file_is_rejected(self):
        (self.root / ".gitignore").write_text("codexclaw.toml\n")
        (self.root / "codexclaw.toml").write_text("placeholder only")
        self.git("add", "-f", "codexclaw.toml")
        self.assertEqual(1, self.scan("--staged").returncode)

    def test_push_catches_secret_removed_in_later_commit(self):
        path = self.root / "notes.md"
        path.write_text(TOKEN)
        self.git("add", ".")
        self.commit()
        path.unlink()
        self.git("add", "-u")
        self.commit()
        self.assertEqual(0, self.scan("--staged").returncode)
        head = self.git("rev-parse", "HEAD").decode().strip()
        line = "refs/heads/main " + head + " refs/heads/main " + "0" * 40 + "\n"
        result = self.scan("--pre-push", input_text=line)
        self.assertEqual(1, result.returncode)
        self.assertNotIn(TOKEN, result.stdout + result.stderr)

    def test_push_deletion_and_malformed_input(self):
        deletion = "(delete) " + "0" * 40 + " refs/heads/main " + "1" * 40 + "\n"
        self.assertEqual(0, self.scan("--pre-push", input_text=deletion).returncode)
        self.assertEqual(2, self.scan("--pre-push", input_text="invalid\n").returncode)

    def test_commit_and_annotated_tag_messages_are_scanned(self):
        (self.root / "notes.md").write_text("safe")
        self.git("add", ".")
        self.commit(TOKEN)
        self.assertEqual(1, self.scan("--history", "HEAD").returncode)
        self.git("commit", "--amend", "--quiet", "-m", "safe")
        self.git("tag", "-a", "fixture", "-m", TOKEN)
        self.assertEqual(1, self.scan("--history", "fixture").returncode)

    def test_odd_paths_and_symlinks_do_not_leak_values(self):
        name = "notes with space\nand-newline.md"
        (self.root / name).write_text(TOKEN)
        self.git("add", "--", name)
        result = self.scan("--staged")
        self.assertEqual(1, result.returncode)
        self.assertNotIn(TOKEN, result.stdout + result.stderr)
        self.git("rm", "--cached", "--", name)
        outside = self.root / ".untracked-sensitive"
        outside.write_text(TOKEN)
        (self.root / "link.md").symlink_to(outside)
        self.git("add", "link.md")
        self.assertEqual(0, self.scan("--tracked").returncode)

    def test_forbidden_values_and_sensitive_path_output_are_redacted(self):
        (self.root / ".local").mkdir()
        (self.root / ".local/sensitive-values.txt").write_text("private-host\n")
        name = "private-host-" + SERVER + ".md"
        (self.root / name).write_text("private-host\n" + SERVER)
        self.git("add", "--", name)
        result = self.scan("--staged")
        self.assertEqual(1, result.returncode)
        self.assertNotIn(SERVER, result.stdout + result.stderr)
        self.assertNotIn("private-host", result.stdout + result.stderr)

    def test_sensitive_filenames_are_rejected_even_with_safe_contents(self):
        for name in (SERVER + ".md", TOKEN + ".md"):
            (self.root / name).write_text("safe content")
            self.git("add", "--", name)
        result = self.scan("--staged")
        self.assertEqual(1, result.returncode)
        self.assertNotIn(SERVER, result.stdout + result.stderr)
        self.assertNotIn(TOKEN, result.stdout + result.stderr)

    def test_credential_assignment_in_filename_is_not_printed(self):
        value = "sensitive-filename-fixture"
        name = "api_key" + '="' + value + '".md'
        (self.root / name).write_text("safe content")
        self.git("add", "--", name)
        result = self.scan("--staged")
        self.assertEqual(1, result.returncode)
        self.assertNotIn(value, result.stdout + result.stderr)

    def test_hook_scripts_use_blocking_scanner_modes(self):
        # Invoke Git itself so a missing executable bit cannot silently pass.
        (self.root / "scripts").mkdir()
        (self.root / "scripts/check_sensitive_data.py").symlink_to(SCRIPT)
        (self.root / "notes.md").write_text(TOKEN)
        self.git("add", "notes.md")
        self.git("config", "core.hooksPath", str(HOOKS))
        result = subprocess.run(["git", "commit", "--quiet", "-m", "fixture"], cwd=self.root, env=self.env,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.assertEqual(1, result.returncode)
        result = subprocess.run([str(HOOKS / "pre-push")], cwd=self.root, env=self.env,
                                input=b"invalid\n", stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.assertEqual(2, result.returncode)


if __name__ == "__main__":
    unittest.main()
