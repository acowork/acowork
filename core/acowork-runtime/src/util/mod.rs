//! 横切工具模块（cross-cutting utilities）
//!
//! 集中放置那些本不属于任何业务子领域、但又被多处复用的纯工具。
//! 每个子文件都对应一个独立的横切关注点：
//!
//! - [`text`] — 字符安全的字符串预览/截断（替代散落的 `&s[..bytes]` 字节切片，
//!   避免命中多字节 UTF-8 字符时 panic）
//! - [`image_raster`] — 送模型的图片的统一栅格处理（嗅探 / 限幅 / 降采样 /
//!   确定性重编码）。`image_read` 工具与附件流水线共用同一份物理。

pub mod image_raster;
pub mod text;
