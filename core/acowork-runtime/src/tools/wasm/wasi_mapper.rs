//! Permission → WASI capability mapping
//!
//! Translates ACowork permission declarations from the manifest
//! into WASI Preview 2 capabilities. WASI is the sole security
//! boundary for filesystem and network access in WASM tools.
//!
//! Mapping rules (ADR-008):
//! - `filesystem:read:<path>`  → WASI preopen directory (readonly)
//! - `filesystem:write:<path>` → WASI preopen directory (readwrite)
//! - `network:<url>`           → WASI socket capability (or deny if unsupported)
//! - `shell`/`wasm`/`identity`/`intent` → No WASI mapping (host-controlled)

use acowork_core::Permission;

/// A WASI directory permission derived from a ACowork permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasiDirPermission {
    /// The directory path to grant access to
    pub path: String,
    /// Whether write access is granted
    pub writable: bool,
}

/// A WASI network permission derived from a ACowork permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasiNetPermission {
    /// The URL pattern to grant access to
    pub url_pattern: String,
}

/// Complete set of WASI capabilities for a WASM tool.
#[derive(Debug, Clone, Default)]
pub struct WasiCapabilities {
    /// Preopened directories with read/write flags
    pub dirs: Vec<WasiDirPermission>,
    /// Network URL patterns allowed
    pub networks: Vec<WasiNetPermission>,
}

/// Map a single ACowork Permission to WASI capabilities.
///
/// Returns None for permissions that don't need WASI mapping
/// (e.g., identity, shell, wasm, intent). Memory is not a permission any
/// more — it is a hosted service now (ADR-100).
pub fn map_permission_to_wasi(perm: &Permission) -> Option<WasiCapabilities> {
    match perm {
        Permission::FilesystemRead(path) => {
            let dir_path = path.clone().unwrap_or_else(|| ".".to_string());
            Some(WasiCapabilities {
                dirs: vec![WasiDirPermission {
                    path: dir_path,
                    writable: false,
                }],
                networks: vec![],
            })
        }
        Permission::FilesystemWrite(path) => {
            let dir_path = path.clone().unwrap_or_else(|| ".".to_string());
            Some(WasiCapabilities {
                dirs: vec![WasiDirPermission {
                    path: dir_path,
                    writable: true,
                }],
                networks: vec![],
            })
        }
        Permission::Network(url_pattern) => {
            let pattern = url_pattern.clone().unwrap_or_else(|| "*".to_string());
            Some(WasiCapabilities {
                dirs: vec![],
                networks: vec![WasiNetPermission {
                    url_pattern: pattern,
                }],
            })
        }
        // These permissions don't map to WASI capabilities.
        // They are controlled by host functions or are internal.
        Permission::IdentityRead
        | Permission::IdentityWrite
        | Permission::Shell
        | Permission::Wasm
        | Permission::IntentSend(_)
        | Permission::IntentReceive(_)
        | Permission::RagQuery(_) => None,
    }
}

/// Map a list of ACowork Permissions into aggregated WASI capabilities.
pub fn map_permissions_to_wasi(permissions: &[Permission]) -> WasiCapabilities {
    let mut caps = WasiCapabilities::default();

    for perm in permissions {
        if let Some(perm_caps) = map_permission_to_wasi(perm) {
            // Merge directories: if same path exists with write, keep write;
            // if read-only exists and write is added, upgrade to write.
            for dir in perm_caps.dirs {
                if let Some(existing) = caps.dirs.iter_mut().find(|d| d.path == dir.path) {
                    // Upgrade read to write if new permission is write
                    if dir.writable {
                        existing.writable = true;
                    }
                } else {
                    caps.dirs.push(dir);
                }
            }

            // Merge networks
            for net in perm_caps.networks {
                if !caps
                    .networks
                    .iter()
                    .any(|n| n.url_pattern == net.url_pattern)
                {
                    caps.networks.push(net);
                }
            }
        }
    }

    caps
}

/// Check if a WASM tool's requested access is within its WASI capabilities.
///
/// Uses longest-prefix matching: if multiple directories match,
/// the most specific (longest) path wins.
pub fn check_wasi_access(
    capabilities: &WasiCapabilities,
    requested_path: &str,
    write_access: bool,
) -> bool {
    // Find the longest matching directory
    let best_match = capabilities
        .dirs
        .iter()
        .filter(|dir| requested_path.starts_with(&dir.path))
        .max_by_key(|dir| dir.path.len());

    match best_match {
        Some(dir) => {
            if write_access {
                dir.writable
            } else {
                true
            }
        }
        None => false,
    }
}

/// Check if a network URL is within WASI capabilities.
pub fn check_wasi_network(capabilities: &WasiCapabilities, url: &str) -> bool {
    for net in &capabilities.networks {
        if net.url_pattern == "*" {
            return true;
        }
        if url.starts_with(&net.url_pattern) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_filesystem_read() {
        let perm = Permission::FilesystemRead(Some("/tmp/data".to_string()));
        let caps = map_permission_to_wasi(&perm).unwrap();
        assert_eq!(caps.dirs.len(), 1);
        assert_eq!(caps.dirs[0].path, "/tmp/data");
        assert!(!caps.dirs[0].writable);
    }

    #[test]
    fn test_map_filesystem_write() {
        let perm = Permission::FilesystemWrite(Some("/tmp/output".to_string()));
        let caps = map_permission_to_wasi(&perm).unwrap();
        assert_eq!(caps.dirs.len(), 1);
        assert_eq!(caps.dirs[0].path, "/tmp/output");
        assert!(caps.dirs[0].writable);
    }

    #[test]
    fn test_map_network() {
        let perm = Permission::Network(Some("https://api.example.com".to_string()));
        let caps = map_permission_to_wasi(&perm).unwrap();
        assert_eq!(caps.networks.len(), 1);
        assert_eq!(caps.networks[0].url_pattern, "https://api.example.com");
    }

    #[test]
    fn test_map_shell_no_wasi() {
        assert!(map_permission_to_wasi(&Permission::Shell).is_none());
    }

    #[test]
    fn test_map_identity_no_wasi() {
        assert!(map_permission_to_wasi(&Permission::IdentityRead).is_none());
        assert!(map_permission_to_wasi(&Permission::IdentityWrite).is_none());
    }

    #[test]
    fn test_map_permissions_merge_dirs() {
        let perms = vec![
            Permission::FilesystemRead(Some("/data".to_string())),
            Permission::FilesystemWrite(Some("/data".to_string())),
        ];
        let caps = map_permissions_to_wasi(&perms);
        // Should merge into one dir with write access
        assert_eq!(caps.dirs.len(), 1);
        assert!(caps.dirs[0].writable);
    }

    #[test]
    fn test_map_permissions_separate_dirs() {
        let perms = vec![
            Permission::FilesystemRead(Some("/read-only".to_string())),
            Permission::FilesystemWrite(Some("/write-dir".to_string())),
        ];
        let caps = map_permissions_to_wasi(&perms);
        assert_eq!(caps.dirs.len(), 2);
    }

    #[test]
    fn test_check_wasi_read_access_allowed() {
        let caps = WasiCapabilities {
            dirs: vec![WasiDirPermission {
                path: "/data".to_string(),
                writable: false,
            }],
            networks: vec![],
        };
        assert!(check_wasi_access(&caps, "/data/file.txt", false));
    }

    #[test]
    fn test_check_wasi_write_access_denied() {
        let caps = WasiCapabilities {
            dirs: vec![WasiDirPermission {
                path: "/data".to_string(),
                writable: false,
            }],
            networks: vec![],
        };
        assert!(!check_wasi_access(&caps, "/data/file.txt", true));
    }

    #[test]
    fn test_check_wasi_network_allowed() {
        let caps = WasiCapabilities {
            dirs: vec![],
            networks: vec![WasiNetPermission {
                url_pattern: "https://api.example.com".to_string(),
            }],
        };
        assert!(check_wasi_network(&caps, "https://api.example.com/v1/data"));
    }

    #[test]
    fn test_check_wasi_network_denied() {
        let caps = WasiCapabilities {
            dirs: vec![],
            networks: vec![WasiNetPermission {
                url_pattern: "https://api.example.com".to_string(),
            }],
        };
        assert!(!check_wasi_network(&caps, "https://other.com/api"));
    }

    #[test]
    fn test_check_wasi_network_wildcard() {
        let caps = WasiCapabilities {
            dirs: vec![],
            networks: vec![WasiNetPermission {
                url_pattern: "*".to_string(),
            }],
        };
        assert!(check_wasi_network(&caps, "https://any-url.com/path"));
    }

    #[test]
    fn test_check_wasi_path_not_in_caps() {
        let caps = WasiCapabilities {
            dirs: vec![WasiDirPermission {
                path: "/data".to_string(),
                writable: true,
            }],
            networks: vec![],
        };
        assert!(!check_wasi_access(&caps, "/other/file.txt", false));
    }
}
