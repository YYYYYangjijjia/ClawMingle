"""Offline tests for the QQ diagnostic's credential and send boundaries."""

import io
import json
from contextlib import redirect_stdout
from pathlib import Path
import tempfile
import unittest
from unittest.mock import AsyncMock, patch
import urllib.error

import qq_smoke as probe


def event(command, **changes):
    payload = {
        "op": 0,
        "t": "GROUP_AT_MESSAGE_CREATE",
        "d": {
            "group_openid": "test_group",
            "id": "test_message",
            "author": {"bot": False},
            "content": command,
        },
    }
    payload.update(changes)
    return payload


class CredentialTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.root.chmod(0o700)
        self.path = self.root / "qq.env"
        self.write()

    def write(self, secret="test-value", extra=""):
        values = {"QQ_APP_ID": "123", "QQ_APP_SECRET": secret}
        self.path.write_text("\n".join(f"{name}={value!r}" for name, value in values.items()) + "\n" + extra)
        self.path.chmod(0o600)

    def test_quoted_values_read_without_shell_execution(self):
        marker = self.root / "should-not-exist"
        value = f"$(touch${{IFS}}{marker})"
        self.write(value)
        self.assertEqual(probe.load_credentials(self.path)["QQ_APP_SECRET"], value)
        self.assertFalse(marker.exists())

    def test_empty_and_duplicate_fields_rejected(self):
        for secret, extra in [("", ""), ("value", "QQ_APP_ID='456'\n")]:
            self.write(secret, extra)
            with self.assertRaises(probe.ProbeError):
                probe.load_credentials(self.path)

    def test_file_and_directory_permissions_required(self):
        self.path.chmod(0o644)
        with self.assertRaises(probe.ProbeError):
            probe.load_credentials(self.path)
        self.path.chmod(0o600)
        self.root.chmod(0o755)
        with self.assertRaises(probe.ProbeError):
            probe.load_credentials(self.path)

    def test_symlinks_rejected(self):
        link = self.root / "link.env"
        link.symlink_to(self.path)
        with self.assertRaises(OSError):
            probe.load_credentials(link)

    def test_bad_format_errors_never_echo_values(self):
        sensitive = "do-not-display-this-value"
        self.write(extra="UNEXPECTED=" + sensitive)
        with self.assertRaises(probe.ProbeError) as caught:
            probe.load_credentials(self.path)
        self.assertNotIn(sensitive, str(caught.exception))


class RoutingTests(unittest.TestCase):
    def test_only_exact_group_mention_command_matches(self):
        command = "/clawmingle-ping test"
        self.assertEqual(probe.matching_group_message(event(command), command), ("test_group", "test_message"))
        for kind in ["GROUP_MESSAGE_CREATE", "C2C_MESSAGE_CREATE", "READY"]:
            self.assertIsNone(probe.matching_group_message(event(command, t=kind), command))
        self.assertIsNone(probe.matching_group_message(event(command + " more"), command))
        self.assertIsNone(probe.matching_group_message(event(command.upper()), command))

    def test_bot_and_invalid_target_do_not_match(self):
        command = "/clawmingle-ping test"
        for key, value in [("author", {"bot": True}), ("group_openid", "../other"), ("id", "")]:
            payload = event(command)
            payload["d"][key] = value
            self.assertIsNone(probe.matching_group_message(payload, command))

    def test_older_mention_prefix_supported(self):
        command = "/clawmingle-ping test"
        self.assertIsNotNone(probe.matching_group_message(event("<@!123> " + command), command))

    def test_all_event_requires_structured_mention_of_ready_bot(self):
        command = "/clawmingle-register test"
        payload = event(command, t="GROUP_MESSAGE_CREATE")
        for mentions in ([], [{"id": "other-bot"}], "bot-1", None):
            payload["d"]["mentions"] = mentions
            self.assertIsNone(probe.matching_group_message(payload, command, "bot-1"))
        for field in ("id", "member_openid"):
            payload["d"]["mentions"] = [{field: "bot-1", "bot": True}]
            self.assertEqual(probe.matching_group_message(payload, command, "bot-1"), ("test_group", "test_message"))
            self.assertIsNone(probe.matching_group_message(payload, command))

    def test_command_whitespace_is_flexible_without_accepting_extra_words(self):
        command = "/clawmingle-register test"
        for content in ("/clawmingle-register\ntest", "<@!bot-1>\n/clawmingle-register  test"):
            self.assertIsNotNone(probe.matching_group_message(event(content), command))
        for content in (command + " please", command + "!", "/clawmingle-register te st"):
            self.assertIsNone(probe.matching_group_message(event(content), command))

    def test_server_self_marker_requires_strict_boolean_and_not_ready_id_equality(self):
        command = "/clawmingle-register test"
        payload = event(command, t="GROUP_MESSAGE_CREATE")
        for marker in (True, False, "true", 1, None):
            payload["d"]["mentions"] = [{"member_openid": "group-scoped-bot", "is_you": marker}]
            actual = probe.matching_group_message(payload, command, "unrelated-ready-id")
            self.assertEqual(actual is not None, marker is True)
        payload["d"]["mentions"] = [{"is_you": True}]
        self.assertIsNotNone(probe.matching_group_message(payload, command))

    def test_gateway_must_use_official_secure_host(self):
        self.assertEqual(probe.validate_gateway("wss://api.bot.qq.com/websocket"), "wss://api.bot.qq.com/websocket")
        for url in ["ws://api.bot.qq.com", "wss://example.com", "wss://user:pass@api.bot.qq.com", "wss://api.bot.qq.com:444"]:
            with self.assertRaises(probe.ProbeError):
                probe.validate_gateway(url)

    def test_error_responses_redact_private_details(self):
        raw = json.dumps({"code": 123, "message": "private-detail"}).encode()
        error = urllib.error.HTTPError(probe.API_BASE, 403, "private-detail", {}, io.BytesIO(raw))
        with patch.object(probe.urllib.request, "build_opener") as opener:
            opener.return_value.open.side_effect = error
            with self.assertRaises(probe.ProbeError) as caught:
                probe.http_json(probe.API_BASE, stage="QQ 登录")
        self.assertIn("403", str(caught.exception))
        self.assertIn("123", str(caught.exception))
        self.assertNotIn("private-detail", str(caught.exception))


class RegistrationTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.root.chmod(0o700)
        self.path = self.root / "social.toml"
        self.original = '# private settings\nallowed_groups = [] # test group\nmode = "mode-1"\n[codex]\nmodel = "example-model"\n'
        self.path.write_text(self.original)
        self.path.chmod(0o600)

    def test_registers_once_preserving_other_fields_comments_and_permissions(self):
        probe.register_group(self.path, "test_group")
        raw, config = probe.registration_config(self.path)
        self.assertEqual(config["allowed_groups"], ["test_group"])
        self.assertEqual(config["codex"]["model"], "example-model")
        self.assertIn("# test group", raw)
        self.assertIn("# private settings", raw)
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)
        probe.register_group(self.path, "test_group")
        self.assertEqual(self.path.read_text(), raw)

    def test_existing_other_group_is_never_overwritten_or_printed(self):
        probe.register_group(self.path, "private-group-marker")
        before = self.path.read_text()
        with self.assertRaises(probe.ProbeError) as caught:
            probe.register_group(self.path, "other_group")
        self.assertNotIn("private-group-marker", str(caught.exception))
        self.assertEqual(self.path.read_text(), before)

    def test_repository_paths_and_symlinks_rejected(self):
        link = self.root / "link.toml"
        link.symlink_to(self.path)
        with self.assertRaises(OSError):
            probe.registration_config(link)
        (self.root / ".git").mkdir()
        with self.assertRaises(probe.ProbeError):
            probe.registration_config(self.path)

    def test_permissions_and_oversized_or_malformed_config_rejected(self):
        self.path.chmod(0o644)
        with self.assertRaises(probe.ProbeError):
            probe.registration_config(self.path)
        self.path.chmod(0o600)
        self.root.chmod(0o755)
        with self.assertRaises(probe.ProbeError):
            probe.registration_config(self.path)
        self.root.chmod(0o700)
        for raw in ("#" * 65537, "secret-marker = ["):
            self.path.write_text(raw)
            with self.assertRaises(probe.ProbeError) as caught:
                probe.registration_config(self.path)
            self.assertNotIn("secret-marker", str(caught.exception))

    def test_failed_atomic_write_preserves_original_and_removes_temporary(self):
        with patch.object(probe.os, "replace", side_effect=OSError):
            with self.assertRaises(OSError):
                probe.register_group(self.path, "test_group")
        self.assertEqual(self.path.read_text(), self.original)
        self.assertEqual(list(self.root.glob(".group-registration-*")), [])

    def test_multiline_allowlist_requires_manual_edit_without_mutation(self):
        raw = "allowed_groups = [\n]\n"
        self.path.write_text(raw)
        with self.assertRaises(probe.ProbeError):
            probe.register_group(self.path, "test_group")
        self.assertEqual(self.path.read_text(), raw)

    def test_diagnostics_distinguish_routing_without_disclosing_event_data(self):
        command = "/clawmingle-register test-nonce"
        wrong = event("private-content-marker")
        self.assertIn("口令不一致", probe.registration_diagnostic(wrong, command))
        self.assertIn("匹配", probe.registration_diagnostic(event(command), command))
        self.assertIn("普通群消息", probe.registration_diagnostic(event(command, t="GROUP_MESSAGE_CREATE"), command))
        self.assertIsNone(probe.registration_diagnostic(event(command, t="C2C_MESSAGE_CREATE"), command))
        invalid = event(command)
        invalid["d"]["group_openid"] = "../private-group-marker"
        self.assertIn("标识不符合", probe.registration_diagnostic(invalid, command))
        for payload in (wrong, invalid):
            diagnostic = probe.registration_diagnostic(payload, command)
            for marker in ("private-content-marker", "private-group-marker", "test_message"):
                self.assertNotIn(marker, diagnostic)

    def test_all_event_identity_diagnostic_contains_only_boolean_comparisons(self):
        command = "/clawmingle-register test-nonce"
        payload = event("<@!private-app-id> " + command, t="GROUP_MESSAGE_CREATE")
        payload["d"]["mentions"] = [{"id": "private-app-id", "bot": True}]
        diagnostic = probe.registration_diagnostic(payload, command, "private-ready-id", "private-app-id")
        self.assertNotIn("private-app-id", diagnostic)
        self.assertNotIn("private-ready-id", diagnostic)
        bits = json.loads(diagnostic.split("：", 1)[1])
        self.assertTrue(all(type(value) is bool for value in bits.values()))
        self.assertTrue(bits["mention_matches_app"])
        self.assertFalse(bits["prefix_matches_ready"])
        self.assertTrue(bits["command_matches"])


class GatewayTests(unittest.IsolatedAsyncioTestCase):
    async def test_registration_accepts_all_event_with_verified_bot_mention(self):
        command = "/clawmingle-register test-nonce"
        message = event("<@!bot-1>\n/clawmingle-register\ntest-nonce", t="GROUP_MESSAGE_CREATE")
        message["d"]["mentions"] = [{"id": "bot-1"}]
        ws = AsyncMock()
        ws.recv.side_effect = [json.dumps(p) for p in [
            {"op": 10, "d": {"heartbeat_interval": 45000}},
            {"op": 0, "t": "READY", "s": 1, "d": {"user": {"id": "bot-1"}}},
            message,
        ]]
        path = Path("/private/social.toml")
        with patch.object(probe, "register_group") as register, patch.object(probe, "send_reply", new_callable=AsyncMock) as send, patch("builtins.print"):
            await probe.listen(ws, {}, "secret-marker", command, path)
        register.assert_called_once_with(path, "test_group")
        send.assert_not_awaited()

    async def test_registration_filters_events_saves_once_and_never_replies(self):
        command = "/clawmingle-register test-nonce"
        ws = AsyncMock()
        ws.recv.side_effect = [json.dumps(p) for p in [
            {"op": 10, "d": {"heartbeat_interval": 45000}},
            {"op": 0, "t": "READY", "s": 1},
            event(command, t="C2C_MESSAGE_CREATE"),
            event(command, t="GROUP_MESSAGE_CREATE"),
            event("wrong nonce"),
            event(command),
            event(command),
        ]]
        output = io.StringIO()
        path = Path("/private/social.toml")
        with patch.object(probe, "register_group") as register, patch.object(probe, "send_reply", new_callable=AsyncMock) as send, redirect_stdout(output):
            await probe.listen(ws, {}, "secret-marker", command, path)
        register.assert_called_once_with(path, "test_group")
        send.assert_not_awaited()
        self.assertEqual(ws.recv.await_count, 6)
        for private in ("test_group", "test_message", "secret-marker"):
            self.assertNotIn(private, output.getvalue())

    async def test_handshake_filter_and_single_reply_then_exit(self):
        command = "/clawmingle-ping test"
        ws = AsyncMock()
        ws.recv.side_effect = [json.dumps(p) for p in [
            {"op": 10, "d": {"heartbeat_interval": 45000}},
            {"op": 0, "t": "READY", "s": 1},
            event(command, t="C2C_MESSAGE_CREATE"),
            event("unrelated text"),
            event(command),
            event(command),
        ]]
        with patch.object(probe, "send_reply", new_callable=AsyncMock) as send, patch("builtins.print"):
            await probe.listen(ws, {}, "test-value", command)
        send.assert_awaited_once_with({}, "test-value", "test_group", "test_message")
        identify = json.loads(ws.send.call_args_list[0].args[0])
        self.assertEqual(identify["d"]["intents"], 1 << 25)
        self.assertEqual(ws.recv.await_count, 5)

    async def test_send_uses_passive_group_endpoint_and_sequence(self):
        with patch.object(probe, "http_json", return_value={"id": "sent"}) as request:
            await probe.send_reply({"QQ_APP_ID": "123"}, "test-value", "test_group", "message")
        self.assertEqual(request.call_count, 1)
        self.assertTrue(request.call_args.args[0].endswith("/v2/groups/test_group/messages"))
        body = request.call_args.kwargs["body"]
        self.assertEqual(body["msg_seq"], 1)
        self.assertEqual(body["msg_id"], "message")
        self.assertEqual(body["content"], probe.REPLY)

    async def test_unknown_send_is_not_retried(self):
        with patch.object(probe, "http_json", return_value={}) as request:
            with self.assertRaisesRegex(probe.ProbeError, "结果未知"):
                await probe.send_reply({"QQ_APP_ID": "123"}, "test-value", "group", "message")
        self.assertEqual(request.call_count, 1)

    async def test_session_rejection_stops_without_reconnect(self):
        ws = AsyncMock()
        ws.recv.return_value = json.dumps({"op": 9, "d": False})
        with self.assertRaisesRegex(probe.ProbeError, "拒绝会话"):
            await probe.listen(ws, {}, "test-value", "command")
        ws.send.assert_not_awaited()


if __name__ == "__main__":
    unittest.main()
