# mise for Lithe

让 [Lithe](https://github.com/1lck/Lithe-IDEA) 自动找到 [mise](https://mise.jdx.dev) 安装的 JDK、Node.js、Maven、Gradle、Go、Python 和 pnpm，
并知道项目里的 `mise.toml` / `.tool-versions` 要求哪个版本，不需要再在 Lithe 里手动添加路径。
这是一个通过 [Lithe Store](https://github.com/Lithe-IDEA/Lithe-store) 分发的**社区插件**。

> English: A Lithe toolchain provider. Its small backend asks mise itself which runtimes are installed and which versions
> your project and global config request, and hands those facts to Lithe.

## 工作方式

插件是 Lithe 工具链框架里的一个**提供方**（`toolchainProviders`，见 Lithe 插件契约的 `toolchains.md`）。
Lithe 需要知道某个工具有哪些版本时，向插件后端 `bin/mise-provider` 发 `toolchains/query`。后端运行 mise 自己的命令，把结果整理成答复：

| 后端运行 | 运行目录 | 得到 |
| --- | --- | --- |
| `mise ls --json --local` | 被查询的项目目录 | 项目要求：版本、来源文件、是否已安装 |
| `mise ls --json --installed` | `$HOME` | 已安装的版本 |
| `mise ls --json --global` | `$HOME` | 全局配置的要求 |
| `mise bin-paths <tool>@<version>…` | `$HOME` | 真实 home（例如 Maven 的 `apache-maven-3.9.16/`） |

- 版本怎么选，以及 `lts`、`latest`、前缀、`conf.d`、`MISE_ENV` 怎么解释，全部由 mise 决定。插件不读 mise 的配置文件，也不实现 mise 的任何规则。
- 最终用哪个版本由 Lithe 按「固定值 → 项目要求 → 全局 → 自动」决定，插件只报告事实。
- 要求了但没装的版本，答复里的 `installation` 为 `null`，Lithe 会提示「未满足」。插件不会替你安装。

## 安全

- **不执行 `mise trust`、`mise install`，也不运行 shims。** 后端只运行上表四条只读命令。
- 默认设置 `MISE_OFFLINE=1`，mise 不联网，可以在设置里关闭。
- 项目配置**未受信任**时（例如含 `[env]` 的 `mise.toml`），mise 拒绝在该目录下运行。
  - 这时项目查询返回错误，原文说明是哪个文件。请确认内容后，自己在终端运行 `mise trust`。
  - 全局查询从 `$HOME` 运行，不受影响，所以已安装的版本仍然可见。
- 以下内容不会上报：
  - 位于工作区内的安装，因为工作区里的程序属于项目代码；
  - 来自环境变量的要求（如 `MISE_JAVA_VERSION`），因为没有对应的文件；
  - 来源文件不在清单 `watchFiles` 里的要求。
- 后端只有一个文件，没有运行时依赖，也不依赖它要找的 Node 或 Java。
- 每次运行 mise 都有超时（4 秒）。取消或超时会结束 mise 所在的整个进程组；Lithe 断开时后端立即退出，不留子进程。

## 设置

在 **设置 → 插件 → 已安装 → mise → 设置** 中：

| 设置 | 默认 | 说明 |
| --- | --- | --- |
| mise 路径 | 空（自动查找） | 留空时依次查找 `~/.local/bin/mise`、Homebrew（`/opt/homebrew/bin`、`/usr/local/bin`）和 `PATH`。填写后只用这个路径；路径无效时报错，不会自动回落。 |
| 离线查询 | 开 | 查询时设置 `MISE_OFFLINE=1`。 |

## 协议细节

- **协商**：初始化时，Lithe 在 `capabilities.experimental.litheToolchains` 中提供 `{versions: [1], providers: ["mise"]}`。只有提供里同时有版本 1 和 `mise`，后端才回答 `{version: 1, providers: ["mise"]}`；否则不声明，也不接受查询。
- **查询范围**：`workspace: null` 的查询只返回已安装版本和全局要求；工作区查询还会返回项目要求。
- **监听**：`watch` 列出三类路径，任何一个变化，Lithe 都会整体重新查询：
  - 来源文件；
  - 全局配置：`~/.config/mise/config.toml`、`config.local.toml`、`conf.d`、`~/.tool-versions` 等；
  - mise 的安装目录。
- **去重**：别名链接（如 `installs/java/latest → 25.0.2`、`17 → 17.0.2`）不重复上报；缺少可执行文件的残缺安装也不上报。
- **上限**：答复不超过 1 MiB，安装不超过 512 个，要求不超过 256 条，`watch` 不超过 256 条。
  - 条数超过上限时截断。要求先保留，项目要求排在全局要求前面。
  - 安装里先保留被要求引用的那些，保证每条引用的安装都在列表里。
  - 截断后仍超过 1 MiB 时，返回 `-32075 plugin.toolchain.limitExceeded`。
- **错误**：按契约映射。`error.data` 只有 `code`、`operationID`、`providerID` 和可选的 `details`：

| 情况 | JSON-RPC code | `data.code` | `details.reason` |
| --- | --- | --- | --- |
| 配置未受信任 | -32073 | `plugin.toolchain.providerUnavailable` | `untrusted` |
| 找不到 mise，或设置的路径无效 | -32073 | `plugin.toolchain.providerUnavailable` | `miseNotFound` |
| mise 运行失败或输出无法解析 | -32073 | `plugin.toolchain.providerUnavailable` | `queryFailed` / `invalidOutput` |
| mise 超时 | -32072 | `plugin.toolchain.timeout` | — |
| `$/cancelRequest` | -32800 | `plugin.toolchain.cancelled` | — |
| 远程工作区 | -32071 | `plugin.toolchain.authorityUnsupported` | — |
| 参数错误 | -32602 | `plugin.toolchain.invalidParams` | — |

## 安装

需要带工具链提供方框架的 Lithe，即 `toolchainProviders` 能力：[1lck/Lithe-IDEA#1581](https://github.com/1lck/Lithe-IDEA/pull/1581) 加上之后的查询客户端。
不支持的 Lithe 版本会在插件市场中隐藏本插件。安装时，安装审查会列出后端程序 `bin/mise-provider`。

1. 在 Lithe 中打开 **设置 → 插件 → 插件市场**，搜索 “mise”，安装并确认。
2. 打开项目后，运行环境设置里会出现来自 mise 的版本。

## 开发

需要：

- Rust，含 `aarch64-apple-darwin` 和 `x86_64-apple-darwin` 两个 target；
- Node.js 22+；
- Xcode 命令行工具（`lipo`、`codesign`）；
- `zip`。

```bash
cd backend && cargo test     # 单元测试，以及用伪 mise 脚本驱动真实后端的 stdio 测试
cd .. && node scripts/package.mjs
```

打包脚本：

- 分别构建两个架构，用 `lipo` 合成 universal binary，并做 ad-hoc 签名；
- 输出 `dist/mise-<版本>-darwin.zip`（同时适用 Apple Silicon 与 Intel Mac）和它的 SHA-256。

后端只依赖 `serde_json`，构建时不下载其他东西。

## 发布新版本

1. 同时修改 `io.github.fxrayhughes.mise/plugin.json` 和 `backend/Cargo.toml` 的 `version`（打包脚本会检查两者一致），合入 `main`。
2. 推送同名标签：

   ```bash
   git tag v0.2.0 && git push origin v0.2.0
   ```

3. `Release` 工作流会核对标签与版本一致，运行测试，打包并发布 Release（说明里附 SHA-256）。
   已存在的 Release 不会被覆盖，因为 Lithe Store 固定了资产的 SHA-256。
4. Lithe Store 的自动追踪会在 6 小时内发现新版本并开合并请求。合并后，用户即可在 Lithe 中更新。

## 许可

[Apache-2.0](LICENSE)。mise 是 [jdx/mise](https://github.com/jdx/mise) 的项目，本插件与其作者无关联。
