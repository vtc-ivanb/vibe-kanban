pub mod client;
pub mod jsonrpc;
pub mod normalize_logs;
pub mod review;
pub mod slash_commands;
use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

/// Returns the Codex home directory.
///
/// Checks the `CODEX_HOME` environment variable first, then falls back to `~/.codex`.
/// This allows users to configure a custom location for Codex configuration and state.
pub fn codex_home() -> Option<PathBuf> {
    if let Ok(codex_home) = env::var("CODEX_HOME")
        && !codex_home.trim().is_empty()
    {
        return Some(PathBuf::from(codex_home));
    }
    dirs::home_dir().map(|home| home.join(".codex"))
}

pub(crate) fn resolve_model(model: Option<&str>) -> (Option<&str>, bool) {
    match model.and_then(|m| m.strip_suffix("-fast")) {
        Some(base) => (Some(base), true),
        None => (model, false),
    }
}

pub(crate) fn fork_params_from(thread_id: String, params: ThreadStartParams) -> ThreadForkParams {
    ThreadForkParams {
        thread_id,
        model: params.model,
        model_provider: params.model_provider,
        cwd: params.cwd,
        approval_policy: params.approval_policy,
        sandbox: params.sandbox,
        config: params.config,
        base_instructions: params.base_instructions,
        developer_instructions: params.developer_instructions,
        service_tier: params.service_tier,
        ..Default::default()
    }
}

use async_trait::async_trait;
use codex_app_server_protocol::{
    AskForApproval as V2AskForApproval, Model as CatalogModel, ReviewTarget,
    SandboxMode as V2SandboxMode, ThreadForkParams, ThreadStartParams, UserInput,
};
use codex_protocol::config_types::ServiceTier;
use command_group::AsyncGroupChild;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use strum_macros::{AsRefStr, EnumString};
use tokio::process::Command;
use ts_rs::TS;
use workspace_utils::{command_ext::GroupSpawnNoWindowExt, msg_store::MsgStore};

use self::{
    client::{AppServerClient, LogWriter},
    jsonrpc::{ExitSignalSender, JsonRpcPeer},
    normalize_logs::{Error, normalize_logs},
};
use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandBuildError, CommandBuilder, CommandParts, apply_overrides},
    env::{ExecutionEnv, RepoContext},
    executor_discovery::ExecutorDiscoveredOptions,
    executors::{
        AppendPrompt, AvailabilityInfo, BaseCodingAgent, ExecutorError, ExecutorExitResult,
        SlashCommandDescription, SpawnedChild, StandardCodingAgentExecutor,
    },
    logs::utils::patch,
    model_selector::{ModelInfo, ModelSelectorConfig, PermissionPolicy, ReasoningOption},
    profile::ExecutorConfig,
    stdout_dup::create_stdout_pipe_writer,
};

/// Sandbox policy modes for Codex
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema, AsRefStr)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum SandboxMode {
    Auto,
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

/// Determines when the user is consulted to approve Codex actions.
///
/// - `UnlessTrusted`: Read-only commands are auto-approved. Everything else will
///   ask the user to approve.
/// - `OnFailure`: All commands run in a restricted sandbox initially. If a
///   command fails, the user is asked to approve execution without the sandbox.
/// - `OnRequest`: The model decides when to ask the user for approval.
/// - `Never`: Commands never ask for approval. Commands that fail in the
///   restricted sandbox are not retried.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema, AsRefStr)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum AskForApproval {
    UnlessTrusted,
    OnFailure,
    OnRequest,
    Never,
}

/// Reasoning effort for the underlying model.
///
/// Mirrors the efforts Codex names in its model catalog, so any effort a model
/// advertises can be selected and written back as a config override.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema, AsRefStr, EnumString)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    Ultra,
    Persistent,
}

/// Model reasoning summary style
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema, AsRefStr)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum ReasoningSummary {
    Auto,
    Concise,
    Detailed,
    None,
}

/// Format for model reasoning summaries
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema, AsRefStr)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum ReasoningSummaryFormat {
    None,
    Experimental,
}

enum CodexSessionAction {
    Chat { prompt: String },
    Review { target: ReviewTarget },
}

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct Codex {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_for_approval: Option<AskForApproval>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oss: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_reasoning_effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_reasoning_summary: Option<ReasoningSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_reasoning_summary_format: Option<ReasoningSummaryFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_apply_patch_tool: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
    #[serde(default)]
    pub plan: bool,
    #[serde(flatten)]
    pub cmd: CmdOverrides,

    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    approvals: Option<Arc<dyn ExecutorApprovalService>>,
}

#[async_trait]
impl StandardCodingAgentExecutor for Codex {
    fn apply_overrides(&mut self, executor_config: &ExecutorConfig) {
        if let Some(model_id) = &executor_config.model_id {
            self.model = Some(model_id.clone());
        }
        if let Some(reasoning_id) = &executor_config.reasoning_id
            && let Ok(reasoning_effort) = ReasoningEffort::from_str(reasoning_id)
        {
            self.model_reasoning_effort = Some(reasoning_effort)
        }
        if let Some(permission_policy) = &executor_config.permission_policy {
            match permission_policy {
                crate::model_selector::PermissionPolicy::Auto => {
                    self.ask_for_approval = Some(AskForApproval::Never);
                    self.plan = false;
                }
                crate::model_selector::PermissionPolicy::Supervised => {
                    if matches!(self.ask_for_approval, None | Some(AskForApproval::Never)) {
                        self.ask_for_approval = Some(AskForApproval::UnlessTrusted);
                    }
                    self.plan = false;
                }
                crate::model_selector::PermissionPolicy::Plan => {
                    self.plan = true;
                }
            }
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
        self.spawn_slash_command(current_dir, prompt, None, env)
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
        self.spawn_slash_command(current_dir, prompt, Some(session_id), env)
            .await
    }

    fn normalize_logs(
        &self,
        msg_store: Arc<MsgStore>,
        worktree_path: &Path,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        normalize_logs(msg_store, worktree_path)
    }

    fn default_mcp_config_path(&self) -> Option<PathBuf> {
        codex_home().map(|home| home.join("config.toml"))
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        if let Some(timestamp) = codex_home()
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

        let installation_indicator_found = codex_home()
            .map(|home| home.join("version.json").exists())
            .unwrap_or(false);

        if mcp_config_found || installation_indicator_found {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }

    fn get_preset_options(&self) -> ExecutorConfig {
        use crate::model_selector::*;
        let permission_policy = if self.plan {
            PermissionPolicy::Plan
        } else if matches!(self.ask_for_approval, None | Some(AskForApproval::Never)) {
            PermissionPolicy::Auto
        } else {
            PermissionPolicy::Supervised
        };

        ExecutorConfig {
            executor: BaseCodingAgent::Codex,
            variant: None,
            model_id: self.model.clone(),
            agent_id: None,
            reasoning_id: self
                .model_reasoning_effort
                .as_ref()
                .map(|e| e.as_ref().to_string()),
            permission_policy: Some(permission_policy),
        }
    }

    async fn discover_options(
        &self,
        workdir: Option<&std::path::Path>,
        repo_path: Option<&std::path::Path>,
    ) -> Result<futures::stream::BoxStream<'static, json_patch::Patch>, ExecutorError> {
        use futures::StreamExt as _;

        use crate::{
            executor_discovery::ExecutorConfigCacheKey, executors::utils::executor_options_cache,
        };

        let cache = executor_options_cache();
        let cmd_key = self.compute_cmd_key();
        // A project's `.codex/config.toml` can name its own model and model
        // catalog, so the answer belongs to the directory it was asked about
        // and cannot be shared across workspaces.
        //
        // One case this cannot cover: Codex disables the project layer until
        // the project is trusted, and it is `thread/start` that trusts it. A
        // query answers for the untrusted project, so the first session in a
        // fresh repository can run the project's model while the selector
        // still names the user-level one. Previewing the project layer would
        // mean deciding trust from a read-only query, which is worse.
        let discovery_path = workdir.or(repo_path).map(std::path::Path::to_path_buf);
        let cache_key = ExecutorConfigCacheKey::new(
            discovery_path.as_ref(),
            cmd_key.clone(),
            BaseCodingAgent::Codex,
        );
        if let Some(cached) = cache.get(&cache_key) {
            return Ok(Box::pin(futures::stream::once(async move {
                patch::executor_discovered_options(cached.as_ref().clone().with_loading(false))
            })));
        }

        // Show something straight away, preferring what a directory above this
        // one already found over the built-in list, and replace it once this
        // directory's own app server answers.
        let mut initial_options = repo_path
            .map(std::path::Path::to_path_buf)
            .filter(|_| workdir.is_some())
            .and_then(|repo| {
                cache.get(&ExecutorConfigCacheKey::new(
                    Some(&repo),
                    cmd_key.clone(),
                    BaseCodingAgent::Codex,
                ))
            })
            .or_else(|| {
                cache.get(&ExecutorConfigCacheKey::new(
                    None,
                    cmd_key.clone(),
                    BaseCodingAgent::Codex,
                ))
            })
            .map(|cached| cached.as_ref().clone())
            .unwrap_or_else(default_discovered_options);
        initial_options.loading_models = true;
        let initial_patch = patch::executor_discovered_options(initial_options);

        let query_path = discovery_path
            .clone()
            .unwrap_or_else(|| std::path::Path::new(".").to_path_buf());
        let this = self.clone();

        let discovery_stream = async_stream::stream! {
            let mut final_options = default_discovered_options();

            // An empty answer counts as one only when the config names no model
            // either: a catalog whose models are all hidden still has a model
            // the session would run, and that one belongs in the selector.
            let discovered = match this.fetch_model_catalog(&query_path).await {
                Ok(catalog) => {
                    let models = catalog.selectable_models();
                    if models.is_empty() {
                        tracing::warn!("Codex offered no models");
                        None
                    } else {
                        let default_model = catalog.default_model(&models);
                        Some((models, default_model))
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to fetch the Codex model catalog: {e}");
                    None
                }
            };

            match discovered {
                Some((models, default_model)) => {
                    final_options.model_selector.models = models;
                    final_options.model_selector.default_model = default_model;

                    yield patch::update_models(final_options.model_selector.models.clone());
                    yield patch::update_default_model(
                        final_options.model_selector.default_model.clone(),
                    );
                    yield patch::models_loaded();

                    cache.put(cache_key, final_options);
                }
                // Fall back to the built-in list: an unauthenticated or offline
                // Codex still runs, it just cannot describe itself. It has to be
                // sent explicitly, or a provisional list borrowed from another
                // directory would stay on screen as this directory's answer.
                None => {
                    yield patch::update_models(final_options.model_selector.models);
                    yield patch::update_default_model(None);
                    yield patch::models_loaded();
                }
            }
        };

        Ok(Box::pin(
            futures::stream::once(async move { initial_patch }).chain(discovery_stream),
        ))
    }

    async fn spawn_review(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let command_parts = self.build_command_builder()?.build_initial()?;
        let review_target = ReviewTarget::Custom {
            instructions: prompt.to_string(),
        };
        let action = CodexSessionAction::Review {
            target: review_target,
        };
        self.spawn_inner(current_dir, command_parts, action, session_id, env)
            .await
    }
}

/// What a discovery query learned from one Codex app server.
struct ModelCatalog {
    /// The whole catalog, hidden models included.
    models: Vec<CatalogModel>,
    /// The model the effective config names for the queried directory, if any.
    configured_model: Option<String>,
    /// Whether that config puts a session on the priority ("fast") tier.
    configured_fast: bool,
}

impl ModelCatalog {
    /// The models to offer for this directory.
    ///
    /// Hidden models are left out, as the catalog intends, except for one the
    /// config names: the frontend falls back to the first entry when the
    /// default is missing from the list, and would then name a model other
    /// than the one a session would run.
    fn selectable_models(&self) -> Vec<ModelInfo> {
        let visible: Vec<CatalogModel> = self
            .models
            .iter()
            .filter(|model| !model.hidden)
            .cloned()
            .collect();
        let mut models = model_infos(&visible);

        let Some(configured) = self.configured_model.as_deref() else {
            return models;
        };
        if is_fast_variant_id(configured) || models.iter().any(|model| model.id == configured) {
            return models;
        }

        // Describe it from the catalog when the catalog knows it, so that a
        // hidden or aliased model keeps its reasoning efforts and its fast
        // variant. Otherwise the id is all there is to show.
        let described = match self.describing_entry(configured) {
            Some(entry) => {
                let name = if entry.model == configured {
                    entry.display_name.clone()
                } else {
                    // An alias is not the catalog entry it borrows from, so
                    // show what the config actually asked for.
                    configured.to_string()
                };
                model_entries(configured, &name, entry)
            }
            None => vec![ModelInfo {
                id: configured.to_string(),
                name: configured.to_string(),
                provider_id: None,
                reasoning_options: Vec::new(),
            }],
        };

        models.splice(0..0, described);
        models
    }

    /// The catalog entry whose metadata describes `configured`.
    ///
    /// Mirrors how Codex resolves a configured slug against its catalog: the
    /// longest catalog slug that prefixes it, so a pinned `gpt-5.5-2026-01-01`
    /// is described by `gpt-5.5`, and failing that one stripped namespace
    /// segment, so `custom/gpt-5.5` is too. Finding nothing is fine; the model
    /// is then offered undescribed rather than not at all.
    fn describing_entry(&self, configured: &str) -> Option<&CatalogModel> {
        if let Some(entry) = self.longest_prefix_entry(configured) {
            return Some(entry);
        }

        let (namespace, suffix) = configured.split_once('/')?;
        // One segment only, and only when it looks like a provider id, so that
        // arbitrary aliases do not borrow a description they have no claim to.
        if suffix.contains('/')
            || namespace.is_empty()
            || !namespace
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return None;
        }

        // Prefix matching again, so a namespaced slug may also be pinned.
        self.longest_prefix_entry(suffix)
    }

    fn longest_prefix_entry(&self, slug: &str) -> Option<&CatalogModel> {
        self.models
            .iter()
            .filter(|model| slug.starts_with(&model.model))
            .max_by_key(|model| model.model.len())
    }

    /// The model a session with no model of its own would run here, as long as
    /// the selector can offer it.
    ///
    /// The config outranks the catalog's own default, which only marks the
    /// first model in the picker rather than the one that would run.
    ///
    /// A configured model that cannot be offered at all yields the catalog
    /// default instead. That does advertise a model the session would not run,
    /// but naming the unofferable one changes nothing: the frontend falls back
    /// to the first entry whenever the default is missing from the list.
    fn default_model(&self, models: &[ModelInfo]) -> Option<String> {
        let catalog_default = self
            .models
            .iter()
            .find(|model| model.is_default)
            .map(|model| model.model.as_str());

        let base = self
            .configured_model
            .as_deref()
            .into_iter()
            .chain(catalog_default)
            .find(|candidate| models.iter().any(|model| model.id == *candidate))?;

        // The config can put a session on the priority tier without naming a
        // model for it. `-fast` is how this executor spells that tier, so the
        // default has to point at the variant to describe the same session.
        if self.configured_fast && !is_fast_variant_id(base) {
            let fast = fast_variant_id(base);
            if models.iter().any(|model| model.id == fast) {
                return Some(fast);
            }
        }

        Some(base.to_string())
    }
}

/// Owns a discovery app server for as long as its query runs.
///
/// `kill_on_drop` is not enough on its own: it reaps the `npx` parent and
/// leaves the Codex process it spawned behind, still holding the pipes the
/// reader task is parked on. Killing the whole group and cancelling the token
/// from `Drop` keeps a dropped query from leaking either.
struct AppServerQueryGuard {
    child: AsyncGroupChild,
    cancel: tokio_util::sync::CancellationToken,
}

impl AppServerQueryGuard {
    /// Tear the app server down on the normal path, waiting for it to go.
    async fn shutdown(&mut self) {
        self.cancel.cancel();
        let _ = self.child.kill().await;
    }
}

impl Drop for AppServerQueryGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        let _ = self.child.start_kill();
    }
}

/// How long a one-off app server query may take, `npx` bootstrap included.
const APP_SERVER_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Turn a Codex catalog into selectable models, adding a `-fast` entry for every
/// model that can also run on the priority service tier.
fn model_infos(catalog: &[CatalogModel]) -> Vec<ModelInfo> {
    let mut models = Vec::with_capacity(catalog.len());
    for model in catalog {
        if is_fast_variant_id(&model.model) {
            // `-fast` is how this executor spells a priority-tier variant, so
            // an id already ending that way is stripped back to a different
            // model when the session starts. Leaving it out of the selector
            // beats offering an entry that runs something else.
            tracing::warn!(
                "Skipping Codex model {:?}: its id collides with the fast-tier suffix",
                model.model
            );
            continue;
        }

        models.extend(model_entries(&model.model, &model.display_name, model));
    }
    models
}

/// The selector entries for one model: the model itself, plus its priority-tier
/// variant when the catalog entry describing it offers that tier.
fn model_entries(id: &str, name: &str, described_by: &CatalogModel) -> Vec<ModelInfo> {
    let reasoning_options = reasoning_options_for(described_by);
    let mut entries = vec![ModelInfo {
        id: id.to_string(),
        name: name.to_string(),
        provider_id: None,
        reasoning_options: reasoning_options.clone(),
    }];

    if supports_fast_tier(described_by) {
        entries.push(ModelInfo {
            id: fast_variant_id(id),
            name: format!("{name} Fast"),
            provider_id: None,
            reasoning_options,
        });
    }

    entries
}

fn reasoning_options_for(model: &CatalogModel) -> Vec<ReasoningOption> {
    let mut options = ReasoningOption::from_names(
        model
            .supported_reasoning_efforts
            .iter()
            .map(|option| option.reasoning_effort.as_str())
            // Drop anything this executor could not write back as a config
            // override, so the selector never offers a no-op choice.
            .filter(|effort| ReasoningEffort::from_str(effort).is_ok())
            .map(str::to_string),
    );

    let default_effort = model.default_reasoning_effort.as_str();
    if options.iter().any(|option| option.id == default_effort) {
        for option in &mut options {
            option.is_default = option.id == default_effort;
        }
    }
    options
}

/// Whether an id is one this executor would read back as a fast-tier variant.
fn is_fast_variant_id(id: &str) -> bool {
    matches!(resolve_model(Some(id)), (_, true))
}

/// How this executor spells `base` running on the priority tier.
fn fast_variant_id(base: &str) -> String {
    format!("{base}-fast")
}

/// Whether a session asking for the priority tier on this model would get it.
///
/// Codex keeps the tier only when the model's `service_tiers` lists that exact
/// id. It does not consult the deprecated `additional_speed_tiers`, and it does
/// not accept the tier's other spelling, so neither can be honoured here: a
/// Fast entry built on either would quietly run at the ordinary tier.
fn supports_fast_tier(model: &CatalogModel) -> bool {
    model
        .service_tiers
        .iter()
        .any(|tier| tier.id == ServiceTier::Fast.request_value())
}

/// The catalog Codex served when this was written, used only until the app
/// server reports the account's own list. `default_model` is deliberately left
/// unset here so an unreachable app server falls back to whatever model Codex
/// itself considers default.
fn fallback_models() -> Vec<ModelInfo> {
    use ReasoningEffort::{High, Low, Max, Medium, Ultra, Xhigh};

    [
        (
            "gpt-6-astra",
            "GPT-6-Astra",
            &[Low, Medium, High, Xhigh, Max, Ultra][..],
            Medium,
        ),
        (
            "gpt-6-sol",
            "GPT-6-Sol",
            &[Low, Medium, High, Xhigh, Max, Ultra][..],
            Medium,
        ),
        (
            "gpt-6-luna",
            "GPT-6-Luna",
            &[Low, Medium, High, Xhigh, Max][..],
            Medium,
        ),
        (
            "gpt-5.6-sol",
            "GPT-5.6-Sol",
            &[Low, Medium, High, Xhigh, Max, Ultra][..],
            Low,
        ),
        (
            "gpt-5.6-terra",
            "GPT-5.6-Terra",
            &[Low, Medium, High, Xhigh, Max, Ultra][..],
            Medium,
        ),
        (
            "gpt-5.6-luna",
            "GPT-5.6-Luna",
            &[Low, Medium, High, Xhigh, Max][..],
            Medium,
        ),
        (
            "gpt-5.5",
            "GPT-5.5",
            &[Low, Medium, High, Xhigh][..],
            Medium,
        ),
    ]
    .into_iter()
    .flat_map(|(id, name, efforts, default_effort)| {
        let mut reasoning_options =
            ReasoningOption::from_names(efforts.iter().map(|effort| effort.as_ref().to_string()));
        for option in &mut reasoning_options {
            option.is_default = option.id == default_effort.as_ref();
        }

        // Every model listed here also runs on the priority ("fast") tier.
        [
            ModelInfo {
                id: id.to_string(),
                name: name.to_string(),
                provider_id: None,
                reasoning_options: reasoning_options.clone(),
            },
            ModelInfo {
                id: format!("{id}-fast"),
                name: format!("{name} Fast"),
                provider_id: None,
                reasoning_options,
            },
        ]
    })
    .collect()
}

/// Models and commands used until the installed Codex reports its own catalog.
fn default_discovered_options() -> ExecutorDiscoveredOptions {
    ExecutorDiscoveredOptions {
        model_selector: ModelSelectorConfig {
            models: fallback_models(),
            permissions: vec![
                PermissionPolicy::Auto,
                PermissionPolicy::Supervised,
                PermissionPolicy::Plan,
            ],
            ..Default::default()
        },
        slash_commands: vec![
            SlashCommandDescription {
                name: "compact".to_string(),
                description: Some(
                    "summarize conversation to prevent hitting the context limit".to_string(),
                ),
            },
            SlashCommandDescription {
                name: "init".to_string(),
                description: Some(
                    "create an AGENTS.md file with instructions for Codex".to_string(),
                ),
            },
            SlashCommandDescription {
                name: "status".to_string(),
                description: Some(
                    "show current session configuration and token usage".to_string(),
                ),
            },
            SlashCommandDescription {
                name: "mcp".to_string(),
                description: Some("list configured MCP tools".to_string()),
            },
            SlashCommandDescription {
                name: "model".to_string(),
                description: Some("view or switch the active model".to_string()),
            },
            SlashCommandDescription {
                name: "fast".to_string(),
                description: Some(
                    "toggle fast mode for highest speed inference (2× plan usage). Use `/fast on` or `/fast off` to set explicitly".to_string(),
                ),
            },
        ],
        ..Default::default()
    }
}

impl Codex {
    pub fn base_command() -> &'static str {
        "npx -y @openai/codex@0.156.1"
    }

    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        apply_overrides(self.base_app_server_builder(), &self.cmd)
    }

    /// The app server a discovery query talks to.
    ///
    /// A session carries the provider in its `thread/start` params, but
    /// `model/list` and `config/read` have nowhere to put it, so it goes in as
    /// a startup config override instead. Without it a query answers for
    /// whichever provider the config happens to name, which need not be the
    /// one the session runs on.
    ///
    /// Note the override goes on last, after the profile's own parameters.
    /// Codex applies repeated `-c` keys in order, and a session's typed
    /// `thread/start` provider outranks anything on its command line, so
    /// letting a hand-written `-c model_provider=` win here would describe a
    /// provider the session does not use.
    fn build_discovery_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        let mut builder = apply_overrides(self.base_app_server_builder(), &self.cmd)?;
        if let Some(provider) = &self.model_provider {
            // `{value:?}` is a quoted, escaped string, which is also how a TOML
            // basic string is spelled; Codex parses the override as TOML.
            builder =
                builder.extend_params(["-c".to_string(), format!("model_provider={provider:?}")]);
        }

        Ok(builder)
    }

    fn base_app_server_builder(&self) -> CommandBuilder {
        let mut builder = CommandBuilder::new(Self::base_command()).extend_params(["app-server"]);
        if self.oss.unwrap_or(false) {
            builder = builder.extend_params(["--oss"]);
        }
        builder
    }

    /// Distinguishes discovery results that were produced against a different
    /// Codex install or provider.
    fn compute_cmd_key(&self) -> String {
        serde_json::to_string(&(&self.cmd, self.oss, &self.model_provider)).unwrap_or_default()
    }

    /// Ask the installed Codex which models it offers under `cwd`, and which
    /// one it would run there if the session names none.
    ///
    /// Codex stopped shipping hardcoded model presets and now derives its
    /// listing from a catalog it fetches at runtime, so this is the only way to
    /// offer a model list that stays correct as that catalog changes. The
    /// answer is per directory, not per account: a project's own
    /// `.codex/config.toml` is a config layer, and it can name both a model and
    /// a model catalog.
    async fn fetch_model_catalog(&self, cwd: &Path) -> Result<ModelCatalog, ExecutorError> {
        let cwd_string = cwd.to_string_lossy().to_string();
        self.query_app_server(cwd, |client| async move {
            let mut models = Vec::new();
            let mut cursor = None;
            loop {
                let page = client.model_list(cursor).await?;
                models.extend(page.data);
                match page.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }

            // The catalog's own default only marks the first model in the
            // picker; a config that names a model outranks it, and that is the
            // model a session with no override actually gets. The same is true
            // of the service tier, which `/fast` writes to this config.
            let config = client
                .config_read(Some(cwd_string))
                .await
                .map(|response| response.config)
                .inspect_err(|e| tracing::warn!("Failed to read the Codex config: {e}"))
                .ok();

            // Codex drops the priority tier when its `fast_mode` feature is
            // off, but `config/read` reports the tier before that gate and has
            // no way to ask about the feature, so a disabled fast mode still
            // reads as fast here.
            let configured_fast = config
                .as_ref()
                .and_then(|config| config.service_tier.as_deref())
                .and_then(ServiceTier::from_request_value)
                .is_some_and(|tier| matches!(tier, ServiceTier::Fast));

            Ok(ModelCatalog {
                models,
                configured_model: config.and_then(|config| config.model),
                configured_fast,
            })
        })
        .await
    }

    /// Run a single query against a short-lived `codex app-server`.
    ///
    /// Unlike [`Self::spawn_app_server`] nothing reaches the session log, so the
    /// client writes to a sink and the process is torn down with the query.
    async fn query_app_server<T, F, Fut>(&self, cwd: &Path, query: F) -> Result<T, ExecutorError>
    where
        F: FnOnce(Arc<AppServerClient>) -> Fut,
        Fut: std::future::Future<Output = Result<T, ExecutorError>>,
    {
        let command_parts = self.build_discovery_command_builder()?.build_initial()?;
        let (program_path, args) = command_parts.into_resolved().await?;

        let mut process = Command::new(program_path);
        process
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .current_dir(cwd)
            .env("NPM_CONFIG_LOGLEVEL", "error")
            .env("NODE_NO_WARNINGS", "1")
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "error")
            .args(&args);

        // The profile's environment decides which Codex install and account a
        // query sees, so it has to match the one a session would run with.
        ExecutionEnv::new(RepoContext::default(), false, String::new())
            .with_profile(&self.cmd)
            .apply_to_command(&mut process);

        let mut child = process.group_spawn_no_window()?;
        let child_stdout = child.inner().stdout.take().ok_or_else(|| {
            ExecutorError::Io(std::io::Error::other("Codex app server missing stdout"))
        })?;
        let child_stdin = child.inner().stdin.take().ok_or_else(|| {
            ExecutorError::Io(std::io::Error::other("Codex app server missing stdin"))
        })?;

        let cancel = tokio_util::sync::CancellationToken::new();
        // Whoever is streaming discovery can go away mid-query, which drops
        // this future. The guard is what tears the app server down in that
        // case, so cleanup cannot be left to the end of the function.
        let mut guard = AppServerQueryGuard {
            child,
            cancel: cancel.clone(),
        };
        let (exit_signal_tx, _exit_signal_rx) = tokio::sync::oneshot::channel();
        let client = AppServerClient::new(
            LogWriter::new(tokio::io::sink()),
            // No approval service, and auto-approve on: a query starts no turn,
            // so nothing can ask for permission in the first place.
            None,
            true,
            false,
            RepoContext::default(),
            false,
            String::new(),
            cancel.clone(),
        );
        let rpc_peer = JsonRpcPeer::spawn(
            child_stdin,
            child_stdout,
            client.clone(),
            ExitSignalSender::new(exit_signal_tx),
            cancel.clone(),
        );
        client.connect(rpc_peer);

        let result = tokio::time::timeout(APP_SERVER_QUERY_TIMEOUT, async {
            client.initialize().await?;
            query(client.clone()).await
        })
        .await;

        guard.shutdown().await;

        result.unwrap_or_else(|_| {
            Err(ExecutorError::Io(std::io::Error::other(
                "Codex app server query timed out",
            )))
        })
    }

    fn build_thread_start_params(&self, cwd: &Path) -> ThreadStartParams {
        let sandbox = match self.sandbox.as_ref() {
            None | Some(SandboxMode::Auto) => Some(V2SandboxMode::WorkspaceWrite), // match the Auto preset in codex
            Some(SandboxMode::ReadOnly) => Some(V2SandboxMode::ReadOnly),
            Some(SandboxMode::WorkspaceWrite) => Some(V2SandboxMode::WorkspaceWrite),
            Some(SandboxMode::DangerFullAccess) => Some(V2SandboxMode::DangerFullAccess),
        };

        let approval_policy = match self.ask_for_approval.as_ref() {
            None if matches!(self.sandbox.as_ref(), None | Some(SandboxMode::Auto)) => {
                // match the Auto preset in codex
                Some(V2AskForApproval::OnRequest)
            }
            None => None,
            Some(AskForApproval::UnlessTrusted) => Some(V2AskForApproval::UnlessTrusted),
            // Codex folded the old on-failure policy into on-request in the v2
            // app-server protocol.
            Some(AskForApproval::OnFailure) => Some(V2AskForApproval::OnRequest),
            Some(AskForApproval::OnRequest) => Some(V2AskForApproval::OnRequest),
            Some(AskForApproval::Never) => Some(V2AskForApproval::Never),
        };

        let mut config = self.build_config_overrides();
        // V1 top-level params that moved into config overrides in v2
        if let Some(profile) = &self.profile {
            // Codex 0.154 rejects the legacy `profile` key outright, and its
            // replacement (`--profile <name>` with `<name>.config.toml`) does
            // not apply to `app-server`. Passing it would abort the session
            // before it starts, so drop it and say why.
            tracing::warn!(
                "Ignoring Codex profile {profile:?}: this Codex no longer supports selecting a \
                 config profile through the app server"
            );
        }
        if let Some(include) = self.include_apply_patch_tool {
            config
                .get_or_insert_with(HashMap::new)
                .insert("include_apply_patch_tool".to_string(), Value::Bool(include));
        }
        if let Some(compact) = &self.compact_prompt {
            config
                .get_or_insert_with(HashMap::new)
                .insert("compact_prompt".to_string(), Value::String(compact.clone()));
        }
        if !matches!(approval_policy, None | Some(V2AskForApproval::Never)) {
            let map = config.get_or_insert_with(HashMap::new);
            map.insert(
                "features.default_mode_request_user_input".to_string(),
                Value::Bool(true),
            );
            map.insert(
                "suppress_unstable_features_warning".to_string(),
                Value::Bool(true),
            );
        }

        let (model, is_fast) = resolve_model(self.model.as_deref());
        let service_tier = if is_fast {
            Some(Some(ServiceTier::Fast.request_value().to_string()))
        } else {
            None
        };

        ThreadStartParams {
            model: model.map(|m| m.to_string()),
            cwd: Some(cwd.to_string_lossy().to_string()),
            approval_policy,
            sandbox,
            config,
            base_instructions: self.base_instructions.clone(),
            model_provider: self.model_provider.clone(),
            developer_instructions: self.developer_instructions.clone(),
            service_tier,
            ..Default::default()
        }
    }

    fn build_config_overrides(&self) -> Option<HashMap<String, Value>> {
        let mut overrides = HashMap::new();

        if let Some(effort) = &self.model_reasoning_effort {
            overrides.insert(
                "model_reasoning_effort".to_string(),
                Value::String(effort.as_ref().to_string()),
            );
        }

        if let Some(summary) = &self.model_reasoning_summary {
            overrides.insert(
                "model_reasoning_summary".to_string(),
                Value::String(summary.as_ref().to_string()),
            );
        }

        if let Some(format) = &self.model_reasoning_summary_format
            && format != &ReasoningSummaryFormat::None
        {
            overrides.insert(
                "model_reasoning_summary_format".to_string(),
                Value::String(format.as_ref().to_string()),
            );
        }

        if overrides.is_empty() {
            None
        } else {
            Some(overrides)
        }
    }

    async fn spawn_inner(
        &self,
        current_dir: &Path,
        command_parts: CommandParts,
        action: CodexSessionAction,
        resume_session: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let params = self.build_thread_start_params(current_dir);
        let resume_session = resume_session.map(|s| s.to_string());

        self.spawn_app_server(
            current_dir,
            command_parts,
            env,
            move |client, _| async move {
                match action {
                    CodexSessionAction::Chat { prompt } => {
                        Self::launch_codex_agent(params, resume_session, prompt, client).await
                    }
                    CodexSessionAction::Review { target } => {
                        review::launch_codex_review(params, resume_session, target, client).await
                    }
                }
            },
        )
        .await
    }

    async fn launch_codex_agent(
        thread_start_params: ThreadStartParams,
        resume_session: Option<String>,
        combined_prompt: String,
        client: Arc<AppServerClient>,
    ) -> Result<(), ExecutorError> {
        let account = client.get_account().await?;
        if account.requires_openai_auth && account.account.is_none() {
            return Err(ExecutorError::AuthRequired(
                "Codex authentication required".to_string(),
            ));
        }

        let (thread_id, resolved_model) = match resume_session {
            None => {
                let response = client.thread_start(thread_start_params).await?;
                (response.thread.id, response.model)
            }
            Some(session_id) => {
                let response = client
                    .thread_fork(fork_params_from(session_id, thread_start_params))
                    .await?;
                tracing::debug!("forked thread, new thread_id={}", response.thread.id);
                (response.thread.id, response.model)
            }
        };

        client.set_resolved_model(resolved_model);
        client.register_session(&thread_id).await?;
        let collaboration_mode = client.initial_collaboration_mode()?;
        client
            .turn_start_with_mode(
                thread_id,
                vec![UserInput::Text {
                    text: combined_prompt,
                    text_elements: vec![],
                }],
                Some(collaboration_mode),
            )
            .await?;

        Ok(())
    }

    /// Common boilerplate for spawning a Codex app server process
    /// Handles process spawning, stdout/stderr piping, exit signal handling, client initialization, and error logging.
    /// Delegates the actual Codex session logic to the provided `task` closure.
    async fn spawn_app_server<F, Fut>(
        &self,
        current_dir: &Path,
        command_parts: CommandParts,
        env: &ExecutionEnv,
        task: F,
    ) -> Result<SpawnedChild, ExecutorError>
    where
        F: FnOnce(Arc<AppServerClient>, ExitSignalSender) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), ExecutorError>> + Send + 'static,
    {
        let (program_path, args) = command_parts.into_resolved().await?;

        let mut process = Command::new(program_path);
        process
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .current_dir(current_dir)
            .env("NPM_CONFIG_LOGLEVEL", "error")
            .env("NODE_NO_WARNINGS", "1")
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "error")
            .args(&args);

        env.clone()
            .with_profile(&self.cmd)
            .apply_to_command(&mut process);

        let mut child = process.group_spawn_no_window()?;

        let child_stdout = child.inner().stdout.take().ok_or_else(|| {
            ExecutorError::Io(std::io::Error::other("Codex app server missing stdout"))
        })?;
        let child_stdin = child.inner().stdin.take().ok_or_else(|| {
            ExecutorError::Io(std::io::Error::other("Codex app server missing stdin"))
        })?;

        let new_stdout = create_stdout_pipe_writer(&mut child)?;
        let (exit_signal_tx, exit_signal_rx) = tokio::sync::oneshot::channel();
        let cancel = tokio_util::sync::CancellationToken::new();

        let auto_approve = matches!(
            (&self.sandbox, &self.ask_for_approval),
            (Some(SandboxMode::DangerFullAccess), None)
        );
        let plan_mode = self.plan;
        let approvals = self.approvals.clone();
        let repo_context = env.repo_context.clone();
        let commit_reminder = env.commit_reminder;
        let commit_reminder_prompt = env.commit_reminder_prompt.clone();
        let cancel_for_task = cancel.clone();

        tokio::spawn(async move {
            let exit_signal_tx = ExitSignalSender::new(exit_signal_tx);
            let log_writer = LogWriter::new(new_stdout);

            // Initialize the AppServerClient
            let client = AppServerClient::new(
                log_writer.clone(),
                approvals,
                auto_approve,
                plan_mode,
                repo_context,
                commit_reminder,
                commit_reminder_prompt,
                cancel_for_task.clone(),
            );
            let rpc_peer = JsonRpcPeer::spawn(
                child_stdin,
                child_stdout,
                client.clone(),
                exit_signal_tx.clone(),
                cancel_for_task,
            );
            client.connect(rpc_peer);

            let result = async {
                client.initialize().await?;
                task(client, exit_signal_tx.clone()).await
            }
            .await;

            if let Err(err) = result {
                match &err {
                    ExecutorError::Io(io_err)
                        if io_err.kind() == std::io::ErrorKind::BrokenPipe =>
                    {
                        // Broken pipe likely means the parent process exited, so we can ignore it
                        return;
                    }
                    ExecutorError::AuthRequired(message) => {
                        log_writer
                            .log_raw(&Error::auth_required(message.clone()).raw())
                            .await
                            .ok();
                        exit_signal_tx
                            .send_exit_signal(ExecutorExitResult::Failure)
                            .await;
                        return;
                    }
                    _ => {
                        tracing::error!("Codex spawn error: {}", err);
                        log_writer
                            .log_raw(&Error::launch_error(err.to_string()).raw())
                            .await
                            .ok();
                    }
                }
                exit_signal_tx
                    .send_exit_signal(ExecutorExitResult::Failure)
                    .await;
            }
        });

        Ok(SpawnedChild {
            child,
            exit_signal: Some(exit_signal_rx),
            cancel: Some(cancel),
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{CatalogModel, Codex, ModelCatalog, ModelInfo, model_infos, resolve_model};

    #[cfg(unix)]
    mod app_server_guard {
        use std::{process::Stdio, time::Duration};

        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio_util::sync::CancellationToken;
        use workspace_utils::command_ext::GroupSpawnNoWindowExt;

        use crate::executors::codex::AppServerQueryGuard;

        fn is_alive(pid: i32) -> bool {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        }

        /// A dropped query must take the whole process group with it. Codex runs
        /// as a child of `npx`, and `kill_on_drop` only reaps the leader.
        #[tokio::test]
        async fn dropping_a_query_kills_the_process_group() {
            let mut command = tokio::process::Command::new("sh");
            command
                .arg("-c")
                .arg("sleep 60 & echo $!; wait")
                .kill_on_drop(true)
                .stdout(Stdio::piped());

            let mut child = command.group_spawn_no_window().expect("spawn the leader");
            let stdout = child.inner().stdout.take().expect("leader stdout");
            let grandchild: i32 = BufReader::new(stdout)
                .lines()
                .next_line()
                .await
                .expect("read the grandchild pid")
                .expect("grandchild pid line")
                .trim()
                .parse()
                .expect("parse the grandchild pid");
            assert!(is_alive(grandchild));

            let cancel = CancellationToken::new();
            drop(AppServerQueryGuard {
                child,
                cancel: cancel.clone(),
            });

            assert!(cancel.is_cancelled(), "the reader task was left running");

            // killpg lands asynchronously.
            for _ in 0..100 {
                if !is_alive(grandchild) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("grandchild {grandchild} outlived the dropped query");
        }
    }

    /// A catalog entry shaped like the ones `model/list` returns.
    fn catalog_model(
        model: &str,
        display_name: &str,
        efforts: &[&str],
        default_effort: &str,
        fast: bool,
    ) -> CatalogModel {
        catalog_model_with_visibility(model, display_name, efforts, default_effort, fast, false)
    }

    fn hidden_catalog_model(
        model: &str,
        display_name: &str,
        efforts: &[&str],
        default_effort: &str,
        fast: bool,
    ) -> CatalogModel {
        catalog_model_with_visibility(model, display_name, efforts, default_effort, fast, true)
    }

    fn catalog_model_with_visibility(
        model: &str,
        display_name: &str,
        efforts: &[&str],
        default_effort: &str,
        fast: bool,
        hidden: bool,
    ) -> CatalogModel {
        serde_json::from_value(json!({
            "id": model,
            "model": model,
            "upgrade": null,
            "upgradeInfo": null,
            "availabilityNux": null,
            "displayName": display_name,
            "description": "",
            "hidden": hidden,
            "supportedReasoningEfforts": efforts
                .iter()
                .map(|effort| json!({"reasoningEffort": effort, "description": ""}))
                .collect::<Vec<_>>(),
            "defaultReasoningEffort": default_effort,
            "multiAgentVersion": null,
            "additionalSpeedTiers": if fast { vec!["fast"] } else { vec![] },
            "serviceTiers": if fast {
                vec![json!({"id": "priority", "name": "Fast", "description": ""})]
            } else {
                vec![]
            },
            "isDefault": false,
        }))
        .expect("catalog model fixture should deserialize")
    }

    fn effort_ids(model: &ModelInfo) -> Vec<&str> {
        model
            .reasoning_options
            .iter()
            .map(|option| option.id.as_str())
            .collect()
    }

    #[test]
    fn model_infos_pairs_each_fast_capable_model_with_a_fast_variant() {
        let models = model_infos(&[
            catalog_model("gpt-6-astra", "GPT-6-Astra", &["low", "high"], "low", true),
            catalog_model("gpt-5.5", "GPT-5.5", &["low", "high"], "low", false),
        ]);

        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["gpt-6-astra", "gpt-6-astra-fast", "gpt-5.5"]);
        assert_eq!(models[1].name, "GPT-6-Astra Fast");
        // The fast variant resolves back to the model it was derived from.
        assert_eq!(
            resolve_model(Some(&models[1].id)),
            (Some("gpt-6-astra"), true)
        );
    }

    fn catalog(models: Vec<CatalogModel>, configured_model: Option<&str>) -> ModelCatalog {
        ModelCatalog {
            models,
            configured_model: configured_model.map(str::to_string),
            configured_fast: false,
        }
    }

    #[test]
    fn a_config_on_the_fast_tier_defaults_to_the_fast_variant() {
        // `/fast on` writes the tier to the Codex config, so a session that
        // names no model of its own still runs on the priority tier.
        let mut astra = catalog_model("gpt-6-astra", "GPT-6-Astra", &["low"], "low", true);
        astra.is_default = true;
        let catalog = ModelCatalog {
            models: vec![astra],
            configured_model: None,
            configured_fast: true,
        };

        let models = catalog.selectable_models();
        assert_eq!(
            catalog.default_model(&models),
            Some("gpt-6-astra-fast".to_string())
        );
    }

    #[test]
    fn a_fast_tier_config_leaves_a_model_without_a_fast_variant_alone() {
        let mut astra = catalog_model("gpt-6-astra", "GPT-6-Astra", &["low"], "low", false);
        astra.is_default = true;
        let catalog = ModelCatalog {
            models: vec![astra],
            configured_model: None,
            configured_fast: true,
        };

        let models = catalog.selectable_models();
        assert_eq!(
            catalog.default_model(&models),
            Some("gpt-6-astra".to_string())
        );
    }

    #[test]
    fn hidden_models_stay_out_of_the_selector() {
        let catalog = catalog(
            vec![
                catalog_model("gpt-6-astra", "GPT-6-Astra", &["low"], "low", false),
                hidden_catalog_model("private-coder", "Private Coder", &["low"], "low", false),
            ],
            None,
        );

        let ids: Vec<String> = catalog
            .selectable_models()
            .into_iter()
            .map(|model| model.id)
            .collect();
        assert_eq!(ids, ["gpt-6-astra"]);
    }

    #[test]
    fn a_hidden_configured_model_keeps_its_catalog_description() {
        // The catalog describes it even though the picker hides it, so the
        // entry that has to be offered anyway can be a real one.
        let catalog = ModelCatalog {
            models: vec![
                catalog_model("gpt-6-astra", "GPT-6-Astra", &["low"], "low", false),
                hidden_catalog_model(
                    "private-coder",
                    "Private Coder",
                    &["low", "high"],
                    "high",
                    true,
                ),
            ],
            configured_model: Some("private-coder".to_string()),
            configured_fast: true,
        };

        let models = catalog.selectable_models();
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["private-coder", "private-coder-fast", "gpt-6-astra"]);
        assert_eq!(models[0].name, "Private Coder");
        assert_eq!(effort_ids(&models[0]), ["low", "high"]);
        // A hidden model supporting the priority tier still gets the fast
        // default its config asks for.
        assert_eq!(
            catalog.default_model(&models),
            Some("private-coder-fast".to_string())
        );
    }

    #[test]
    fn a_pinned_model_version_is_described_by_its_catalog_entry() {
        // Codex resolves a configured slug by longest catalog prefix, so a
        // pinned date suffix still runs with `gpt-5.5`'s metadata.
        let catalog = ModelCatalog {
            models: vec![
                catalog_model("gpt-5.5", "GPT-5.5", &["low", "high"], "high", true),
                catalog_model("gpt-6-astra", "GPT-6-Astra", &["low"], "low", false),
            ],
            configured_model: Some("gpt-5.5-2026-01-01".to_string()),
            configured_fast: true,
        };

        let models = catalog.selectable_models();
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "gpt-5.5-2026-01-01",
                "gpt-5.5-2026-01-01-fast",
                "gpt-5.5",
                "gpt-5.5-fast",
                "gpt-6-astra"
            ]
        );
        // The alias is shown as configured, not as the entry it borrows from.
        assert_eq!(models[0].name, "gpt-5.5-2026-01-01");
        assert_eq!(effort_ids(&models[0]), ["low", "high"]);
        assert_eq!(
            catalog.default_model(&models),
            Some("gpt-5.5-2026-01-01-fast".to_string())
        );
    }

    #[test]
    fn a_namespaced_model_is_described_by_the_entry_it_names() {
        let catalog = catalog(
            vec![catalog_model(
                "gpt-5.5",
                "GPT-5.5",
                &["low", "high"],
                "high",
                false,
            )],
            Some("custom/gpt-5.5"),
        );

        let models = catalog.selectable_models();
        assert_eq!(models[0].id, "custom/gpt-5.5");
        assert_eq!(effort_ids(&models[0]), ["low", "high"]);
    }

    #[test]
    fn a_namespaced_model_may_also_be_pinned_to_a_version() {
        // Codex prefix-matches the namespaced suffix too, so both forms
        // combined still resolve to `gpt-5.5`.
        let catalog = ModelCatalog {
            models: vec![catalog_model(
                "gpt-5.5",
                "GPT-5.5",
                &["low", "high"],
                "high",
                true,
            )],
            configured_model: Some("custom/gpt-5.5-2026-01-01".to_string()),
            configured_fast: true,
        };

        let models = catalog.selectable_models();
        assert_eq!(models[0].id, "custom/gpt-5.5-2026-01-01");
        assert_eq!(effort_ids(&models[0]), ["low", "high"]);
        assert_eq!(
            catalog.default_model(&models),
            Some("custom/gpt-5.5-2026-01-01-fast".to_string())
        );
    }

    #[test]
    fn an_alias_with_no_claim_to_a_catalog_entry_stays_undescribed() {
        // Two segments, so not the single provider-style namespace Codex
        // strips: it must not borrow another model's efforts.
        let catalog = catalog(
            vec![catalog_model("gpt-5.5", "GPT-5.5", &["low"], "low", true)],
            Some("vendor/team/gpt-5.5"),
        );

        let models = catalog.selectable_models();
        assert_eq!(models[0].id, "vendor/team/gpt-5.5");
        assert!(models[0].reasoning_options.is_empty());
        assert!(
            !models
                .iter()
                .any(|model| model.id == "vendor/team/gpt-5.5-fast")
        );
    }

    #[test]
    fn a_catalog_with_nothing_visible_still_offers_the_configured_model() {
        // `include_hidden: false` can empty the list while the config still
        // names the model the session would run.
        let catalog = catalog(Vec::new(), Some("private-coder"));

        let models = catalog.selectable_models();
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["private-coder"]);
        assert_eq!(
            catalog.default_model(&models),
            Some("private-coder".to_string())
        );
    }

    #[test]
    fn the_discovery_provider_override_outranks_hand_written_parameters() {
        // A session's typed `thread/start` provider wins over its command line,
        // so discovery has to resolve the same way round. Codex applies
        // repeated `-c` keys in order, which makes this a question of position.
        let codex: Codex = serde_json::from_value(json!({
            "model_provider": "openai",
            "additional_params": ["-c", "model_provider=other"],
        }))
        .expect("codex config");

        let rendered = format!(
            "{:?}",
            codex
                .build_discovery_command_builder()
                .expect("discovery command")
                .build_initial()
                .expect("initial command")
        );

        let ours = rendered
            .rfind(r#"model_provider=\"openai\""#)
            .expect("the generated override is present");
        let theirs = rendered
            .rfind("model_provider=other")
            .expect("the hand-written override is present");
        assert!(ours > theirs, "the generated override must be applied last");
    }

    #[test]
    fn a_deprecated_speed_tier_alone_does_not_make_a_fast_variant() {
        // Codex reads `service_tiers` and nothing else, so a Fast entry built
        // on the deprecated field would run at the ordinary tier.
        let mut model = catalog_model("acme", "Acme", &["low"], "low", false);
        model.additional_speed_tiers = vec!["fast".to_string()];

        let ids: Vec<String> = model_infos(&[model])
            .into_iter()
            .map(|model| model.id)
            .collect();
        assert_eq!(ids, ["acme"]);
    }

    #[test]
    fn model_infos_skips_a_catalog_id_that_collides_with_the_fast_suffix() {
        // Selecting `acme-fast` would resolve back to `acme`, a different
        // model, so it must not be offered at all.
        let models = model_infos(&[
            catalog_model("acme-fast", "Acme Fast", &["low"], "low", false),
            catalog_model("gpt-5.5", "GPT-5.5", &["low"], "low", false),
        ]);

        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["gpt-5.5"]);
    }

    #[test]
    fn the_configured_model_is_offered_even_when_the_catalog_omits_it() {
        let catalog = catalog(
            vec![catalog_model(
                "gpt-6-astra",
                "GPT-6-Astra",
                &["low"],
                "low",
                false,
            )],
            Some("private-coder"),
        );

        let models = catalog.selectable_models();
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["private-coder", "gpt-6-astra"]);
        // Without the entry above, the frontend would fall back to the first
        // model and name one the session would not run.
        assert_eq!(
            catalog.default_model(&models),
            Some("private-coder".to_string())
        );
    }

    #[test]
    fn the_configured_model_outranks_the_catalogs_own_default() {
        let mut astra = catalog_model("gpt-6-astra", "GPT-6-Astra", &["low"], "low", false);
        astra.is_default = true;
        let catalog = catalog(
            vec![
                astra,
                catalog_model("gpt-5.5", "GPT-5.5", &["low"], "low", false),
            ],
            Some("gpt-5.5"),
        );

        let models = catalog.selectable_models();
        assert_eq!(catalog.default_model(&models), Some("gpt-5.5".to_string()));
    }

    #[test]
    fn an_unofferable_configured_model_falls_back_to_the_catalog_default() {
        let mut astra = catalog_model("gpt-6-astra", "GPT-6-Astra", &["low"], "low", false);
        astra.is_default = true;
        let catalog = catalog(vec![astra], Some("acme-fast"));

        let models = catalog.selectable_models();
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["gpt-6-astra"]);
        assert_eq!(
            catalog.default_model(&models),
            Some("gpt-6-astra".to_string())
        );
    }

    #[test]
    fn model_infos_marks_the_reasoning_effort_the_catalog_defaults_to() {
        let models = model_infos(&[catalog_model(
            "gpt-5.6-sol",
            "GPT-5.6-Sol",
            &["low", "medium", "high"],
            "low",
            false,
        )]);

        let defaults: Vec<&str> = models[0]
            .reasoning_options
            .iter()
            .filter(|option| option.is_default)
            .map(|option| option.id.as_str())
            .collect();
        assert_eq!(defaults, ["low"]);
    }

    #[test]
    fn model_infos_keeps_the_efforts_the_executor_can_apply_in_order() {
        let models = model_infos(&[catalog_model(
            "gpt-6-astra",
            "GPT-6-Astra",
            // `escalated` is not an effort Codex has ever named, so it must not
            // reach the selector: it would be silently dropped on the way back.
            &["ultra", "escalated", "max", "low"],
            "max",
            false,
        )]);

        assert_eq!(effort_ids(&models[0]), ["low", "max", "ultra"]);
    }

    #[test]
    fn resolve_model_detects_fast_suffix() {
        assert_eq!(resolve_model(Some("gpt-5.5-fast")), (Some("gpt-5.5"), true));
        assert_eq!(resolve_model(Some("gpt-5.4-fast")), (Some("gpt-5.4"), true));
    }

    #[test]
    fn resolve_model_leaves_non_fast_models_unchanged() {
        assert_eq!(resolve_model(Some("gpt-5.5")), (Some("gpt-5.5"), false));
        assert_eq!(
            resolve_model(Some("gpt-5.4-mini")),
            (Some("gpt-5.4-mini"), false)
        );
        assert_eq!(resolve_model(None), (None, false));
    }
}
