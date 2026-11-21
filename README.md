# prev

单文件 MDict（`.mdx` / `.mdd`）浏览器预览器：一个可执行文件内嵌解析器、HTTP/WebSocket 服务与前端，用浏览器读词典。

- 词表常驻左侧，滚动分页；输入即过滤，命中高亮
- 词条正文在右侧 iframe 中渲染，`.mdd` 里的 CSS / JS / 图片按需取用
- 支持 MDict v1.x / v2.0、`Encrypted=2`、UTF-8 / UTF-16 / GBK 等编码、`@@@LINK=` 重定向
- 记录块内存映射 + 按需解压，常驻内存接近文件大小

## 快速开始

```sh
prev dictionary.mdx
# prev: 68584 headwords from dictionary.mdx
# prev: 18316 resources from 1 mdd file(s)
# listening on http://127.0.0.1:3030/mdx-preview/
```

打开打印出的地址即可。放在 GoldenDict 词典目录里运行时，会自动加载同名的 `dictionary.mdd`、`dictionary.1.mdd`、`dictionary.2.mdd`。

### 参数

```
prev <dictionary.mdx> [--port N] [--base PATH]

  --port N      监听端口，默认 3030（只绑定 127.0.0.1）
  --base PATH   公开挂载前缀，默认 /mdx-preview
```

- `--base ""` 表示挂在站点根路径（`http://127.0.0.1:3030/`）
- 前缀可以是 `mdx-preview`、`/mdx-preview/` 等写法，自动规范化
- 环境变量 `PREVIEW_BASE` 可覆盖 `--base`

## 界面与快捷键

| 操作 | 行为 |
|---|---|
| 启动 | 焦点直接落在搜索框，词表列出全部词头 |
| 输入 | 90 ms 防抖，按子串过滤，命中片段高亮 |
| ↑ / ↓ | 在词表中上下移动选择 |
| Enter | 打开选中词条（未选中时打开第一行） |
| Esc | 清空关键字，回到完整词表 |
| PgUp / PgDn | 按可视高度翻页；到已加载的任一端会继续向前/后取页，按住可连续翻 |
| Home / End | 无关键字时直接跳到词典的最前/最后一条；有关键字时跳到已加载结果的首/尾 |
| F5 | 随机查一个词条，词头填入搜索框并加载 |
| Ctrl+L | 回到搜索框并全选 |
| Alt+← / Alt+→ | 在访问过的词条间后退 / 前进 |
| 搜索框右侧按钮 | `↑` 正序 / `↓` 倒序（词典文件原序的正向或反向），刷新后保持 |
| 搜索框下方首字条 | 跳到该字符在词典中的第一处；CJK 等按出现位置均匀采样，最多 48 个桶；只有一个词头的桶不显示 |
| 左栏分隔条 | 拖动调整宽度（存 localStorage），双击复位 260 px |
| Ctrl + 滚轮 | 浏览器自带缩放，不再单独实现 |

状态栏显示词条总数与容器编码。

## 实现要点

- **解析**：单文件内实现 MDict v2/v1 读取，`Encrypted=2` 的 key block info 用 RIPEMD128 + 滚动 XOR 还原
- **内存**：`mmap` 整个 `.mdx`，只保留 key 索引与记录块目录；取词条时按需解压所在记录块并切片
- **编码**：按头部 `Encoding` 用 `encoding_rs` 解码（UTF-8 / GBK / Big5 / Shift_JIS …），UTF-16 走单独分支
- **重定向**：`@@@LINK=目标词头` 在请求时解析（最多 16 跳，带环检测）
- **资源**：`.mdd` 同样只建索引，按需读取压缩块；`/api/res/{path}` 提供资源，词条页用 `<base>` 把相对路径指向该路由，根绝对路径由兜底路由处理
- **挂载**：所有路由通过 `web::scope(--base)` 挂载，页面内生成的链接与路由前缀一致；裸 `/` 302 到前缀

## 性能（gb 实测：Debian 13，4 vCPU，8 GB）

| 词典 | 词条 | 启动 | 常驻内存 (RSS) | 取词条 p50 | 搜索 p50 |
|---|---|---|---|---|---|
| swift-mt | 872 | 0.06 s | 8.8 MB | 0.3 ms | 0.1 ms |
| Wiktionary EN 2021（CC BY-SA） | 977 956 | 0.45 s | 141.3 MB | 0.3 ms | 3.6 ms |

常驻内存的主体是词头索引；正文按需解压，不随词典体积线性占用。改造前会把全部正文解压进内存，数 GB 的词典无法加载。

## 内存实测（gb，2000 次取词 + 120 次搜索）

| 词典 | 起始 RSS / 匿名 | 压测后 RSS / 匿名 | 空闲 3 s 后 |
|---|---|---|---|
| swift-mt | 8.8 / 3.6 MB | 9.0 / 3.7 MB | 不变 |
| Wiktionary EN 2021 | 141.3 / 128.5 MB | 155.0 / 128.6 MB | 不变 |

压测后 RSS 的增长来自按需解压的记录块（约 14 MB），匿名内存（索引 + 堆）基本不变。

## 构建

```sh
cargo build --release
# 产物：target/release/prev
```

Windows 交叉编译（Linux 上）：

```sh
cargo install cargo-zigbuild
cargo zigbuild --release --target x86_64-pc-windows-gnu
# 产物：target/x86_64-pc-windows-gnu/release/prev.exe
```

仓库自带 GitHub Actions：Linux 构建 `prev`；Windows 用 zig 交叉编译 GNU 版 `prev.exe`，与本地 `cargo zigbuild` 产物同一目标（`x86_64-pc-windows-gnu`），不是 MSVC 构建。

## 已知限制

- LZO 压缩的词典不支持（v1 老词典偶见），会给出明确错误
- `Encrypted=1/3`（需要用户识别码）的词典拒绝加载；对应的 `.mdd` 会被跳过
- 记录块没有缓存，同一页大量图片会重复解压所在块（实测 95 张图仍为秒开）
- 词条页的 `<base>` 会改变相对链接语义，`#锚点` 已单独拦截处理；相对路径资源全部走 `.mdd`，缺失时 404
- 超大词典的索引常驻内存：百万级词条约 130–150 MB，随词头数量线性增长

## 目录

```
src/main.rs             解析器 + HTTP/WebSocket 服务
src/static/index.html   外壳：左栏词表 + 右侧词条 iframe
src/static/utils.js     前端逻辑：分页、排序、首字跳转、快捷键
src/static/site.css     外壳样式
src/static/item.html    词条模板（通用排版 + 资源 base）
```
