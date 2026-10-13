use crate::manager::ModelsManager;
use crate::manager::ModelsManagerFuture;
use crate::manager::RefreshStrategy;
use crate::manager::SharedModelsManager;
use codex_http_client::HttpClientFactory;
use codex_login::AuthManager;
use codex_protocol::ModelContextPreset;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelsResponse;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::TryLockError;

/// Adds local context choices to each fresh provider catalog without owning its
/// cache, authentication, refresh policy, or model capability metadata.
#[derive(Debug)]
struct ContextPresetsModelsManager {
    inner: SharedModelsManager,
    presets: BTreeMap<String, ModelContextPreset>,
}

pub fn with_context_presets(
    inner: SharedModelsManager,
    presets: BTreeMap<String, ModelContextPreset>,
) -> SharedModelsManager {
    if presets.is_empty() {
        inner
    } else {
        Arc::new(ContextPresetsModelsManager { inner, presets })
    }
}

impl ContextPresetsModelsManager {
    fn add_presets(&self, mut models: Vec<ModelInfo>) -> Vec<ModelInfo> {
        let additions = self
            .presets
            .iter()
            .filter_map(|(id, preset)| {
                let (_, base) = id.split_once('/')?;
                if models.iter().any(|model| model.slug == *id) {
                    return None;
                }
                let mut model = models.iter().find(|model| model.slug == base)?.clone();
                model.request_model = Some(base.to_string());
                model.slug = id.clone();
                model.display_name = preset.display_name.clone();
                model.visibility = codex_protocol::openai_models::ModelVisibility::List;
                // A local context choice must not trigger the base model's migration UI.
                model.upgrade = None;
                model.availability_nux = None;
                Some(model)
            })
            .collect::<Vec<_>>();
        models.extend(additions);
        models
    }
}

impl ModelsManager for ContextPresetsModelsManager {
    fn set_api_key_model_discovery_enabled(&self, enabled: bool) {
        self.inner.set_api_key_model_discovery_enabled(enabled);
    }

    fn raw_model_catalog(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ModelsResponse> {
        Box::pin(async move {
            let mut catalog = self
                .inner
                .raw_model_catalog(refresh_strategy, http_client_factory)
                .await;
            catalog.models = self.add_presets(catalog.models);
            catalog
        })
    }

    fn refresh_after_auth_change(
        &self,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ()> {
        self.inner.refresh_after_auth_change(http_client_factory)
    }

    fn get_remote_models(&self) -> ModelsManagerFuture<'_, Vec<ModelInfo>> {
        Box::pin(async { self.add_presets(self.inner.get_remote_models().await) })
    }

    fn try_get_remote_models(&self) -> Result<Vec<ModelInfo>, TryLockError> {
        self.inner
            .try_get_remote_models()
            .map(|models| self.add_presets(models))
    }

    fn auth_manager(&self) -> Option<&AuthManager> {
        self.inner.auth_manager()
    }

    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        self.inner.list_collaboration_modes()
    }

    fn refresh_if_new_etag(
        &self,
        etag: String,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ()> {
        self.inner.refresh_if_new_etag(etag, http_client_factory)
    }
}
