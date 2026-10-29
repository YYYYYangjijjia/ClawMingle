"""Offline checks for scoped, idempotent QQ panel publication."""

from contextlib import redirect_stdout
import io
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

import qq_panels as panels

CATALOG = [{"type": "command", "name": "/help", "desc": "查看帮助", "only_admin": False}]


class PanelTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.backup = Path(self.directory.name) / "panels"

    def test_empty_terminal_page_can_omit_records(self):
        request = Mock(return_value={"is_end": True, "next_cursor": ""})
        self.assertEqual(panels.list_panels(request, "group"), [])
        request.assert_called_once()

    def test_pagination_cycle_is_rejected(self):
        request = Mock(return_value={"records": [], "is_end": False, "next_cursor": "repeat"})
        with self.assertRaises(panels.ProbeError):
            panels.list_panels(request, "group")
        self.assertEqual(request.call_count, 2)

    def test_new_panel_is_created_once_scoped_and_read_back_without_echoing_ids(self):
        detail = {"panel_id": "private-panel-marker", "scope": "group", "target_type": "specific",
                  "group_openids": ["private-group-marker"], "panel": {"items": CATALOG}}
        request = Mock(side_effect=[{"records": [], "is_end": True}, {"panel_id": "private-panel-marker"}, detail])
        output = io.StringIO()
        with redirect_stdout(output):
            panels.publish_scope(request, "group", CATALOG, ["private-group-marker"], self.backup)
        writes = [call for call in request.call_args_list if call.args[0] == "POST"]
        self.assertEqual(len(writes), 1)
        self.assertEqual(writes[0].args[2]["group_openids"], ["private-group-marker"])
        self.assertEqual(writes[0].args[2]["target_type"], "specific")
        self.assertNotIn("private-panel-marker", output.getvalue())
        self.assertNotIn("private-group-marker", output.getvalue())
        for p in self.backup.iterdir():
            self.assertEqual(p.stat().st_mode & 0o777, 0o600)

    def test_existing_panel_preserves_unrelated_items_and_does_not_recreate(self):
        extra = {"type": "link", "name": "existing", "desc": "现有入口", "link": "https://example.com"}
        record = {"panel_id": "existing-panel", "scope": "c2c", "target_type": "all",
                  "panel": {"remark": panels.REMARK, "items": [extra]}}
        after = dict(record, panel={"items": [extra] + CATALOG, "remark": panels.REMARK})
        request = Mock(side_effect=[{"records": [record], "is_end": True}, record, {"version": 2}, after])
        with redirect_stdout(io.StringIO()):
            panels.publish_scope(request, "c2c", CATALOG, [], self.backup)
        self.assertFalse(any(call.args[0] == "POST" for call in request.call_args_list))
        update = [call for call in request.call_args_list if call.args[0] == "PUT"][0]
        self.assertEqual(update.args[2]["panel"]["items"], [extra] + CATALOG)

    def test_existing_other_group_is_never_overwritten(self):
        record = {"panel_id": "existing-panel", "scope": "group", "target_type": "specific",
                  "group_openids": ["different-group"], "panel": {"remark": panels.REMARK, "items": []}}
        request = Mock(side_effect=[{"records": [record], "is_end": True}, record])
        with self.assertRaises(panels.ProbeError):
            panels.publish_scope(request, "group", CATALOG, ["intended-group"], self.backup)
        self.assertTrue(all(call.args[0] == "GET" for call in request.call_args_list))

    def test_server_normalized_commands_are_idempotent(self):
        record = {"panel_id": "existing-panel", "scope": "c2c", "target_type": "all",
                  "panel": {"remark": panels.REMARK, "items": [{"type": "command", "name": "help", "desc": "查看帮助"}]}}
        request = Mock(side_effect=[{"records": [record], "is_end": True}, record, record])
        with redirect_stdout(io.StringIO()):
            panels.publish_scope(request, "c2c", CATALOG, [], self.backup)
        self.assertTrue(all(call.args[0] == "GET" for call in request.call_args_list))
        self.assertFalse(panels.same_items(record["panel"]["items"], [dict(CATALOG[0], only_admin=True)]))

    def test_inconsistent_confirmation_does_not_repeat_write(self):
        request = Mock(side_effect=[{"records": [], "is_end": True}, {"panel_id": "created"}, {}])
        with self.assertRaises(panels.ProbeError):
            panels.publish_scope(request, "group", CATALOG, ["group"], self.backup)
        self.assertEqual(sum(call.args[0] == "POST" for call in request.call_args_list), 1)

    def test_panel_limit_never_discards_existing_items(self):
        existing = {"items": [{"name": f"old-{i}"} for i in range(20)]}
        with self.assertRaises(panels.ProbeError):
            panels.prepare_panel(existing, CATALOG)
        self.assertEqual(len(existing["items"]), 20)

    def test_network_failure_does_not_echo_or_retry(self):
        with patch.object(panels.urllib.request, "build_opener") as opener:
            opener.return_value.open.side_effect = OSError("secret-marker")
            with self.assertRaises(panels.ProbeError) as caught:
                panels.api("POST", "/v2/panels", {"QQ_APP_ID": "example"}, "secret-marker", {})
        self.assertEqual(opener.return_value.open.call_count, 1)
        self.assertNotIn("secret-marker", str(caught.exception))


if __name__ == "__main__":
    unittest.main()
