# 多词典支持（阶段一：加载 + 切换）设计与实施计划

**目标：** 一次启动可加载多部 `.mdx`，左栏仍只显示当前活动词典，顶部提供切换器；所有接口按词典命名空间化，为后续“GoldenDict 式合并视图”留出不改接口的扩展位。

**范围外：** 合并词头列表与多词典堆叠词条（阶段二）；GoldenDict 分组配置；自定义预览模板（`--template`/`--css`）；HTTP/2。

**构建环境：** 源码与 git 仓库在 tx（`/root/cleantest/mdx-preview`），tx 已无 Rust 工具链；编译在 gb（`/data/home/mdx-preview`，cargo 1.99）。改动流程：编辑 tx 源码 → `tar` 同步到 gb → gb 上 `cargo build --release` 与 `cargo zigbuild --release --target x86_64-pc-windows-gnu` → 冒烟在 gb（浏览器）+ 本机 Windows（exe）。

## 一、现状约束（来自代码，非推测）

- `AppState { dict: Arc<Dict>, mdds: Vec<Mdd>, res: HashMap<String, Resource>, tx }` 只持有单部词典。
- 词条 id 是“词典内序号 + 1”（`Mdx::words` 下标），跨词典不可复用。
- `render_item` 用内嵌 `ITEM_HTML` 填 `{base}`，`<base href="{base}/api/res/">` 决定词条内相对资源的解析；根绝对路径（`/img.png`）由 `fallback` 直接查当前唯一词典。
- 前端 `state` 是单例：`items/base/hasMore/selected/query/desc`；历史 `nav.ids` 只存 k_id；iframe `src = /api/item/<k_id>`；排序持久化在 `preview.desc`。
- `load_mdx` 在启动时就解压全部 key block 建好 `words`，并同时建 `sorted`；`load_resources` 在启动时索引全部 `.mdd`。单词典可接受，目录模式（几百部）会撑爆内存和启动时间。

## 二、设计决策

### 1. CLI 与发现

```
prev <path>... [--port N] [--base PATH]
```

- 每个 path 是 `.mdx` 文件或目录；目录递归扫描 `*.mdx`（大小写不敏感的后缀匹配），不跟随符号链接目录；结果按路径字典序排序，重复的真实路径去重。
- 每部词典的 `.mdd` / `.mdd.N` 沿用现有 `mdd_candidates`（同名同目录）。
- 单个词典头解析失败：`eprintln!("prev: skipping {path}: {e}")` 后继续；全部失败退出 1；没有可加载项退出 2。
- 启动摘要：`prev: 3 dictionaries, 17582 entries`，随后 `listening on ...`。单词典时保持现有输出格式不变（`prev: 872 headwords from x.mdx`）。

### 2. 懒加载（目录模式的前提）

启动只做“打开”，不做索引：

| 成本 | 内容 | 时机 |
|---|---|---|
| 低（每部都做） | `mmap`、头解析、记录块目录（`blocks/starts/ends`）、key block 偏移表（`Encrypted=2` 的 kb_info 解密） | 启动 |
| 高 | key block 解压 → `words` + `sorted`；`lower`；`initials` | 首次需要词表时（`suggest_words`/`jump`/`random`/`lookup`/`@@@LINK`） |
| 高 | `.mdd` 索引（`mdds/res`） | 首次请求资源时 |

- `Mdx::open(path)` 拆出现在的 `load_mdx` 前半段；`Mdx::index()` 产出 `KeyIndex { words: Vec<String>, sorted: Vec<u32> }`。
- `Dict` 内使用 `OnceLock` 缓存：`index: OnceLock<Result<KeyIndex, String>>`（失败也缓存，避免反复重试）、`lower: OnceLock<Vec<String>>`、`initials: OnceLock<Vec<(String, usize, usize)>>`、`resources: OnceLock<Result<Resources, String>>`。
- 词条正文 `text(id)` 只依赖 `blocks/starts/ends`，索引未建时也能打开词条；`@@@LINK` 解析需要 `sorted`，触发索引构建（可接受）。
- 计数：`/api/parser` 优先用头里的 `num_entries`（无需索引），索引建好后以 `words.len()` 为准（两者不一致时以实际为准）。
- 首次激活的索引构建在 worker 线程内同步执行，浏览器等待期间前端显示“正在读取索引…”。失败时该词典返回 500，UI 显示错误文本，不崩溃。

### 3. 数据模型

```rust
struct AppState {
    dicts: Vec<Arc<Dict>>,          // 加载顺序即 id
    tx: broadcast::Sender<String>,
}

struct Dict {
    id: usize,
    name: String,                   // 文件名词干，重名时 UI tooltip 显示完整路径
    path: PathBuf,
    mdx: Mdx,                       // 头 + 记录块表；词表惰性
    index: OnceLock<Result<KeyIndex, String>>,
    lower: OnceLock<Vec<String>>,
    initials: OnceLock<Vec<(String, usize, usize)>>,
    resources: OnceLock<Result<Resources, String>>,
}

struct Resources { mdds: Vec<Mdd>, map: HashMap<String, Resource> }
```

`mdds/res` 从 `AppState` 移入 `Dict`；`Resource.mdd` 索引仍相对该词典自己的 `mdds`。

### 4. HTTP 契约（破坏性变更，唯一消费者是自带前端）

| 现在 | 之后 |
|---|---|
| `GET /api/parser` → `{name,count}` | `GET /api/parser` → `{dicts:[{id,name,count,path}], total}` |
| `GET /api/item/{k}` | `GET /api/d/{d}/item/{k}` |
| `GET /api/lookup/{w}` | `GET /api/d/{d}/lookup/{w}` |
| `GET /api/res/{p}` | `GET /api/d/{d}/res/{p}` |
| fallback 在唯一词典查资源 | fallback 按加载顺序在全部词典查，首个命中返回（重名资源歧义见风险） |
| — | `d` 非数字或越界 → 404，body `unknown dictionary` |

- `render_item` 的 `{base}` 值改为 `{base}/api/d/{d}/res/`，`item.html` 模板本身不变。
- `d` 是进程内加载顺序下标，不持久化；前端持久化的是词典名。

### 5. WebSocket 契约

请求体增加必填 `dict`：

```json
{"id":1,"method":"suggest_words","dict":0,"params":["",0,false]}
```

- 所有现有方法（`suggest_words`/`load_text`/`random`/`initials`/`jump`/`redirect`）都从 `dict` 取目标词典；缺失或越界返回 `{"id":..,"result":null,"error":"unknown dictionary"}`。
- 广播事件 `redirect` 追加 `dict` 字段：`{"event":"redirect","dict":0,"params":[k_id]}`。

### 6. 前端

- 搜索行内新增词典按钮（显示当前词典名），点击或 `Ctrl+K` 打开选择浮层：过滤输入框 + ↑/↓ + Enter，Esc 关闭；`Alt+1..9` 直选前九部。样式复用侧栏配色，浮层绝对定位在按钮下。
- `state.dict` 为当前 id；`dictStates[id] = {items, base, hasMore, selected, query, desc, scrollTop, k_id}`，切换时保存快照、恢复目标词典快照（首次为空则新建）。
- `rpc(cb, method, ...args)` 统一注入 `state.dict`；切换词典后重新拉 `initials`，状态栏立即更新。
- iframe：`/api/d/<dict>/item/<k>`；`navigate(k, remember)`、`record_history`、`go_history`、`pick`、`random` 全部携带 `(dict, k)`；历史项为 `{d,k}`，Alt+←/→ 可跨词典回退（切词典 + 定位词条）。
- 状态栏：`3613 词条 · sample (1/3)`；索引构建中显示 `正在读取索引…`；词典加载失败显示该词典错误。
- 持久化：`preview.dict` 存词典**名**，启动时按名匹配，找不到回落到 0；`preview.desc` 保持全局（不做每词典排序，YAGNI）。

### 7. 错误与降级

- 目录里损坏的 `.mdx`：启动跳过并打印；不影响其余词典。
- 索引构建失败（key block 损坏）：该词典 500 + UI 错误，其他词典不受影响。
- `.mdd` 全部失败：词条仍可看，图片 404（现状行为）。
- 未知 `dict`：404（HTTP）/ error 文本（WS）。
- 单词典启动行为与现有输出一致，`prev x.mdx` 仍可直接用。

## 三、任务分解

> 每个任务结束：`cargo build --release` 通过 → 最小验证 → 在 tx 提交。验证命令中的 `X` 为一份真实词典，`Y` 为另一份（没有第二份时把 `sample.mdx` 复制成两个不同文件名，用不同 hash 的文件头不行——词表相同也可验证命名空间与 base）。

### T1 CLI 多路径与目录发现

- 文件：`src/main.rs`（`main`、新增 `discover(paths: &[String]) -> Vec<PathBuf>`）
- 内容：位置参数收集为 `Vec<String>`；`discover` 递归展开目录、按 `*.mdx` 过滤、排序、`fs::canonicalize` 去重；空结果 → `usage()`；每项在 T2 打开后汇总打印。
- 验证：`./prev DIR --port 3120` 打印 `prev: N dictionaries, M entries`；`./prev DIR/a.mdx DIR/b.mdx` 与 `./prev DIR` 得到相同列表；不存在路径退出 1。

### T2 Mdx 拆分打开/索引，Dict 懒加载

- 文件：`src/main.rs`（`Mdx`、`load_mdx` → `Mdx::open` + `Mdx::index`、`Dict`、`load_resources` → `Dict::resources`）
- 内容：见“设计决策 2/3”；所有读取词表处改为 `dict.index()?`，读取资源处改为 `dict.resources()?`。
- 验证：启动两部词典（一部 3613 词条）后 `/api/parser` 立即返回两个 count，且启动耗时 < 0.2 s；首次 `suggest_words` 返回正确词表；`/api/d/0/item/1` 索引未建时仍返回词条；RSS 在首次搜索后才增长（`ps -o rss` 采两次）。

### T3 HTTP 路由命名空间 + parser 列表

- 文件：`src/main.rs`（`parser`/`item`/`lookup`/`res_file`/`fallback`/`render_item`/`routes`）
- 验证：
  - `/api/parser` 返回 `dicts` 数组，长度 = N；
  - `/api/d/0/item/1` 的 HTML 含 `<base href=".../api/d/0/res/"`，`/api/d/1/item/1` 含 `/api/d/1/res/`；
  - `/api/d/9/item/1` → 404；
  - 把某部词典的 `.mdd` 里某图片路径记下，`/api/d/<其下标>/res/<path>` 200，另一部 404；
  - 根绝对路径资源经 fallback 命中（用含 `/xxx.png` 的词条）。

### T4 WS 加 dict

- 文件：`src/main.rs`（`ws` 处理函数）
- 验证：同一连接分别发 `dict:0` / `dict:1` 的 `suggest_words`，得到各自词表；`dict:9` 返回 `unknown dictionary`；`redirect` 广播带 `dict`。

### T5 前端切换器与每词典状态

- 文件：`src/static/index.html`、`src/static/site.css`、`src/static/utils.js`
- 验证（gb + chromium `--dump-dom`）：初始状态栏 `3613 词条 · sample (1/2)`；点按钮/`Ctrl+K` 弹出列表，↑/↓/Enter 切换到第二部后状态栏与词表均变；`Alt+2` 直选；切回第一部恢复上次的选择与滚动位置；搜索词按词典隔离。

### T6 历史、随机、首字母、持久化

- 文件：`src/static/utils.js`
- 验证：在词典 A 打开词条 → 切到 B 打开词条 → `Alt+←` 回到 A 且定位同一词条；`F5` 只随机当前词典；刷新页面回到上次词典（按名字）；排序按钮行为不变。

### T7 文档与冒烟脚本

- 文件：`README.md`、`tests/smoke.py`（新增，仅标准库）
- `smoke.py <prev二进制> <词典目录>`：启动 → 断言 parser 列表条目数 → 每部 item 的 base 前缀正确 → lookup 每部一致 → WS `suggest_words` 带 dict（含越界错误）→ fallback 资源 → 结束。
- README：用法改为 `prev <path>...`、目录模式说明、切换键、状态栏文案、阶段二未实现说明。
- 验证：`python3 tests/smoke.py ./target/release/prev /path/to/dir` 退出 0。

## 四、验证矩阵

| 场景 | gb（Linux） | 本机（Windows exe） |
|---|---|---|
| 单词典启动 | 输出/接口与现状一致 | 同 |
| 目录加载 2 部 | parser 列表、切换、各自词条与资源 | 同 |
| 损坏 mdx | 跳过并继续 | 同 |
| 索引懒加载 | 启动时 RSS/耗时 | 不测（无 perf 要求） |
| 浏览器 UI | chromium `--dump-dom` 断言状态栏/词表 | 人工打开确认一次 |

构建命令（gb）：

```sh
cd /data/home/mdx-preview
cargo build --release
cargo zigbuild --release --target x86_64-pc-windows-gnu
```

## 五、风险与边界

- **资源重名**：根绝对路径 `/x.png` 在两部词典都存在时按加载顺序取第一个，可能是错的图；相对路径经 per-dict `<base>` 不受影响。README 里写明；合并视图阶段再考虑按词条归属解析。
- **首次激活延迟**：百万词条词典建索引约 0.4 s（当前实测启动 0.45 s 含索引），切换时会有一次卡顿；UI 必须显示“正在读取索引…”，否则像卡死。
- **内存**：目录模式只常驻头/块表；每部词典被实际使用的索引常驻且不释放（与现状一致）。若以后需要，再加 LRU 释放。
- **`@@@LINK` 跨词典**：阶段一仍在同一词典内解析；不同词典之间的链接不跳。
- **URL 破坏性变更**：`/api/item/*`、`/api/res/*`、`/api/lookup/*` 去掉；无外部消费者，README 不承诺兼容。
- **名字重复**：同名词干的多部词典 id 不同、显示名相同；选择器 title 显示完整路径，阶段一不做重命名。

## 六、阶段二（不在本计划内）

合并视图需要：合并词头索引（按规范化词头排序 + 每部词典的命中位图，惰性构建）、`/api/merged/suggest`、`/api/merged/item`（返回多段 HTML 并用分隔条/词典名标注）、左栏行样式与“命中 n 部”标记。接口新增即可，不需要再改阶段一的 `/api/d/*`。
