//! 存储层：项目 / 任务 / 附件的物理 IO + 内存索引。
//!
//! ## 子模块
//!
//! - [`atomic`] —— 原子写 + 路径校验工具
//! - [`index`] —— 二级内存索引（按 project / assignee / 状态 / 反向依赖图）
//! - [`tree`] —— `TreePmStore` + `PmStore` trait（基于目录树的实现）
//!
//! ## 数据流
//!
//! ```text
//! HTTP API / MCP tool
//!       ↓
//!   PmStore trait
//!       ↓
//! TreePmStore::create_task / get_task / ...
//!       ↓
//! ┌──────────────┬───────────────────┐
//! │ atomic_write │ index.update()    │
//! │   (fs)       │   (内存 HashMap)  │
//! └──────────────┴───────────────────┘
//!       ↓                  ↓
//!   task.json         TaskIndex
//! ```

pub mod atomic;
pub mod index;
pub mod tree;