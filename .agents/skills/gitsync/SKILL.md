---
name: gitsync
description: 同步源仓库（Microsoft Garnet）代码更新到我的 Rust 改写版本
---

同步微软官方源仓库 Garnet (C#) 到 Rust 版本 (wedb)。

# 仓库与相对路径

- 官方仓库：microsoft/garnet (ssh://git@ssh.github.com:443/microsoft/garnet.git)
- Fork 仓库：webc-fork/garnet (ssh://git@ssh.github.com:443/webc-fork/garnet.git)
- 源仓库相对路径：../garnet
- Rust 改写仓库：.
- 同步记录：gitsync.md
- 检查工具：./sh/clippy.sh 与 ./test.sh

1. 运行 ./init.sh  初始化 garnet 仓库到 ../garnet

然后

```bash
git -C ../garnet fetch source main
git -C ../garnet log <BASE_COMMIT>..source/main --oneline
```
无新提交则终止并汇报。有新提交则快进合并并推送到 Fork：

```bash
git -C ../garnet checkout main
git -C ../garnet merge --ff-only source/main
git -C ../garnet push origin main
```

### 2. 变更分类

执行 `git -C ../garnet diff <BASE_COMMIT>..source/main --stat` 分类：
- 文档与 CI：记录，无需移植。
- C# 核心代码， 定位目标 crate，分析逻辑变动
- 测试用例：提取到对应 crate 的 tests/

### 3. Rust 改写

遵循 .agents/skills/rust_review：
- 零拷贝、状态机优先，避免堆分配
- 依赖用 cargo add 添加，禁止直接编辑 Cargo.toml
- 注释用中文
- 复刻测试确保行为对齐
- 注意，我们有不少自己的优化，请 code review，思考是否需要合并

### 4. 审查

- 运行 ./sh/clippy.sh，确保 0 警告
- 运行 ./test.sh，确保测试全部通过
- 开子代理，对照 c# 改动，审查代码优化，代码规范 .agents/skills/rust_review

### 5. 归档

更新 gitsync.md，记录改动