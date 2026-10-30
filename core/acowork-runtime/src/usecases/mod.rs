//! UseCase traits — business-logic abstractions for Runtime adapters.
//!
//! ADR-040: adapter modules (HTTP server, gateway loop) depend on these
//! traits rather than calling concrete functions directly.

pub mod agent_config;
pub mod agent_token;
pub mod agent_tools;
pub mod attachment;
pub mod debug_service;
pub mod git_query;
pub mod mcp_recommendation;
pub(crate) mod package_workspace;
pub mod session_config;
pub mod session_control;
pub mod session_metadata;
pub mod workspace_mutation;
pub mod workspace_query;

// Trait re-exports
pub use agent_config::{
    AgentConfigError, AgentConfigService, ConfigField, ConfigFieldPatch, FieldPatch,
    GetAgentConfigResponse, PutAgentConfigBody, PutAgentConfigResult,
};
pub use agent_token::AgentTokenService;
pub use agent_tools::{
    AgentToolsError, AgentToolsService, BuiltinToolsResponse, McpServerView, McpToolsResponse,
    MergedToolsResponse, PutBuiltinToolsBody, PutMcpServersBody, PutMcpToolsBody,
};
pub use attachment::{
    AttachmentError, AttachmentService, MAX_UPLOAD_BYTES, UploadFileParams, UploadedFileResponse,
};
pub use debug_service::DebugService;
pub use git_query::{
    GitCommitDto, GitDiffKind, GitDiffParams, GitDiffResponse, GitError, GitIndexStatus,
    GitLogParams, GitLogResponse, GitQueryService, GitStatusParams, GitStatusResponse,
    GitWorktreeStatus,
};
pub use session_config::SessionConfigService;
pub use session_control::SessionControlService;
pub use session_metadata::SessionMetadataService;
pub use workspace_mutation::WorkspaceMutationService;
pub use workspace_query::{WorkspaceError, WorkspaceQueryService};

// Implementation structs.
pub mod agent_config_impl;
pub mod agent_token_impl;
pub mod agent_tools_impl;
pub mod attachment_impl;
pub mod debug_service_impl;
pub mod git_query_impl;
pub mod session_config_impl;
pub mod session_metadata_impl;
pub mod workspace_mutation_impl;
pub mod workspace_query_impl;

pub use agent_config_impl::RuntimeAgentConfigService;
pub use agent_token_impl::RuntimeAgentTokenService;
pub use agent_tools_impl::RuntimeAgentToolsService;
pub use attachment_impl::RuntimeAttachmentService;
pub use debug_service_impl::RuntimeDebugService;
pub use git_query_impl::RuntimeGitQueryService;
pub use session_config_impl::{RuntimeSessionConfigService, SharedSessionConfigs};
pub use session_metadata_impl::RuntimeSessionMetadataService;
pub use workspace_mutation_impl::RuntimeWorkspaceMutationService;
pub use workspace_query_impl::RuntimeWorkspaceQueryService;
