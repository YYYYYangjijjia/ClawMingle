"""Publish ClawMingle's command catalog through QQ's official panel API."""

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tempfile
import urllib.error
import urllib.parse
import urllib.request

from qq_smoke import API_BASE, NoRedirect, ProbeError, load_credentials, registration_config

REMARK = "ClawMingle commands v1"


def api(method, path, credentials, token, body=None):
    request = urllib.request.Request(
        API_BASE + path,
        method=method,
        data=json.dumps(body).encode() if body is not None else None,
        headers={"Content-Type": "application/json", "Authorization": "QQBot " + token,
                 "X-Union-Appid": credentials["QQ_APP_ID"]},
    )
    try:
        with urllib.request.build_opener(NoRedirect()).open(request, timeout=15) as response:
            raw = response.read(1_048_577)
    except urllib.error.HTTPError as error:
        status = error.code
        error.close()
        raise ProbeError(f"QQ 面板操作被拒绝：HTTP {status}；未显示响应内容。") from None
    except (OSError, urllib.error.URLError):
        raise ProbeError("QQ 面板操作结果无法确认；程序不会自动重复写入。") from None
    try:
        value = json.loads(raw)
    except (ValueError, UnicodeError):
        raise ProbeError("QQ 面板响应无法解析；程序不会自动重复写入。") from None
    if len(raw) > 1_048_576 or not isinstance(value, dict):
        raise ProbeError("QQ 面板响应结构异常。")
    return value


def private_save(path, value):
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    metadata = path.parent.lstat()
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.getuid() or stat.S_IMODE(metadata.st_mode) != 0o700:
        raise ProbeError("面板备份目录必须属于当前用户，权限为 0700。")
    fd, name = tempfile.mkstemp(prefix=".panels-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as handle:
            json.dump(value, handle, ensure_ascii=False)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(name, path)
    finally:
        Path(name).unlink(missing_ok=True)


def identifier(value):
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,256}", value):
        raise ProbeError("QQ 面板标识结构异常；未显示原值。")
    return value


def list_panels(request, scope):
    records = []
    cursor = ""
    seen = set()
    for _ in range(4):
        query = urllib.parse.urlencode({"scope": scope, "limit": 20, "cursor": cursor})
        result = request("GET", "/v2/panels?" + query)
        page = result.get("records")
        # Observed QQ empty terminal pages omit the records field entirely.
        if page is None and result.get("is_end") is True and not result.get("next_cursor"):
            page = []
        if not isinstance(page, list) or any(not isinstance(record, dict) for record in page):
            raise ProbeError("QQ 面板列表结构异常。")
        records.extend(page)
        if result.get("is_end") is True:
            return records
        cursor = result.get("next_cursor")
        if not isinstance(cursor, str) or not cursor or cursor in seen:
            raise ProbeError("QQ 面板分页结构异常。")
        seen.add(cursor)
    raise ProbeError("QQ 面板分页超过安全上限。")


def normalized_item(item):
    # QQ strips the command slash and omits false only_admin on readback.
    normalized = dict(item)
    if normalized.get("type") == "command":
        normalized["name"] = normalized.get("name", "").lstrip("/")
        normalized.setdefault("only_admin", False)
    return normalized


def same_items(left, right):
    return isinstance(left, list) and [normalized_item(item) for item in left] == [normalized_item(item) for item in right]


def prepare_panel(existing, catalog):
    names = {normalized_item(item)["name"] for item in catalog}
    extras = [item for item in existing.get("items", []) if normalized_item(item).get("name") not in names]
    items = extras + catalog
    if len(items) > 20:
        raise ProbeError("已有面板与新增指令合计超过 20 项，未覆盖原配置。")
    return {"items": items, "remark": REMARK}


def publish_scope(request, scope, catalog, groups, backup_dir):
    records = list_panels(request, scope)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S-%f")
    private_save(backup_dir / f"{scope}-before-{stamp}.json", records)
    owned = [record for record in records if record.get("panel", {}).get("remark") == REMARK]
    if len(owned) > 1:
        raise ProbeError("存在多个 ClawMingle 面板，需先核对，未写入。")
    target_type = "specific" if scope == "group" else "all"
    if owned:
        panel_id = identifier(owned[0].get("panel_id"))
        detail = request("GET", "/v2/panels/" + panel_id)
        private_save(backup_dir / f"{scope}-detail-before-{stamp}.json", detail)
        if detail.get("scope") != scope or detail.get("target_type") != target_type:
            raise ProbeError("现有面板的场景或作用范围不匹配，未覆盖。")
        if scope == "group" and set(detail.get("group_openids", [])) != set(groups):
            raise ProbeError("现有面板关联群不匹配，未覆盖。")
        panel = prepare_panel(detail.get("panel", {}), catalog)
        if not same_items(detail.get("panel", {}).get("items"), panel["items"]):
            result = request("PUT", "/v2/panels/" + panel_id, {"panel": panel})
            if type(result.get("version")) is not int:
                raise ProbeError("QQ 未确认面板版本；请先核对，程序不重写。")
    else:
        panel = prepare_panel({}, catalog)
        body = {"scope": scope, "target_type": target_type, "panel": panel}
        if scope == "group":
            body["group_openids"] = groups
        result = request("POST", "/v2/panels", body)
        panel_id = identifier(result.get("panel_id"))
    detail = request("GET", "/v2/panels/" + panel_id)
    saved = detail.get("panel", {}).get("items")
    if detail.get("scope") != scope or detail.get("target_type") != target_type or not same_items(saved, panel["items"]):
        raise ProbeError("面板写入后的读取核对不一致。")
    if scope == "group" and set(detail.get("group_openids", [])) != set(groups):
        raise ProbeError("面板关联群核对不一致。")
    private_save(backup_dir / f"{scope}-current.json", detail)
    print(f"{scope} 指令面板服务端已保存并核对：{len(catalog)} 个命令。")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true", help="Explicitly publish after checking existing panels")
    parser.add_argument("--social-config", type=Path, default=Path.home() / ".config/clawmingle/social.toml")
    parser.add_argument("--binary", type=Path, default=Path.home() / ".local/lib/clawmingle/clawmingle")
    args = parser.parse_args()
    if not args.apply:
        parser.print_help()
        return 0
    try:
        _, config = registration_config(args.social_config)
        groups = config.get("allowed_groups", [])
        if not groups or len(groups) > 20:
            raise ProbeError("先登记测试群后再发布面板。")
        result = subprocess.run([str(args.binary), "commands"], capture_output=True, timeout=10, check=True)
        catalog = json.loads(result.stdout)
        if not isinstance(catalog, list) or not 1 <= len(catalog) <= 20:
            raise ProbeError("命令目录无效。")
        for item in catalog:
            if not isinstance(item, dict) or not re.fullmatch(r"/[a-z][a-z-]{0,12}", item.get("name", "")) or item.get("type") != "command" or not isinstance(item.get("desc"), str) or len(item["desc"].encode("utf-16-le")) // 2 > 30:
                raise ProbeError("命令目录字段无效。")
        credentials_path = Path(config.get("qq_credentials_file", "~/.config/clawmingle/qq.env")).expanduser()
        credentials = load_credentials(credentials_path)
        from qq_smoke import http_json
        token = http_json(API_BASE + "/app/getAppAccessToken", body={"appId": credentials["QQ_APP_ID"], "clientSecret": credentials["QQ_APP_SECRET"]}, stage="QQ 菜单鉴权").get("access_token")
        if not isinstance(token, str) or not token:
            raise ProbeError("QQ 菜单鉴权失败。")
        request = lambda method, path, body=None: api(method, path, credentials, token, body)
        backup_dir = args.social_config.parent / "panels"
        for scope in ("group", "c2c"):
            publish_scope(request, scope, catalog, groups, backup_dir)
    except ProbeError as error:
        print(str(error))
        return 1
    except (OSError, ValueError, TypeError, KeyError, subprocess.SubprocessError):
        print("面板配置未完成；未输出可能包含私有配置或身份的底层错误。")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
