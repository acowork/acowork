//! P0 阶段烟雾测试。
//!
//! 验证：
//! - crate 可被外部使用
//! - 关键 re-export 存在
//! - PmConfig 默认值合法
//! - SqlitePmStore 可在空数据目录建库

use acowork_pm::{AttachmentId, PmConfig, PmMaintenance, ProjectId, TaskId};

#[test]
fn crate_re_exports_compile() {
    let _: ProjectId = ProjectId::generate();
    let _: TaskId = TaskId::generate();
    let _: AttachmentId = AttachmentId::generate();
}

#[test]
fn default_config_is_valid() {
    let cfg = PmConfig::default();
    cfg.validate().expect("default config must validate");
}

#[test]
fn sqlite_path_is_under_data_dir() {
    let cfg = PmConfig::default();
    assert!(cfg.sqlite_path().starts_with(&cfg.data_dir));
}

#[test]
fn store_constructs_with_tempdir() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = PmConfig {
        data_dir: tmp.path().to_path_buf(),
        ..Default::default()
    };
    let store =
        acowork_pm::SqlitePmStore::open(cfg.clone()).expect("store construction should succeed");
    assert_eq!(store.config().data_dir, cfg.data_dir);
    assert!(cfg.sqlite_path().exists());
    assert_eq!(store.storage_diagnostics()["rows"]["projects"], 0);
}

#[test]
fn id_roundtrip() {
    let pid = ProjectId::generate();
    let s = pid.to_string();
    let parsed: ProjectId = s.parse().expect("parse must succeed");
    assert_eq!(pid, parsed);
}
