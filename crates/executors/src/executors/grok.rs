use std::{
    env,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use derivative::Derivative;
use futures::stream::BoxStream;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides},
    env::ExecutionEnv,
    executor_discovery::ExecutorDiscoveredOptions,
    executors::{
        AppendPrompt, AvailabilityInfo, BaseCodingAgent, ExecutorError, SpawnedChild,
        StandardCodingAgentExecutor, gemini::AcpAgentHarness,
    },
    logs::utils::patch,
    model_selector::{ModelInfo, ModelSelectorConfig, PermissionPolicy, ReasoningOption},
    profile::ExecutorConfig,
};

const GROK_DISABLE_AUTOUPDATER: &str = "GROK_DISABLE_AUTOUPDATER";

fn execution_env(env: &ExecutionEnv) -> ExecutionEnv {
    disable_autoupdater(env, env::var_os(GROK_DISABLE_AUTOUPDATER).is_some())
}

/// Disable update checks unless this is already set.
fn disable_autoupdater(env: &ExecutionEnv, inherited: bool) -> ExecutionEnv {
    let mut env = env.clone();
    if !inherited && !env.contains_key(GROK_DISABLE_AUTOUPDATER) {
        env.insert(GROK_DISABLE_AUTOUPDATER, "1");
    }
    env
}

/// Grok Build keeps its config, credentials and managed binary under `~/.grok`,
/// or under `GROK_HOME` when that is set.
fn grok_home() -> Option<PathBuf> {
    resolve_grok_home(env::var("GROK_HOME").ok(), dirs::home_dir())
}

fn resolve_grok_home(env_value: Option<String>, home_dir: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(grok_home) = env_value.filter(|value| !value.trim().is_empty()) {
        return Some(PathBuf::from(grok_home));
    }
    home_dir.map(|home| home.join(".grok"))
}

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct Grok {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub always_approve: Option<bool>,
    #[serde(flatten)]
    pub cmd: CmdOverrides,
    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub approvals: Option<Arc<dyn ExecutorApprovalService>>,
}

impl Grok {
    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        let mut builder = CommandBuilder::new("npx -y @xai-official/grok@1.0.5 agent");

        if let Some(model) = &self.model {
            builder = builder.extend_params(["-m", model.as_str()]);
        }

        if let Some(effort) = &self.reasoning_effort {
            builder = builder.extend_params(["--reasoning-effort", effort.as_str()]);
        }

        if self.always_approve.unwrap_or(false) {
            builder = builder.extend_params(["--always-approve"]);
        }

        // `stdio` is appended after the overrides: options belong to `grok agent`,
        // and the subcommand only accepts its own handful of flags.
        Ok(apply_overrides(builder, &self.cmd)?.extend_params(["stdio"]))
    }

    fn harness(&self) -> AcpAgentHarness {
        let mut harness = AcpAgentHarness::with_session_namespace("grok_sessions");
        if let Some(model) = &self.model {
            harness = harness.with_model(model);
        }
        harness
    }

    /// With `--always-approve` Grok never asks, so there is nothing to bridge.
    fn harness_approvals(&self) -> Option<Arc<dyn ExecutorApprovalService>> {
        if self.always_approve.unwrap_or(false) {
            None
        } else {
            self.approvals.clone()
        }
    }
}

#[async_trait]
impl StandardCodingAgentExecutor for Grok {
    fn apply_overrides(&mut self, executor_config: &ExecutorConfig) {
        if let Some(model_id) = executor_config.model_id.as_ref() {
            self.model = Some(model_id.clone());
        }
        if let Some(reasoning_id) = executor_config.reasoning_id.as_ref() {
            self.reasoning_effort = Some(reasoning_id.clone());
        }
        if let Some(permission_policy) = executor_config.permission_policy.clone() {
            self.always_approve = Some(matches!(permission_policy, PermissionPolicy::Auto));
        }
    }

    fn use_approvals(&mut self, approvals: Arc<dyn ExecutorApprovalService>) {
        self.approvals = Some(approvals);
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let grok_command = self.build_command_builder()?.build_initial()?;
        let combined_prompt = self.append_prompt.combine_prompt(prompt);
        self.harness()
            .spawn_with_command(
                current_dir,
                combined_prompt,
                grok_command,
                &execution_env(env),
                &self.cmd,
                self.harness_approvals(),
            )
            .await
    }

    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let grok_command = self.build_command_builder()?.build_follow_up(&[])?;
        let combined_prompt = self.append_prompt.combine_prompt(prompt);
        self.harness()
            .spawn_follow_up_with_command(
                current_dir,
                combined_prompt,
                session_id,
                grok_command,
                &execution_env(env),
                &self.cmd,
                self.harness_approvals(),
            )
            .await
    }

    fn normalize_logs(
        &self,
        msg_store: Arc<MsgStore>,
        worktree_path: &Path,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        crate::executors::acp::normalize_logs(msg_store, worktree_path)
    }

    fn default_mcp_config_path(&self) -> Option<PathBuf> {
        grok_home().map(|home| home.join("config.toml"))
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        if let Some(timestamp) = grok_home()
            .and_then(|home| std::fs::metadata(home.join("auth.json")).ok())
            .and_then(|m| m.modified().ok())
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
        {
            return AvailabilityInfo::LoginDetected {
                last_auth_timestamp: timestamp,
            };
        }

        let mcp_config_found = self
            .default_mcp_config_path()
            .map(|p| p.exists())
            .unwrap_or(false);

        let installation_indicator_found = grok_home()
            .map(|home| home.join("version.json").exists())
            .unwrap_or(false);

        if mcp_config_found || installation_indicator_found {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }

    fn get_preset_options(&self) -> ExecutorConfig {
        ExecutorConfig {
            executor: BaseCodingAgent::Grok,
            variant: None,
            model_id: self.model.clone(),
            agent_id: None,
            reasoning_id: self.reasoning_effort.clone(),
            permission_policy: Some(if self.always_approve.unwrap_or(false) {
                PermissionPolicy::Auto
            } else {
                PermissionPolicy::Supervised
            }),
        }
    }

    async fn discover_options(
        &self,
        _workdir: Option<&Path>,
        _repo_path: Option<&Path>,
    ) -> Result<BoxStream<'static, json_patch::Patch>, ExecutorError> {
        let options = ExecutorDiscoveredOptions {
            model_selector: ModelSelectorConfig {
                models: vec![
                    ModelInfo {
                        id: "grok-4.6".to_string(),
                        name: "Grok 4.6".to_string(),
                        provider_id: None,
                        // `xhigh` is grok-4.6 and later only.
                        reasoning_options: ReasoningOption::from_names([
                            "low", "medium", "high", "xhigh",
                        ]),
                    },
                    ModelInfo {
                        id: "grok-4.5".to_string(),
                        name: "Grok 4.5".to_string(),
                        provider_id: None,
                        reasoning_options: ReasoningOption::from_names(["low", "medium", "high"]),
                    },
                ],
                default_model: Some("grok-4.6".to_string()),
                permissions: vec![PermissionPolicy::Auto, PermissionPolicy::Supervised],
                ..Default::default()
            },
            ..Default::default()
        };
        Ok(Box::pin(futures::stream::once(async move {
            patch::executor_discovered_options(options)
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::RepoContext;

    fn grok() -> Grok {
        Grok {
            append_prompt: AppendPrompt::default(),
            model: None,
            reasoning_effort: None,
            always_approve: None,
            cmd: CmdOverrides::default(),
            approvals: None,
        }
    }

    fn base_env() -> ExecutionEnv {
        ExecutionEnv::new(
            RepoContext::new(PathBuf::from("/tmp/workspace"), vec![]),
            false,
            String::new(),
        )
    }

    #[test]
    fn disables_the_autoupdater_on_the_spawned_process() {
        let env = disable_autoupdater(&base_env(), false);

        assert_eq!(env.get(GROK_DISABLE_AUTOUPDATER), Some(&"1".to_string()));
    }

    #[test]
    fn keeps_an_explicit_autoupdater_setting() {
        let mut base = base_env();
        base.insert(GROK_DISABLE_AUTOUPDATER, "0");

        let env = disable_autoupdater(&base, false);

        assert_eq!(env.get(GROK_DISABLE_AUTOUPDATER), Some(&"0".to_string()));
    }

    /// The child inherits the launching process's environment, and setting the
    /// var here would override whatever the user exported.
    #[test]
    fn leaves_an_inherited_autoupdater_setting_alone() {
        let env = disable_autoupdater(&base_env(), true);

        assert_eq!(env.get(GROK_DISABLE_AUTOUPDATER), None);
    }

    #[test]
    fn grok_home_prefers_the_env_override() {
        let resolved = resolve_grok_home(
            Some("/tmp/custom-grok".to_string()),
            Some(PathBuf::from("/home/user")),
        );

        assert_eq!(resolved, Some(PathBuf::from("/tmp/custom-grok")));
    }

    #[test]
    fn grok_home_falls_back_to_the_dot_directory() {
        let resolved = resolve_grok_home(None, Some(PathBuf::from("/home/user")));

        assert_eq!(resolved, Some(PathBuf::from("/home/user/.grok")));
    }

    #[test]
    fn grok_home_ignores_a_blank_env_override() {
        let resolved = resolve_grok_home(Some("  ".to_string()), Some(PathBuf::from("/home/user")));

        assert_eq!(resolved, Some(PathBuf::from("/home/user/.grok")));
    }

    #[test]
    fn builds_acp_stdio_command_by_default() {
        let builder = grok().build_command_builder().unwrap();

        assert_eq!(builder.base, "npx -y @xai-official/grok@1.0.5 agent");
        assert_eq!(builder.params, Some(vec!["stdio".to_string()]));
    }

    #[test]
    fn passes_model_and_reasoning_effort_before_the_subcommand() {
        let builder = Grok {
            model: Some("grok-4.6".to_string()),
            reasoning_effort: Some("high".to_string()),
            ..grok()
        }
        .build_command_builder()
        .unwrap();

        assert_eq!(
            builder.params,
            Some(vec![
                "-m".to_string(),
                "grok-4.6".to_string(),
                "--reasoning-effort".to_string(),
                "high".to_string(),
                "stdio".to_string(),
            ])
        );
    }

    #[test]
    fn always_approve_adds_the_auto_approval_flag() {
        let builder = Grok {
            always_approve: Some(true),
            ..grok()
        }
        .build_command_builder()
        .unwrap();

        assert_eq!(
            builder.params,
            Some(vec!["--always-approve".to_string(), "stdio".to_string()])
        );
    }

    /// `grok agent stdio` only accepts a handful of its own flags, so every
    /// option — including user-supplied ones — has to precede the subcommand.
    #[test]
    fn user_supplied_params_stay_ahead_of_the_subcommand() {
        let builder = Grok {
            cmd: CmdOverrides {
                additional_params: Some(vec!["--no-subagents".to_string()]),
                ..Default::default()
            },
            ..grok()
        }
        .build_command_builder()
        .unwrap();

        assert_eq!(
            builder.params,
            Some(vec!["--no-subagents".to_string(), "stdio".to_string()])
        );
    }
}
