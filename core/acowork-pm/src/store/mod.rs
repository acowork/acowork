//! 存储层：SQLite 权威 + 文件系统二进制资产。
//!
//! ## 子模块
//!
//! - [`atomic`] —— 原子写 + 路径校验工具（导入测试与树夹具共用）
//! - [`corrupt`] —— 坏文件取证副本与 `/health` 注册表
//! - [`index`] —— 二级内存索引（树夹具使用）
//! - [`sqlite`] —— 唯一运行后端：schema、一次性 JSON 导入、`SqlitePmStore`
//! - [`tree`] —— **已退出运行路径**。保留为 JSON 归档格式的参考实现、
//!   被 SQLite 后端复用的规则 helpers（`parse_task_id` /
//!   `validate_transition` / `seeded_activity` 等），以及导入器测试用来生成
//!   真实树布局的夹具；`build_store()` 不再装配它。

pub mod atomic;
pub mod corrupt;
pub mod index;
pub mod sqlite;
pub mod tree;
