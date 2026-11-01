# voltdir

**阅读语言：** [English](README.md) · [正體中文](README.zh-TW.md) · 简体中文 · [한국어](README.ko.md) · [日本語](README.ja.md) · [Tiếng Việt](README.vi.md)

**为 AI agent 打造的 Windows 磁盘分析工具。** 一次调用就能看清一块硬盘：哪里最大、哪种扩展名占得最多、哪里有空文件、这几天多了什么。只读、单个可执行文件、输出格式固定的 JSON。

| 含 100 万个文件的文件夹 | 整个 `C:\`（管理员） | agent 连续追问 6 个问题 |
|:---:|:---:|:---:|
| 内存 **约省 11 倍**<br>14.7 MB 对 163 MB | 耗时 **约快 5.7 倍**<br>6.6 秒对 37.5 秒 | 耗时 **约快 4.8 倍**<br>46 秒对 220 秒 |

对照组都是写得很好的 PowerShell／.NET 脚本，取其中最强的一个；每项各跑 5 次取中位数。完整数据以及**尚未胜出的地方**见 [`benchmarks/RESULTS.md`](benchmarks/RESULTS.md)，用 [`benchmarks/`](benchmarks/README.md) 里的脚本可以在你自己的机器上复现。

- **只读：** 只读取名称、大小、时间和属性；不读文件内容、不删除、不移动、不联网、不自行提权。
- **单个 `voltdir.exe`**（x64，约 0.75 MB）：静态链接 C 运行时，只导入 Windows 自带的 DLL，无需另外安装任何依赖。
- **给 agent 的 skill：** [`skills/voltdir/SKILL.md`](skills/voltdir/SKILL.md)（约 4,500 字符），附带构建好的可执行文件，第一步总是 `overview`。
- 状态：**v1 候选版**，仅支持 Windows x64。

---

## 安装

### 作为 Claude Code 插件

```text
/plugin marketplace add CubeLink-oss/voltdir
/plugin install voltdir@voltdir
```

插件包含 skill 和构建好的 `voltdir.exe`（[`skills/voltdir/bin/`](skills/voltdir/bin)）。遇到“硬盘为什么满了”“找出大文件”“这周多了什么”之类的问题时，agent 会自动使用它。

### 直接使用可执行文件

把 [`skills/voltdir/bin/voltdir.exe`](skills/voltdir/bin) 复制到任意文件夹即可使用，无需安装。可执行文件目前还没有代码签名，首次运行时 Windows 可能弹出 SmartScreen 警告。运行前可以先确认它确实由本仓库源码构建，见[校验附带的可执行文件](#校验附带的可执行文件)。

---

## 快速上手

```powershell
# 第一次调用：总量、最大的文件夹和文件、扩展名、长期未修改的大文件、空文件，
# 并附上可直接运行的 next 命令
voltdir overview D:\Projects

# 在同一次扫描中保存精简索引，之后追问无需重新扫描
voltdir overview D:\Projects --save-index C:\Temp\p.idx
voltdir report --index C:\Temp\p.idx --top-dirs 20 --by-ext 15

# 需要 find 或 diff 时才保存完整快照
voltdir overview D:\Projects --save-snapshot C:\Temp\p.ndjson
voltdir find --snapshot C:\Temp\p.ndjson --ext .log --min-size 100M --sort size --limit 20
voltdir diff C:\Temp\monday.ndjson C:\Temp\friday.ndjson --match-roots
```

扫描整块磁盘时，请**以管理员身份**运行 PowerShell，voltdir 才能使用直接读取 NTFS 主文件表（MFT）的 `mft` 引擎。普通权限下会自动改用 `walk` 引擎，部分系统文件夹会无法读取，输出中的 `coverage` 会逐一列出。

### agent 如何使用

```mermaid
flowchart LR
    A["用户：硬盘为什么满了？"] --> B["voltdir overview 路径<br/>--save-index x.idx"]
    B --> C{"还有追问吗？"}
    C -- "最大的文件夹、扩展名、空文件" --> D["voltdir report --index x.idx<br/>约 0.03 秒，无需重扫"]
    D --> C
    C -- "查找特定文件，或比较两个时间点" --> E["--save-snapshot<br/>再用 find／diff"]
    C -- "没有" --> F["回答用户"]
    E --> F
```

---

## 基准测试

以下数据均来自 `benchmarks\` 中可复现的基准测试。每项跑 5 次取中位数；测量时如有后台进程占用 CPU，会标注噪声警告。

### 内存：含 100 万个文件的文件夹

![峰值内存：含 100 万个文件的文件夹](assets/zh-CN/chart-memory.svg)

voltdir 边遍历边汇总：一个文件夹遍历完就并入上一层，不保留逐个文件的记录。对照组的 163 MB 大部分是 `pwsh` 宿主本身，原生编译的 .NET 程序会更低。

### 整个 C:\（管理员）

![整个 C:\，约 310 万个文件](assets/zh-CN/chart-mft.svg)

### agent 连续追问

![agent 连续追问 6 个问题](assets/zh-CN/chart-agent.svg)

同一个模型、同一组问题、同样的权限，两组各跑 5 次，全部答对。差距来自追问：保存索引后，每个追问约 3 到 8 秒；对照组每个追问要花 7 到 33 秒重新计算。agent 层面的对比需要 agent 运行环境，方法见 [`benchmarks/RESULTS.md`](benchmarks/RESULTS.md)。

### 尚未胜出的地方

![单次扫描：含 100 万个文件的文件夹](assets/zh-CN/chart-scan.svg)

| 项目 | 现状 |
|---|---|
| 普通权限下的单次扫描 | 100 万个文件只比最强的 .NET 对照组快约 1.7 倍；整个 `C:\` 反而更慢（0.78 倍）。瓶颈在于 NTFS 打开目录的开销 |
| token 用量 | 与对照组相近，voltdir 组多用 4% 到 10%。每次 agent 调用约有 7.3 万 token 的固定开销，任何工具都无法减少 |
| 提权扫描整盘的内存 | 186 MB，与 .NET 流式写法（177 MB）持平，没有领先 |

原因分析见 [`benchmarks/RESULTS.md`](benchmarks/RESULTS.md)。

---

## 子命令

| 命令 | 用途 | 输出 |
|---|---|---|
| `overview <PATH>... \| --snapshot <FILE> \| --index <FILE>` | **一次回答常见问题**（约 3 KB）：总量、最大的文件夹和文件、扩展名分布、长期未修改的大文件、空文件和空文件夹、未能读取的位置，以及可直接运行的 `next` 命令。选项：`--save-index`、`--save-snapshot`、`--rank physical\|logical`、`--dir-rank rollup\|direct`、`--detail concise\|full` | 单个 JSON（`voltdir.overview/1`） |
| `report <PATH>... \| --snapshot <FILE> \| --index <FILE>` | 直接汇总：`--top-files`、`--top-dirs`、`--by-ext`、`--many-files` | 单个 JSON（`voltdir.report/1`） |
| `find <PATH>... \| --snapshot <FILE>` | 按名称（通配符或 `--name-regex`）、路径、类型、扩展名、大小、日期、是否为空进行搜索 | 单个 JSON（`voltdir.find/1`） |
| `diff <A> <B>` | 比较两份快照：变大、变小、新增、消失。两份快照路径不同时加 `--match-roots`（或 `--a-root`／`--b-root`） | 单个 JSON（`voltdir.diff/1`） |
| `scan <PATH>... --out <FILE>` | 只输出**快照**。快照可能有数百 MB，因此 `--out` 为必填；要输出到标准输出请明确写 `--out -` | NDJSON（`voltdir.snapshot/1`） |
| `info` | 环境与能力：版本、是否提权、可用引擎、卷 | 单个 JSON |

常用选项：`--out <FILE|->`、`--engine auto|mft|walk`、`--threads <N>`、`--exclude <GLOB>`、`--follow-links mount|symlink|junction`、`--hardlinks dedupe|none`、`--timeout-sec <N>`、`-q`。完整语法请运行 `voltdir --help` 或 `voltdir help <子命令>`。

## 两个引擎

| | `walk` | `mft` |
|---|---|---|
| 需要管理员权限 | 否 | **是**（读取卷的 MFT） |
| 范围 | 任意文件夹、任意文件系统 | 仅限本地 NTFS 驱动器的**根目录**，例如 `C:\` |
| 整个 `C:\`（约 310 万个文件） | 约 54 秒 | **约 6.6 秒，峰值 186 MB** |
| 可见范围 | 受权限限制；看不到 `$MFT` 等保留项 | 能看到保留项（标记为 `reserved: true`）和 `System Volume Information` 的内容 |

`--engine auto`（默认）在条件满足时使用 `mft`，否则改用 `walk` 并在 stderr 说明原因；`--engine mft` 为严格模式，无法使用时以退出码 3 结束。

## 退出码

| 退出码 | 含义 |
|---|---|
| 0 | 完全成功 |
| 1 | 用法错误，包括无法使用的快照或索引，以及 `diff` 的两份快照根目录不重叠却没加 `--match-roots` |
| 2 | 结果有效，但**部分文件夹未能读取**（多为访问被拒绝）；`coverage` 会列出位置，**不能当作完整结果** |
| 3 | 指定的引擎不可用（例如未提权却指定 `--engine mft`，或卷不是 NTFS） |
| 4 | 写入输出文件失败（不会留下写了一半的文件） |
| 5 | 超时或被取消（已扫描的部分仍会输出） |

## 输出格式要点

- 路径为 Win32 形式，不带 `\\?\` 前缀；未配对的 UTF-16 代理字符以 `\uXXXX` 转义，不会丢失。控制字符和双向文本字符一律转义，防止有人借助特制的文件名对 agent 进行提示注入。
- 时间为 UTC 的 RFC 3339 格式；大小为以字节为单位的整数。`size_logical` 是文件结尾位置，`size_physical` 是磁盘上实际分配的空间。
- 顺序固定：同一份输入、同一个引擎，输出逐字节相同。
- **硬链接**的实际大小只计一次；逻辑大小则每个名称各计一次。

## 构建与检查

需要 Rust 1.94 或更高版本（MSVC 工具链）。

```powershell
cargo build --release          # 生成 target\release\voltdir.exe
.\scripts\ci.ps1               # 格式、clippy、全部测试、许可证检查、基准脚本自测、skill 检查
```

复现基准测试的步骤见 [`benchmarks/README.md`](benchmarks/README.md)。

## 校验附带的可执行文件

[`skills/voltdir/bin/`](skills/voltdir/bin) 中的 `voltdir.exe` 并非另行获取的文件，而是用 `cargo build --release` 从本仓库源码构建而来；其 SHA-256 哈希值记录在旁边的 `voltdir.exe.sha256` 中。构建过程是确定性的：链接器带有 `/Brepro`，可执行文件中不含时间戳，也不含构建机器上的路径。同一个 commit、同一套工具链，构建出的字节完全相同。

发布的可执行文件使用 **Rust 1.94.0**（`x86_64-pc-windows-msvc`）构建。自行校验的步骤：

```powershell
git clone https://github.com/CubeLink-oss/voltdir
cd voltdir
rustup toolchain install 1.94.0
cargo +1.94.0 build --release
(Get-FileHash target\release\voltdir.exe -Algorithm SHA256).Hash
Get-Content skills\voltdir\bin\voltdir.exe.sha256
```

两个哈希值应当一致（前者以大写显示）。如果不一致，请确认 commit 相同、`rustc +1.94.0 -V` 显示 1.94.0；MSVC 链接器版本不同也可能导致字节不同。如果不想运行非自己构建的文件，改用你自己的 `target\release\voltdir.exe` 即可；skill 也接受已在 `PATH` 中的 `voltdir`。

有些安全扫描器只要发现 skill 附带可执行文件就会报警。这在意料之中，上面的步骤就是让你自行确认的方法。

## 仓库结构

| 位置 | 内容 |
|---|---|
| [`skills/voltdir/`](skills/voltdir) | 给 agent 的 skill：[`SKILL.md`](skills/voltdir/SKILL.md)、完整参考 [`reference.md`](skills/voltdir/reference.md)、构建好的 `bin/voltdir.exe` |
| [`.claude-plugin/`](.claude-plugin) | Claude Code 插件与插件市场配置 |
| [`benchmarks/`](benchmarks/README.md) | 可复现的基准测试脚本与[结果摘要](benchmarks/RESULTS.md) |
| `src/`、`tests/` | 源代码与测试 |
| `scripts/` | CI、许可证检查、测试数据生成器 |

## 许可证

[Apache License 2.0](LICENSE)，版权所有者见 [`NOTICE`](NOTICE)。
