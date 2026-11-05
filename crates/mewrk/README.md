# Mewrk 原生版

[Mewrk 1.0](https://github.com/catblob-hash/Mewrk-previous)（已废弃的 Tauri 版）的纯 Rust 重写，界面和功能与原版对齐，样式保持 Fluent。在 macOS 和 Windows 上运行过。

## 运行

```bash
./scripts/bundle-macos.sh
```

第一次构建会下载 CEF（`cef_binary_154.0.23…_macosarm64_minimal.tar.bz2`，来自 cef-builds.spotifycdn.com）到 `.cef`，并用 cmake 和 ninja 编译它的 wrapper。不想要 Chromium 时，宿主关掉 `chromium` 特性就回到 WKWebView，什么都不用下载。

```bash
open target/release/Mewrk.app
```

Windows 上，在 Git Bash 里运行 `./scripts/package-windows.sh` 生成安装包，或者 `cargo run -p mewrk` 直接运行。不需要下载 Chromium：预览面板用系统的 WebView2 运行时。

第一次打开后，到 **设置 → 模型供应商** 添加一个供应商。数据放在 `~/Library/Application Support/dev.mewrk.native`（Windows 上是 `%APPDATA%\dev.mewrk.native`），格式和原版相同（`document.v1.json` 加 `conversations.v1.sqlite3`）。

调试时用 `MEWRK_DATA_DIR` 把数据放到别处。`--headless --shot x.png` 不开窗口渲染一帧，`--scene NAME` 选一个界面场景，场景列表见 `src/scenes.rs`，`--right-click XxY` 在截图前按一次右键。`MEWRK_TRACE=1` 把每一帧的开销打到 stderr，`MEWRK_ANIMATE=1` 让窗口不停地画，`--fullscreen-test` 每 4 秒切一次全屏。

## 结构

原版是 Tauri 应用：宿主用 Rust，界面用 React，模型层用 Node sidecar，浏览器用 CEF。这里的做法：

| 原版 | 这里 | 位置 |
|---|---|---|
| `src-tauri` | 几乎原样搬过来，所以持久化、上下文、设置、工具、MCP、hooks、LSP、workflow、SSH 都是原版代码 | `crates/mewrk-host/host` |
| Tauri | 窄接口的替身，宿主按命令名加 JSON 调用（共 162 个命令） | `crates/mewrk-host/tauri-shim` |
| Node sidecar（AI SDK） | 进程内的 Rust 实现，覆盖全部 11 种协议族 | `crates/mewrk-host/aisdk-rs` |
| CEF | 同样是 CEF（Chromium 154），页面是窗口的子视图 | `crates/mewrk-host/host/src/cef_host` |
| 远程屏幕的 HTML 查看页 | 原生视图，像素直接进 GPU 纹理，没有页面也没有 Chromium | `crates/panes/browser/src/screen_view.rs` |
| React 渲染 | 每帧一棵元素树，taffy 排版，wgpu 绘制 | `crates/fluent/src/el.rs` |
| `src/lib` | 应用状态和运行控制 | `crates/mewrk-client` |
| `src/components` | 按界面分 crate | `crates/panes/*`、`crates/ui/*`、`crates/mewrk` |

移植约定见 [`crates/mewrk-client/PORTING.md`](../mewrk-client/PORTING.md)，各 crate 需要外壳接什么见 [`crates/mewrk-client/WIRING.md`](../mewrk-client/WIRING.md)。

## 和原版的差别

**做不到或没有做的：**

- **无障碍。** 元素树、键盘（`Tab`、回车、空格、对话框内循环、焦点框）和原版的标签都有了，但没有用真实的 VoiceOver 跑过。消息的标题、链接、列表、代码块和图片可以逐个走到；Git 差异里靠近视图的行可以读到；终端的字符格和原版一样不开放（原版的 xterm 没开 `screenReaderMode`），只有输入和状态。
- **拼写检查只在 macOS 上有。** 其他平台没有检查器，设置项没有效果。macOS 上用 `NSSpellChecker`：写完的词下面画波浪线，右键菜单最上面是建议、忽略和学习。
- **玻璃的小地方。** 模糊是烘好的一张小图，和原版一样，放在侧栏和面板下面。没有做的：表面左上角的那层高光；输入框上方芯片底下的玻璃；设置页主题卡片里的磨砂预览。
- **PDF 附件。** 用系统的 PDFKit 取文字，所以只有 macOS 能用，其他平台会拒收。
- **版本更新。** 这个版本从 1.0.0 开始有发布，但还没有自己的更新清单：GitHub 连不上时，退回的 `dl.mewrk.dev/latest.json` 写的是原版的 1.0.3，比它高，还指向原版的安装包。所以这一页把检查关了（`UpdatesPage::release_channel`），等有自己的清单再打开。这一页也没有原版的 AI SDK 组件卡片：模型层在进程内，没有要安装的组件。
- **公式。** 自写的 TeX 排版，不是 MathJax，字形也来自系统的数学字体。原版加载的 29 个宏包定义的 1564 个名字，`crates/ui/common/tests/suite/math_packages.rs` 逐条核对过都有含义，`\ce` 和 `\pu` 是 `mhchemParser` 的移植；`textmacros` 里的 `\href`、`\style`、`\class`、`\data`、`\cssId` 和原版一样标红。还差：`flalign`、`xalignat` 不撑满整列；`p{...}` 列和 `\parbox` 只占宽度，不会自动折行；`\hfill` 和 `\breakAlign` 不起作用；`\shortvdotswithin` 不收紧行距；`\Newextarrow` 的箭头不拉伸字形，只画一条线加箭头。

**Windows。** 窗口没有系统边框，顶栏就是标题栏，按钮是自己画的（和原版一样）；关闭窗口后留在通知区域，从图标菜单退出。和 macOS 比，下面这些还没有：

- **PDF。** 预览和附件都依赖 PDFKit：文件面板只显示文件卡片，附件会被拒收。
- **HTML 预览。** 不跑网页引擎，画的是原生的简化版。
- **音频预览的播放。** 能解码和显示波形，但没有播放控件。
- **动态背景。** 只支持图片，选择器里不列视频。解码视频用的是 AVFoundation。
- **HEIC、AVIF 图片。** 依赖系统的 ImageIO，这两种格式打不开。
- **Android 屏幕的 H.264 流。** 依赖 VideoToolbox，回退到 `screencap`。
- **拼写检查。** 见上文。

安装包没有 Authenticode 签名，第一次运行时 SmartScreen 会提示。

**Vertex。** 外部账号的 `certificate` 凭据来源不支持，它需要双向 TLS。其余凭据类型都支持，但只对过假服务器。

**Claude Agent。** 原版从 npm 下载 Claude Agent SDK 和它自带的 CLI，锁定版本。这里不下载任何东西，也不用 SDK：直接用本机的 `claude`（`PATH`、常见安装位置、Claude 桌面应用自带的那份，依次查找；`MEWRK_CLAUDE_BIN` 可以指定），用它的 stream-json 协议通信。设置页上只有查找结果和重新检测，没有安装和更新。没有锁定版本，所以新版 CLI 的行为变化会直接影响这里；`aisdk-rs/tests/claude_agent.rs` 用真实 CLI 对本地模拟服务器验证这些依赖，低于 2.1.284 的版本会被拒绝。

**故意不做的：**

- **自定义 CSS。** 这里的界面不是网页，CSS 没有东西可以作用，所以外观页没有这张卡片。文档里的 `customCss` 字段保留，原版写下的值读进来再存回去不变，除此之外没有地方读它。

**依赖：** 宿主目前还带着 C 代码（`rusqlite` 捆绑的 SQLite、`rquickjs`、`ring`），和 Fluent Agent 的“不编译 C”不同。之后再替换。

## 预览的性能和回收

**Chromium。** 打开预览面板不启动它：空白标签页不要页面，第一次有网址可去时引擎才启动。不在前台的页面先睡（`was_hidden` 加一次内存压力通知，渲染进程丢掉能重建的缓存），醒着的最多两个，睡着的最多留四个；睡满 10 分钟的页面关掉渲染进程，只留地址，回来时重新加载。窗口被遮挡或最小化时页面停止出帧，模型正在操作的页面除外。关掉了 BackForwardCache、备用渲染进程和 Chromium 自己的后台流量。

**手机和远程桌面。** 视图直接从会话拿像素，没人看时什么都不跑，所以设备不会白白编码：

| 屏幕 | 画面来源 | 备注 |
|---|---|---|
| VNC | 服务器推送的增量 | 只上传变化的矩形，4K 桌面上一个光标闪烁只有几百字节 |
| Android | `screenrecord` 的 H.264，VideoToolbox 硬解 | 手机上不用装任何东西。不可用时退回 `screencap` |
| iOS | WebDriverAgent 的 MJPEG（端口 9100） | 端口不可达时退回截图 |

HTML 查看页和它的 HTTP 服务还在，直接打开那个地址时仍然能用，只是预览面板不再走它。

## 验证情况

`cargo test --workspace` 跑真实的宿主、真实文件和进程、真实 Git 仓库和真实 PTY，用本地模拟服务器代替模型。跑测试时建议把 `HOME` 指到临时目录，`CARGO_HOME` 和 `RUSTUP_HOME` 指回真实位置，再加 `METADATA_SERVER_DETECTION=none` 和 `PYTHONUSERBASE`（`cef-dll-sys` 用到的 cmake 要它），这样测试碰不到本机钥匙串。

设了 `MEWRK_CLAUDE_EXECUTABLE`（或者 `PATH` 上有 `claude`）时，`aisdk-rs/tests/claude_agent.rs` 用真实的 Claude Code 跑一个脚本化的本地上游，临时 `HOME`，请求出不了本机。没有 CLI 时这些测试自己跳过。最近用 2.1.308 和 2.1.309 都全部通过。

`crates/mewrk/tests/suite/run.rs` 从头走完一次运行：界面上打字，经过宿主和 Rust 模型层，到 socket 上的模拟供应商，再回到时间线和磁盘。覆盖发送、工具调用、批准和拒绝、提问、停止、排队、供应商报错、重启恢复、分支、分叉和回退（真实文件）。

预览相关的验证：真实窗口里用真实 Chromium 154 打开过本地页面、在页面里打过字；5K 全屏 60 fps；退出 0.15 秒，没有残留进程。4K 假 VNC 服务器上的原生视图约 60 fps、每秒约 12 MB。Android 的流用本机编码器生成的真实 H.264 过了假 adb 服务器和真实解码器，iOS 的 MJPEG 解析过了单元测试。

`examples/mock_provider.rs` 提供同一个模拟供应商，可以接真实窗口。真实窗口里用真实系统事件走过：打字、回车发送、流式回复、点击批准（文件确实写到磁盘）、右键菜单、终端输入、侧栏拖动、窗口拖动、托盘、隐藏后重新打开、⌘Q。

真实窗口里用真实按键走过键盘操作：`Tab` 和 `Shift+Tab` 移动焦点框，悬停才显示的按钮对键盘也显示，回车打开菜单，`Esc` 关闭。无障碍树用 AppKit 从真实窗口读过，按钮、对话框、输入框和 CEF 页面都读得到，也按得动。

对照原版的机械检查：原版 2826 条界面文案，这里缺 60 条，都是已经不存在的东西：Claude Agent 的下载面板、AI SDK 更新卡片、自定义 CSS、浏览器预览专用的提示、Windows 标题栏按钮。前端调用的 161 个宿主命令只缺 3 个，就是 Claude Agent 的安装、更新和取消，换成了 `claude_agent_cli_status`。

**Windows（Windows 11，x64）。** 真实窗口里用真实鼠标和按键走过：窗口的最小化、最大化、关闭到通知区域和从图标菜单退出；重复启动时唤起已有窗口；模型工具（读、写，写入要批准）和窗口隐藏时跑完一轮；PowerShell 终端里输入中文、`Ctrl+C` 中断程序；系统文件夹选择框、新建项目、审阅面板看差异（含 CRLF 文件）、文件面板打开文件；预览面板打开本地页面、在页面里点击和输入、菜单盖在页面上、面板缩放和应用缩放时页面跟着走。`crates/panes/browser/examples/win_engine.rs` 单独走一遍 WebView2 引擎的每个调用，有截图为证。

Windows 上的 `cargo test --workspace`（不含没进应用的 `mewrk-core`、`chaos`、`stress`，以及测试只写了 Unix 版的 `mewrk-term`）：7482 个通过，27 个失败，74 个忽略。失败的都是测试本身写死了 Unix：硬编码 `zsh`、`/tmp`、把 `\\?\` 路径交给 Git Bash。

安装包在非管理员下装到临时目录、运行、卸载过，界面和语言看过；**以管理员身份实际装到 `Program Files`、写注册表和快捷方式、卸载旧版这一路没有跑过**。

**没有验证过的：**

- **真实模型。** 只对过本地模拟服务器和差分测试，没有连过真实服务。Claude Agent 用的是真实 CLI，但上游是本地的，没有用过真实登录。
- **真实屏幕阅读器。** 无障碍树只经 AppKit 读过，没有跑过 VoiceOver 会话。
- **真实输入法。** 组字只用脚本测过。
- **拼写检查的波浪线和右键菜单，还有烘好的玻璃。** 都只看过无窗口截图，没有在真实窗口里看过。截图和 `spell.rs` 的测试问的是真的 `NSSpellChecker`；菜单是脚本按的右键；忽略和学习只对假的检查器跑过，没有写过真实词典。
- **系统文件对话框和拖放。** 代码路径用脚本测过，没有经过真实系统界面。
- **SSH 和 WSL。** 没有连过真实机器。
- **Windows 上的高 DPI 和多显示器。** 只在 100% 缩放的单显示器上跑过。预览页面的位置在应用自身缩放下验证过，按系统 DPI 换算的路径是同一段代码，但没有实机验证。
- **Windows 上的真实输入法和屏幕阅读器。** 中文只用 `SendInput` 的 Unicode 字符输入过，没有经过输入法的组字；讲述人没有跑过。
- **Linux。** 没有运行过。
- **真实手机和真实 VNC 服务器。** 本机没有 adb、设备和 WebDriverAgent。Android 流、iOS 流和设备上的输入都没有连过真机。旋转手机时流会重开，这段逻辑也只靠推理。

## 许可证

GPL-3.0-or-later，和 Mewrk 相同，因为宿主、提示词文本和界面行为都来自 Mewrk。
