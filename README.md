# mise for Lithe

让 [Lithe](https://github.com/1lck/Lithe-IDEA) 自动找到 [mise](https://mise.jdx.dev) 安装的 JDK、Node.js、Maven、Gradle、Go、Python 和 pnpm，
并按项目里的 `mise.toml` / `.tool-versions` 选择版本，不需要再在 Lithe 里手动添加路径。
这是一个通过 [Lithe Store](https://github.com/Lithe-IDEA/Lithe-store) 分发的**社区插件**。

> English: Lets Lithe discover runtimes installed by mise and pick the versions your project's `mise.toml` or `.tool-versions` request.

## 功能

| 工具 | Lithe 里的效果 |
| --- | --- |
| Java | mise 装的 JDK 出现在运行时列表；项目要求的版本成为项目 JDK，显示“来自 mise” |
| Maven | mise 装的 Maven 出现在 Maven 列表；项目要求的版本排在系统 Maven 前（`mvnw` 仍然优先） |
| Node.js、pnpm、Go、Python、Gradle | 语言服务器、运行配置和构建优先使用项目要求的版本，排在 `PATH` 和 Homebrew 之前（`gradlew` 仍然优先） |

版本按 mise 的规则选择：

- 项目目录逐级向上查找，同一目录内依次读 `.mise.local.toml`、`mise.local.toml`、`.mise.toml`、`mise.toml`、
  `mise/config.toml`、`.mise/config.toml`、`.config/mise.toml`、`.config/mise/mise.toml`、`.config/mise/config.toml`、`.tool-versions`；
  更深的目录优先，最后才是全局的 `~/.config/mise/config.local.toml` 和 `~/.config/mise/config.toml`。
- 支持精确版本、前缀（`21` 匹配 `21.0.2`）、`latest`、`lts` 等 mise 建立的别名、`prefix:`，以及指向项目外部目录的 `path:`。
- 尊重 `MISE_DATA_DIR`、`MISE_INSTALLS_DIR`、`MISE_CONFIG_DIR`、`MISE_GLOBAL_CONFIG_FILE` 和 `XDG_*` 目录。

你在 Lithe 里手动指定的 JDK、Maven 或 `LITHE_*_PATH` 覆盖永远优先。

## 安全与限制

- 插件只有一份 `plugin.json`，不附带任何程序；Lithe 只读取 mise 的安装目录和版本文件，**不会运行 `mise`**，
  不会安装或下载任何版本，也不使用 shims。
- 需要先用 mise 装好版本（例如 `mise install`）。未安装的版本不会被选中，Lithe 回到原来的选择。
- `system`、`ref:`、`sub-*:` 等需要 mise 在运行时解析的写法，以及指向项目内部的 `path:`，不会选择任何版本。
- 暂不支持 `conf.d/*.toml` 和环境专属配置（`mise.<env>.toml`）。
- 远程工作区使用远端主机自己的运行时，不受本插件影响。

## 安装

需要支持 `toolchainSources` 能力的 Lithe（[1lck/Lithe-IDEA#1581](https://github.com/1lck/Lithe-IDEA/pull/1581) 合并后的版本）。
不支持的 Lithe 版本会在插件市场中隐藏本插件。

1. 在 Lithe 中打开 **设置 → 插件 → 插件市场**，搜索 “mise”，安装并确认。
2. 不需要重启；打开项目后，运行时设置里即可看到 mise 的 JDK 和 Maven。

Lithe 已预置 Lithe Store 社区插件源。如果你之前移除了它，先在 **⚙ → 管理插件仓库…** 中重新添加
`https://github.com/Lithe-IDEA/Lithe-store`。

## 本地打包

需要 Node.js 22+ 和 `zip`：

```bash
node scripts/package.mjs
```

输出 `dist/mise-<版本>-darwin.zip`（同时适用 Apple Silicon 与 Intel Mac）和它的 SHA-256。

## 发布新版本

1. 修改 `io.github.fxrayhughes.mise/plugin.json` 的 `version`，合入 `main`。
2. 推送同名标签：

   ```bash
   git tag v0.2.0 && git push origin v0.2.0
   ```

3. `Release` 工作流会核对标签与版本一致、打包并发布 Release（说明里附 SHA-256）。
4. Lithe Store 的自动追踪会在 6 小时内发现新版本并开合并请求，合并后用户即可在 Lithe 中更新。

## 许可

[Apache-2.0](LICENSE)。mise 是 [jdx/mise](https://github.com/jdx/mise) 的项目，本插件与其作者无关联。
