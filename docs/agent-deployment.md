# Agent 部署手册

本手册面向在用户指定 Linux 机器上操作的编程 Agent，目标是把 ClawMingle 安装成可验证、可维护的服务。用户通过本机或 SSH 隧道中的网页填写凭据、登记群和完成模型登录。用户说明见[部署入口](deploy.md)。

## 执行边界与完成条件

先读该机器及仓库的有效指导、检查 Git 状态和现有安装。明确用户授权的是准备、安装、上线还是更新；普通代码审查不授权启动服务。部署本项目不需要修改防火墙、SSH、VPN、已有网站或开放公网端口。管理服务只监听 `127.0.0.1`。

本仓库的产品入口是 `clawmingle`，不是默认 `cargo run` 的上游 `codex-claw`。不要套用[上游存档](upstream/README.md)中的个人认证与会话复制步骤。不要读取、复制或输出用户的个人 Codex auth、聊天历史或网站凭据；本产品使用独立机器人运行目录。

新安装完成的证据包括：

- 程序、辅助文件、页面、登记依赖与用户服务安装完整；安装版本可追溯。
- 管理台从本机或 SSH 隧道可访问，首次密码设定、登录、退出及访问保护有效。
- 用户完成 QQ 凭据、测试群登记和独立模型认证；本地配置检查通过。
- 在用户已授权启动的情况下，机器人实际运行，并由用户在测试群确认一条明确 @ 的回复。
- 配置和运行数据留在 Git 外；交付服务管理、备份及回退位置。

如果缺少用户凭据或账号授权，完成不依赖它们的安装步骤，并准确标记“等待网页配置／登录”；不要把安装成功写成机器人已经接通。

## 1. 检查机器与已有安装

使用普通部署用户；不要把 root 的认证或目录作为机器人的运行资料。检查以下项目，发现冲突先查明来源，不覆盖现有文件：

| 项目 | 要求与检查 |
| --- | --- |
| 操作系统 | Linux；运行环境支持 x86_64／aarch64，前提是对应依赖可用。 |
| 编译工具 | Rust/Cargo 支持 edition 2024 和锁定依赖，另有 C 编译器、链接器和 Git。SQLite 由 Cargo 的 bundled 依赖编译。 |
| Python | Python 3.11+，具备 `venv`／pip；后端使用标准库，群登记通过私有 venv 安装 `scripts/requirements-qq-smoke.txt`。 |
| Codex | 已安装可信的原生 Codex；可发现官方 npm 安装内的原生程序。不要把任意脚本当作原生程序。 |
| 隔离 | 可用的非特权 Bubblewrap。缺失时通过该机器正常的软件安装流程处理，不关闭隔离。 |
| 服务管理 | 可用的 systemd 用户管理器；后端的服务控制依赖它。 |
| 已有状态 | 核对 `~/.config/clawmingle/`、`~/.local/share/clawmingle/`、`~/.local/lib/clawmingle/` 和两个用户服务。不要输出秘密。 |
| 磁盘和网络 | 构建空间充足，可访问 Cargo/Python 包源、QQ 和所选模型服务。浏览器另有下节条件。 |

可先用只读命令查看工具版本、用户服务状态和目录是否存在。若 `cargo` 不在 PATH，查项目声明的环境，而不是立即重装；仓库提供 [environment.dev.yml](../environment.dev.yml)。使用已有隔离环境时让构建和安装器运行在同一环境下。

原生执行的记录验证基线为 Codex `0.161.0`，这不是所有后续版本的兼容承诺。缺少 Codex 时按[官方 CLI 安装说明](https://developers.openai.com/codex/cli)选择适合目标机器的安装方法；版本与模型目录需要在该部署中核对。初始准备不要求启动真实模型对话。

### 浏览器条件

启用浏览器且未指定已有运行环境时，机器人启动会自动在自己的私有目录准备固定版本 Node、Playwright 和 Chromium。它不会安装系统包。准备失败会阻止本次机器人启动，因此基础聊天部署应先检查设置，不要无意触发下载。

- 下载需可访问官方 Node、npm 和 Chromium 源。
- Linux 系统需具备 Chromium 要求的共享库；安装器列出缺失项后，由机器正常的包管理流程补齐。
- 自动准备要求保留 `max(15 GiB, 文件系统容量的 20%)` 空间，另加 `1 GiB` 安装余量。
- 显式关闭浏览器不会触发这项下载；也可以使用已配置、经过检查的独立运行环境。

完整规则见[自动准备说明](native-runtime.md)。不要升级或改变与本项目无关的系统服务来绕过缺失依赖。

## 2. 克隆、预检和安装

在用户指定的源码根目录克隆仓库；已有仓库先检查工作区，不丢弃本地更改。进入仓库后运行安装器的只读计划：

```sh
python3 -B scripts/install_clawmingle.py --plan
```

默认无参数也是计划模式：不构建、不写用户安装目录、不下载依赖、不启动服务。检查计划中的目标、依赖与阻塞原因。用户已经授权新部署且依赖齐全时：

```sh
python3 -B scripts/install_clawmingle.py --apply --build --start-admin
```

`--build` 在当前工具链中执行 `cargo build --bins --locked`，限制构建并行度；默认产物来自 `target/debug`（设置了 `CARGO_TARGET_DIR` 时使用对应目录）。编译不是安全检查的替代；修改源码的部署先按[开发约定](development.md)完成相应检查。

若已构建 release，可用明确路径安装；不要将 `--build` 与 `--binaries-dir` 同用：

```sh
cargo build --release --locked --bin clawmingle --bin clawmingle-hook-proxy --bin clawmingle-hook-input
python3 -B scripts/install_clawmingle.py --apply --binaries-dir "$PWD/target/release" --start-admin
```

安装器写入以下约定位置：

| 位置 | 内容 |
| --- | --- |
| `~/.local/lib/clawmingle/` | `clawmingle`、`clawmingle-hook-proxy`、`clawmingle-hook-input` 三个程序。 |
| `~/.local/lib/clawmingle/admin/` | 后端、认证与初始化辅助模块、能力目录与 QQ 登记脚本、QQ 文档登录辅助脚本。 |
| `~/.local/lib/clawmingle/admin/assets/` | 管理、登录与首次设置页面。 |
| `~/.local/lib/clawmingle/admin-venv/` | 私有 Python 环境及群登记依赖。 |
| `~/.config/systemd/user/` | `clawmingle.service`、`clawmingle-admin.service` 用户服务。 |

安装器不调用 sudo、不安装系统软件、不调整网络规则，也不会启动机器人。相同安装可重复检查；现有未知文件或不同版本的冲突会停止，不能借安装步骤覆盖既有部署。首次配置需要新的数据目录，因此不要提前把 venv 或下载文件放进 `~/.local/share/clawmingle/`。

`--start-admin` 只启动管理台；需要随用户服务启动时可在已授权范围内加 `--enable-admin`。两个参数均不启动机器人。安装器为服务设置经过筛选的程序路径，不复制整个 shell 环境；服务不能依赖未声明的交互式 shell 激活步骤。

用户服务模板也可检查：[机器人](../config/clawmingle.service.example)、[管理台](../config/clawmingle-admin.service.example)。已有反向代理部署保持原认证方式，不能用独立模板覆盖它；见[代理接入](admin.md#可选鉴权代理)。

## 3. 将初始授权交给用户

新管理台使用 `--standalone --init`。它生成一次性配对码，保存在 `~/.config/clawmingle/admin-bootstrap.json`；服务日志只显示文件位置。配置目录和文件分别限制为 `0700`／`0600`。**Agent 不读取或在报告中转发配对码、用户密码及账号凭据。**

本机用户打开 `http://127.0.0.1:17430/`。远程主机由用户用自己的 SSH 访问建立隧道，例如：

```sh
ssh -N -L 17430:127.0.0.1:17430 YOUR_USER@YOUR_HOST
```

`YOUR_USER`／`YOUR_HOST` 是用户自己的连接信息，不保存到仓库。隧道在用户电脑执行；页面仍打开 `http://127.0.0.1:17430/`。若该端口已占用，选择一致的管理服务与本地转发端口，并重新核对服务和浏览器访问地址。

用户在自己的终端读取配对文件，输入网页并创建管理密码。首次注册后只存带盐密码哈希，配对文件删除；`--init` 留在服务参数中不会重新开放注册。

随后用户在首次设置页填写 QQ 凭据、可选 Jev Key、模型设置，并通过以下方式之一登记：

- 点击 QQ 登记，在有效期内于目标群真正 @ 机器人并发送当前一次性口令；登记者的成员 OpenID 成为首位群命令管理员。
- 已知准确平台 OpenID 时，手动填写群 OpenID 与管理员成员 OpenID。

普通 QQ 群号、昵称或仅在文本中输入机器人名字不等于上述身份和真实 @。登记只连接 QQ 验证本次口令，不发回复、不调用模型，不会自动开始群聊。完成后进入正常管理页。

保存路径默认是 `~/.config/clawmingle/social.toml`，运行资料默认是 `~/.local/share/clawmingle/`。初始化拒绝覆盖已有配置或数据；出现冲突应调查已有安装，不删除目录重试。

## 4. 独立模型登录和配置检查

让用户在管理台发起机器人 Codex 设备登录，并在官方页面完成授权。后台通过安装的 `clawmingle codex-binary` 查找经过验证的原生程序，使用独立登录目录，不导入个人历史。登录码和原始输出不要写入共享日志。

使用其他模型服务时，由用户配置独立 Provider 和 API 凭据；需兼容 Codex 要求的 Responses 协议。不要将任意 Chat Completions 地址认定为兼容。账号、模型和额度的真实可用性要单独验证。

在启动前检查配置：

```sh
"$HOME/.local/lib/clawmingle/clawmingle" check --config "$HOME/.config/clawmingle/social.toml"
```

`check` 只检查本地配置，不能证明 QQ／Jev／模型服务已经可用。确认默认模式、私聊范围、记忆自动整理、表情收集、浏览器与工具授权符合部署意图；Jev 尚未配置时保留 `mode-1`。不要为了让状态显示成功而跳过认证或放宽权限。

## 5. 启动并验收

在用户已授权启动、完成配置与登录后，显式启动机器人：

```sh
systemctl --user start clawmingle.service
systemctl --user is-active clawmingle.service clawmingle-admin.service
```

如需自动启动，在相应授权范围内执行：

```sh
systemctl --user enable clawmingle.service clawmingle-admin.service
```

用户服务是否在用户退出后保持运行取决于该机器的用户服务策略。需要修改 linger 等主机设置时按本机管理规则处理；安装器不会自动修改。不要把 `enable` 当作机器重启后的实际验证。

验收至少区分三个层次：

1. **安装**：三个程序能执行，Python 模块与页面齐全，群登记依赖可导入；安装内容与所选版本一致。
2. **管理**：本机／隧道页面可用；匿名访问业务 API 被拒绝；密码登录、退出和 CSRF 防护有效；管理台在机器人停止时仍可用。测试不得输出会话 Cookie、配对码或凭据。
3. **真实连接**：用户在登记群明确 @ 并提一个简短问题，确认回复；按配置需要再验收 Jev、图片或浏览器。群登记成功和模拟测试均不能替代这一步。

如果当前任务只授权准备，不执行第 3 层。交付中列出实际完成与未验证项目，说明源代码版本、实际运行文件、服务状态及配置／数据位置。

## 更新与回退

安装器首版处理新安装和相同安装的重复核对，遇到已有不同文件会拒绝覆盖，不是自动升级器。更新既有部署应由 Agent 单独处理：

1. 核对工作区、源版本、服务正在执行的程序、安装清单和已有用户服务；确认此次授权包含停机或重启。
2. 完成变更对应的离线检查，准备同一套版本的三个程序、后台模块和页面，不混合新旧文件。
3. 等待业务空闲，保存受限的配置／数据备份和旧安装文件。机器人保持运行时不要直接复制可能不一致的数据库；使用项目备份流程或停止后快照。
4. 停止受影响服务后更换安装文件，保留原认证方式、路径、聊天、记忆和工作区；记录确切文件清单。管理台和机器人可以分别更新，但互相依赖的接口必须一起验证。
5. 恢复原来的启用与运行状态，重复安装、管理和真实连接检查。进程路径及文件校验值应与本次安装相符。

回退时停止受影响服务，恢复保存的程序与配置，并按旧版本的数据兼容范围处理；不要默认用旧数据库覆盖上线后产生的新聊天。没有验证数据格式兼容时，保留新数据，说明恢复方案后再操作。

项目数据备份不等于完整机器备份，配置和认证文件需要单独受限保留。任何备份都不能进 Git 或公开日志。排障从[常见问题](deploy.md#常见问题)、[运行环境](native-runtime.md)和[管理说明](admin.md)继续。
