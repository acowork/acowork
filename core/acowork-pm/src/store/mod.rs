//! 存储层：SQLite 权威 + 文件系统二进制资产。
//!
//! - [`contract`] —— `PmStore` / `PmMaintenance` trait，上层与后端的唯一边界
//! - [`rules`] —— 与存储格式无关的 ID / 状态机 / 路径规则
//! - [`sqlite`] —— 唯一运行后端：schema、业务方法与维护钩子

pub mod contract;
pub mod rules;
pub mod sqlite;
