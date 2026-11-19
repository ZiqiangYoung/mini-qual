# Rust 开发约定

- 使用 Cargo 的常规目录结构和稳定 Rust。遵循 [Google Rust Style Guide](https://google.github.io/styleguide/rustguide.html) 及其引用的 Rust 风格指南。用 `cargo fmt` 保持格式一致。
- 按职责组织模块。导入所需类型，避免通配导入；调用其他模块的自由函数时带上模块名。使用能表达用途的完整名称和具名字段。
- 优先采用清晰的控制流、借用和现有库。用 `Result` 保留操作上下文，避免因设备、服务或用户输入错误而 panic。确需 `unsafe` 时说明安全前提。
- 自动化测试放在 `tests/`。提交前运行格式检查、Clippy 和测试。用模拟设备或临时文件覆盖 I/O 编排和失败路径，减少实体或虚拟 NVMe 的写入。
- 将现有 fio 和 nvme-cli 源码作为工具参考。用普通用户开发和运行，涉及设备访问的授权由部署者执行。
