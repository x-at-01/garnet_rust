---
name: transpile
description: 跨语言系统代码转写与微 Crate 拆分重构指南
---

# 跨语言系统代码转写与微 Crate 拆分重构指南

本规范指导如何将大型复杂系统（如 Microsoft Garnet C#、RocksDB C++ 等）高保真、零成本抽象地转写为现代化 Rust 架构，重点指导如何科学拆分微 Crate 以及由浅入深启动转写工程。

---

## 一、 核心准则：微 Crate 化拆分原则（越细越好）

绝不能将原系统的整个底层直接搬进一个巨大的单体 Crate 中。拆分越细，职责越单一，越容易进行独立单测、独立编译加速与全并发审查。

垂直依赖分层体系（由底至顶）：

1. Layer 1（硬件适配与内存基础层，无内部依赖，天然全并发）：
   - 扇区对齐内存与缓冲：负责物理扇区对齐（512B/4096B）、AlignedBuf、Direct I/O 缓冲区与异步运行时（如 compio_buf::IoBuf）的零拷贝对接。
   - 并发与安全纪元：无锁内存回收（LightEpoch / RCU）、64 字节 CPU Cacheline 对齐条目防伪共享。
   - 基础算法与无锁索引：核心哈希表、B 树或跳表，只负责基于内存的索引定位，不沾染磁盘 I/O。
   - 变长记录格式：变长 Key-Value 序列化布局、定长 Header、反向版本链指针、只读零拷贝借用与可变原位更新视图。

2. Layer 2（存储驱动层，依赖 Layer 1）：
   - 异步块设备：基于 compio (io_uring / IOCP) 的 Direct I/O (O_DIRECT) 异步块读写，管理物理分段文件（Segment Files）与历史段截断。

3. Layer 3（核心状态机与分配器层，依赖 Layer 1 与 Layer 2）：
   - 混合日志分配器：整合内存环形页缓冲池、64 位连续逻辑地址空间，驱动 Mutable（原位修改）→ ReadOnly（RCU 追加）→ OnDisk（磁盘冷读）三区动态滑动与换页异步刷盘。

4. Layer 4（顶层引擎聚合与会话层，依赖 Layer 1 至 Layer 3）：
   - 统一存储引擎与 Session：整合下层全部组件，提供客户端并发 Session，调度热点原位覆盖、RCU 追加与冷数据异步读取。

---

## 二、 如何开始转写：标准化执行流程

系统转写标准化五步执行流：

梳理原版源码与测试 → 建立 TODO.md 1:1 映射看板与 DAG 拓扑 → 使用 ./sh/new.sh 创建微 Crate → 派发 Developer 子代理编写核心实现与基准单测 → 派发 Reviewer 子代理逐行比对原版源码并迁移测试至 tests/csharp_compat.rs → 启动多轮独立 Reviewer 对抗审查（直到连续 3 次无反馈意见） → 模块终验收敛冻结 → 推进至下一层

### 步骤 1：建立 1:1 映射关系与 TODO.md
在开始写第一行 Rust 代码前，在工作区根目录建立 TODO.md，明确每层拆分出的 Crate 所对应的原系统（如 C#）核心源码文件路径、核心 Class/Struct、核心测试文件路径与当前研发状态。

### 步骤 2：标准化微包脚手架
统一使用工作区脚手架创建微包：./sh/new.sh <crate_name>。
依赖管理红线：只能使用 cargo add 或 cargo add --path，绝对禁止直接手动编辑 Cargo.toml。

### 步骤 3：开发子代理编码规范
必须严格遵循 .agents/skills/rust_review 代码规范：
- 日志统一使用 log crate（info!, debug!, trace!）。
- 测试统一使用 #[ctor::ctor(unsafe)] log_init::init()，测试函数返回 aok::Void 并以 OK 结尾。
- 异步测试统一使用 compio::runtime::Runtime::new()?.block_on(async { ... })。
- 全部注释必须使用中文。
- 消除绝对路径导入（-W clippy::absolute_paths）。

### 步骤 4：原版测试复刻迁移 (Test Migration)
原版系统的语义正确性由其自身的测试体系保证。转写时必须创建专门的审查子代理，研读原版对应的测试文件，将其核心用例 1:1 转写复刻到 Rust 的 tests/csharp_compat.rs 中，用实际测试断言检验行为一致性。

---

## 三、 “连续 3 次无反馈” 终验收敛机制

为防止单次审查存在认知盲区，对每个 Crate 必须实施多轮对抗式迭代审查环：

新开独立 Review 子代理 → 源码逐行比对与测试复刻 → 发现瑕疵或新增测试？ → [是] 修复代码并重置连续无反馈计数为 0 → 重新派发下一轮 Review 子代理；[否] 确认 0 缺陷 0 遗漏 → 连续无反馈计数 + 1 → 计数达到 3 次？ → [是] 终验收敛冻结；[否] 继续派发下一轮 Review 子代理

收敛判定准则：
- 每一轮必须由全新派生的子代理独立审查，杜绝上下文思维定势；
- 只有当连续 3 个独立的 Reviewer 子代理均明确汇报：“无反馈意见 / 0 代码缺陷 / 测试全部覆盖通过”，该 Crate 方可正式进入终验冻结状态。

---

## 四、 质量保障与执行红线

- 依赖管控红线：只能使用 cargo add 添加依赖，严禁私自修改 Cargo.toml。
- 门禁红线：每轮修改后均须通过 ../sh/clippy.sh（0 警告）和 ./test.sh（100% 通过）双重门禁。
- 并发效率最大化：识别 DAG 中无依赖关系的独立分支，一次性发起多个并发子代理；开发与审查流水线重叠（一边审查上一层 crate，一边开发下一层 crate）。