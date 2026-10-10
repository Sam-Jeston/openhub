mod config;
mod db;
mod git;
mod highlight;
mod icons;
mod markdown;
mod ssh;
mod state;
mod sync;
mod validate;
mod web;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("openhub=info")),
        )
        .init();

    let mut config = Config::parse();
    config.normalize();
    std::fs::create_dir_all(config.repos_dir())
        .with_context(|| format!("creating data directory {}", config.data_dir.display()))?;
    check_git(&config.git_bin)?;

    let db = db::connect(&config.db_path()).await?;
    let state = AppState::new(config, db);

    tokio::select! {
        res = web::serve(state.clone()) => res.context("web server stopped"),
        res = ssh::serve(state.clone()) => res.context("SSH server stopped"),
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
            Ok(())
        }
    }
}

/// Fetches and pushes are served by the system git, so make sure it exists.
fn check_git(git_bin: &str) -> anyhow::Result<()> {
    let output = std::process::Command::new(git_bin)
        .arg("--version")
        .output()
        .with_context(|| {
            format!("could not run `{git_bin}`; Openhub requires git to be installed")
        })?;
    tracing::info!("using {}", String::from_utf8_lossy(&output.stdout).trim());
    Ok(())
}
