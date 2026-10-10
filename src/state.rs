use std::ops::Deref;
use std::sync::Arc;

use crate::config::Config;
use crate::db::{self, Db, Repo, User};

pub struct Inner {
    pub config: Config,
    pub db: Db,
}

/// Shared application state for the web and SSH servers.
#[derive(Clone)]
pub struct AppState(Arc<Inner>);

impl AppState {
    pub fn new(config: Config, db: Db) -> Self {
        AppState(Arc::new(Inner { config, db }))
    }
}

impl Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

/// What a user may do with a repository.
#[derive(Debug, Clone, Copy, Default)]
pub struct Access {
    pub read: bool,
    pub write: bool,
    pub admin: bool,
}

impl AppState {
    pub async fn access(&self, repo: &Repo, user: Option<&User>) -> Result<Access, sqlx::Error> {
        let Some(user) = user else {
            return Ok(Access {
                read: !repo.is_private,
                write: false,
                admin: false,
            });
        };
        if user.is_admin || user.id == repo.owner_id {
            return Ok(Access {
                read: true,
                write: true,
                admin: true,
            });
        }
        let collaborator = db::is_collaborator(&self.db, repo.id, user.id).await?;
        Ok(Access {
            read: collaborator || !repo.is_private,
            write: collaborator,
            admin: false,
        })
    }
}
