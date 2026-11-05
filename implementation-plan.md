# CyIME Wayclip 实施路线图

本文是当前实现与后续工作的对照，不再沿用早期“尚未实现 TLS / v2 仅能配对”的说明。
实现、自动化验证和真实设备验收分别记录；用户已确认当前版本可以连接，不能据此推断全部故障场景通过。

## 1. 产品边界

- Linux Wayland 用户会话中的 CyIME 文本剪贴板同步服务，不是输入法或通用剪贴板管理器。
- 产品主路径为 Android 原生 v2：HTTPS、叶证书 SHA-256 指纹固定、Bearer Token。
- 有效 UTF-8 纯文本，单条最多 65536 字节；拒绝 NUL、纯空白、格式错误和超限网络正文，不截断后接受。
- 不做 X11 回退、PRIMARY、图片/HTML/文件/历史批量同步、云中继、公网穿透、多 seat 自动合并、剪贴板正文持久化。
- KDE/KWin、Sway、Hyprland 等按真实 registry 与访问策略判断，不按桌面品牌猜测支持。无 data-control 的 GNOME 会话不承诺后台访问。
- 运行于用户图形会话，不是 root/system service；不承诺未登录时读取桌面。
- Secret Service、D-Bus/托盘 GUI 属于可选扩展，不是 v2 MVP 的前置条件。

## 2. 当前功能状态

| 阶段 | 当前实现 | 尚需完成 |
| --- | --- | --- |
| A：协议与能力验证 | 文本 profile、HTTP 状态、QR、status、stamp 与 Android 报文兼容；用户已确认可连接 | 完整目标 APK / Windows 互操作矩阵与跨桌面验收 |
| B：Wayland provider | 已切换为常驻原生 ext/wlr 后端，完成有界 FD 传输、进程内 source 生命周期、generation/session、seat 选择及断连恢复；同步层保留去重、暂停和时钟语义 | KWin/Sway/Hyprland 的真实环境矩阵；不将 niri 与隔离协议测试泛化为所有桌面通过 |
| C：旧 Lua v1 PoC（可跳过） | 有显式 serve-v1 与内存 profile HTTP 路径 | 尚未接入真实桌面；认证/ETag/allow-insecure-v1 契约未闭环，不宣称完整旧插件互通 |
| D：v2 安全同步 | HTTPS/pinning、扫码、身份持久化、令牌轮换/撤销、文本同步与时钟持久化已接通 | 扫码配对失败限速、secret 恒定时间比较、后台配对输出隔离、多网卡二维码端点保留 |
| E：推送/发现/组网/交付 | peer 回调、最新内容队列、退避重试、nearby 本地批准、grant/introduce、mDNS 广播、用户服务和打包元数据已有实现 | Linux 平台发现身份、正式包构建/升级验收、图形会话生命周期、多设备真实验收 |

“已有实现”不等同于全部真机验收通过。附近配对有自己的限速；不能据此称扫码 /api/pair 也有限速。

### 已实现的同步约束

- v2 GET 200/204/304、ETag、方向暂停 403、桌面不可用 503；网络超限 413、错误格式 400。
- 写桌面失败不伪报成功，不推进 profile/ETag；同 hash 不重写，旧 stamp 不覆盖新内容。
- Android 数字 stamp 为毫秒时间戳；兼容旧对象 stamp。超过本机时间五分钟的数字时间戳拒绝。
- clock.json 仅保存 hash、stamp、旧计数器，不保存正文；未知启动前旧 selection 不当成新复制外发。
- TLS 身份、server_id、设备授权和回调持久化；私密状态目录 0700、文件 0600。
- 出站 HTTPS 固定证书指纹并验证握手签名；回调地址由真实请求源 IP 与端口构成，限制局域网字面量地址，不跟随重定向。
- 推送只保留每设备最新待推内容；临时失败按 30/120/600 秒退避，400/401/403/413/415 不自动重试。
- 暂停会取消在途请求和待推内容；恢复不重放已取消的内容。反向注册与剪贴板推送分开排队。
- mDNS 当前仅广播，不主动扫描设备；为兼容 Android 仍使用 kind=windows，不能称为已完成 kind=linux 适配。
- 图片协议终端可显示内存生成的二维码图片；普通终端与重定向回退半块字符。此功能不代表图片剪贴板同步。

## 3. 本轮：原生 Wayland 后端

### 3.1 目标与非目标

结果：已移除 wl-copy 子进程及 wl-clipboard-rs 读路径，所有 selection 读写由本进程中的原生 Wayland worker 完成。
保留 v2 线协议、凭据、clock.json 与现有去重/方向语义；不借本轮修改配对协议、打包或旧 v1 功能。

### 3.2 架构与接口

- 一个 worker 独占 Connection、EventQueue、manager、seat、device、offer 和 source；其它线程不得 dispatch 这些对象。
- 完整 registry 初始化后优先 ext_data_control_manager_v1；没有 ext 时使用 zwlr_data_control_manager_v1。
- 只处理 Regular selection。只存在一个 seat 时自动选择；多个 seat 或指定名称不存在时明确不可用，不任意选择。
- 使用 CYIME_WAYCLIP_SEAT 指定 seat 名称；切换 seat 需要重启，不在运行中合并不同 seat 的内容。
- Snapshot 包含 generation 和共享的原始文本。读取从内存快照返回，不再每次 HTTP 请求创建连接或阻塞读取 compositor。
- 写请求通过有界队列进入 worker，携带预期 generation 与 deadline。worker 检查 selection 没有被新事件替换后再 set_selection。
- 写入在 compositor 的同步回调确认后完成，不等待其它应用粘贴。source 在 worker 中存活并可响应多次 send；取消/替换时及时释放。
- 同步监控由 selection 变化唤醒；500ms 仅保留配置/授权维护时限，不再每 500ms 发起一次系统剪贴板传输。
- 所有调用方切换到同一个 backend 实例；删除旧 provider 占位、独立读写函数与依赖，不保留兼容垫片。

### 3.3 有界 I/O 与一致性

- worker 使用 poll 同时处理 Wayland socket、命令唤醒和非阻塞传输 FD，不阻塞在某个剪贴板 owner 的管道上。
- 文本传输最多读取 65537 字节以判断超限；offer 读取、source 发送、初始化握手和写请求均有 deadline。
- 同时在途 FD 数与写队列容量有上限，繁忙时返回错误，不能无限创建线程或积累内容。
- 每次 selection 替换使 generation 递增；旧 offer 的迟到数据不得覆盖新 selection。
- 无文本 MIME / null selection 为无内容；超时、非法 UTF-8、断连等为不可用，不使用旧正文冒充当前系统内容。
- 写入遇到 generation 不匹配或连接失效时失败，HTTP 不推进时钟/ETag。Wayland 无跨进程原子 CAS，仍明确承认最后检查之后可能发生外部竞争。
- source 提供 text/plain;charset=utf-8、text/plain、UTF8_STRING 等已验证的文本 MIME；保持 UTF-8、emoji、CRLF 原样，不做 trim 或 normalization。

### 3.4 生命周期

Starting → Ready / Unsupported / Disconnected → Reconnecting → Ready。

- 初始化失败、socket 断开、设备 finished、选中 seat/manager 消失时清除快照和所有旧 offer/source/FD；请求返回不可用，不回填旧正文。
- 按 1/2/5/10/30 秒上限退避重连，并加入小幅抖动；等待期间仍能停止 worker 和拒绝过期命令。
- 重连后重新枚举协议与 seat，只读取当前 compositor 的 selection，不重放断连前的写入。
- 最后一个 backend handle 释放时唤醒并停止 worker，关闭连接与传输；不残留 wl-copy 或后台子进程。
- run 不再因为启动时 compositor 暂不可用而正常退出；后台服务可等待恢复。doctor 报告真实 worker 状态、协议、seat 与诊断原因。

### 3.5 文件分工和实施顺序

1. 更新本文与 README，记录状态、边界和验收标准。
2. src/clipboard/mod.rs：保留状态类型，定义带 generation 的快照，删除未接通的旧 trait/占位实现。
3. src/clipboard/wayland.rs 及其内部协议模块：连接 owner、ext/wlr 调度、FD 传输、source、seat、重连与有界命令。
4. src/sync_runtime.rs：快照 generation、条件写入、事件唤醒；迁移相应测试，不改变 v2 时钟和暂停契约。
5. src/http_server.rs、src/main.rs：共享 backend、服务启动与 doctor；移除旧独立读写路径。
6. Cargo.toml/Cargo.lock：移除 wl-clipboard-rs，启用直接 wlr bindings；测试需要的 server bindings 只作为开发依赖。
7. 补充真正传递 Wayland 请求、事件和 FD 的隔离协议测试，再做真实 compositor 冒烟，最后回填已完成状态。

### 3.6 验收

- [x] ext 优先、wlr 独立回退，无协议与多个 seat 明确报不可用；显式 seat 选择有效。
- [x] 真实请求/事件/FD 测试覆盖远端 offer 读取、原生 source 写入和重复粘贴，UTF-8/emoji/CRLF 精确一致。
- [x] 65536/65537 字节、非法 UTF-8、无文本 MIME、null selection 的结果正确。
- [x] 慢 owner / 停止读取的消费者不会卡死 worker；超时后新 selection 和新写入仍能工作。
- [x] offer 替换、source cancelled、generation 冲突、seat 消失和 socket 断开不会返回旧内容或接受过期写入。
- [x] 重连后读取新 selection，不重放旧写入；worker 退出释放 FD 与连接。
- [x] 同步层在条件写失败时不推进 clock/ETag；保留既有 HTTP、暂停和撤销测试。
- [x] 使用 PATH=/nonexistent 也可运行真实读写；原生后端没有旧工具依赖或占位入口。
- [x] niri 独立嵌套会话读写冒烟通过，当前桌面只读探测为 ext-data-control-v1 / seat0 / Ready；跨桌面验收另列，未修改用户桌面剪贴板。
- [x] cargo fmt --all -- --check、cargo test --all-features（147 项）、cargo test --no-default-features（131 项）、cargo clippy --all-features --all-targets -- -D warnings 通过。

## 4. 原生后端之外的待办

### 安全和运行时

- 入站 HTTP header/body、连接数/并发数的硬上限与超时；不能把出站 deadline 当成入站保护。
- 扫码 /api/pair 失败限速和 secret 恒定时间比较；二维码输出与 systemd/journal 日志隔离。
- 多网卡 QR 保留所有可达 endpoint，而非默认 compact 只保留第一个 IPv4 地址。
- 暂停恢复、授权文件读取失败与回调恢复的故障语义继续核验。
- status 当前只是 doctor 的别名，不是守护进程运行状态/队列面板。

### 发现与部署

- Android 端接受 kind=linux 并显示正确平台文案，之后移除 Linux 端的 windows 兼容广播；Android 改动独立提交。
- 修正 DEB payload/control 组装，RPM 构建失败必须失败而非忽略；统一许可证元数据。
- 完成用户服务图形会话环境、注销停止、安装失败返回值及升级保留身份的验收。
- 旧 Lua v1：选择真正接通桌面并完善契约，或正式从产品范围移除；本轮不扩大到该分支。

## 5. 验证证据与后续矩阵

### 已有依据

- 用户已确认二维码显示与当前版本连接可用；此前 Android 实机验证过上传格式兼容修复。
- 现有 Rust 测试涉及 profile/HTTP、设备持久化与撤销、nearby、推送、loopback TLS 和 mesh。
- 网络集成测试和静态代码不代表 Windows/Android 全量互操作已通过。
- 原生后端包含 16 项隔离 Wayland 协议测试及 4 项同步层 generation/session 回归测试。隔离 compositor 使用真实 Unix socket、Wayland 请求/事件和 SCM_RIGHTS 文件描述符，而不是模拟返回文本的空实现。
- 真实冒烟环境：niri 26.04（v26.04-610-g8ebd6466）独立嵌套会话，三个以上原生客户端交替持有 selection、反复读取同一 source、UTF-8/emoji/CRLF 与 65536 字节背压传输均通过。
- 同一隔离会话运行生产 serve_v2_tls 入口并使用临时身份/授权目录：校验证书 pin 后扫码配对 201、文本 PUT 200、独立原生客户端读取完全一致，随后 GET 200 的正文、SHA-256 和 ETag 均一致。
- 冒烟时 PATH=/nonexistent，未调用外部剪贴板工具；测试拒绝连接用户父会话，退出后关闭嵌套 compositor，未改写用户当前桌面剪贴板。

### 仍需真实验收

- 目标 APK：双向文本、收起/显示键盘、后台/恢复、断网重连、热点切换和时钟差。
- Linux/Android/第三设备：不回环、推送与轮询切换、撤销及时生效、暂停恢复。
- Wayland：KWin/Sway/Hyprland 的具体版本与 registry、重复粘贴、source 保持、seat 变化、compositor 重启。
- 发布：DEB/RPM/tarball 安装、卸载、升级，TLS 身份、server_id 与已配对授权保持不变。

验证记录必须注明实际环境与执行范围；未执行的验收不勾选，不用单测数量替代真实设备证据。
