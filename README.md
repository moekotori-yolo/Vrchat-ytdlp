# Vrchat-ytdlp
# vrc-ytdlp 项目结构说明

这份文档面向刚接手项目的同学，目标是用尽量短的时间回答三个问题：

1. 这个项目是干什么的
2. 代码入口和主流程在哪里
3. 每个目录/文件分别负责什么

## 1. 项目作用

`vrc-ytdlp` 是一个基于 Rust 的 `yt-dlp` 包装器，主要服务于 VRChat 视频播放场景。

它有三种运行模式：

- 普通 CLI 模式：直接调用 `yt-dlp`，把参数透传给它执行
- 可视化 UI 模式：无参数启动或显式传 `--ui` 时，启动本地控制台界面
- 本地媒体服务模式：当参数里包含 `--get-url` 且 URL 是 YouTube 时，先启动本地 HTTP 服务，再把请求改写为本地可访问的流媒体地址

这样做的目的，是把 YouTube 直链提取、HLS 重定向、下载转封装、缓存复用这些逻辑统一收口到本地服务里，尽量兼容 VRChat 播放器的行为。

## 2. 目录结构

当前仓库结构比较精简，核心逻辑基本都在 `src/` 下：

```text
vrc-ytdlp-main/
├─ .github/
│  └─ workflows/
│     ├─ rust_build.yml
│     └─ rust_release.yml
├─ src/
│  ├─ main.rs
│  ├─ lifecycle.rs
│  ├─ server.rs
│  ├─ pipeline.rs
│  ├─ cache.rs
│  ├─ downloader.rs
│  ├─ executor.rs
│  ├─ ui.rs
│  └─ util.rs
├─ build.ps1
├─ setup.ps1
├─ config.json
├─ Cargo.toml
├─ Cargo.lock
└─ .gitignore
```

## 3. 根目录文件说明

### `Cargo.toml`

Rust 项目的清单文件，定义了：

- 包名：`vrc-ytdlp`
- 二进制入口：`src/main.rs`
- 主要依赖：`tokio`、`reqwest`、`axum`、`serde`、`tracing`
- Windows 专用依赖：`windows-sys`

从依赖上可以快速看出，这个项目同时做了几件事：

- 命令行进程管理
- HTTP 服务
- 异步下载与子进程调用
- 配置读写
- 日志记录

### `config.json`

运行配置样例。程序启动时会从可执行文件同目录读取它。

当前示例配置主要覆盖：

- `yt-dlp` 和 `ffmpeg` 路径
- 允许透传的参数
- 默认附加参数
- cookies 开关
- 执行超时
- 更新检查周期
- yt-dlp 插件目录
- extractor 参数

需要注意的是，源码里的 `Config` 结构体字段比这个示例文件更多，比如：

- `server_port`
- `server_idle_timeout_secs`
- `bgutil_pot_port`
- `cache_dir`
- `cache_max_size_mb`
- `cache_ttl_secs`

也就是说，即使 `config.json` 没显式写出这些字段，程序也会使用 `main.rs` 里的默认值。

### `setup.ps1`

初始化脚本，用来下载运行依赖到 `tools/` 目录：

- `yt-dlp.exe`
- `ffmpeg.exe`
- `ffprobe.exe`
- yt-dlp 插件

适合首次部署或手动更新依赖时使用。

### `build.ps1`

打包脚本，职责比 `setup.ps1` 更完整：

1. 构建 Rust release 二进制
2. 下载或复用 `yt-dlp` / `ffmpeg` / `ffprobe`
3. 下载 yt-dlp 插件
4. 组装 `dist/` 下的发布目录和 zip 包

### `.github/workflows/`

CI/CD 配置：

- `rust_build.yml`：在 `push`/`pull_request` 时做构建检查
- `rust_release.yml`：手动触发发布流程，自动 bump 版本、打 tag、构建并上传 release 资产

## 4. `src/` 模块职责

### `src/main.rs`

项目主入口，也是最值得先读的文件。

它主要负责：

- 解析命令行参数
- 加载配置和日志
- 解析 `yt-dlp` / `ffmpeg` 路径
- 确定当前是普通模式还是 `--serve` 服务模式
- 在普通模式下决定：
  - 直接调用 `yt-dlp`
  - 或者走“本地服务 + 流媒体地址注册”这条链路

关键判断逻辑：

- 如果存在 `--serve`，进入后台媒体服务模式
- 否则进入普通 CLI 模式
- 如果参数里包含 `--get-url`，并且 URL 是 YouTube，则优先走本地服务
- 非 YouTube 请求直接透传给 `yt-dlp`

这说明项目的核心设计不是“接管所有站点”，而是“只对 YouTube 特殊处理，其它站点尽量保持原生 yt-dlp 行为”。

### `src/lifecycle.rs`

服务模式的顶层协调器。

它把后台服务相关的生命周期收口到一个地方，负责：

- 启动 `bgutil-pot` 子进程
- 注册 Ctrl+C / SIGTERM 信号处理
- 启动 HTTP 媒体服务
- 处理退出时的清理逻辑
- 在 `bgutil-pot` 崩溃时自动重启

Windows 下还会创建 Job Object，确保父进程退出时子进程一并退出。

如果你要排查“服务为什么没跟着退出”或者“bgutil-pot 为什么反复重启”，这里是第一入口。

### `src/server.rs`

本地 HTTP 服务实现，是真正承接播放请求的地方。

它暴露了三个路由：

- `GET /health`：健康检查
- `POST /stream`：注册一个播放请求，返回 `stream id`
- `GET /stream/{id}`：真正向播放器提供内容

内部状态 `AppState` 维护了：

- 已注册的流请求
- 自增 `stream id`
- 最近活动时间
- 正在运行的 pipeline 数量
- 全局服务配置
- 视频缓存

`GET /stream/{id}` 是整条业务链的核心：

1. 先根据 `stream id` 找到原始视频 URL 和 `yt-dlp` 参数
2. 解析 `Range` 请求头，支持播放器拖动/跳转
3. 优先查缓存
4. 缓存未命中时，先尝试 Mode 1：提取 HLS 地址并 302 重定向
5. 如果 HLS 不可用，再走 Mode 2：下载、转封装、落盘缓存后再返回 MP4

另外这里还做了两件很重要的运维型工作：

- 定时清理长时间未访问的 stream 注册记录
- 空闲超时后自动关闭服务

### `src/pipeline.rs`

下载与转封装流水线，是媒体处理层的核心模块。

它实现了两种策略：

#### Mode 1：HLS passthrough

调用 `yt-dlp --get-url` 提取真实播放地址，如果判断为 HLS 且校验通过，就直接让播放器跳到该地址。

优点：

- 启动快
- 不需要先完整下载

缺点：

- 依赖上游 HLS 可访问性
- 对源站/签名/时效性更敏感

#### Mode 2：download + remux

当 HLS 不可靠时，退回到更稳的模式：

1. 用 `yt-dlp` 下载媒体
2. 用 `ffprobe` 检查音视频编码
3. 用 `ffmpeg` 做 remux，必要时转码为更兼容的 H.264/AAC
4. 输出为支持 Range 的标准 MP4

这个模式更慢，但对 VRChat 播放器更稳定。

这个文件里还包含几类关键辅助逻辑：

- `yt-dlp` 参数拼装
- `ffmpeg` / `ffprobe` 调用
- `TEMP/TMP/PATH` 环境修正
- Windows 下隐藏子进程窗口
- `yt-dlp` stderr 日志分级

### `src/cache.rs`

视频缓存模块，负责把 Mode 2 的结果复用起来，避免重复下载。

核心能力包括：

- 用 URL 哈希生成稳定缓存 key
- 管理 `.tmp`、`.mp4`、`.meta` 文件
- 启动时扫描磁盘重建缓存索引
- TTL 过期清理
- 按最近最少使用进行容量淘汰
- 合并同 URL 的并发下载请求
- 支持后台缓存下载状态跟踪
- 从缓存文件直接返回带 Range 支持的 HTTP 响应

这里是项目里“工程化味道”最重的一层，因为它同时处理了：

- 并发控制
- 磁盘状态恢复
- HTTP Range
- 缓存淘汰

如果后续要扩展缓存策略，这里会是主要改动点。

### `src/downloader.rs`

`yt-dlp` 自更新模块。

主要职责：

- 检查本地是否已有 `yt-dlp`
- 根据 `version.txt` 的修改时间决定是否检查更新
- 从 GitHub Release 拉取最新版本
- 下载完成后原子替换本地二进制

这部分逻辑只负责 `yt-dlp` 本体，不负责 `ffmpeg`。

### `src/executor.rs`

普通 CLI 模式下的执行器。

当请求不需要走本地媒体服务时，这里直接启动 `yt-dlp` 子进程，并负责：

- 设置临时目录
- 透传 stdout/stderr
- 超时等待
- Windows Job Object 清理

可以把它理解为“最薄的一层命令包装”。

### `src/ui.rs`

本地可视化界面模块。

它会启动一个只监听 `127.0.0.1` 的 Web 控制台，并提供几类能力：

- 查看工具状态和媒体服务状态
- 生成 VRChat 可播放的本地地址
- 直接执行 `yt-dlp` 并查看输出
- 编辑并保存 `config.json`
- 手动触发 `yt-dlp` 更新检查

这个模块本质上是对现有 CLI 和服务逻辑做了一层更友好的可视化封装，不替代底层下载、缓存和服务实现。

### `src/util.rs`

只有一个工具函数：返回当前 Unix 时间戳秒数。

虽然简单，但它被多个模块复用：

- 服务空闲时间计算
- stream 访问时间记录
- 缓存 TTL / LRU 逻辑

## 5. 运行流程

### 普通模式

入口：`main.rs`

流程如下：

1. 读取配置
2. 确保 `yt-dlp` 存在，必要时自动下载/更新
3. 过滤用户参数，只保留允许透传的选项
4. 判断请求是不是 `--get-url + YouTube URL`
5. 如果不是，直接调用 `executor.rs` 中的 `run_ytdlp`

### 服务模式

入口：`main.rs --serve` -> `lifecycle::run_managed_server` -> `server::run_server`

流程如下：

1. 解析服务端口和空闲超时
2. 加载 `yt-dlp` / `ffmpeg` / 插件目录 / 缓存目录配置
3. 若存在 `bgutil-pot` 可执行文件，则一起拉起
4. 启动本地 HTTP 服务
5. 等待客户端注册 stream
6. 按缓存命中、HLS 重定向、下载转封装的顺序处理请求
7. 空闲超时后自动退出

### YouTube `--get-url` 特殊处理流程

这是项目最关键的一条链路：

1. 用户调用 CLI，请求 `--get-url <youtube-url>`
2. `main.rs` 检查本地服务是否在线
3. 如果不在线，后台拉起一个 `--serve` 进程
4. CLI 通过 `POST /stream` 注册该视频请求
5. 服务返回一个 `stream id`
6. CLI 输出本地地址 `http://127.0.0.1:<port>/stream/<id>`
7. VRChat 或播放器随后访问这个本地地址
8. 服务端再决定是走 HLS 直链还是本地下载缓存

也就是说，CLI 只负责“注册请求并返回本地 URL”，真正的数据传输发生在 HTTP 服务中。

## 6. 接手时建议优先阅读顺序

如果你是第一次进项目，建议按下面顺序读源码：

1. `src/main.rs`
2. `src/server.rs`
3. `src/pipeline.rs`
4. `src/cache.rs`
5. `src/lifecycle.rs`
6. `src/ui.rs`
7. `src/downloader.rs`
8. `src/executor.rs`

这样能先建立主流程，再去看辅助模块，不容易迷路。

## 7. 当前项目的一些实现特点

### 只对 YouTube 做特殊分流

源码里明确只把 YouTube URL 导向本地媒体服务，其它站点交回 `yt-dlp` 原生处理。

### 强依赖外部工具

Rust 本身不是直接下载和转码媒体，而是把这些能力委托给：

- `yt-dlp`
- `ffmpeg`
- `ffprobe`
- 可选的 `bgutil-pot`

因此很多问题排查时要区分：

- 是 Rust 逻辑问题
- 还是外部工具、网络、签名、cookies、插件导致的问题

### Windows 兼容性考虑很多

代码里有不少 Windows 特化处理，例如：

- Job Object
- `CREATE_NO_WINDOW`
- `DETACHED_PROCESS`
- temp 目录修正

这说明当前项目的主要运行环境大概率是 Windows，尤其是和 VRChat 配合使用的场景。

## 8. 建议后续补充的文档

如果后面要继续完善交接材料，比较值得补的还有：

- 一份“从用户输入到播放器取流”的时序图
- 一份“配置项逐条说明”
- 一份“常见故障排查手册”
- 一份“发布/打包流程说明”

其中“配置项说明”和“故障排查”对后续维护最有帮助。
