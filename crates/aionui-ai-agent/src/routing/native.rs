use crate::{
    AgentError,
    factory::aionrs::{
        map_aionrs_provider, resolve_aionrs_url_and_compat_with_mode, resolve_bedrock_config,
        resolve_model_compat_overrides,
    },
};
use aion_config::config::{CliArgs, Config};
use aion_providers::{LlmProvider, create_provider};
use aionui_db::models::Provider;
use std::sync::Arc;

pub(super) fn configured_provider(
    row: &Provider,
    model: &str,
    key: &[u8; 32],
    workspace: &str,
) -> Result<(Arc<dyn LlmProvider>, Config), AgentError> {
    let provider = map_aionrs_provider(&row.platform, model, row.model_protocols.as_deref())?;
    let overrides = resolve_model_compat_overrides(model, &row.model_settings)?;
    let (base_url, compat) = resolve_aionrs_url_and_compat_with_mode(
        &row.platform,
        &row.base_url,
        &provider,
        model,
        row.is_full_url,
        overrides.openai_api_mode,
    );
    let args = CliArgs {
        provider: Some(provider),
        api_key: Some(
            aionui_common::decrypt_string(&row.api_key_encrypted, key)
                .map_err(|_| AgentError::bad_request("Router provider credentials are unavailable"))?,
        ),
        base_url,
        model: Some(model.into()),
        max_tokens: None,
        thinking: None,
        thinking_budget: None,
        max_turns: None,
        max_tool_call_malformed_turns: None,
        max_tool_call_failure_turns: None,
        system_prompt: None,
        profile: None,
        auto_approve: false,
        project_dir: (!workspace.is_empty()).then(|| std::path::PathBuf::from(workspace)),
    };
    let mut config = Config::resolve_isolated(&args)
        .map_err(|_| AgentError::bad_request("Router provider configuration is invalid"))?;
    let provider_settings =
        Config::resolve(&args).map_err(|_| AgentError::bad_request("Provider configuration is unavailable"))?;
    config.compat = provider_settings.compat;
    config.vertex = provider_settings.vertex;
    config.prompt_caching = provider_settings.prompt_caching;
    let defaults = match config.provider {
        aion_config::config::ProviderType::Anthropic | aion_config::config::ProviderType::Vertex => {
            aion_config::compat::ProviderCompat::anthropic_defaults()
        }
        aion_config::config::ProviderType::OpenAI => aion_config::compat::ProviderCompat::openai_defaults(),
        aion_config::config::ProviderType::Bedrock => aion_config::compat::ProviderCompat::bedrock_defaults(),
    };
    config.compat.transport.default_max_tokens = defaults.transport.default_max_tokens;
    config.compat.transport.model_max_tokens = defaults.transport.model_max_tokens;
    config.compat.image_input = Some(overrides.image_input.unwrap_or_else(|| {
        crate::capability::image_input::resolve_image_input_capability(
            args.provider.as_deref().unwrap_or_default(),
            args.base_url.as_deref(),
            model,
        )
    }));
    if let Some(mode) = compat.openai_api_mode {
        config.compat.transport.openai_api_mode = Some(mode);
    }
    if let Some(field) = compat.max_tokens_field {
        config.compat.transport.max_tokens_field = Some(field);
    }
    if let Some(path) = compat.api_path {
        config.compat.transport.api_path = Some(path);
    }
    if let Some(image) = overrides.image_input {
        config.compat.image_input = Some(image);
    }
    config.bedrock = resolve_bedrock_config(row.bedrock_config.as_deref());
    Ok((create_provider(&config), config))
}
