# CyIME Wayclip

**CyIME Wayclip 是 CyIME 的 Linux 桌面剪贴板同步服务。**

它运行在 Linux Wayland 用户会话中，与 Android 端 CyIME 配对后，实现 Linux 与手机之间的文本剪贴板双向同步。本项目不是输入法，也不是通用的剪贴板管理器；核心目标是为 CyIME 提供可靠、安全、可持久运行的 Linux 服务端。

## 项目关系

- Linux 服务端（本项目）：https://github.com/lingdiansr/cyime-wayclip
- Android 客户端（CyIME）：https://github.com/Cyletix/CyIME
- Android APK：https://github.com/Cyletix/CyIME/releases

## 当前能力

- Wayland 桌面剪贴板读取和写入
- Linux 与 Android 文本剪贴板双向同步
- Android v2 HTTPS/二维码配对
- 证书指纹固定和 Bearer Token 认证
- ETag 轮询和剪贴板内容去重
- `stamp` 防止旧内容覆盖新内容
- TLS 身份、服务端身份和配置持久化
- systemd 用户服务模式
- Linux x86_64 发布包、Debian/RPM 打包元数据

当前版本主要支持**文本**剪贴板。二进制数据、主动推送和完整多设备 mesh 仍在后续计划中。

## 环境要求

- Linux
- Wayland 合成器，并启用可用的 clipboard data-control 协议
- Android 9（API 28）及以上的 CyIME
- Linux 与 Android 位于同一可信局域网/Wi-Fi

本项目不提供 X11 回退，也不建议将服务暴露到公网。

## 安装 Linux 服务端

### 发布包

从 [CyIME Wayclip Releases](https://github.com/lingdiansr/cyime-wayclip/releases) 下载 Linux x86_64 tarball：

```sh
tar -xzf cyime-wayclip-0.1.0-x86_64-unknown-linux-gnu.tar.gz
install -Dm755 cyime-wayclip-*/usr/bin/cyime-wayclip ~/.local/bin/cyime-wayclip
```

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

终端会输出二维码。然后在 Android CyIME 中：

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
- **已连接但不同步**：确认 Linux 在 Wayland 会话中，运行 `doctor` 检查 clipboard data-control 能力；
- **服务启动后立即退出**：查看 `journalctl --user`，确认 user systemd、Wayland 环境变量和权限可用。

## 开发与验证

```sh
cargo fmt --all -- --check
cargo test --all-features
cargo clippy --all-features --all-targets -- -D warnings
```

GitHub Actions 会在 push 和 pull request 时执行检查；推送 `v*` 标签会自动构建 Linux x86_64 发布包。

## 版本与许可证

当前版本：`0.1.0`。

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
