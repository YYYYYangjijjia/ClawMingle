"""One-shot QQ group transport diagnostic; never starts the Rust application."""

import argparse
import asyncio
import json
import logging
import os
from pathlib import Path
import re
import secrets
import shlex
import stat
import tempfile
import tomllib
import urllib.error
import urllib.parse
import urllib.request


API_BASE = "https://api.bot.qq.com"
DEFAULT_CONFIG = Path.home() / ".config/clawmingle/qq.env"
REPLY = "ClawMingle 收发测试成功：已收到你的 @ 消息。这是连接测试，尚未接入模型。"


class ProbeError(Exception):
    """Only fixed, non-sensitive diagnostic messages may enter this exception."""


def load_credentials(path):
    directory = path.parent.lstat()
    if (
        not stat.S_ISDIR(directory.st_mode)
        or directory.st_uid != os.getuid()
        or stat.S_IMODE(directory.st_mode) != 0o700
    ):
        raise ProbeError("QQ 配置目录必须属于当前用户，权限为 0700。")
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "r", encoding="utf-8") as handle:
        metadata = os.fstat(handle.fileno())
        if (
            not stat.S_ISREG(metadata.st_mode)
            or metadata.st_uid != os.getuid()
            or stat.S_IMODE(metadata.st_mode) != 0o600
            or metadata.st_size > 16384
        ):
            raise ProbeError("QQ 配置必须为当前用户的普通文件，权限为 0600。")
        values = {}
        for line in handle:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            match = re.fullmatch(r"(QQ_APP_ID|QQ_APP_SECRET)=(.*)", line)
            if not match or match[1] in values:
                raise ProbeError("QQ 配置格式错误：每个字段只填写一次。")
            try:
                parts = shlex.split(match[2])
            except ValueError:
                raise ProbeError("QQ 配置引号不匹配。") from None
            if len(parts) != 1 or not parts[0] or any(c.isspace() for c in parts[0]):
                raise ProbeError("QQ 配置尚未填写完整，或字段包含空白。")
            values[match[1]] = parts[0]
    if set(values) != {"QQ_APP_ID", "QQ_APP_SECRET"}:
        raise ProbeError("请填写 QQ_APP_ID 和 QQ_APP_SECRET。")
    if not values["QQ_APP_ID"].isascii() or not values["QQ_APP_ID"].isdigit():
        raise ProbeError("QQ_APP_ID 应填写开放平台中的数字 AppID。")
    return values


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def http_json(url, *, body=None, token=None, app_id=None, stage):
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"QQBot {token}"
        headers["X-Union-Appid"] = app_id
    request = urllib.request.Request(
        url,
        data=json.dumps(body).encode() if body is not None else None,
        headers=headers,
    )
    opener = urllib.request.build_opener(NoRedirect())
    try:
        with opener.open(request, timeout=15) as response:
            raw = response.read(1024 * 1024)
    except urllib.error.HTTPError as error:
        # Never display response bodies: QQ can include private IDs or addresses.
        with error:
            try:
                code = json.loads(error.read(16384)).get("code")
            except (ValueError, AttributeError):
                code = None
        suffix = f"，QQ 错误码 {code}" if type(code) is int else ""
        raise ProbeError(f"{stage}被拒绝：HTTP {error.code}{suffix}。") from None
    except (OSError, urllib.error.URLError):
        if stage == "群消息发送":
            raise ProbeError("群消息发送结果未知；请先检查 QQ，程序不会自动重发。") from None
        raise ProbeError(f"{stage}网络请求失败；未显示连接详情。") from None
    try:
        value = json.loads(raw)
    except ValueError:
        if stage == "群消息发送":
            raise ProbeError("群消息发送响应无法解析，结果未知；请检查 QQ，程序不会自动重发。") from None
        raise ProbeError(f"{stage}响应无法解析；未显示响应内容。") from None
    if not isinstance(value, dict):
        raise ProbeError(f"{stage}响应结构异常。")
    return value


def validate_gateway(url):
    if not isinstance(url, str):
        raise ProbeError("QQ 未返回可用的网关地址。")
    parsed = urllib.parse.urlsplit(url)
    if (
        parsed.scheme != "wss"
        or parsed.hostname not in {"api.bot.qq.com", "api.sgroup.qq.com"}
        or parsed.username
        or parsed.password
        or parsed.port not in (None, 443)
    ):
        raise ProbeError("QQ 返回了不在本测试允许范围内的网关地址。")
    return url


def group_mentions_bot(payload, bot_id=None):
    if payload.get("op") != 0:
        return False
    if payload.get("t") == "GROUP_AT_MESSAGE_CREATE":
        return True
    data = payload.get("d")
    if payload.get("t") != "GROUP_MESSAGE_CREATE" or not isinstance(data, dict):
        return False
    mentions = data.get("mentions")
    return isinstance(mentions, list) and any(
        isinstance(user, dict) and (
            user.get("is_you") is True
            or bool(bot_id and (user.get("id") == bot_id or user.get("member_openid") == bot_id))
        )
        for user in mentions
    )


def command_matches(content, command):
    content = re.sub(r"^<@!?[A-Za-z0-9_-]+>\s*", "", content.strip())
    return content.split() == command.split()


def matching_group_message(payload, command, bot_id=None):
    if not group_mentions_bot(payload, bot_id):
        return None
    data = payload.get("d")
    if not isinstance(data, dict):
        return None
    author = data.get("author")
    if not isinstance(author, dict) or author.get("bot") is True:
        return None
    content = data.get("content")
    if not isinstance(content, str):
        return None
    # Current QQ events remove the bot prefix; accept the older wire form too.
    if not command_matches(content, command):
        return None
    group = data.get("group_openid")
    message = data.get("id")
    if not isinstance(group, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,256}", group):
        return None
    if not isinstance(message, str) or not message:
        return None
    return group, message


def registration_diagnostic(payload, command, bot_id=None, app_id=None):
    """Only fixed statuses; never interpolate event fields, text, IDs, or errors."""
    if payload.get("op") != 0:
        return None
    if payload.get("t") not in ("GROUP_AT_MESSAGE_CREATE", "GROUP_MESSAGE_CREATE"):
        return None
    if not group_mentions_bot(payload, bot_id):
        data = payload.get("d")
        if not isinstance(data, dict):
            return "收到群消息，但事件结构无效。"
        mentions = data.get("mentions")
        users = [user for user in mentions if isinstance(user, dict)] if isinstance(mentions, list) else []
        identities = [user.get(field) for user in users for field in ("id", "member_openid")]
        prefix = re.match(r"^<@!?([A-Za-z0-9_-]+)>", str(data.get("content", "")).strip())
        bits = {
            "ready_identity_known": bool(bot_id),
            "mentions_present": bool(users),
            "mentioned_user_marked_bot": any(user.get("bot") is True for user in users),
            "self_mention_marker": any(user.get("is_you") is True for user in users),
            "mention_matches_app": bool(app_id and app_id in identities),
            "prefix_matches_ready": bool(prefix and bot_id and prefix[1] == bot_id),
            "prefix_matches_app": bool(prefix and app_id and prefix[1] == app_id),
            "command_matches": isinstance(data.get("content"), str) and command_matches(data["content"], command),
        }
        return "收到普通群消息；未找到对本机器人的结构化 @。脱敏结构检查：" + json.dumps(bits, sort_keys=True)
    if matching_group_message(payload, command, bot_id) is not None:
        return "收到匹配的群 @ 登记消息。"
    data = payload.get("d")
    if not isinstance(data, dict) or not isinstance(data.get("content"), str):
        return "收到群 @ 事件，但消息结构不符合登记要求。"
    if not command_matches(data["content"], command):
        return "收到群 @ 事件，但正文与本次登记口令不一致。"
    return "收到群 @ 事件，但发送者或群消息标识不符合登记要求。"


def registration_config(path):
    if not path.is_absolute() or any((parent / ".git").exists() for parent in path.resolve().parents):
        raise ProbeError("群登记配置必须是 Git 仓库外的绝对路径。")
    directory = path.parent.lstat()
    if (
        not stat.S_ISDIR(directory.st_mode)
        or directory.st_uid != os.getuid()
        or stat.S_IMODE(directory.st_mode) != 0o700
    ):
        raise ProbeError("群登记配置目录必须属于当前用户，权限为 0700。")
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW), "r", encoding="utf-8") as handle:
        metadata = os.fstat(handle.fileno())
        if (
            not stat.S_ISREG(metadata.st_mode)
            or metadata.st_uid != os.getuid()
            or stat.S_IMODE(metadata.st_mode) != 0o600
            or metadata.st_size > 65536
        ):
            raise ProbeError("群登记配置必须为当前用户的普通文件，权限为 0600，大小不超过 64 KiB。")
        raw = handle.read(65537)
    try:
        config = tomllib.loads(raw)
    except tomllib.TOMLDecodeError:
        raise ProbeError("群登记配置 TOML 格式错误；未显示内容。") from None
    if not isinstance(config.get("allowed_groups"), list):
        raise ProbeError("群登记配置缺少顶层 allowed_groups 列表。")
    return raw, config


def register_group(path, group):
    raw, config = registration_config(path)
    if config["allowed_groups"] == [group]:
        return
    if config["allowed_groups"] not in ([], ["YOUR_GROUP_OPENID"]):
        raise ProbeError("配置已有其他群白名单，拒绝覆盖。")
    updated, count = re.subn(
        r"(?m)^allowed_groups[ \t]*=[ \t]*\[[^\r\n]*\]([ \t]*(?:#[^\r\n]*)?)$",
        lambda match: "allowed_groups = " + json.dumps([group]) + match[1],
        raw,
    )
    expected = dict(config, allowed_groups=[group])
    if count != 1 or tomllib.loads(updated) != expected:
        raise ProbeError("仅支持模板中的顶层单行群白名单；未修改配置。")
    temporary = None
    try:
        fd, name = tempfile.mkstemp(prefix=".group-registration-", dir=path.parent)
        temporary = Path(name)
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(updated)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
        temporary = None
        directory_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


async def send_reply(credentials, token, group, message):
    result = await asyncio.to_thread(
        http_json,
        f"{API_BASE}/v2/groups/{urllib.parse.quote(group, safe='')}/messages",
        body={"msg_type": 0, "content": REPLY, "msg_id": message, "msg_seq": 1},
        token=token,
        app_id=credentials["QQ_APP_ID"],
        stage="群消息发送",
    )
    if not isinstance(result.get("id"), str) or not result["id"]:
        raise ProbeError("群消息发送未取得确认，结果未知；请检查 QQ，程序不会自动重发。")


async def listen(websocket, credentials, token, command, registration_path=None,
                 registration_callback=None, status_callback=None):
    report = status_callback or (lambda text: print(text, flush=True))
    ready = False
    bot_id = None
    last_seq = None
    unacked = 0
    heartbeat_interval = None
    next_heartbeat = None
    loop = asyncio.get_running_loop()
    while True:
        if next_heartbeat is not None and loop.time() >= next_heartbeat:
            if unacked >= 2:
                raise ProbeError("QQ 心跳未获确认，测试停止；没有自动重连。")
            await websocket.send(json.dumps({"op": 1, "d": last_seq}))
            unacked += 1
            next_heartbeat = loop.time() + heartbeat_interval
        wait = max(0, next_heartbeat - loop.time()) if next_heartbeat is not None else 15
        try:
            raw = await asyncio.wait_for(websocket.recv(), timeout=wait)
        except TimeoutError:
            if next_heartbeat is None:
                raise ProbeError("QQ 未及时发送连接握手。") from None
            continue
        try:
            payload = json.loads(raw)
        except (ValueError, TypeError):
            raise ProbeError("QQ 网关消息无法解析；未显示内容。") from None
        if not isinstance(payload, dict):
            raise ProbeError("QQ 网关消息结构异常。")
        if isinstance(payload.get("s"), int):
            last_seq = payload["s"]
        if ready and registration_path is not None:
            if diagnostic := registration_diagnostic(payload, command, bot_id, credentials.get("QQ_APP_ID")):
                report(diagnostic)
        opcode = payload.get("op")
        if opcode == 10:
            if heartbeat_interval is not None:
                raise ProbeError("QQ 重复发送连接握手，测试停止。")
            hello = payload.get("d")
            interval = hello.get("heartbeat_interval") if isinstance(hello, dict) else None
            if type(interval) is not int or not 1000 <= interval <= 120000:
                raise ProbeError("QQ 心跳设置异常。")
            heartbeat_interval = interval / 1000
            next_heartbeat = loop.time() + heartbeat_interval
            await websocket.send(json.dumps({
                "op": 2,
                "d": {"token": f"QQBot {token}", "intents": 1 << 25, "shard": [0, 1]},
            }))
        elif opcode == 11:
            unacked = 0
        elif opcode == 1:
            await websocket.send(json.dumps({"op": 1, "d": last_seq}))
        elif opcode in (7, 9):
            raise ProbeError("QQ 要求重新连接或拒绝会话，测试停止；没有自动重连。")
        elif opcode == 0 and payload.get("t") == "READY":
            if not ready:
                ready = True
                data = payload.get("d")
                user = data.get("user") if isinstance(data, dict) else None
                identity = user.get("id") if isinstance(user, dict) else None
                bot_id = identity if isinstance(identity, str) and identity else None
                report("QQ 已登录。请在要接入的群里真正 @机器人，并发送本次登记口令。")
                if status_callback is None:
                    print(command, flush=True)
        elif ready and (match := matching_group_message(payload, command, bot_id)):
            if registration_callback is not None:
                # Use the nonce-matched event's stable platform member OpenID,
                # never a nickname, mention target, or READY bot identity.
                member = payload["d"]["author"].get("member_openid")
                if not isinstance(member, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,256}", member):
                    raise ProbeError("登记消息缺少有效的成员 OpenID；未授予管理员权限。")
                registration_callback(match[0], member)
                return
            if registration_path is not None:
                register_group(registration_path, match[0])
                print("测试群已登记到仓库外私有配置。登记连接已退出，未发送消息或调用模型。", flush=True)
                return
            print("已收到本次测试口令，正在发送一条固定回复。", flush=True)
            # One send attempt only; no retry on ambiguous results or duplicate events.
            try:
                await send_reply(credentials, token, *match)
            except asyncio.CancelledError:
                raise ProbeError("群消息发送被中断，结果未知；请检查 QQ，程序不会自动重发。") from None
            print("QQ 已确认发送成功。请检查群中的回复；测试已退出。", flush=True)
            return


async def run_probe(credentials, seconds, registration_path=None, registration_nonce=None,
                    registration_callback=None, status_callback=None):
    try:
        from websockets.asyncio.client import connect
        from websockets.exceptions import WebSocketException
    except ImportError:
        raise ProbeError("请先在隔离环境安装 requirements-qq-smoke.txt 中的依赖。") from None

    class OfficialGatewayConnect(connect):
        # Do not follow a redirect and send Identify credentials to another host.
        def process_redirect(self, error):
            return error

    token_result = await asyncio.to_thread(
        http_json,
        f"{API_BASE}/app/getAppAccessToken",
        body={"appId": credentials["QQ_APP_ID"], "clientSecret": credentials["QQ_APP_SECRET"]},
        stage="QQ 登录",
    )
    token = token_result.get("access_token")
    if not isinstance(token, str) or not token:
        raise ProbeError("QQ 登录未返回 AccessToken，请检查 AppID 和 AppSecret。")
    (status_callback or (lambda text: print(text, flush=True)))("QQ 凭据验证成功。正在连接消息网关。")
    gateway = await asyncio.to_thread(
        http_json, f"{API_BASE}/gateway", token=token,
        app_id=credentials["QQ_APP_ID"], stage="QQ 网关获取",
    )
    url = validate_gateway(gateway.get("url"))
    prefix = "/clawmingle-register" if registration_path is not None or registration_callback is not None else "/clawmingle-ping"
    command = f"{prefix} {registration_nonce or secrets.token_hex(16)}"
    try:
        async with asyncio.timeout(seconds):
            async with OfficialGatewayConnect(url, open_timeout=15, close_timeout=3, max_size=1048576) as websocket:
                await listen(websocket, credentials, token, command, registration_path,
                             registration_callback, status_callback)
    except TimeoutError:
        raise ProbeError("等待测试消息超时，连接已关闭。") from None
    except (WebSocketException, OSError):
        raise ProbeError("QQ WebSocket 连接中断或被拒绝；未显示连接详情。") from None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    parser.add_argument("--seconds", type=int, default=300, choices=range(30, 601), metavar="30..600")
    parser.add_argument("--check-config", action="store_true", help="Only check local configuration; no network")
    parser.add_argument("--register-group", type=Path, metavar="ABSOLUTE_CONFIG", help="Register one nonce-matched @ group in a private social.toml outside Git, without replying")
    parser.add_argument("--registration-nonce", help="Continue the current registration nonce during its original validity window")
    args = parser.parse_args()
    if args.check_config and args.register_group is not None:
        parser.error("--check-config and --register-group cannot be combined")
    if args.registration_nonce is not None and (
        args.register_group is None or not re.fullmatch(r"[0-9a-f]{32}", args.registration_nonce)
    ):
        parser.error("--registration-nonce requires --register-group and a 32-character lowercase hex value")
    # Suppress third-party debug output, including frame contents and headers.
    logging.disable(logging.CRITICAL)
    try:
        credentials = load_credentials(args.config)
        if args.check_config:
            print("QQ 配置已填写，格式与权限检查通过。未发起网络请求。")
        else:
            if args.register_group is not None:
                _, config = registration_config(args.register_group)
                if config["allowed_groups"] not in ([], ["YOUR_GROUP_OPENID"]):
                    raise ProbeError("配置已有群白名单，无需重新登记；未启动连接。")
            asyncio.run(run_probe(credentials, args.seconds, args.register_group, args.registration_nonce))
    except ProbeError as error:
        print(str(error), flush=True)
        return 1
    except (OSError, UnicodeError, ValueError):
        print("QQ 配置或连接数据无法读取；未显示敏感内容。", flush=True)
        return 1
    except KeyboardInterrupt:
        print("测试已手动停止。", flush=True)
        return 130
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
