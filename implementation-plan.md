# CyIME Wayclip 实施方案与插件复用评估

## 1. 目标与决策

用户目标：在 `~/code/Rust` 建立独立 Rust 项目，规划 Linux 剪贴板同步后端；已明确放弃 X11；评估 Android 现有 Win 剪贴板同步插件能否复用。

项目名：**CyIME Wayclip**，目录/二进制名 `cyime-wayclip`。不是 Linux 输入法移植，不涉及 Rime/IBus。

依据：本地 `/home/wks/code/Other/CyIME`，检查时 HEAD `ea6cd05c66793a371cc3cbfde337f8bbc1b88965`。源码能力不等于已发布/已安装 APK 能力。本阶段只写方案，不修改 Android、不打包、不使用 ADB。

决策：

1. 建独立 Rust 用户态 daemon，分离 Wayland provider、同步状态机和兼容 HTTP/TLS 服务端。
2. **现有 Lua Win v1 插件可不改协议代码连接 Linux**；名称写 Windows 不限制 HTTP 对端平台。
3. **v2 加密扫码是 Kotlin 原生路径，不是旧 Lua 插件能力**。产品应复用该原生客户端；不新造 Noise/QUIC/WebSocket 协议。
4. 原插件 v1 仅为可选互通 PoC，显式启用、默认关闭；安全产品目标是已有 v2 HTTPS + 叶证书指纹固定 + Bearer。
5. 不把 Windows 类全面重命名为 Desktop 作为前置工作。先验证线协议，必要的 Linux 发现/UI 补丁独立处理。
6. Wayland 同时评估 `ext-data-control-v1` 与 `wlr-data-control-unstable-v1`，按真实 compositor 能力绑定；不只凭桌面名称判断支持。

## 2. 明确边界

首版仅同步有效 UTF-8 纯文本，单条≤65536 字节。不做 PRIMARY、图片/HTML/文件/历史批量同步、云中继、公网穿透、GNOME Shell 扩展、多 seat 自动合并、剪贴板正文持久化。

- 没有 X11/XWayland 专用后端，也不以临时焦点窗口 hack 回退。
- 不能声称“禁止 XWayland 应用同步”：若 compositor 将应用剪贴板桥接到 Wayland selection，它仍可能互通。
- KDE/KWin、Sway、Hyprland 是候选验收环境，实际支持取决于版本、registry global 和访问策略，不承诺某品牌全版本可用。
- 默认 GNOME 不承诺后台监听；无可用协议就明确不支持。
- 没有可假定全桌面通用的任意后台剪贴板 portal；GTK/Qt、wl-clipboard 都不能绕过 compositor 权限。
- 运行于用户图形会话，不是 root/system service，不承诺未登录也能取桌面剪贴板。

## 3. Android 复用证据

以下均为已检查源码的完整路径：

| 路径 | 已核实能力及意义 |
|---|---|
| `/home/wks/code/Other/CyIME/plugins/cyime-windows-sync/main.lua` | HTTP/HTTPS origin、Basic、GET/PUT `/api/clipboard`、Profile SHA-256/size、ETag；无 Windows 平台校验 |
| `/home/wks/code/Other/CyIME/plugins/cyime-windows-sync/manifest.yaml` | ID `com.cyime.plugin.windows_clipboard`，协议 `cyime-windows-v1`，前台轮询 2000 ms，max_text_bytes=65536，允许自定义 host |
| `/home/wks/code/Other/CyIME/plugin-core/src/main/java/com/kingzcheung/xime/plugin/core/api/ClipboardSyncPlugin.kt` | ClipboardProfile、push/pull/acknowledgePull/testConnection；复用客户端和契约，不是 Rust 直接链接 Kotlin |
| `/home/wks/code/Other/CyIME/plugin-core/src/main/java/com/kingzcheung/xime/plugin/core/sync/ClipboardSyncSession.kt` | 串行同步、重试、写回检查、回声抑制；不重复塞进 Lua |
| `/home/wks/code/Other/CyIME/app/src/main/java/com/kingzcheung/xime/clipboard/sync/ClipboardSyncBridge.kt` | 系统事件、敏感过滤、可见性/轮询调度；原生 mesh 有额外路径 |
| `/home/wks/code/Other/CyIME/app/src/main/java/com/kingzcheung/xime/clipboard/sync/WindowsSecureTransport.kt` | v2 HTTPS、固定叶证书 SHA-256、Bearer、status/clipboard/peer、多设备请求 |
| `/home/wks/code/Other/CyIME/app/src/main/java/com/kingzcheung/xime/clipboard/sync/WindowsPairingProtocol.kt` | QR/origin/token/profile/stamp/注册校验，是互通的关键解析器 |
| `/home/wks/code/Other/CyIME/app/src/main/java/com/kingzcheung/xime/clipboard/sync/WindowsDevices.kt` | QR 配对、反向注册、mDNS；发现仅接受 windows/android kind；QR 不需要依赖发现 |
| `/home/wks/code/Other/CyIME/app/src/main/java/com/kingzcheung/xime/clipboard/sync/WindowsNearbyPairing.kt` | nearby proof、120 秒确认窗口；201 仅 ready，用户确认后才 commit |
| `/home/wks/code/Other/CyIME/app/src/main/java/com/kingzcheung/xime/clipboard/sync/DeviceHost.kt` | Android 自身提供 pair/status/clipboard/peer/grant/introduce，是现成 v2 服务端参考 |
| `/home/wks/code/Other/CyIME/app/src/main/java/com/kingzcheung/xime/clipboard/sync/ClipboardClock.kt` | hash/stamp 去重；收到的内容不再次转发；旧 stamp 不覆盖 |

### 3.1 旧 Lua v1：可直接复用

服务端满足以下条件，Android 用户即可选择现有 Windows 同步插件，填写 Linux IP origin/配对码，无需新建 Linux 插件：

- `Authorization: Basic base64("cyime:" + pairingCode)`；码长 16～128，不含空白/控制字符，建议 32 随机字节 base64url 无填充，不能用六位数字替代。
- serverUrl 仅 origin，不含路径/query/userinfo，不填写 `/api/clipboard`。
- GET/PUT 均只访问 `/api/clipboard`；连接测试也使用 GET，不需要 v1 status/pair API。
- 单次 timeout=3000 ms；PUT 不能等用户未来粘贴才回复。
- ETag 只在宿主实际接受内容后由 acknowledgePull 推进；GET 成功不是手机写入成功。
- 默认只同步文本≤64 KiB；HTTP body 还受 262144 字节限制。
- 空/图片 GET=204，不要求手机清空；304 表示无变化。
- 手机键盘显示时约 2 秒拉取，隐藏停止持续拉取；Linux 常驻不能解除 Android 生命周期限制。
- v1 不提供跨设备时间顺序，不强制客户端未发送的 If-Match，不宣称可自动判断哪台历史内容更新。

限制：HTTP Basic 明文不保密。Lua 接受 HTTPS URL 不代表支持自签名指纹扫码：其 host.http 的信任行为需另核实，不能关闭证书校验绕过问题。复用不等于安全产品已完成。

### 3.2 原生 Kotlin v2：推荐产品路线

Linux 实现已有协议，Android 走现有扫码入口。QR 路径有无需 Android 协议修改的条件，但必须拿真实 APK 联调，不能先宣布兼容通过。

关键兼容条件：

- origin 为 HTTPS + 数字 IP + 显式端口，限 loopback/private/link-local/IPv6 ULA；不放 DNS/public origin，例如主机名.local 不符合当前解析器。
- pin 是叶证书 DER SHA-256 小写 hex，不是 SPKI；还检查证书有效期。
- token/offer secret 为32随机字节 base64url 无填充，匹配 `[A-Za-z0-9_-]{43}`。
- 优先 JSON QR：`cyime://pair?v=2&data=<base64url(JSON)>`；字段 v=2、server_id、name、certificate_sha256、pairing_id、secret、endpoints、expires_at。
- UUID/端点校验按原解析器；端点1～8个；JSON≤4096字节、QR字符串≤8192字符；compact c= 编码可后做。
- `/api/pair` POST 返回201，包含 v=2、server_id、server_name、device_id、access_token。offer 短期单次消费，重新配对轮换 token；失效/错误/限流明确响应。
- `/api/status` 的 push、push_pin、push_port、push_failed 影响轮询与注册；没有实测主动推送时 push=false，不虚报能力。
- `/api/peer` 注册手机动态 TLS 端口、pin、token。回调以连接远端 IP + 注册端口为基础，不接受任意 origin 引发 SSRF。
- `/api/grant`、`/api/introduce` 关系到多设备引介；未实现不能宣称完整 mesh 等价。
- `/api/pair/nearby` 是附近连接码确认，不是扫码 pair 的别名；未实现附近功能不阻塞扫码 MVP，但不能声称支持该功能。
- mDNS `_cyime-clip._tcp.` 当前会过滤 kind=linux。首版只扫码，不伪装 Windows；后续给 Android 独立增加 linux kind 和文案。

| 目标 | Android 改动 | 判断 |
|---|---|---|
| 原 Lua v1 手动文本互通 | 原则无 | 可做最早互通验证 |
| 原生 v2 QR + 前台同步 | 原则无协议修改，待实测 | 推荐安全 MVP |
| Lua 原文件实现 v2 pin/QR/mesh | 需要新桥接或重做 | 不推荐，已有 Kotlin 路径 |
| mDNS 自动列出 Linux | 需最小 kind/UI 补丁 | 后续功能 |
| 主动推送/多设备 mesh | 尽量复用原生端 | Linux 实现更多接口，逐项验证 |
| 全面 Windows→Desktop 重命名 | 可选重构 | 不应是本项目互通前提 |

## 4. Rust/Wayland 技术设计

### 能力与数据通道

1. 连接 Wayland，枚举 registry、manager、seat、协议版本；环境变量仅用于诊断。
2. 优先可用 ext-data-control，次选 wlr-data-control；阶段 A 验证 Rust bindings/目标 compositor 后定案。同 seat 只启用一个 provider。
3. 首版单 seat；不明确时要求选择，不混合多个剪贴板。
4. offer MIME 优先 text/plain;charset=utf-8，其次经验证的文本变体。无文本不是清空远端的命令。
5. pipe/FD 有界异步读取；边读边限制65536字节，并设超时、并发及 FD 数上限。
6. 严格 UTF-8，拒绝 NUL/空白/超限；不 trim、不改换行、不做 Unicode normalization，避免哈希变化。
7. 写入创建 data source、set_selection、持续响应 send FD，保持 source 存活；不等待用户 paste 才回复网络 ACK。
8. 每个 selection 事件带 generation；写回前检查更新；用 origin+hash 抑制自身回声，不粗暴“忽略下一条事件”。
9. 处理 cancelled、null selection、失效 offer、seat 消失、断连重连。缓存不能在失效后继续冒充当前系统内容。

Wayland 没有跨进程剪贴板原子 CAS，最后检查和 set_selection 仍有竞态，不能宣称完全消除。协议也无法统一提供 Linux 敏感标记；密码过滤只能尽力而为并明确限制。

状态：Starting→Ready/Unsupported→Disconnected→Reconnecting→Ready。退避建议1/2/5/10/30秒加抖动。不可用时 authenticated clipboard 请求返回503，不发旧正文；恢复后读取真实 selection，不回填断连前旧文本。

### 模块布局（规划，尚未创建源码）

```text
src/main.rs                    CLI/runtime
src/config.rs                  XDG路径、开关、权限
src/clipboard/{mod,wayland,ext,wlr}.rs
src/protocol/{profile,v1,v2,pairing,peers}.rs
src/sync.rs                    来源、generation、stamp、方向
src/server.rs                  鉴权/TLS/HTTP限流
src/state.rs                   身份/授权元数据，不存正文历史
tests/                         协议/竞态/golden/真实桌面
packaging/                     systemd用户服务
```

候选依赖：tokio、axum、rustls、rcgen、serde/serde_json、sha2、base64、uuid、wayland-client、wayland-protocols、wayland-protocols-wlr、必要时 wayland-scanner；后续 reqwest、二维码输出、tracing。实现前核实版本/API/MSRV、ext/wlr bindings、XML来源/许可证，锁 Cargo.lock；当前未声称已验证某版本。

Wayland 专用 event loop 作为单一 owner，Tokio 网络通过有界 command/event channel 交互；不让网络任务持有 Wayland 对象，不在 mutex 内等待网络/FD，不无限积压请求。

不以 GTK/Qt/arboard 作为已解决后台访问的假设。wl-clipboard 可做人工诊断/验收工具，不是正式核心依赖或权限兜底。

## 5. 线协议和状态一致性

Profile：type=text，text=原始文本，hash=小写SHA256(UTF8)，size=UTF8字节数，has_data=false，data_name=null，source=CyIME-Wayclip；v2另携带stamp。v1 source长度≤128按原 Lua 校验兼容。

| 请求/条件 | 响应原则 |
|---|---|
| GET 有效文本 | 200 Profile + `ETag: "<hash>"` |
| If-None-Match 命中 | 304，无正文 |
| 无有效文本 | 204，不清空手机 |
| v1 PUT 已接受写入 | 200，返回相同hash的Profile，不能提前确认排队成功 |
| 鉴权失败/方向禁用 | 401/403 |
| 超限/格式或类型错误 | 413、400/415，按冻结契约 |
| 不可用/暂时忙 | 503 |
| 并发冲突 | 按v1/v2真实语义，不统一滥发409 |

JSON body上限262144字节；即使文本≤65536，转义后body仍可能超限，拒绝而非截断。hash/size 使用真正UTF-8，不能用CESU-8。

方向以Linux为参照：send禁用则GET不得给正文；receive禁用则拒绝PUT。v2暂停结构包含v=2、sync_paused、send_enabled、receive_enabled，与客户端解析一致。

- v1使用本地generation检测竞争，无法建立全设备总序。
- v2按现有ClipboardClock，保留收到的stamp、同hash不重写、旧stamp不覆盖，remote内容不再fan-out；不自行更换为新设备ID/Lamport线协议。
- 当前stamp不是严格分布式总序：同stamp不按设备ID破平局，墙钟可能偏差；必须保留兼容并记录边界。
- Android DeviceHost对stale消息可200回包并不实际写系统；分清accepted/applied/ignored-stale，不能将所有200都描述为“系统现值就是来包”。
- v2无stamp旧端按现有回退行为处理，先用fixtures冻结。
- provider失败不得先推进profile/ETag；用户后来再次复制和之前网络ACK是不同事件。

## 6. 安全、配置与部署

- 默认v2；v1须显式allow-insecure-v1并警示可信LAN风险，不自动降级。
- 初始不无提示监听全部接口；loopback/未开放为默认，用户选择LAN地址再开配对。
- TLS身份稳定保存，升级不换pin；重置/过期更换证书须重新配对。
- 配对窗口建议120秒、一次使用、失败限速/次数；token hash常量时间比较；撤销立即生效。
- 拒绝无鉴权读写；body/连接/并发/超时全有限；不开放CORS，不使用cookie，不把Basic当网页登录。
- QR含secret；凭据/正文/QR不进入journal或普通日志。显式本地pair输出与日志分离。
- 不持久化正文。缓存过期清理不同于主动清空系统剪贴板，后者默认不做，避免清掉用户新内容。
- 提供暂停/只发/只收/设备撤销；不能保证识别所有密码内容。
- 配置建议`$XDG_CONFIG_HOME/cyime-wayclip/config.toml`，身份/授权`$XDG_STATE_HOME/cyime-wayclip/`，临时IPC`$XDG_RUNTIME_DIR/cyime-wayclip/`；目录0700、私密文件0600、原子替换。
- Secret Service可选；无钥匙环时明确选择受权限保护文件或不保存，不能宣称所有Linux必有libsecret。
- `systemd --user`绑定图形会话，正确导入WAYLAND_DISPLAY/XDG_RUNTIME_DIR；不借linger宣称未登录同步。会话退出关闭服务。
- 沙箱候选NoNewPrivileges、受控state/runtime写入、AF_UNIX/AF_INET/AF_INET6限制；实测不破坏Wayland socket/TLS/Secret Service，不照抄即算通过。
- 首版CLI规划doctor/run/pair/status/devices list/revoke；完整DBus/托盘UI不作为MVP前提。

## 7. 阶段与验收

工期是熟悉Rust/Wayland单人、测试桌面和手机可用情况下的人日估算，不是承诺。

### A：协议冻结和能力验证，2～3人日

核实目标桌面版本/registry、bindings；冻结Profile/HTTP/QR/status fixtures；核对v2原客户端push=false轮询路径。查阅完整Windows服务端/规范（当前未提供，不当作已读）。

门槛：真实Wayland读写/粘贴成功，unsupported可检测；契约有真实解析器依据。

### B：Wayland provider + 状态机，3～5人日

初始化Cargo并锁依赖；实现一个可用adapter、FD限流、source生命周期、generation/来源、断连恢复。另一个adapter按目标桌面分开排期。

门槛：中文/emoji/CRLF精确保留；65536/65537边界；重复粘贴仍有数据；断连不发旧内容；没有X11回退。

### C：旧Lua插件互通PoC，2～3人日，可跳过

显式v1服务，原Lua+宿主harness互通，再真实手机填写LinuxIP。不要改插件掩盖服务端错误。

门槛：原插件双向文本、ETag/ACK/重试/方向和空内容行为正确，无回声风暴；明确隐藏键盘不持续拉取。HTTP不能当安全发布。

### D：v2安全扫码MVP，4～7人日

稳定身份、QR offer、pair/token撤销轮换、status/v2clipboard/stamp。先保守push=false，验证原生QR+轮询；若真实调度需要额外接口，按证据补齐，不关掉客户端安全校验绕过。

门槛：实际目标APK扫码成功；pin错误/过期offer/token撤销失败；不启用v1也能同步；手机生命周期边界准确。

### E：主动推送/发现/打包，4～7人日，独立范围

peer注册及固定pin回调，失败退避、push状态真实；grant/introduce mesh与nearby按需逐项实现；Android linux kind/UI最小补丁单独提交；user service/升级保留身份；第二桌面回归。

门槛：三设备不回环，断网重试有界，撤销立即生效，推送与轮询切换正确，打包升级实际验证。

A+B+C约7～11人日；A+B+D约9～15人日；加跨桌面和E约3～5周量级。bindings/协议差异/桌面权限/设备可用性均会扩大估算。

## 8. 测试资产与验收矩阵

1. 单元：精确UTF-8 hash/size，补充平面emoji、CRLF、不规范化Unicode、空白/NUL/非法UTF-8、文本/body双上限、同/旧stamp。
2. HTTP契约：200/204/304、hash确认、鉴权、方向、畸形/截断JSON、慢请求、超限、503/409真实条件。
3. 竞态：GET等待中新复制、写入失败不推进ETag、自己selection回声、外部抢占、重复请求、断连重连。
4. 安全：QR/私网origin、pin改变/证书过期、offer超时/重放、token轮换撤销、回调SSRF、日志脱敏。
5. 真实Wayland：registry能力、offer取消、FD泄漏/阻塞、连续粘贴、source保持、seat选择、compositor重启。
6. 真机：显示/收起键盘、重连、双向同时复制、2～3设备不回环；真机/用户验收单独记，不能由单测替代。

可复用Android测试思路/断言：

- `/home/wks/code/Other/CyIME/plugin-core/src/test/java/com/kingzcheung/xime/plugin/core/lua/LuaWindowsClipboardSyncTest.kt`
- `/home/wks/code/Other/CyIME/plugin-core/src/test/java/com/kingzcheung/xime/plugin/core/lua/WindowsClipboardInteropTest.kt`
- `/home/wks/code/Other/CyIME/plugin-core/src/test/java/com/kingzcheung/xime/plugin/core/sync/ClipboardSyncSessionTest.kt`

`/home/wks/code/Other/CyIME/tools/clipboard-sync-fixture/README.md`的fixture编译外部Windows C#服务端，不是已经可直接切换Rust的全套测试。需要另加可配置origin/Rust测试宿主；DLL缺失导致skipped不能算通过。

本方案阶段上述测试均未执行，仅完成源码检查和文档校验。

## 9. 开工前未决项

- 首个验收桌面/版本、ext/wlr实际能力和Rust crate版本/MSRV。
- 目标安装APK是否含本快照v2原生扫码；旧插件README只谈v1，不足以否定新原生代码，也不能用新代码推断已安装能力。
- 完整Windows规范/服务端与Android DeviceHost差异，尤其status/push/stale语义。
- QR+push=false的真实轮询调度，是否可直接跳过v1阶段。
- 依赖与引用源码/XML许可证。复用协议/测试不等于无条件复制Android/Windows源码。
- 目标目录当前只读：创建`/home/wks/code/Rust/cyime-wayclip`失败（只读文件系统）；当前文档暂存工作区，未修改挂载或权限。

最终建议：**先锁定Wayland provider与既有wire contract，再用原客户端互通；产品默认v2，v1只是可跳过验证路径。重点复用现有Android协议客户端，而不是再写一个Linux同步插件。**

## 当前实现运行说明

`cyime-wayclip serve` 当前启动 v1 HTTP listener；默认仅绑定 `127.0.0.1:8765`，尚未实现 TLS。Android Lua v1 插件配置时使用 server origin（例如 `http://192.168.1.10:8765`），插件会访问 `/api/clipboard`，认证需按协议使用 Basic `cyime:<pairing-code>`。公网或不可信 LAN 不应启用；v2 HTTPS/pinning 仍待实现。

Serve LAN example: `cyime-wayclip serve 192.168.1.10:8765`. Only bind a trusted private interface. Pairing codes must be transported out-of-band; v1 Basic authentication is plaintext on HTTP and is not suitable for hostile networks.

### v2 HTTPS pairing (experimental)

Build with `cargo run --features wayland -- serve-v2 192.168.1.10:9876`. The command prints a `cyime://pair?v=2&data=...` URI. Android must validate the HTTPS endpoint and pin the printed certificate DER SHA-256 fingerprint; do not disable certificate verification. The current listener implements pairing only; clipboard v2 sync remains incomplete.

`serve-v2` requires an explicit reachable LAN address, e.g. `cargo run --features wayland -- serve-v2 192.168.1.10:9876`; it rejects 0.0.0.0, 127.0.0.1, and localhost because Android would resolve those to itself. Bind the listener to the LAN interface and scan the printed QR on the same network.
