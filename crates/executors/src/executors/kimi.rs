use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use derivative::Derivative;
use futures::stream::BoxStream;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use workspace_utils::{msg_store::MsgStore, shell::resolve_executable_path_blocking};

use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides},
    env::ExecutionEnv,
    executor_discovery::ExecutorDiscoveredOptions,
    executors::{
        AppendPrompt, AvailabilityInfo, BaseCodingAgent, ExecutorError, SpawnedChild,
        StandardCodingAgentExecutor, acp::AcpAgentHarness,
    },
    logs::utils::patch,
    model_selector::{ModelInfo, ModelSelectorConfig, PermissionPolicy},
    profile::ExecutorConfig,
};

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct KimiCode {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo: Option<bool>,
    #[serde(flatten)]
    pub cmd: CmdOverrides,
    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub approvals: Option<Arc<dyn ExecutorApprovalService>>,
}

impl KimiCode {
    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        // `kimi acp` accepts neither --model nor --yolo. Configure the session
        // over ACP so the same settings apply to initial and follow-up runs.
        apply_overrides(CommandBuilder::new("kimi").params(["acp"]), &self.cmd)
    }

    fn harness(&self) -> AcpAgentHarness {
        let mut harness = AcpAgentHarness::with_session_namespace("kimi_sessions").with_mode(
            if self.yolo.unwrap_or(false) {
                "yolo"
            } else {
                "default"
            },
        );
        if let Some(model) = &self.model {
            harness = harness.with_model(model);
        }
        harness
    }

    fn harness_approvals(&self) -> Option<Arc<dyn ExecutorApprovalService>> {
        if self.yolo.unwrap_or(false) {
            None
        } else {
            self.approvals.clone()
        }
    }

    fn home(&self) -> Option<PathBuf> {
        let override_home = self
            .cmd
            .env
            .as_ref()
            .and_then(|env| env.get("KIMI_CODE_HOME"))
            .cloned()
            .or_else(|| std::env::var("KIMI_CODE_HOME").ok());
        resolve_home(override_home, dirs::home_dir())
    }
}

fn resolve_home(override_home: Option<String>, home: Option<PathBuf>) -> Option<PathBuf> {
    override_home
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".kimi-code")))
}

// Deserialize only model metadata; provider credentials never leave the config file.
#[derive(Default, Deserialize)]
#[serde(default)]
struct KimiConfig {
    default_model: Option<String>,
    models: BTreeMap<String, KimiModel>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct KimiModel {
    display_name: Option<String>,
    overrides: KimiModelOverrides,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct KimiModelOverrides {
    display_name: Option<String>,
}

impl KimiConfig {
    fn into_selector(self) -> ModelSelectorConfig {
        ModelSelectorConfig {
            models: self
                .models
                .into_iter()
                .map(|(id, model)| ModelInfo {
                    name: model
                        .overrides
                        .display_name
                        .or(model.display_name)
                        .unwrap_or_else(|| id.clone()),
                    id,
                    provider_id: None,
                    reasoning_options: vec![],
                })
                .collect(),
            default_model: self.default_model,
            permissions: vec![PermissionPolicy::Auto, PermissionPolicy::Supervised],
            ..Default::default()
        }
    }
}

#[async_trait]
impl StandardCodingAgentExecutor for KimiCode {
    fn apply_overrides(&mut self, executor_config: &ExecutorConfig) {
        if let Some(model) = &executor_config.model_id {
            self.model = Some(model.clone());
        }
        if let Some(policy) = &executor_config.permission_policy {
            self.yolo = Some(matches!(policy, PermissionPolicy::Auto));
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
        self.harness()
            .spawn_with_command(
                current_dir,
                self.append_prompt.combine_prompt(prompt),
                self.build_command_builder()?.build_initial()?,
                env,
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
        self.harness()
            .spawn_follow_up_with_command(
                current_dir,
                self.append_prompt.combine_prompt(prompt),
                session_id,
                self.build_command_builder()?.build_follow_up(&[])?,
                env,
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
        super::acp::normalize_logs(msg_store, worktree_path)
    }

    fn default_mcp_config_path(&self) -> Option<PathBuf> {
        self.home().map(|home| home.join("mcp.json"))
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        let config_found = self
            .home()
            .is_some_and(|home| home.join("config.toml").is_file());
        if config_found || resolve_executable_path_blocking("kimi").is_some() {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }

    fn get_preset_options(&self) -> ExecutorConfig {
        ExecutorConfig {
            executor: BaseCodingAgent::KimiCode,
            variant: None,
            model_id: self.model.clone(),
            agent_id: None,
            reasoning_id: None,
            permission_policy: Some(if self.yolo.unwrap_or(false) {
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
        let config = self
            .home()
            .and_then(|home| std::fs::read_to_string(home.join("config.toml")).ok())
            .and_then(|text| toml::from_str::<KimiConfig>(&text).ok())
            .unwrap_or_default();
        let options = ExecutorDiscoveredOptions {
            model_selector: config.into_selector(),
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
    use crate::{executors::CodingAgent, profile::ExecutorConfigs};

    fn kimi() -> KimiCode {
        serde_json::from_value(serde_json::json!({})).unwrap()
    }

    #[test]
    fn acp_command_keeps_model_and_permissions_out_of_cli_flags() {
        let mut agent = kimi();
        agent.model = Some("kimi-code/kimi-for-coding".into());
        agent.yolo = Some(true);
        let builder = agent.build_command_builder().unwrap();
        assert_eq!(builder.base, "kimi");
        assert_eq!(builder.params.unwrap(), ["acp"]);

        agent.cmd.base_command_override = Some("/custom/bin/kimi".into());
        let builder = agent.build_command_builder().unwrap();
        assert_eq!(builder.base, "/custom/bin/kimi");
        assert_eq!(builder.params.unwrap(), ["acp"]);
    }

    #[test]
    fn permission_override_can_disable_default_auto_approval() {
        let profiles = ExecutorConfigs::from_defaults();
        let mut agent = profiles
            .get_coding_agent(&crate::profile::ExecutorProfileId::new(
                BaseCodingAgent::KimiCode,
            ))
            .unwrap();
        assert!(matches!(&agent, CodingAgent::KimiCode(kimi) if kimi.yolo == Some(true)));

        let mut config = agent.get_preset_options();
        config.permission_policy = Some(PermissionPolicy::Supervised);
        config.model_id = Some("custom-model".into());
        agent.apply_overrides(&config);
        assert!(matches!(&agent, CodingAgent::KimiCode(kimi) if kimi.yolo == Some(false)));
        assert_eq!(agent.get_preset_options().model_id, config.model_id);
        assert_eq!(
            agent.get_preset_options().permission_policy,
            config.permission_policy
        );
    }

    #[test]
    fn home_override_and_profile_mcp_path() {
        assert_eq!(
            resolve_home(None, Some(PathBuf::from("home"))),
            Some(PathBuf::from("home/.kimi-code"))
        );
        assert_eq!(
            resolve_home(Some(" ".into()), Some(PathBuf::from("home"))),
            Some(PathBuf::from("home/.kimi-code"))
        );
        assert_eq!(
            resolve_home(Some("custom".into()), None),
            Some(PathBuf::from("custom"))
        );
        assert_eq!(resolve_home(None, None), None);

        let mut agent = kimi();
        agent.cmd.env = Some([("KIMI_CODE_HOME".into(), "custom".into())].into());
        assert_eq!(
            agent.default_mcp_config_path(),
            Some(PathBuf::from("custom/mcp.json"))
        );
    }

    #[test]
    fn discovers_configured_model_aliases_and_display_overrides() {
        let config: KimiConfig = toml::from_str(
            r#"
            default_model = "kimi-code/kimi-for-coding"
            [models."kimi-code/kimi-for-coding"]
            model = "kimi-for-coding"
            display_name = "Kimi for Coding"
            [models."kimi-code/kimi-for-coding".overrides]
            display_name = "My Kimi"
            [models.custom]
            model = "provider-model-id"
            [providers.example]
            api_key = "not-exported"
        "#,
        )
        .unwrap();
        let selector = config.into_selector();
        assert_eq!(
            selector.default_model.as_deref(),
            Some("kimi-code/kimi-for-coding")
        );
        assert_eq!(selector.models.len(), 2);
        assert_eq!(selector.models[0].id, "custom");
        assert_eq!(selector.models[0].name, "custom");
        assert_eq!(selector.models[1].id, "kimi-code/kimi-for-coding");
        assert_eq!(selector.models[1].name, "My Kimi");
        assert!(
            !serde_json::to_string(&selector)
                .unwrap()
                .contains("not-exported")
        );
        assert_eq!(
            KimiConfig::default().into_selector().permissions,
            [PermissionPolicy::Auto, PermissionPolicy::Supervised]
        );
    }
}
