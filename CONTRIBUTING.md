# 贡献指南

感谢参与 CyIME Wayclip。项目目标是为 CyIME 提供 Linux Wayland 剪贴板同步服务。

## 开发环境

- Linux + Wayland
- Rust stable，版本由 `rust-toolchain.toml` 固定
- 构建 Wayland 功能：`cargo test --all-features`

## 提交前检查

```sh
cargo fmt --all -- --check
cargo test --all-features
cargo clippy --all-features --all-targets -- -D warnings
```

协议、认证、剪贴板 profile 和持久化行为的修改应补充测试。提交信息使用简洁的命令式描述；每个独立逻辑变更保持为一个 commit。

## Pull Request

请说明变更目的、测试命令和已知限制。涉及 Android 互操作时，请同时说明对应 CyIME 客户端版本和复现步骤。不要提交密钥、TLS 私钥、真实配对 token 或个人日志。

## 许可证

本项目与 Android 端 CyIME 一致，采用 GNU General Public License v3.0 或更高版本（GPL-3.0-or-later）。提交代码即表示你同意在该许可证下发布贡献。
