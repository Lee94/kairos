<p align="center">
    <img width="200" alt="Kairos Logo" src="extra/logo/compat/kairos-term%2Bscanlines.png">
</p>

<h1 align="center">Kairos</h1>

<p align="center">基于 <a href="https://github.com/alacritty/alacritty">Alacritty</a> 的二次开发终端模拟器：在 Alacritty 高性能 OpenGL 内核之上，加入标签页、项目侧边栏、分屏与会话持久化。</p>

## 关于本项目

Kairos 是 [Alacritty](https://github.com/alacritty/alacritty)（基于 0.18.0-dev 版本）的
fork。Alacritty 本身刻意不做标签页和分屏，将这些交给窗口管理器或 tmux；Kairos 的出发点正好相反——
把"项目 + 标签页 + 分屏 + 会话恢复"做进终端本体，让它可以独立作为日常开发的工作台使用，
同时完整保留 Alacritty 的终端模拟核心与渲染性能。

终端模拟、转义序列支持、Vi 模式、搜索、Hints、多窗口等基础能力均继承自上游，
详见 [docs/features.md](./docs/features.md)。

## Kairos 在 Alacritty 之上新增的功能

### 原生 GL 窗口 Chrome

标签栏、项目侧边栏、右键菜单和 Git 状态栏全部使用 Kairos 自带的 GL 渲染器绘制
（纯色矩形 + 网格对齐文本），不引入 egui 等额外 UI 框架。Chrome 字号与终端字体解耦，
只随窗口缩放因子变化，放大终端字体不会撑大标签栏和侧边栏。

### 项目与标签页

侧边栏管理多个项目（名称 + 根目录），每个项目下可以打开多个标签页。
侧边栏宽度可拖拽调整，支持项目删除。

### 分屏（Split Panes）

每个标签页内可以任意嵌套水平/垂直分屏（tmux 风格）。默认快捷键（Windows/Linux，macOS 见
[docs/features.md](./docs/features.md)）：

| 操作 | 快捷键 |
| --- | --- |
| 向右分屏 | <kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>D</kbd> |
| 向下分屏 | <kbd>Alt</kbd> <kbd>Shift</kbd> <kbd>D</kbd> |
| 关闭当前 Pane | <kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>W</kbd> |
| 切换 Pane 焦点 | <kbd>Alt</kbd> <kbd>方向键</kbd> |
| 调整 Pane 大小 | <kbd>Alt</kbd> <kbd>Shift</kbd> <kbd>方向键</kbd> |
| 最大化/还原 Pane | <kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>Enter</kbd> |

也可以用鼠标点击切换焦点、拖拽分隔条调整大小，或通过命令面板
（<kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>P</kbd>）执行同样的操作。

### 会话持久化

打开的项目、标签页、分屏布局（含每个 Pane 的工作目录）、窗口尺寸和当前激活的项目/标签
会保存到平台状态目录下的 SQLite 数据库（Unix 为 `~/.local/state/kairos/session.db`），
下次启动时自动恢复。只恢复布局——每个 Pane 会在保存的目录下启动新 shell，
不会恢复运行中的进程和回滚缓冲区。

### Claude Code 集成

侧边栏会列出当前项目的 [Claude Code](https://docs.anthropic.com/en/docs/claude-code) 会话
（读取 `~/.claude/projects/<项目目录>/` 下的转录文件，按最近修改排序，以首条用户提问作为标题），
点击即可在新标签页中恢复（resume）该会话。

### Windows 集成

MSI 安装包（WiX 构建）会在资源管理器的目录右键菜单中加入"在此处打开 Kairos"。

## 安装与构建

从源码构建（需要 [Rust 工具链](https://rustup.rs/)）：

```sh
cargo build --release
```

更详细的各平台依赖与打包说明见 [INSTALL.md](INSTALL.md)。

### 运行要求

- OpenGL ES 2.0 及以上
- [Windows] ConPTY 支持（Windows 10 1809 或更高版本）

## 配置

Kairos 不会自动创建配置文件，按以下顺序查找：

1. `$XDG_CONFIG_HOME/kairos/kairos.toml`
2. `$XDG_CONFIG_HOME/kairos.toml`
3. `$HOME/.config/kairos/kairos.toml`
4. `$HOME/.kairos.toml`
5. `/etc/kairos/kairos.toml`

Windows 上的查找路径：

- `%APPDATA%\kairos\kairos.toml`

配置格式与 Alacritty 兼容，可参考
[上游配置文档](https://alacritty.org/config-alacritty.html)；Kairos 新增的
`SplitRight` / `SplitDown` / `ClosePane` / `FocusPane*` / `ResizePane*` 等按键动作见
[docs/features.md](./docs/features.md)。

注意：默认的 <kbd>Alt</kbd> <kbd>方向键</kbd> 与 <kbd>Alt</kbd> <kbd>Shift</kbd>
<kbd>方向键</kbd> 分屏绑定会遮蔽部分终端程序使用的单词跳转转义序列，
如有需要可在配置中重新绑定。

## 与上游的关系

- 上游仓库：[alacritty/alacritty](https://github.com/alacritty/alacritty)
- 终端核心（`kairos_terminal`）、配置系统（`kairos_config`）即上游对应 crate 的重命名，
  会跟随上游演进
- 上游的 [CHANGELOG.md](CHANGELOG.md) 与文档予以保留，便于追踪合并历史

## 贡献

参与开发的指引见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可证

与上游一致，Kairos 基于 [Apache License 2.0](LICENSE-APACHE) 发布，
部分代码采用 [MIT License](LICENSE-MIT)。
