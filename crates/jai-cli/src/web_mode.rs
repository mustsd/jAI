//! `jai --web`: expose the agent through the local web UI.

use crate::app::App;
use crate::args::Cli;
use jai_core::TurnHandle;
use jai_core::error::Result;
use jai_core::message::{ModelInfo, PermissionMode};
use jai_core::policy::Approver;
use jai_core::provider::{BoxFuture, Provider};
use jai_core::store::SessionStore;
use jai_web::{WebBackend, WebOptions};
use std::path::PathBuf;
use std::sync::Arc;

/// Adapter that lets the web UI drive the same agent the CLI uses.
struct Runtime {
    app: App,
    provider_name: String,
    provider: Arc<dyn Provider>,
    model: String,
}

impl WebBackend for Runtime {
    fn provider_name(&self) -> String {
        self.provider_name.clone()
    }

    fn model(&self) -> String {
        self.model.clone()
    }

    fn mode(&self) -> PermissionMode {
        self.app.mode
    }

    fn workspace(&self) -> PathBuf {
        self.app.workspace.clone()
    }

    fn tools(&self) -> Vec<String> {
        match self.app.tools() {
            Ok(Some(tools)) => tools
                .enabled_tools()
                .into_iter()
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        }
    }

    fn store(&self) -> Arc<dyn SessionStore> {
        Arc::clone(&self.app.store)
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>>> {
        self.provider.list_models()
    }

    fn start_turn(
        &self,
        session_id: &str,
        input: &str,
        approver: Arc<dyn Approver>,
    ) -> Result<TurnHandle> {
        let engine = self.app.engine(
            Arc::clone(&self.provider),
            &self.provider_name,
            &self.model,
            self.app.mode,
            approver,
        )?;
        Ok(engine.start_turn(session_id, input))
    }
}

/// Run the local web interface until Ctrl-C.
pub async fn run(app: App, cli: &Cli) -> Result<i32> {
    let provider_name = app.provider_name(cli)?;
    let resolved = app.resolved_provider(cli, &provider_name)?;
    let model = app.model_name(cli, &resolved)?;
    let provider = app.provider(&resolved)?;
    app.remember(&provider_name, &model);

    let options = WebOptions {
        bind: app.config.web.bind.clone(),
        port: app.config.web.port,
        local_only: app.config.web.local_only,
    };

    let runtime = Runtime {
        provider_name,
        provider,
        model,
        app,
    };
    jai_web::serve(Arc::new(runtime), options).await?;
    Ok(0)
}
