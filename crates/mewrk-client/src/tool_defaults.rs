//! English wording for the built-in tools (lib/toolDefaults.ts). The tables are upstream's, keyed by parameter name and by
//! the Chinese text the catalog ships.

use crate::model::{ResolvedLanguage, ToolDescriptor, ToolParameter};

/// Keyed by parameter name alone, as in the host's `english_parameter_label`, so `context`, shared by `agent_spawn` (a history
/// switch) and `grep` (a line count), gets the one word that fits both.
fn english_parameter_label(key: &str) -> Option<&'static str> {
    Some(match key {
        "agent_type" => "Named agent type",
        "accept" => "Accept",
        "action" => "Action",
        "actions" => "Actions",
        "activity" => "Activity",
        "after" => "Lines after",
        "app" => "App",
        "args" => "Arguments",
        "arguments" => "Launch arguments",
        "before" => "Lines before",
        "buffer" => "Log buffer",
        "button" => "Button",
        "case_sensitive" => "Case sensitive",
        "character" => "Character",
        "colorScheme" => "Color scheme",
        "command" => "Command",
        "contains" => "Contains text",
        "content" => "File content",
        "context" => "Context",
        "depth" => "Recursion depth",
        "description" => "Description",
        "doubleClick" => "Double-click",
        "downgrade" => "Allow downgrade",
        "effort" => "Effort level",
        "end_line" => "End line",
        "environment" => "Environment variables",
        "expression" => "Expression",
        "schema" => "Output schema",
        "filename" => "File name",
        "filter" => "Filter",
        "filePath" => "File path",
        "find" => "Find text",
        "glob" => "File glob",
        "grant_permissions" => "Grant all permissions",
        "height" => "Height",
        "image_id" => "Image number",
        "keep_data" => "Keep data",
        "label" => "Display name",
        "latitude" => "Latitude",
        "level" => "Level",
        "limit" => "Limit",
        "line" => "Line",
        "lines" => "Line limit",
        "longitude" => "Longitude",
        "max_results" => "Maximum results",
        "message" => "Message",
        "name" => "Name",
        "new_text" => "New text",
        "none" => "Empty argument",
        "offset" => "Offset",
        "old_text" => "Original text",
        "operation" => "Operation",
        "output_mode" => "Output mode",
        "package" => "Package",
        "path" => "Path",
        "pattern" => "Search pattern",
        "permission" => "Permission",
        "plan" => "Plan",
        "preset" => "Device preset",
        "prompt" => "Prompt",
        "prompt_text" => "Prompt text",
        "query" => "File name pattern",
        "questions" => "Questions",
        "replace" => "Replacement",
        "replace_all" => "Replace all",
        "replace_existing" => "Replace existing",
        "requestId" => "Request ID",
        "resume_run_id" => "Resume run ID",
        "restart" => "Restart",
        "run_in_background" => "Run in background",
        "script" => "JavaScript",
        "scale" => "Scale",
        "scope" => "Scope",
        "screenshot" => "Screenshot",
        "search" => "Text filter",
        "sections" => "Sections",
        "selector" => "CSS selector",
        "serverId" => "Server ID",
        "start_line" => "Start line",
        "styles" => "CSS properties",
        "summary" => "Summary",
        "tag" => "Tag",
        "task" => "Task address",
        "tasks" => "Tasks",
        "text" => "Text",
        "timeout" => "Timeout (ms)",
        "timeout_seconds" => "Timeout (seconds)",
        "to" => "Child agent",
        "token_budget" => "Token budget",
        "uid" => "Element uid",
        "until" => "Wait until",
        "url" => "URL",
        "urls" => "URLs",
        "value" => "Value",
        "width" => "Width",
        "x" => "X",
        "y" => "Y",
        _ => return None,
    })
}

/// Keyed by the catalog's Chinese help text. The server-ID lines of the remote-screen tools name the kind of screen.
fn english_parameter_help(key: &str) -> Option<&'static str> {
    Some(match key {
        "一句自足的查询；不要用代词指代上文，长问题拆成多次检索" => "One self-contained query; no pronouns pointing back at the conversation. Break a long question into several searches.",
        "一批绝对 http(s) 地址；不知道地址时先用联网搜索" => "Absolute http(s) page URLs. Search first when you do not know the URL.",
        "任务地址数组：子代理与工作流直接写名称（工作流也可写 workflow:<runId>），后台命令写 shell:<id>，终端写 terminal:<id>，开发服务器写 preview:<serverId>（对话有多个工作区时写 preview:<serverId>@<工作区编号>）；等待到点名的任务全部给出结果为止，省略时等待本对话全部子代理、工作流与后台命令（不含终端与开发服务器）" => "Array of task addresses: a child agent or workflow run by its bare name (a workflow also answers to workflow:<runId>), a background command as shell:<id>, a terminal as terminal:<id>, a dev server as preview:<serverId> (preview:<serverId>@<workspace> when the conversation has several workspaces). The wait ends once every named task has produced a result. Omit to wait for every child agent, workflow run and background command in this conversation (terminals and dev servers excluded).",
        "以 `export const meta = {…}` 开头的 JS 编排脚本：用 agent()/parallel()/pipeline()/phase()/log() 派生并组织步骤子代理，正文的 return 值就是运行结果。给某一步加 { isolation: \"worktree\" } 会为它单开一棵从 HEAD 检出的 git 工作树（看不到未提交改动；留下改动就保留，没改动就自动拆除）" => "JavaScript orchestration starting with `export const meta = {…}`: spawn steps with agent()/parallel()/pipeline(), narrate with phase()/log(); the body's return value is the run result. Adding { isolation: \"worktree\" } to a step gives it its own git worktree checked out from HEAD (uncommitted changes are not in it; a step that leaves changes keeps its worktree, one that changes nothing has it removed).",
        "原样暴露给脚本的 JSON 值（全局 args）；数组与对象直接传，不要编码成字符串" => "JSON value exposed to the script as the global `args`, verbatim; pass arrays and objects directly, not as encoded strings.",
        "本次运行允许消耗的 token 硬顶，脚本经 budget 读到；耗尽后新的 agent() 调用抛错" => "Hard token ceiling for this run, readable as budget in the script; once exhausted, further agent() calls throw.",
        "上一次运行报出的运行 ID；提示词与选项没变的步骤即时重放，上次停下时还在跑的步骤单独重跑，从第一个改动或失败的步骤起其余全部重跑。可省略脚本与 args 沿用上次的；传入改过的脚本会重新审批" => "Run id from a previous run; steps whose prompt and options are unchanged replay, a step the last attempt left running re-runs alone, and everything from the first changed or failed step re-runs. script and args may be omitted to reuse the last ones; an edited script is approved again.",
        "记忆索引里列出的文档名，.md 后缀可写可不写" => "A document name listed in the memory index. The .md suffix is optional.",
        "memory 目录下的单个文档名，不能包含路径分隔符；.md 后缀可写可不写" => "A single document name inside the memory directory. Path separators are forbidden; the .md suffix is optional.",
        "要修改的记忆文档名，.md 后缀可写可不写" => "The memory document to modify. The .md suffix is optional.",
        "这份记忆的完整 Markdown 正文" => "The document's complete Markdown body.",
        "文档中要被替换的原文，必须唯一匹配；不唯一时请提供更长的片段" => "The passage to replace. It must match exactly once; supply a longer excerpt when it is not unique.",
        "替换后的文本；留空表示删除这段内容" => "The replacement text. Leave it empty to delete the passage.",
        "一句话说明这份记忆记录了什么，会写进 MEMORY.md 索引供以后判断要不要读取" => "One sentence describing what this memory records. It is written into the MEMORY.md index so a later run can decide whether to read the document.",
        "修改后这份记忆的一句话说明，会刷新 MEMORY.md 索引里的对应条目" => "One sentence describing this memory after the change. It refreshes the document's entry in the MEMORY.md index.",
        "0 仅列出当前目录" => "0 lists only the current directory.",
        "只搜索路径匹配该 glob 的文件，例如 *.rs 或 src/**/*.tsx" => "Only search files whose path matches this glob, such as *.rs or src/**/*.tsx.",
        "files_with_matches（默认）只列出含匹配的文件；content 列出匹配行；count 统计每个文件的匹配行数" => "files_with_matches (default) lists the files that match; content lists the matching lines; count gives the number of matching lines per file.",
        "仅 content 模式：每处匹配前后各带几行上下文，0 至 20" => "content mode only: lines of context to show before and after each match, 0 to 20.",
        "仅 content 模式：每处匹配之前的上下文行数，0 至 20" => "content mode only: lines of context to show before each match, 0 to 20.",
        "仅 content 模式：每处匹配之后的上下文行数，0 至 20" => "content mode only: lines of context to show after each match, 0 to 20.",
        "最多返回所选模式的结果条数，默认 250，至多 1000" => "Most results of the chosen mode to return: 250 by default, at most 1,000.",
        "先跳过所选模式的这么多条结果，用于翻到下一页" => "Results of the chosen mode to skip first, to fetch the next page.",
        "本对话已选技能的名字，取自 schema 的 enum" => "Name of a skill this conversation selected; the schema lists them as an enum.",
        "`select:<名字>[,<名字>…]` 按名取，或者用关键词搜索" => "`select:<name>[,<name>…]` to fetch exact tools, or keywords to search for them.",
        "关键词搜索最多返回几个工具；按名取时不生效" => "How many tools a keyword search may return; ignored when fetching by name.",
        ".mewrk/launch.json 里的服务器名称" => "Server name from .mewrk/launch.json.",
        "要停止的服务器 ID" => "Server ID to stop",
        "服务器 ID" => "Server ID",
        "按级别过滤：all（默认）返回全部输出，error 只返回含 error、exception、failed 或 fatal 的行" => "Filter by level: 'all' (default) shows all output, 'error' shows only lines containing error/exception/failed/fatal",
        "最多返回行数（默认 50）" => "Max lines to return (default: 50)",
        "只保留包含该文本的行（例如 [DEBUG]、POST /api）" => "Filter to lines containing this text (e.g., '[DEBUG]', 'POST /api')",
        "九种之一：goToDefinition、findReferences、hover、documentSymbol、workspaceSymbol、goToImplementation、prepareCallHierarchy、incomingCalls、outgoingCalls" => "One of nine: goToDefinition, findReferences, hover, documentSymbol, workspaceSymbol, goToImplementation, prepareCallHierarchy, incomingCalls, outgoingCalls.",
        "从 1 开始，与编辑器显示的一致" => "1-based, as shown in editors.",
        "只用于 workspaceSymbol；空查询在多数语言服务器上没有结果" => "workspaceSymbol only; most language servers return nothing for an empty query.",
        "按级别过滤：all（默认）、error（只看错误）、warn（警告加错误）" => "Filter by level: 'all' (default), 'error' (errors only), 'warn' (warnings + errors)",
        "最多返回行数（默认 50，上限 200）" => "Max lines to return (default: 50, max: 200)",
        "返回图像的缩放系数，取值 0.1 到 1；图像越小消耗的 token 越少。preview_click 和 preview_fill 按 CSS 选择器或 preview_snapshot 给出的 uid 找元素，而不是按像素坐标；遥控屏幕上的像素坐标交给 preview_input" => "Scale factor in [0.1, 1] for the returned image; smaller images use fewer tokens. preview_click and preview_fill find elements by CSS selector or by a uid from preview_snapshot, not by pixel coordinates; pixel coordinates on remote-control screens go through preview_input.",
        "要检查的元素 CSS 选择器" => "CSS selector (e.g., '.button', '#header')",
        "要返回的 CSS 属性名数组；不给时返回一组常用属性" => "CSS properties to return (e.g., ['padding', 'color']). Defaults to common properties.",
        "要点击的元素 CSS 选择器；也可以改给 uid" => "CSS selector for the element to click. Give this or uid.",
        "preview_snapshot 给这个元素标出的 uid，即行首方括号里的数字；也可以改给 CSS 选择器" => "The uid preview_snapshot printed for the element to click, the number in brackets at the start of its line. Give this or selector.",
        "改为双击" => "Perform a double-click",
        "要填写的输入框 CSS 选择器；也可以改给 uid" => "CSS selector for the input element. Give this or uid.",
        "preview_snapshot 给这个输入框标出的 uid，即行首方括号里的数字；也可以改给 CSS 选择器" => "The uid preview_snapshot printed for the input element, the number in brackets at the start of its line. Give this or selector.",
        "要填入的值" => "Value to fill",
        "在页面上下文里求值的 JavaScript 表达式；返回值按 JSON 序列化" => "JavaScript expression to evaluate in the page context. Return values are serialized as JSON.",
        "过滤：all（默认）返回全部请求，failed 只返回 4xx、5xx 与网络错误；给了 requestId 时本项被忽略" => "Filter: 'all' (default) shows all requests, 'failed' shows only 4xx/5xx and network errors. Ignored when requestId is provided.",
        "给出时返回该请求的响应正文，而不是列出全部请求；requestId 取自列表输出" => "If provided, returns the response body for this specific request instead of listing all requests. Get requestIds from the listing output.",
        "设备预设；给出时覆盖 width 与 height。desktop 清除尺寸模拟，回到面板自身的响应式尺寸" => "Device preset. Overrides width/height if provided. \"desktop\" clears the size emulation (back to the pane's responsive size).",
        "视口宽度，单位 CSS 像素（需同时给 height）" => "Viewport width in CSS pixels (requires height)",
        "视口高度，单位 CSS 像素（需同时给 width）" => "Viewport height in CSS pixels (requires width)",
        "模拟 prefers-color-scheme 媒体特性，用于测试深色与浅色" => "Emulate prefers-color-scheme media feature for dark/light mode testing.",
        "对话里的图片编号，如 3、#3 或 [Image #3]；也接受 64 位十六进制摘要" => "The conversation image number, e.g. 3, #3, or [Image #3]. A 64-character hex digest also resolves.",
        "目标 file 输入框的 CSS 选择器；不给时放进页面已打开的文件选择框，没有就放进页面上第一个 file 输入框（隐藏的也算）" => "CSS selector of the target file input. Omitted, the image goes into the file chooser the page has open, or else into the first file input on the page, hidden or not.",
        "页面看到的文件名；不给时用附件原名" => "The file name the page sees. Defaults to the attachment's own name.",
        "网页上默认接受；手机或平板上没有默认，须给 button 或 accept" => "On the web page default true. On a phone or tablet there is no default: give button or accept.",
        "网页上仅在接受 prompt 时生效；iPhone 或 iPad 上填进弹窗的输入框，不给 accept 时只输入" => "On the web page used only when accepting a prompt. On an iPhone or iPad the text for the alert's field; without accept it is only typed.",
        "仅手机或平板：要按的按钮的完整标签" => "Phone or tablet only: the exact label of the button to press.",
        "遥控屏幕的服务器 ID；不给时用预览面板正在显示的屏幕" => "Server ID of the remote-control screen. Defaults to the screen the preview pane shows.",
        "按顺序执行的动作数组，至多 20 个，遇错即停；x、y 是 preview_screenshot 截图（或 preview_snapshot 的 @x,y 框）上的像素。动作有 click、long_press、drag、scroll、move、type、key、wait；type 带 via: \"paste\" 时经剪贴板粘贴（仅桌面）" => "Actions to run in order, at most 20; the first one that fails stops the rest. x and y are pixels of the latest preview_screenshot (or preview_snapshot's @x,y boxes). Actions: click, long_press, drag, scroll, move, type, key, wait; a type with via \"paste\" pastes through the clipboard (desktops only).",
        "执行完后再返回一张屏幕截图" => "Also return a screenshot of the screen after the actions.",
        "默认子代理看不到当前对话，任务描述必须自包含全部背景" => "Child agents do not see this conversation by default; include all required context in the task.",
        "可选的 JSON Schema 子集；给出后子代理必须调用 structured_output 交回符合该模式的结果，返回值会随 task_wait 一起回来。顶层必须是 type 为 object 的对象模式；支持 type、properties、required、items、enum、const、additionalProperties、minItems/maxItems、minLength/maxLength、minimum/maximum，其余关键字会被当场拒绝" => "Optional JSON Schema subset. When set, the child must call structured_output with a result matching it, and that value comes back with task_wait. The top level must be an object schema; type, properties, required, items, enum, const, additionalProperties, minItems/maxItems, minLength/maxLength and minimum/maximum are supported and every other keyword is rejected on the spot.",
        "可选的可信命名定义短名称；可用的名称与用途列在本轮的可用 Agent 清单里。由宿主解析，不能与 context=conversation 同时使用" => "Optional trusted definition slug; the available names and what each is for are listed in this turn's available-agents context. Resolved by the host; cannot be combined with context=conversation.",
        "必填。用于 task_wait 寻址，也是任务栏里这一行的标题；小写字母开头，可含数字、_ 和 -；整个对话分支树内不可重名" => "Required. Name the child yourself: it is the address task_wait takes, and the title the task is listed under. Start with a lowercase letter; digits, _ and - are allowed. It must be unused anywhere in this conversation's branch tree.",
        "必填。这次运行在会话代理命名空间里的地址，也是任务栏里这一行的标题；小写字母开头，可含数字、_ 和 -；整个对话分支树内不可重名，续跑也要换新名字" => "Required. This run's address in the same namespace agents are named in, and the title the task is listed under. Start with a lowercase letter; digits, _ and - are allowed. It must be unused anywhere in this conversation's branch tree, so a resume still needs a fresh one.",
        "显示在时间线上的短名称" => "Short name shown in the timeline.",
        "none（默认）：只看到任务；conversation：携带当前对话历史副本" => "none (default): task only; conversation: include a copy of the current conversation history.",
        "5–600 秒，默认 60" => "5–600 seconds; default: 60.",
        "要停止的任务地址：后台命令写 shell:<id>，子代理与工作流直接写名称（工作流也可写 workflow:<runId>）；终端会话不能用它停止，开发服务器用 preview_stop。被停任务的结果仍会照常交付" => "Address of the task to stop: a background command as shell:<id>, a child agent or workflow run by its bare name (a workflow also answers to workflow:<runId>). Terminal sessions cannot be stopped with this tool, and dev servers are stopped with preview_stop. The stopped task's result is still delivered.",
        "Claude Code AskUserQuestion 格式：1–4 题；每题含 header、question、2–4 个 label/description 选项及 multiSelect；无需添加 Other" => "Claude Code AskUserQuestion format: 1–4 questions, each with header, question, 2–4 label/description options, and multiSelect. Do not add Other.",
        "分叉会话的第一条用户消息，也是你唯一一次下达指令的机会；说清任务与需要的全部背景" => "First user message of the forked conversation, and your only chance to instruct it; state the task and all the background it needs",
        "选择操作：write 写入或覆盖计划、read 读取当前计划" => "Choose an action: write stores or replaces the plan, read returns the current one.",
        "write 必填；计划的 Markdown 正文，整篇覆盖上一版" => "Required for write; the plan's Markdown body, which replaces the previous one in full.",
        "交接索引里列出的文档名，.md 后缀可写可不写" => "A note name listed in the handoff index. The .md suffix is optional.",
        "交接文档名，不能包含路径分隔符；.md 后缀可写可不写" => "The handoff note's name. Path separators are forbidden; the .md suffix is optional.",
        "要修改的交接文档名，.md 后缀可写可不写" => "The handoff note to modify. The .md suffix is optional.",
        "交接文档的完整 Markdown 正文" => "The note's complete Markdown body.",
        "一句话说明这份交接文档写了什么，会写进交接索引" => "One sentence describing what this handoff note holds. It is written into the handoff index.",
        "修改后这份交接文档的一句话说明，会刷新交接索引里的对应条目" => "One sentence describing this handoff note after the change. It refreshes the note's entry in the handoff index.",
        "agent_spawn 时起的名字" => "Name the child was spawned with.",
        "时间线上显示的 5–10 词预览" => "Optional 5–10 word preview shown on the timeline.",
        "始终为空数组" => "Always an empty list.",
        "遥控屏幕的服务器 ID；不给时用预览面板正在显示的屏幕，否则用唯一已连接的那块" => "Server ID of the remote-control screen. Defaults to the one the preview pane shows, else the only one connected.",
        "手机或平板的服务器 ID；不给时用预览面板正在显示的那台，否则用唯一已连接的那台" => "Server ID of the Android device, iPhone or iPad. Defaults to the one the preview pane shows, else the only one connected.",
        "iPhone、iPad 或桌面的服务器 ID；不给时用预览面板正在显示的那台，否则用唯一已连接的那台" => "Server ID of the iPhone, iPad or desktop. Defaults to the one the preview pane shows, else the only one connected.",
        "Android 设备的服务器 ID；不给时用预览面板正在显示的设备，否则用唯一已连接的那台" => "Server ID of the Android device. Defaults to the one the preview pane shows, else the only one connected.",
        "区域左边，取最近一次截图的坐标" => "Left edge of the region, in the screenshot's frame.",
        "区域上边，取最近一次截图的坐标" => "Top edge of the region, in the screenshot's frame.",
        "区域宽度，取最近一次截图的坐标" => "Width of the region, in the screenshot's frame.",
        "区域高度，取最近一次截图的坐标" => "Height of the region, in the screenshot's frame.",
        "change（默认）画面一变就返回，still 画面静止后返回" => "change (default) or still.",
        "最多等多少毫秒，100–20000，默认 5000" => "Milliseconds to wait at most, 100-20000; 5000 by default.",
        "只看这个区域：左边，四个一起给或都不给" => "Region to watch, in the latest screenshot's frame; give all four or none.",
        "只看这个区域：上边" => "Region to watch, in the latest screenshot's frame; give all four or none.",
        "只看这个区域：宽度" => "Region to watch, in the latest screenshot's frame; give all four or none.",
        "只看这个区域：高度" => "Region to watch, in the latest screenshot's frame; give all four or none.",
        "list（默认）列出应用，foreground 前台应用，info 单个应用的状态，launch 启动，activate 切到前台，stop 强行停止，open_url 打开链接" => "list (default), foreground, info, launch, activate, stop or open_url.",
        "Android 包名或 iOS bundle id；info、launch、activate、stop 必填，open_url 时指定由哪个应用打开" => "Android package or iOS bundle id. Required for info, launch, activate and stop; for open_url, the app to open the URL in.",
        "仅 open_url：要打开的网址或深层链接" => "For open_url: the URL or deep link.",
        "仅 list：只列包含这段文字的项，不分大小写" => "For list: only entries containing this text, case-insensitive.",
        "仅 list：最多列多少项，1–1000，默认 200" => "For list: at most this many entries, 1-1000; 200 by default.",
        "仅 list，仅 Android：launchable（默认）、third_party 或 all" => "For list, Android only: launchable (default), third_party or all.",
        "仅 launch，仅 Android：要启动的 Activity，写 .MainActivity 或完整类名" => "For launch, Android only: the activity to start instead of the launcher one, as .MainActivity or a full class name.",
        "仅 launch：先停止再启动" => "For launch: stop the app first.",
        "仅 launch，仅 iOS：应用的启动参数" => "For launch, iOS only: launch arguments for the app (a fresh start only).",
        "仅 launch，仅 iOS：应用的环境变量" => "For launch, iOS only: environment variables for the app (a fresh start only).",
        "info（默认）读取状态；lock、unlock、wake、rotate、appearance、animations、location、clear_location、notifications、quick_settings、collapse、stay_awake、dismiss_keyboard 改变设备" => "info (default), lock, unlock, wake, rotate, appearance, animations, location, clear_location, notifications, quick_settings, collapse, stay_awake or dismiss_keyboard.",
        "rotate、appearance、animations、stay_awake 的新值" => "For rotate, appearance, animations and stay_awake: the new value.",
        "仅 info：device、display、power、notifications 中的任意几个" => "For info: any of device, display, power and notifications.",
        "仅 location：-90 到 90 度" => "For location: degrees, -90 to 90.",
        "仅 location：-180 到 180 度" => "For location: degrees, -180 to 180.",
        "read 读取，write 写入" => "read or write.",
        "仅 write：要写入的文字；桌面上至多 60000 个字符" => "For write: the text; at most 60000 characters on a desktop.",
        "install 安装，uninstall 卸载，clear_data 清除数据，grant 授予权限，revoke 撤销权限" => "install, uninstall, clear_data, grant or revoke.",
        "仅 install：本机工作区里的 .apk 文件，相对工作区或绝对路径" => "For install: the .apk file, relative to the workspace or absolute.",
        "uninstall、clear_data、grant、revoke 时的包名" => "For uninstall, clear_data, grant and revoke: the package name.",
        "仅 grant 与 revoke：权限名，例如 android.permission.POST_NOTIFICATIONS" => "For grant and revoke: the permission, such as android.permission.POST_NOTIFICATIONS.",
        "仅 uninstall：保留应用的数据与缓存" => "For uninstall: keep the app's data and cache.",
        "仅 install：覆盖已安装的版本，默认开启" => "For install: replace an installed version; true by default.",
        "仅 install：允许版本号低于已安装版本（仅限可调试应用）" => "For install: allow a lower version code than the installed one (debuggable apps only).",
        "仅 install：授予应用申请的全部运行时权限" => "For install: grant every runtime permission the app requests.",
        "返回最近多少行，1 到 2000，默认 200" => "Lines to return, 1-2000; 200 by default.",
        "保留的最低优先级：V、D、I、W、E 或 F" => "Lowest priority to keep: V, D, I, W, E or F.",
        "只看这个标签" => "Only this tag.",
        "只看这个应用的日志" => "Only lines of this app.",
        "main、system、crash、events 或 all" => "main, system, crash, events or all.",
        "只保留包含这段文字的行，不区分大小写" => "Only lines containing this text, case-insensitive.",
        "在设备 shell 中执行的命令行" => "The shell command line.",
        "最多等待多少毫秒，1000 到 20000，默认 10000" => "Milliseconds to wait, 1000-20000; 10000 by default.",
        _ => return None,
    })
}

/// Keyed by the catalog's Chinese placeholder.
fn english_parameter_placeholder(key: &str) -> Option<&'static str> {
    Some(match key {
        "查看工作树状态" => "Show working tree status",
        "列出当前目录的文件" => "List files in the current directory",
        "Anthropic Claude 4.5 发布日期" => "Anthropic Claude 4.5 release date",
        "用户偏好" => "user-preferences",
        "构建环境" => "build-environment",
        "当前进度" => "current-state",
        "任务目标、已完成的工作与下一步" => "The goal, the work done and the next step",
        "用户长期偏好的语言与代码风格" => "The user's long-standing language and code-style preferences",
        "测试必须用项目自带环境运行" => "Tests must run in the project's own environment",
        "调查 src/ 下的路由结构并总结关键文件" => "Inspect routing under src/ and summarize the key files",
        "调查路由" => "Inspect routing",
        "改用 v2 接口再试一次，并报告结果" => "Retry with the v2 endpoint and report the result",
        "改用 v2 接口重试" => "Retry with the v2 endpoint",
        "在分叉出的会话里要完成的任务" => "The task to complete in the forked conversation",
        "[{\"question\":\"采用哪个方案？\",\"header\":\"实现方案\",\"options\":[{\"label\":\"方案 A\",\"description\":\"保持改动最小\"},{\"label\":\"方案 B\",\"description\":\"完整重构\"}],\"multiSelect\":false}]" => "[{\"question\":\"Which approach should I use?\",\"header\":\"Approach\",\"options\":[{\"label\":\"Approach A\",\"description\":\"Keep the change small\"},{\"label\":\"Approach B\",\"description\":\"Perform a full rewrite\"}],\"multiSelect\":false}]",
        _ => return None,
    })
}

/// English built-in tool labels. Must match the host's `catalog.rs::english_tool_label`. Labels only: tool descriptions are
/// always empty in the seed and are neither read nor localized by the client.
fn english_tool_label(key: &str) -> Option<&'static str> {
    Some(match key {
        "ls" => "List files",
        "grep" => "Search content",
        "pwsh" => "PowerShell 7",
        "powershell" => "Windows PowerShell",
        "bash" => "Bash",
        "zsh" => "zsh",
        "sh" => "sh",
        "write" => "Write file",
        "edit" => "Edit file",
        "find" => "Find files",
        "read" => "Read file",
        "lsp" => "Code navigation",
        "web_search" => "Web search",
        "web_fetch" => "Fetch web pages",
        "workflow" => "Workflow",
        "preview_start" => "Start preview",
        "preview_stop" => "Stop preview",
        "preview_list" => "List previews",
        "preview_logs" => "Server logs",
        "preview_console_logs" => "Console logs",
        "preview_screenshot" => "Page screenshot",
        "preview_snapshot" => "Page snapshot",
        "preview_inspect" => "Inspect element",
        "preview_click" => "Click element",
        "preview_fill" => "Fill input",
        "preview_eval" => "Run script",
        "preview_network" => "Network requests",
        "preview_resize" => "Resize viewport",
        "preview_upload_image" => "Upload image",
        "preview_dialog" => "Answer dialog",
        "preview_input" => "Remote input",
        "preview_zoom" => "Zoom in",
        "preview_wait" => "Wait for the screen",
        "screen_app" => "Screen apps",
        "screen_device" => "Screen device",
        "screen_clipboard" => "Screen clipboard",
        "screen_package" => "Install apps",
        "android_logcat" => "Read Android logs",
        "android_shell" => "Android shell",
        "agent_spawn" => "Subagent",
        "task_wait" => "Wait for tasks",
        "task_list" => "List tasks",
        "task_stop" => "Stop a task",
        "send_message" => "Send message",
        "box" => "Background result",
        "read_global_memory" => "Read global memory",
        "read_project_memory" => "Read project memory",
        "create_global_memory" => "Create global memory",
        "create_project_memory" => "Create project memory",
        "edit_global_memory" => "Edit global memory",
        "edit_project_memory" => "Edit project memory",
        "ask_user" => "Ask user",
        "fork" => "Fork conversation",
        "skill" => "Skill",
        "tool_search" => "Tool discovery",
        "plan" => "Plan document",
        "exit_plan_mode" => "Exit plan mode",
        "read_handoff_note" => "Read handoff note",
        "create_handoff_note" => "Write handoff note",
        "edit_handoff_note" => "Edit handoff note",
        "handoff" => "Hand off",
        _ => return None,
    })
}

fn localize_tool_parameter(parameter: &ToolParameter) -> ToolParameter {
    let translated = |text: &Option<String>, table: fn(&str) -> Option<&'static str>| {
        text.as_ref().map(|text| table(text).map_or_else(|| text.clone(), str::to_owned))
    };
    ToolParameter {
        label: english_parameter_label(&parameter.name).map_or_else(|| parameter.label.clone(), str::to_owned),
        help: translated(&parameter.help, english_parameter_help),
        placeholder: translated(&parameter.placeholder, english_parameter_placeholder),
        ..parameter.clone()
    }
}

/// A tool as the language shows it. Chinese and third-party tools come back as they are; a built-in one gets its English
/// label and parameters.
pub fn localize_tool_descriptor(tool: &ToolDescriptor, language: ResolvedLanguage) -> ToolDescriptor {
    let label = match language {
        ResolvedLanguage::EnUs => english_tool_label(&tool.name),
        ResolvedLanguage::ZhCn => None,
    };
    match label {
        Some(label) => ToolDescriptor {
            label: label.to_owned(),
            parameters: tool.parameters.iter().map(localize_tool_parameter).collect(),
            ..tool.clone()
        },
        None => tool.clone(),
    }
}

/// The host's built-in catalog for tests, read from its source: `catalog.rs` is private to the host crate, and it is what
/// `AppDocument.tools` holds at run time, where upstream's tests read its twin `seed.ts`.
#[cfg(test)]
pub(crate) mod test_catalog {
    use crate::model::{ToolCategory, ToolDescriptor, ToolParameter, ToolParameterType};
    use serde_json::{json, Value};

    const SOURCE: &str = include_str!("../../mewrk-host/host/src/catalog.rs");

    enum Expr {
        Text(String),
        Word(String),
        Call(String, Vec<Expr>),
    }

    /// Just enough of Rust for the catalog's literal calls.
    struct Parser<'a>(&'a str);

    impl Parser<'_> {
        fn skip(&mut self) {
            loop {
                self.0 = self.0.trim_start();
                let Some(comment) = self.0.strip_prefix("//") else { return };
                self.0 = comment.split_once('\n').map_or("", |(_, rest)| rest);
            }
        }

        fn eat(&mut self, token: &str) -> bool {
            self.skip();
            let eaten = self.0.strip_prefix(token);
            self.0 = eaten.unwrap_or(self.0);
            eaten.is_some()
        }

        fn text(&mut self) -> String {
            let mut out = String::new();
            let mut chars = self.0.char_indices();
            while let Some((at, c)) = chars.next() {
                match c {
                    '"' => {
                        self.0 = &self.0[at + 1..];
                        return out;
                    }
                    '\\' => match chars.next().unwrap().1 {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        '\n' => {
                            let mut peek = chars.clone();
                            while peek.next().is_some_and(|(_, c)| c.is_whitespace()) {
                                chars.next();
                            }
                        }
                        other => out.push(other),
                    },
                    other => out.push(other),
                }
            }
            panic!("unterminated string in catalog.rs");
        }

        fn expr(&mut self) -> Expr {
            self.skip();
            if self.eat("r#\"") {
                let (text, rest) = self.0.split_once("\"#").unwrap();
                self.0 = rest;
                return Expr::Text(text.to_owned());
            }
            if self.eat("\"") {
                return Expr::Text(self.text());
            }
            let end = self.0.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | ':' | '.' | '-'))).unwrap_or(self.0.len());
            let (word, rest) = self.0.split_at(end);
            assert!(!word.is_empty(), "unexpected catalog.rs syntax: {:.40}", self.0);
            self.0 = rest;
            self.eat("!");
            for (open, close) in [("(", ")"), ("[", "]")] {
                if self.eat(open) {
                    let mut args = Vec::new();
                    while !self.eat(close) {
                        args.push(self.expr());
                        self.eat(",");
                    }
                    return Expr::Call(word.to_owned(), args);
                }
            }
            Expr::Word(word.to_owned())
        }
    }

    fn body_of(signature: &str) -> Vec<Expr> {
        let after = SOURCE.split_once(signature).unwrap().1;
        let list = &after[after.find("vec![").unwrap()..];
        match Parser(list).expr() {
            Expr::Call(_, items) => items,
            _ => unreachable!(),
        }
    }

    fn text(expr: &Expr) -> String {
        match expr {
            Expr::Text(text) => text.clone(),
            _ => panic!("expected a string literal in catalog.rs"),
        }
    }

    fn word(expr: &Expr) -> &str {
        match expr {
            Expr::Word(word) => word,
            _ => panic!("expected a word in catalog.rs"),
        }
    }

    fn optional_text(expr: &Expr) -> Option<String> {
        match expr {
            Expr::Call(_, args) => Some(text(&args[0])),
            _ => None,
        }
    }

    fn default_value(expr: &Expr) -> Option<Value> {
        let Expr::Call(_, some) = expr else { return None };
        let Expr::Call(_, json) = &some[0] else { panic!("expected json! in catalog.rs") };
        Some(match &json[0] {
            Expr::Text(text) => json!(text),
            Expr::Word(word) => serde_json::from_str(word).unwrap(),
            Expr::Call(..) => panic!("unexpected default value in catalog.rs"),
        })
    }

    fn parameter(expr: &Expr) -> ToolParameter {
        let Expr::Call(call, args) = expr else { panic!("expected parameter() in catalog.rs") };
        assert_eq!(call, "parameter");
        ToolParameter {
            name: text(&args[0]),
            label: text(&args[1]),
            parameter_type: match word(&args[2]) {
                "StringType" => ToolParameterType::String,
                "Number" => ToolParameterType::Number,
                "Boolean" => ToolParameterType::Boolean,
                "Multiline" => ToolParameterType::Multiline,
                "Json" => ToolParameterType::Json,
                other => panic!("unknown parameter type {other}"),
            },
            required: word(&args[3]) == "true",
            default_value: default_value(&args[4]),
            placeholder: optional_text(&args[5]),
            help: optional_text(&args[6]),
        }
    }

    pub(crate) fn tool_catalog() -> Vec<ToolDescriptor> {
        body_of("pub fn tool_catalog() -> Vec<ToolDescriptor> {")
            .iter()
            .map(|expr| {
                let Expr::Call(call, args) = expr else { panic!("expected descriptor() in catalog.rs") };
                assert_eq!(call, "descriptor");
                let category = word(&args[3]).rsplit("::").next().unwrap().to_lowercase();
                let parameters = match &args[5] {
                    Expr::Call(list, items) if list == "vec" => items.iter().map(parameter).collect(),
                    Expr::Call(helper, _) if helper == "powershell_parameters" => {
                        body_of("fn powershell_parameters() -> Vec<ToolParameter> {").iter().map(parameter).collect()
                    }
                    Expr::Call(new, _) if new == "Vec::new" => Vec::new(),
                    _ => panic!("unexpected parameter list in catalog.rs"),
                };
                ToolDescriptor {
                    name: text(&args[0]),
                    label: text(&args[1]),
                    description: text(&args[2]),
                    category: serde_json::from_value::<ToolCategory>(json!(category)).unwrap(),
                    dangerous: word(&args[4]) == "true",
                    parameters,
                    input_schema: None,
                    force_confirmation: false,
                    approval_note: None,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::test_catalog::tool_catalog;
    use super::*;
    use serde_json::{json, Value};
    use std::collections::{BTreeMap, HashSet};

    fn tool(name: &str) -> ToolDescriptor {
        tool_catalog().into_iter().find(|tool| tool.name == name).unwrap_or_else(|| panic!("{name}"))
    }

    fn english(name: &str) -> ToolDescriptor {
        localize_tool_descriptor(&tool(name), ResolvedLanguage::EnUs)
    }

    fn labels(tool: &ToolDescriptor) -> Vec<&str> {
        tool.parameters.iter().map(|parameter| parameter.label.as_str()).collect()
    }

    fn helps(tool: &ToolDescriptor) -> Vec<Option<&str>> {
        tool.parameters.iter().map(|parameter| parameter.help.as_deref()).collect()
    }

    fn help_of<'a>(tool: &'a ToolDescriptor, name: &str) -> Option<&'a str> {
        tool.parameters.iter().find(|parameter| parameter.name == name).and_then(|parameter| parameter.help.as_deref())
    }

    #[test]
    fn does_not_mutate_chinese_defaults_or_unknown_third_party_tools() {
        let first = tool_catalog().remove(0);
        assert_eq!(localize_tool_descriptor(&first, ResolvedLanguage::ZhCn), first);
        let unknown = ToolDescriptor { name: "third_party".into(), ..first };
        assert_eq!(localize_tool_descriptor(&unknown, ResolvedLanguage::EnUs), unknown);
    }

    #[test]
    fn localizes_the_label_and_parameters_but_never_introduces_a_tool_description() {
        // Seed descriptions remain empty; localization must neither create nor display them.
        let first = tool_catalog().remove(0);
        let localized = localize_tool_descriptor(&first, ResolvedLanguage::EnUs);
        assert_eq!(localized.label, "List files");
        assert_eq!(localized.description, "");
    }

    #[test]
    fn describes_greps_modes_its_context_and_its_glob_with_the_defaults_the_tool_runs_with() {
        let grep = tool("grep");
        let names: Vec<&str> = grep.parameters.iter().map(|parameter| parameter.name.as_str()).collect();
        assert_eq!(names, ["pattern", "path", "glob", "case_sensitive", "output_mode", "context", "before", "after", "limit", "offset"]);
        // Case-sensitive and a file list unless the call says otherwise; context has no default.
        let defaults: BTreeMap<&str, &Value> =
            grep.parameters.iter().filter_map(|p| p.default_value.as_ref().map(|value| (p.name.as_str(), value))).collect();
        let expected = json!({ "path": ".", "case_sensitive": true, "output_mode": "files_with_matches", "limit": 250, "offset": 0 });
        assert_eq!(serde_json::to_value(defaults).unwrap(), expected);
        assert_eq!(grep.parameters.iter().find(|p| p.name == "glob").unwrap().placeholder.as_deref(), Some("*.rs"));

        let english = localize_tool_descriptor(&grep, ResolvedLanguage::EnUs);
        assert_eq!(
            labels(&english),
            ["Search pattern", "Path", "File glob", "Case sensitive", "Output mode", "Context", "Lines before", "Lines after", "Limit", "Offset"]
        );
        assert!(help_of(&english, "output_mode").unwrap().starts_with("files_with_matches (default)"));
        assert_eq!(help_of(&english, "limit"), Some("Most results of the chosen mode to return: 250 by default, at most 1,000."));
        assert_eq!(help_of(&english, "offset"), Some("Results of the chosen mode to skip first, to fetch the next page."));
    }

    #[test]
    fn names_the_eight_remote_screen_tools_and_says_which_kind_of_screen_a_server_id_means() {
        for (name, label) in [
            ("preview_zoom", "Zoom in"),
            ("preview_wait", "Wait for the screen"),
            ("screen_app", "Screen apps"),
            ("screen_device", "Screen device"),
            ("screen_clipboard", "Screen clipboard"),
            ("screen_package", "Install apps"),
            ("android_logcat", "Read Android logs"),
            ("android_shell", "Android shell"),
        ] {
            assert_eq!(english(name).label, label);
        }
        // The server ID is the one parameter every screen tool shares; its help names the kinds of screen the tool reaches.
        for (name, screen) in [
            ("preview_zoom", "remote-control screen"),
            ("preview_wait", "remote-control screen"),
            ("screen_app", "Android device, iPhone or iPad"),
            ("screen_device", "Android device, iPhone or iPad"),
            ("screen_clipboard", "iPhone, iPad or desktop"),
            ("screen_package", "Android device"),
            ("android_logcat", "Android device"),
            ("android_shell", "Android device"),
        ] {
            let localized = english(name);
            let server = &localized.parameters[0];
            assert_eq!(server.name, "serverId");
            assert_eq!(server.label, "Server ID");
            let expected = format!("Server ID of the {screen}. Defaults to the one the preview pane shows, else the only one connected.");
            assert_eq!(server.help.as_deref(), Some(expected.as_str()), "{name}");
        }
        // Labels are keyed by parameter name alone, so one name reads the same in every tool that has it.
        let shell = english("android_shell");
        assert_eq!(labels(&shell), ["Server ID", "Command", "Timeout (ms)"]);
        assert_eq!(shell.parameters[1].help.as_deref(), Some("The shell command line."));
        assert_eq!(labels(&english("preview_wait")), ["Server ID", "Wait until", "Timeout (ms)", "X", "Y", "Width", "Height"]);
        assert_eq!(
            helps(&english("preview_zoom")),
            [
                Some("Server ID of the remote-control screen. Defaults to the one the preview pane shows, else the only one connected."),
                Some("Left edge of the region, in the screenshot's frame."),
                Some("Top edge of the region, in the screenshot's frame."),
                Some("Width of the region, in the screenshot's frame."),
                Some("Height of the region, in the screenshot's frame."),
            ]
        );
        // The four edges of the watched region read as one sentence.
        let wait = english("preview_wait");
        let edges: HashSet<Option<&str>> = wait.parameters[3..].iter().map(|parameter| parameter.help.as_deref()).collect();
        assert_eq!(edges, HashSet::from([Some("Region to watch, in the latest screenshot's frame; give all four or none.")]));
        assert_eq!(
            labels(&english("screen_package")),
            ["Server ID", "Action", "Path", "App", "Permission", "Keep data", "Replace existing", "Allow downgrade", "Grant all permissions"]
        );
        assert_eq!(help_of(&english("screen_package"), "app"), Some("For uninstall, clear_data, grant and revoke: the package name."));
        let app = english("screen_app");
        assert_eq!(
            labels(&app),
            ["Server ID", "Action", "App", "URL", "Filter", "Limit", "Scope", "Activity", "Restart", "Launch arguments", "Environment variables"]
        );
        assert_eq!(
            help_of(&app, "app"),
            Some("Android package or iOS bundle id. Required for info, launch, activate and stop; for open_url, the app to open the URL in.")
        );
        assert_eq!(help_of(&app, "url"), Some("For open_url: the URL or deep link."));
        assert_eq!(help_of(&app, "restart"), Some("For launch: stop the app first."));
        let device = english("screen_device");
        assert_eq!(labels(&device), ["Server ID", "Action", "Value", "Sections", "Latitude", "Longitude"]);
        assert_eq!(help_of(&device, "latitude"), Some("For location: degrees, -90 to 90."));
        // The clipboard says what a write may carry, and what a desktop will not take.
        let clipboard = english("screen_clipboard");
        assert_eq!(clipboard.parameters[2].label, "Text");
        assert_eq!(clipboard.parameters[2].help.as_deref(), Some("For write: the text; at most 60000 characters on a desktop."));
    }

    #[test]
    fn describes_preview_dialog_for_a_page_and_for_a_phone_or_tablet() {
        let dialog = tool("preview_dialog");
        let names: Vec<&str> = dialog.parameters.iter().map(|parameter| parameter.name.as_str()).collect();
        assert_eq!(names, ["serverId", "accept", "prompt_text", "button"]);
        // The default is the page's; a phone or tablet has none, and the help says so.
        assert_eq!(dialog.parameters[1].default_value, Some(json!(true)));
        let localized = localize_tool_descriptor(&dialog, ResolvedLanguage::EnUs);
        assert_eq!(labels(&localized), ["Server ID", "Accept", "Prompt text", "Button"]);
        assert_eq!(
            helps(&localized),
            [
                Some("Server ID"),
                Some("On the web page default true. On a phone or tablet there is no default: give button or accept."),
                Some("On the web page used only when accepting a prompt. On an iPhone or iPad the text for the alert's field; without accept it is only typed."),
                Some("Phone or tablet only: the exact label of the button to press."),
            ]
        );
    }

    #[test]
    fn leaves_no_chinese_in_any_built_in_tool_under_the_english_ui() {
        let han = |text: &str| text.chars().any(|c| matches!(c, '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}'));
        let mut leftovers = Vec::new();
        for tool in tool_catalog() {
            let localized = localize_tool_descriptor(&tool, ResolvedLanguage::EnUs);
            if han(&localized.label) {
                leftovers.push(format!("{}: {}", tool.name, localized.label));
            }
            for parameter in &localized.parameters {
                let place = format!("{}.{}", tool.name, parameter.name);
                if parameter.label.is_empty() || han(&parameter.label) {
                    leftovers.push(format!("{place} label: {}", parameter.label));
                }
                for (what, text) in [("help", &parameter.help), ("placeholder", &parameter.placeholder)] {
                    if let Some(text) = text.as_deref().filter(|text| han(text)) {
                        leftovers.push(format!("{place} {what}: {text}"));
                    }
                }
            }
        }
        // Upstream runs this over `seed.ts`. The host's catalog, which is what a document holds, words these three differently,
        // and upstream's table has no entry for that wording, so they stay Chinese upstream too. Kept as upstream has them.
        assert_eq!(
            leftovers,
            [
                "read.start_line help: 仅文本文件；从 1 开始",
                "read.end_line help: 仅文本文件；包含该行",
                "agent_spawn.agent_type help: 可选的可信命名定义名称；可用的名称与用途列在本轮的可用 Agent 清单里。照抄其中一个名称即可——由宿主解析，无歧义时大小写与分隔符不敏感。不能与 context=conversation 同时使用",
            ]
        );
    }
}
