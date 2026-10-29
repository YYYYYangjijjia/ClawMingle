#!/usr/bin/env python3
"""Conservative local Git checks; never print matched sensitive values."""

import argparse
import fnmatch
import ipaddress
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys


SAFE_TEMPLATES = {".env.example", ".env.sample", ".env.template", "codexclaw.example.toml", "clawmingle.example.toml"}
PRIVATE_DIRS = {
    ".local", ".codex", ".codex-claw", ".ssh", ".secrets", "secrets", "credentials",
}
RUNTIME_ROOTS = {"data", "logs", "backups", "exports", "uploads", "inbox", "target"}
PRIVATE_NAMES = (
    ".env", ".env.*", "*.env", "auth.json", "credentials.json", "codexclaw.toml", "codexclaw.*.toml",
    "clawmingle.toml", "clawmingle.*.toml", "social.toml",
    "*.local.toml", "*.local.json", "*.local.yaml", "*.local.yml",
    "*.key", "*.pem", "*.p12", "*.pfx", "id_rsa", "id_ed25519",
    "*.log", "*.db", "*.db-shm", "*.db-wal", "*.sqlite*",
)
# Exact legacy dummy values used by upstream wiremock fixtures and setup docs.
# Never exempt a whole test file or an arbitrary value starting with "test".
SAFE_VALUES = {"", "secret", "token", "token-1", "test-secret", "test-token", "你的AppSecret"}
DOCUMENTATION_NETWORKS = tuple(map(ipaddress.ip_network, (
    "192.0.2.0/24", "198.51.100.0/24", "203.0.113.0/24", "2001:db8::/32",
)))
CREDENTIAL = r"(?:api[_-]?key|app[_-]?secret|client[_-]?secret|access[_-]?token|refresh[_-]?token|password|passwd|secret[_-]?key)"
RULES = (
    ("private-key", re.compile(r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----")),
    ("provider-token", re.compile(
        r"(?<![\w-])(?:sk-(?:proj-|svcacct-)?[A-Za-z0-9_-]{20,}|"
        r"gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,}|AKIA[0-9A-Z]{16})"
    )),
    ("uuid", re.compile(r"\b[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}\b")),
    ("connection-credentials", re.compile(
        r"\b(?:https?|ssh|postgres(?:ql)?|mysql|rediss?|amqps?|mongodb(?:\+srv)?)://[^\s/:]+:[^\s/@]+@",
        re.IGNORECASE,
    )),
    ("vpn-connection", re.compile(r"\b(?:vless|vmess|trojan|ss)://[^\s\"'<>]+", re.IGNORECASE)),
    ("auth-header", re.compile(r"\b(?:Bearer|Basic)\s+[A-Za-z0-9._~+/=-]{16,}", re.IGNORECASE)),
)
QUOTED_CREDENTIAL = re.compile(
    rf"(?i)\b(?:[A-Z0-9]+_)*{CREDENTIAL}[\"']?\s*[:=]\s*([\"'])([^\"'\r\n]*)\1"
)
ENV_CREDENTIAL = re.compile(
    rf"(?im)^\s*(?:export\s+)?(?:\w+_)?{CREDENTIAL}\s*=\s*([^\s#\"']+)"
)
IPV4 = re.compile(r"(?<![0-9.])(?:[0-9]{1,3}\.){3}[0-9]{1,3}(?![0-9]|\.[0-9])")
IPV6 = re.compile(r"(?<![\w:])(?:[0-9a-fA-F]{0,4}:){2,}[0-9a-fA-F:.]*(?:%[\w.-]+)?(?![\w:])")


def git(*args):
    result = subprocess.run(["git", *args], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if result.returncode:
        # Git errors can contain URLs and object contents; do not echo stderr.
        raise RuntimeError("Git read failed; check repository state locally")
    return result.stdout


def private_path(path):
    parts = PurePosixPath(path).parts
    if not parts:
        return False
    name = parts[-1].lower()
    if any(part.lower() in PRIVATE_DIRS for part in parts[:-1]):
        return True
    if parts[0].lower() in RUNTIME_ROOTS:
        return True
    if parts[:2] == ("config", "local"):
        return True
    return name not in SAFE_TEMPLATES and any(fnmatch.fnmatchcase(name, rule) for rule in PRIVATE_NAMES)


def placeholder(value):
    return (
        value in SAFE_VALUES
        or re.fullmatch(r"(?:YOUR|EXAMPLE|PLACEHOLDER)_[A-Z0-9_]+", value, re.IGNORECASE) is not None
        or re.fullmatch(r"<[^<>\r\n]+>", value) is not None
        or re.fullmatch(r"\$\{?[A-Z][A-Z0-9_]*\}?", value) is not None
    )


def safe_ip(value):
    try:
        address = ipaddress.ip_address(value.split("%", 1)[0])
    except ValueError:
        return True  # Not a valid IP address, e.g. Rust syntax.
    if isinstance(address, ipaddress.IPv6Address) and address.ipv4_mapped:
        return safe_ip(str(address.ipv4_mapped))
    return address.is_loopback or address.is_unspecified or any(
        address.version == network.version and address in network
        for network in DOCUMENTATION_NETWORKS
    )


def scan_content(raw, forbidden_values=()):
    """Return only (line, rule); binary assets still need visual/manual review."""
    if b"\0" in raw:
        return []
    content = raw.decode("utf-8", errors="replace")
    findings = set()
    for rule, pattern in RULES:
        for match in pattern.finditer(content):
            findings.add((content.count("\n", 0, match.start()) + 1, rule))
    for pattern, group in ((QUOTED_CREDENTIAL, 2), (ENV_CREDENTIAL, 1)):
        for match in pattern.finditer(content):
            if not placeholder(match[group]):
                findings.add((content.count("\n", 0, match.start()) + 1, "credential-value"))
    for pattern in (IPV4, IPV6):
        for match in pattern.finditer(content):
            if not safe_ip(match[0]):
                findings.add((content.count("\n", 0, match.start()) + 1, "server-ip"))
    for value in forbidden_values:
        start = 0
        while (position := content.find(value, start)) >= 0:
            findings.add((content.count("\n", 0, position) + 1, "local-forbidden-value"))
            start = position + len(value)
    return sorted(findings)


def redact_location(location, forbidden_values):
    if any(rule == "credential-value" for _, rule in scan_content(location.encode())):
        return "[sensitive-path]"
    for _, pattern in RULES:
        location = pattern.sub("[redacted]", location)
    for pattern in (IPV4, IPV6):
        location = pattern.sub(lambda match: match[0] if safe_ip(match[0]) else "[redacted]", location)
    for value in forbidden_values:
        location = location.replace(value, "[redacted]")
    return location


class Scanner:
    def __init__(self, root):
        self.root = root
        self.findings = set()
        self.checked_blobs = set()
        self.blobs = 0
        denylist = root / ".local/sensitive-values.txt"
        self.forbidden_values = (
            [line for line in denylist.read_text().splitlines() if line]
            if denylist.exists() else []
        )

    def scan(self, path, raw, object_id=None, location=None):
        location = location or path
        if private_path(path):
            self.findings.add((location, 0, "private-path"))
        for _, rule in scan_content(path.encode(), self.forbidden_values):
            self.findings.add((location, 0, "path-" + rule))
        if object_id is not None:
            if object_id in self.checked_blobs:
                return
            self.checked_blobs.add(object_id)
        self.blobs += 1
        for line, rule in scan_content(raw, self.forbidden_values):
            self.findings.add((location, line, rule))

    def index(self, staged):
        entries = git("ls-files", "--stage", "-z").split(b"\0")
        for entry in filter(None, entries):
            metadata, name = entry.split(b"\t", 1)
            mode, oid, stage = metadata.decode().split()
            path = name.decode("utf-8")
            if stage != "0":
                raise RuntimeError("Resolve unmerged index entries before scanning")
            if mode == "160000":
                raise RuntimeError("Submodules require a separate sensitive-data review")
            if staged:
                self.scan(path, git("cat-file", "blob", oid), oid)
            else:
                target = self.root / path
                if target.is_symlink():
                    # Inspect the stored link text, never follow links into private files.
                    self.scan(path, git("cat-file", "blob", oid), oid)
                elif target.exists():
                    self.scan(path, target.read_bytes())

    def history(self, revisions):
        for revision in revisions:
            if git("cat-file", "-t", revision).strip() == b"tag":
                self.scan("tag-message", git("cat-file", "tag", revision), location="tag-message")
        commits = git("rev-list", *revisions, "--").decode().splitlines()
        for commit in commits:
            self.scan("commit-message", git("show", "-s", "--format=%B", commit), location=f"{commit[:12]}:commit-message")
            for entry in filter(None, git("ls-tree", "-r", "-z", commit).split(b"\0")):
                metadata, name = entry.split(b"\t", 1)
                mode, kind, oid = metadata.decode().split()
                if kind != "blob":
                    raise RuntimeError("Submodules require a separate sensitive-data review")
                path = name.decode("utf-8")
                self.scan(path, git("cat-file", "blob", oid) if oid not in self.checked_blobs else b"", oid,
                          location=f"{commit[:12]}:{path}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group(required=True)
    modes.add_argument("--tracked", action="store_true")
    modes.add_argument("--staged", action="store_true")
    modes.add_argument("--history", nargs="+", metavar="REV")
    modes.add_argument("--pre-push", action="store_true")
    args = parser.parse_args()
    try:
        root = Path(git("rev-parse", "--show-toplevel").decode().strip())
        # Hooks run at the root; make manual invocation from subdirectories equivalent.
        os.chdir(root)
        scanner = Scanner(root)
        if args.pre_push:
            revisions = []
            for line in sys.stdin:
                fields = line.split()
                if len(fields) != 4 or not re.fullmatch(r"[0-9a-fA-F]{40,64}", fields[1]):
                    raise RuntimeError("Invalid pre-push input")
                if set(fields[1]) != {"0"}:
                    revisions.append(fields[1])
            if revisions:
                scanner.history(revisions)
        elif args.history:
            scanner.history(args.history)
        else:
            scanner.index(args.staged)
        for location, line, rule in sorted(scanner.findings):
            # A path might itself contain a forbidden value. Redact those too.
            location = redact_location(location, scanner.forbidden_values)
            print(f"{location!r}:{line}: {rule}", file=sys.stderr)
        if scanner.findings:
            print("Sensitive-data check failed; values withheld. Do not commit or push.", file=sys.stderr)
            return 1
        print(f"Sensitive-data check passed ({scanner.blobs} text/binary objects checked; binary contents need manual review).")
        return 0
    except (OSError, ValueError, RuntimeError) as error:
        # Do not stringify errors that could contain local paths or secret data.
        print(f"Sensitive-data check could not complete ({type(error).__name__}); commit/push blocked.", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
