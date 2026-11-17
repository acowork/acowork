//! PM 服务配置加载。
//!
//! 配置来源（按优先级降序，由 `main.rs` 的独立进程入口实现）：
//!
//! 1. CLI 参数（`--data-dir` / …）
//! 2. 环境变量（`ACOWORK_PM_*`）
//! 3. TOML 配置文件（`--config <path>`；未指定时若 `./acowork-pm.toml` 存在则加载）
//! 4. [`PmConfig::default`]
//!
//! ADR-064：PM 作为独立进程运行，数据目录独立解析为 `$HOME/.acowork/acowork-pm/`，
//! 不再由 Gateway 内嵌/覆写。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The one and only storage backend, reported by `/health`.
pub const STORAGE_BACKEND: &str = "db";

/// PM 服务运行时配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PmConfig {
    /// 数据根目录（项目/任务/附件存储位置）。
    ///
    /// 解析顺序：环境变量 `ACOWORK_PM_DATA_DIR` → TOML `data_dir` → [`default_data_dir`]。
    /// ADR-064：默认 `$HOME/.acowork/acowork-pm/`（与 `acowork-gateway/`、
    /// `acowork-node/` 平级），不再嵌套在 Gateway 数据目录下。
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    /// HTTP 监听地址（**legacy**，独立进程模式由 CLI `--host`/`--port` 覆盖）。
    ///
    /// 默认 `127.0.0.1:0`。保留以兼容旧配置；独立进程入口（`main.rs`）不使用此字段。
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,

    /// 独立进程 HTTP 监听端口（ADR-064）。
    ///
    /// 默认 `18082`。端口冲突时自动递增（最多 +20）。
    #[serde(default = "default_port")]
    pub port: u16,

    /// 是否启用 PM 服务（ADR-064）。
    ///
    /// 默认 `true`。Gateway 据此决定是否 spawn PM 子进程；独立进程模式下
    /// `false` 时进程直接退出。
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// 任务最大嵌套深度（防滥用 + UI 折叠层数合理）。
    ///
    /// 默认 `5`（根任务 + 4 层子任务）。
    #[serde(default = "default_max_task_depth")]
    pub max_task_depth: u8,

    /// 单个附件文件最大字节数。
    ///
    /// 默认 `10 MiB`。超过返回 400 `attachment_too_large`。
    #[serde(default = "default_max_attachment_size")]
    pub max_attachment_size: u64,

    /// 单个任务下附件数量上限。
    ///
    /// 默认 `20`。超过返回 400 `too_many_attachments`。
    #[serde(default = "default_max_attachments_per_task")]
    pub max_attachments_per_task: usize,

    /// 启用图片附件缩略图生成（需 `image-thumb` feature）。
    ///
    /// 默认 `true`。关闭可加速上传但前端预览需直接拉原图。
    #[serde(default = "default_true")]
    pub generate_thumbnails: bool,

    /// 缩略图最大边长（像素）。
    ///
    /// 默认 `256`。
    #[serde(default = "default_thumbnail_max_edge")]
    pub thumbnail_max_edge: u32,

    /// 是否自动把 pm MCP HTTP 端点注入每个 Agent 的 MCP catalog（设计 §6.1 / T3-4）。
    ///
    /// 默认 `true`：Gateway 在 `acowork/global/mcps` 资源下发中附带一个
    /// `name = "pm"`、transport = http 的 MCP server，Agent 启动后自动获得
    /// `pm_*` 工具。关闭后 Agent 需在 Tools 面板手动添加（通常无需关闭）。
    #[serde(default = "default_true")]
    pub auto_inject_mcp: bool,

    /// pm MCP HTTP 端点的公开路径（含 `/api/pm` 前缀，由 Gateway `nest_service`
    /// 挂载）。默认 `/api/pm/mcp`（设计 §21）。
    #[serde(default = "default_mcp_http_path")]
    pub mcp_http_path: String,
}

fn default_true() -> bool {
    true
}

fn default_mcp_http_path() -> String {
    "/api/pm/mcp".to_string()
}

fn default_data_dir() -> PathBuf {
    // ADR-064: PM 数据目录独立于 Gateway，与 acowork-gateway/、acowork-node/ 平级。
    // 解析顺序：ACOWORK_PM_HOME env > $HOME/.acowork/acowork-pm > ./.acowork-pm
    // （镜像 acowork-node::default_node_home 模式）。
    if let Some(dir) = std::env::var_os("ACOWORK_PM_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    // Windows 没有 `HOME`（只有 `USERPROFILE`）；没有此分支会静默回退到
    // `./.acowork-pm`（cwd），把 PM 数据散落到启动目录。
    #[cfg(windows)]
    if let Some(profile) = std::env::var_os("USERPROFILE")
        && !profile.is_empty()
    {
        return PathBuf::from(profile).join(".acowork").join("acowork-pm");
    }
    if let Some(home) = std::env::var_os("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home).join(".acowork").join("acowork-pm");
    }
    PathBuf::from(".").join(".acowork-pm")
}

fn default_bind_addr() -> String {
    "127.0.0.1:0".to_string()
}

fn default_port() -> u16 {
    18082
}

fn default_max_task_depth() -> u8 {
    5
}

fn default_max_attachment_size() -> u64 {
    10 * 1024 * 1024 // 10 MiB
}

fn default_max_attachments_per_task() -> usize {
    20
}

fn default_thumbnail_max_edge() -> u32 {
    256
}

impl Default for PmConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            bind_addr: default_bind_addr(),
            port: default_port(),
            enabled: default_true(),
            max_task_depth: default_max_task_depth(),
            max_attachment_size: default_max_attachment_size(),
            max_attachments_per_task: default_max_attachments_per_task(),
            generate_thumbnails: default_true(),
            thumbnail_max_edge: default_thumbnail_max_edge(),
            auto_inject_mcp: default_true(),
            mcp_http_path: default_mcp_http_path(),
        }
    }
}

impl PmConfig {
    /// 校验配置合法性（如路径可创建、深度为正、限额为正）。
    pub fn validate(&self) -> crate::Result<()> {
        use crate::error::PmError;

        if self.max_task_depth == 0 {
            return Err(PmError::Internal("max_task_depth must be > 0".to_string()));
        }
        if self.max_attachment_size == 0 {
            return Err(PmError::Internal(
                "max_attachment_size must be > 0".to_string(),
            ));
        }
        if self.max_attachments_per_task == 0 {
            return Err(PmError::Internal(
                "max_attachments_per_task must be > 0".to_string(),
            ));
        }
        Ok(())
    }

    /// SQLite database path — the single authority.
    pub fn sqlite_path(&self) -> PathBuf {
        self.data_dir.join("pm.sqlite")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 串行化修改全局环境变量的测试（`ACOWORK_PM_HOME`），避免并行污染。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn default_validates() {
        PmConfig::default().validate().unwrap();
    }

    #[test]
    fn rejects_zero_depth() {
        let cfg = PmConfig {
            max_task_depth: 0,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    /// ADR-064: 默认端口 18082、默认启用。
    #[test]
    fn default_port_and_enabled() {
        let cfg = PmConfig::default();
        assert_eq!(cfg.port, 18082);
        assert!(cfg.enabled);
    }

    /// ADR-064: 默认数据目录解析到 `$HOME/.acowork/acowork-pm`（与
    /// acowork-gateway/、acowork-node/ 平级），不再使用 `directories::ProjectDirs`
    /// 的 `%APPDATA%\com\acowork\pm`。
    #[test]
    fn default_data_dir_is_under_acowork_home() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = default_data_dir();
        let s = dir.to_string_lossy();
        assert!(
            s.contains(".acowork") && s.contains("acowork-pm"),
            "default data dir should be under $HOME/.acowork/acowork-pm, got: {s}"
        );
        // 不再解析到 ProjectDirs 的 com/acowork/pm 布局
        assert!(
            !s.contains("com") || !s.contains("acowork") || !s.contains("pm"),
            "must not fall back to ProjectDirs layout: {s}"
        );
    }

    /// ADR-064: `ACOWORK_PM_HOME` 环境变量覆盖默认数据目录。
    #[test]
    fn data_dir_respects_acowork_pm_home_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join("acowork-pm-test-home");
        unsafe {
            std::env::set_var("ACOWORK_PM_HOME", &tmp);
        }
        let dir = default_data_dir();
        unsafe {
            std::env::remove_var("ACOWORK_PM_HOME");
        }
        assert_eq!(dir, tmp);
    }

    #[test]
    fn sqlite_path_sits_beside_projects_not_inside_it() {
        let cfg = PmConfig {
            data_dir: PathBuf::from("/tmp/pm-data"),
            ..Default::default()
        };
        assert_eq!(cfg.sqlite_path(), PathBuf::from("/tmp/pm-data/pm.sqlite"));
    }

    /// Unknown config keys must fail loudly rather than being silently ignored.
    #[test]
    fn unknown_config_keys_are_rejected() {
        assert_eq!(STORAGE_BACKEND, "db");
        let err = toml::from_str::<PmConfig>("typo_option = 1\n").unwrap_err();
        assert!(
            err.to_string().contains("unknown field"),
            "error was: {err}"
        );
        assert!(toml::from_str::<PmConfig>("data_dir = \"/tmp/pm-data\"\n").is_ok());
    }
}
