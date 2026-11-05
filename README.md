# CyIME Wayclip

**CyIME Wayclip 是 CyIME 的 Linux 桌面剪贴板同步服务。**

它运行在 Linux Wayland 用户会话中，与 Android 端 CyIME 配对后，实现 Linux 与手机之间的文本剪贴板双向同步。本项目不是输入法，也不是通用的剪贴板管理器；核心目标是为 CyIME 提供可靠、安全、可持久运行的 Linux 服务端。

## 项目关系

- Linux 服务端（本项目）：https://github.com/lingdiansr/cyime-wayclip
- Android 客户端（CyIME）：https://github.com/Cyletix/CyIME
- Android APK：https://github.com/Cyletix/CyIME/releases

## 当前能力

- 原生 Wayland 桌面剪贴板读写：常驻 ext/wlr data-control 后端
- Linux 与 Android 文本剪贴板双向同步
- Android v2 HTTPS/二维码配对
- 证书指纹固定和 Bearer Token 认证
- ETag 轮询和剪贴板内容去重
- `stamp` 防止旧内容覆盖新内容
- TLS 身份、服务端身份和配置持久化
- systemd 用户服务模式
- Linux amd64 发布包（后续扩展 aarch64）、Debian/RPM 打包元数据

当前版本只支持**文本**剪贴板，不支持图片或其它二进制数据。v2 主动推送、附近配对与显式授权的多设备组网见文末说明；不提供 Linux 扫描设备或图形管理界面。

当前实现状态与剩余工作见 [实施路线图](implementation-plan.md)。原生 Wayland 后端已替换旧的外部工具路径，
提供 source 生命周期、有界 I/O、generation 条件写入、seat 选择与断连恢复。
源码实现、自动化测试和跨桌面/多设备验收分别记录，不将“可以连接”等同于全部路线图完成。

## 环境要求

- Linux
- Wayland 合成器，并启用可用的 clipboard data-control 协议
- Android 9（API 28）及以上的 CyIME
- Linux 与 Android 位于同一可信局域网/Wi-Fi

本项目不提供 X11 回退，也不建议将服务暴露到公网。

### 原生 Wayland 后端

剪贴板连接、offer 和 source 由进程内的单一 worker 持有；优先使用 `ext-data-control-v1`，
不可用时使用 `wlr-data-control-unstable-v1`。无需安装或调用 `wl-copy` / `wl-paste`。
剪贴板变化通过协议事件唤醒同步；每次 HTTP 读取使用内存快照，不重新连接 compositor。
读写和 FD 传输有时限与容量上限，桌面不可用、传输超时或 selection 已变化时返回错误，不发送旧缓存冒充当前内容。

默认只在唯一 seat 时自动选择。多个 seat 时先运行 `cyime-wayclip doctor` 查看名称，再显式指定：

```sh
CYIME_WAYCLIP_SEAT=seat0 cyime-wayclip serve-v2
```

systemd 用户服务可在 unit 的 `[Service]` 中设置 `Environment=CYIME_WAYCLIP_SEAT=seat0`。
该环境变量在后端启动时读取，修改后需要重启。没有目标 seat 或 data-control 协议时明确报告不可用，不切换到 X11。
连接断开、选中 seat 消失或 device 失效后，后端清除旧快照，按 1/2/5/10/30 秒上限加小幅抖动重试；
恢复后只观察当前 selection，不重放断连前的写入。服务也可以在 compositor 尚未就绪时启动并等待恢复。


## 安装 Linux 服务端

### 发布包

从 [CyIME Wayclip Releases](https://github.com/lingdiansr/cyime-wayclip/releases) 下载 Linux amd64 tarball：

```sh
tar -xzf cyime-wayclip_0.2.0_amd64.tar.gz
install -Dm755 cyime-wayclip_0.2.0_amd64/usr/bin/cyime-wayclip ~/.local/bin/cyime-wayclip
```

发布资产统一采用 `cyime-wayclip_版本号_架构.打包格式`：
amd64 和 aarch64 使用相同的包名、版本号和架构字段；`tar.gz`、`deb` 和 `rpm`
三种格式也保持一致。

### 从源码构建

```sh
git clone https://github.com/lingdiansr/cyime-wayclip.git
cd cyime-wayclip
cargo build --release --all-features
install -Dm755 target/release/cyime-wayclip ~/.local/bin/cyime-wayclip
```

Debian 和 RPM 构建元数据分别位于 `packaging/debian/` 和 `packaging/rpm.spec`。

## 初始化配置和用户服务

初始化配置文件：

```sh
cyime-wayclip config init
cyime-wayclip doctor
```

安装并启动 systemd 用户服务：

```sh
cyime-wayclip service install
cyime-wayclip service status
```

服务运行在当前登录的图形用户会话中，而不是 system service。卸载：

```sh
cyime-wayclip service uninstall
```

如果发行版没有运行 user systemd manager，也可以直接运行 `serve-v2`；但必须保持进程运行。

## 与 Android CyIME 配对

启动 Linux v2 配对服务：

```sh
cyime-wayclip serve-v2
```

支持图片协议的交互终端会直接显示黑白二维码图片，自动选择 Kitty graphics、iTerm2 inline images 或 Sixel。
图片在内存中生成，不写入临时文件；使用整数倍像素缩放，并保留四模块宽的白色静区，避免终端配色影响扫码。

无法确认图片支持时，自动回退为 Unicode 半块字符（`▀`、`▄`、`█`）；未知终端的能力探测最多等待 150 毫秒。
重定向到文件、管道、日志以及 tmux/Screen 会话时保守使用字符输出，不发送图片控制序列。
字符模式请使用等宽字体；两种模式都应完整显示二维码及周围留白。

然后在 Android CyIME 中：

1. 安装并启用 CyIME 输入法；
2. 打开剪贴板同步设置；
3. 选择扫描二维码添加电脑；
4. 扫描 Linux 终端二维码；
5. 确认连接后测试双向复制。

测试顺序：

1. 在 Linux 上复制文本，确认 Android 剪贴板出现；
2. 在 Android 上复制文本，确认 Linux Wayland 剪贴板出现。

默认监听 `0.0.0.0:9876`。如使用 Android 热点，可显式绑定 Linux 热点地址：

```sh
cyime-wayclip serve-v2 10.42.0.1:9876
```

二维码中的地址必须是 Android 可访问的 Linux 局域网地址。配对 offer 一次性有效且会过期；失败时重新启动服务并扫描新二维码。

## 数据和身份持久化

- 配置：`${XDG_CONFIG_HOME:-~/.config}/cyime-wayclip/config.toml`
- TLS 证书和私钥：`${XDG_STATE_HOME:-~/.local/state}/cyime-wayclip/`
- 服务端身份：配置中的 `server_id`
- Unix 系统上的 TLS 私钥权限为 `0600`

这些文件决定已配对服务的身份。迁移或重装时请一并备份，并当作敏感数据保护。

## 故障排查

```sh
cyime-wayclip doctor
cyime-wayclip service status
journalctl --user -u cyime-wayclip.service -e
ss -ltn | grep 9876
```

常见问题：

- **二维码过期或配对失败**：配对 offer 已消费或过期，重新启动 `serve-v2`；
- **连接超时**：检查手机和 Linux 是否在同一 Wi-Fi、二维码地址是否正确，以及防火墙是否允许 TCP 9876；
- **已连接但不同步**：连接检查成功不代表上传已成功。运行 `cyime-wayclip doctor` 查看原生后端状态、协议、seat 和诊断，再用 `cyime-wayclip -v serve-v2` 查看错误；确认以 `--all-features` 构建并运行在正确的 Wayland 用户会话中。无需安装外部剪贴板工具。上传格式错误返回 400、超限返回 413、桌面不可用或条件写入失败返回 503，不会伪报成功；
- **Wayland 暂不可用**：检查 `WAYLAND_DISPLAY`、`XDG_RUNTIME_DIR` 和 `CYIME_WAYCLIP_SEAT`。原生后端会重连；systemd 本身的启动错误仍应检查 `journalctl --user`；

### Android 上传格式兼容

`PUT /api/clipboard` 接受当前 Android 的文本 profile（可省略 `v`，
`stamp` 为非负 64 位有符号整数），同时保留 `v=2`、
`stamp={"device_id":"…","counter":1}` 的对象格式兼容。
显式提供 `v` 时必须为 `2`。服务保留发送方的原始字段，
不会将手机的时间戳当成逻辑计数器。文本仍需通过 SHA-256、UTF-8 字节数和大小限制校验；
新内容只有桌面写入成功后才返回 200 和对应的 ETag；重复或过旧的有效内容可幂等确认，旧内容不会覆盖新内容。

## 开发与验证

```sh
cargo fmt --all -- --check
cargo test --all-features
cargo clippy --all-features --all-targets -- -D warnings
```

GitHub Actions 会在 push 和 pull request 时执行检查；推送 `v*` 标签会自动构建 Linux amd64 发布包。

## 版本与许可证

当前版本：`0.2.0`。

本项目与 Android 端 CyIME 一致，采用 GNU General Public License v3.0 或更高版本（GPL-3.0-or-later）。详见 [LICENSE](LICENSE) 和 [CONTRIBUTING.md](CONTRIBUTING.md)。

## Verbosity

Pass `-v`, `-vv`, or `-vvv` before the command to increase diagnostics. `-vvv`
enables per-request server logging; verbosity is process-local and does not log
clipboard contents or credentials. This replaces any need for `RUST_LOG`.

## Local CI checks

Run the same checks as CI before pushing:

```sh
cargo fmt --all -- --check
cargo test --all-features
cargo clippy --all-features --all-targets -- -D warnings
```

### Sensitive content logging

Clipboard text is never logged by default. `--show-content` (alias
`--log-content`) opts into sensitive clipboard diagnostics for local debugging;
use only on a trusted machine and disable it afterward.


## v2 配对、设备管理与同步行为

### 凭据持久化和撤销

v2 使用 HTTPS、证书 SHA-256 指纹固定和每设备 Bearer Token。二维码 offer 一次性消费；
已配对凭据持久化，正常重启无需重新扫码。设备登记仅保存入站 token 的摘要，
出站回调 token 则作为敏感凭据保存在本机，默认不写入日志。

```sh
cyime-wayclip devices list
cyime-wayclip devices revoke DEVICE_ID
```

`DEVICE_ID` 取自列表首列，列表不显示 token。撤销同时移除该授权派生的子 grant 和
介绍的回调；运行中的服务会重载授权并取消相关队列和在途请求。
同一客户端重新配对会轮换旧 token，并撤销旧的派生授权及回调。
**使用旧版固定令牌的客户端，升级后需要重新配对。**

状态目录为 `${XDG_STATE_HOME:-~/.local/state}/cyime-wayclip/`，包括 TLS 身份、
`devices.json`、`peers.json` 和 `clock.json` 等。敏感文件使用 `0600`，私有目录使用
`0700`；不要放宽权限、共享给其他用户或通过符号链接替换这些状态路径。
配置中的 `server_id` 和 TLS 私钥共同确定服务身份；`server_id` 在运行期间固定，
手动更换后需重启并重新配对，不应作为热切换选项。

### 附近配对：手机发现，Linux 本地批准

Linux 通过 mDNS 广播服务器，Android 负责发现并发起附近配对。Linux **不主动扫描**
其它设备，也没有批准 GUI。手机发起请求后，在 Linux 终端查看并与手机逐字核对比较码：

```sh
cyime-wayclip nearby list
cyime-wayclip nearby approve REQUEST_ID
# 不认识设备或比较码不一致时：
cyime-wayclip nearby reject REQUEST_ID
```

`REQUEST_ID` 取自列表首列，列表同时显示比较码、设备名和来源地址。
未在本地批准时，接口只返回待批准状态（202），不会签发 token；批准后由手机再次请求完成配对。
不要只凭设备名称批准。

`nearby_enabled` 默认开启，可关闭附近配对和 mDNS 广播：

```sh
cyime-wayclip config set nearby_enabled false
```

附近配对入口会随配置更新；**mDNS 广播开关变更需要重启服务**。

### 显式启用自动组网

`allow_introductions` 默认是 `false`，`/api/grant`、`/api/introduce` 默认拒绝。
仅在信任已配对成员、希望它们互相加入同一文本同步组时开启：

```sh
cyime-wayclip config set allow_introductions true
```

启用后，服务会为已配对并登记回调的成员自动执行 grant/introduce，并向被介绍设备
反向注册回调，使成员无需逐对扫码即可互认；对端也必须支持并允许相应协议。
这不是无认证的 trust 接口，不会替换已信任设备的证书 pin。
撤销介绍者会一并撤销由其引入的授权与回调。

所有出站连接均使用真实 HTTPS，精确验证证书指纹和 TLS 握手签名。
回调登记使用实际请求源 IP 和声明端口，而非信任正文中的地址；介绍地址仅接受
字面量局域网 IP（私有、链路本地地址，另允许本机 loopback）及端口，拒绝公网、DNS、
用户信息和 URL 路径，不跟随重定向。不要将服务公开到互联网。

### 暂停、推送和失败回退

```sh
cyime-wayclip config set send_enabled false
cyime-wayclip config set receive_enabled false
# 恢复对应方向时将 false 改为 true
```

发送暂停时，`GET /api/clipboard` 实际返回 403；接收暂停时，
`PUT /api/clipboard` 实际返回 403，响应携带 `sync_paused` 和两个方向的状态，
不是只改变界面提示。发送暂停会丢弃待推内容并取消在途请求，恢复不会重放已取消内容；
若反向互认注册在暂停时被取消，需要重新介绍设备。
配置通常热载，解析或读取失败时会安全暂停两个同步方向，而不是继续使用旧授权方向。

Linux 通过原生 Wayland selection 事件监控桌面变化，并以至多约 500ms 的维护间隔检查方向配置和授权撤销；
推送只发送每设备最新待推内容，不会为每份内容无限增建线程或积累完整历史。对端可在推送不可用时继续使用
ETag 轮询回退；这不表示 Linux 在主动轮询其它设备。临时出站失败按
30/120/600 秒退避（后续保持 600 秒），新内容覆盖旧待推内容；
400/401/403/413/415 属于永久失败，不自动重试。每设备的反向注册与剪贴板推送有独立队列，
注册优先，成功后才继续相应的剪贴板推送。

### 顺序持久化与机器时间

`clock.json` 仅保存内容 hash、毫秒 `stamp` 和旧格式的设备计数器（legacy），
**不保存剪贴板正文**。重启时，若当前 selection 的 hash 与已知记录一致，可以恢复顺序；
未知的启动前旧 selection 不会直接对外发送，需要复制新的文本触发同步，避免旧内容覆盖新内容。

数字时间戳超过 Linux 当前时间 **5 分钟**的请求返回 400。
请保持 Linux 和手机时钟正确（建议启用自动校时）；这不是通过任意放宽时间戳校验解决的连接问题。

### 验证范围

此前已在 Android 实机验证的是上传兼容性修复。本节新增的持久化、附近配对、组网、
暂停及回调机制，验证依据是 Rust 测试和本机真实 TLS/HTTP 集成测试，
**尚未完成 Windows 与手机的全量互操作实测**。请勿据此假定已有图片同步、Linux 主动扫描或 GUI 支持。

原生 Wayland 后端另外通过隔离 compositor 的真实协议/FD 测试覆盖 ext/wlr 选择、seat、大小/编码边界、
慢 owner/consumer、generation 冲突、设备失效与重连。已在 niri 26.04 的独立嵌套 Wayland 会话完成读写冒烟：
多个原生客户端交替持有 source、重复粘贴、UTF-8/emoji/CRLF，以及 65536 字节背压传输；
同一隔离会话还验证了生产 HTTPS 入口的 pin、配对 201、PUT 200、独立原生客户端读取，
以及 GET 200 的正文、SHA-256 和 ETag 一致性。
测试使用 `PATH=/nonexistent`，未调用外部剪贴板工具，也未写入用户当前桌面的剪贴板。
KWin/Sway/Hyprland 和多设备真实验收仍需按路线图逐项记录。
