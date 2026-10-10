use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

/// Openhub: a minimal, self-hostable git server.
#[derive(Parser, Debug, Clone)]
#[command(name = "openhub", version, about)]
pub struct Config {
    /// Directory holding the database, bare repositories and the SSH host key.
    #[arg(long, env = "OPENHUB_DATA_DIR", default_value = "./data")]
    pub data_dir: PathBuf,

    /// Address the web interface (and git over HTTP) listens on.
    #[arg(long, env = "OPENHUB_HTTP_ADDR", default_value = "0.0.0.0:3000")]
    pub http_addr: SocketAddr,

    /// Address the built-in SSH server listens on.
    #[arg(long, env = "OPENHUB_SSH_ADDR", default_value = "0.0.0.0:2222")]
    pub ssh_addr: SocketAddr,

    /// Public base URL of the web interface, used for links and HTTP clone URLs.
    #[arg(
        long,
        env = "OPENHUB_PUBLIC_URL",
        default_value = "http://localhost:3000"
    )]
    pub public_url: String,

    /// Host name shown in SSH clone URLs. Defaults to the host of the public URL.
    #[arg(long, env = "OPENHUB_SSH_HOST")]
    pub ssh_host: Option<String>,

    /// Port shown in SSH clone URLs. Defaults to the SSH listen port.
    #[arg(long, env = "OPENHUB_SSH_PORT")]
    pub ssh_port: Option<u16>,

    /// Disable self-service sign up. The first account can always be created.
    #[arg(long, env = "OPENHUB_DISABLE_REGISTRATION")]
    pub disable_registration: bool,

    /// Path to the git executable used to serve fetches and pushes.
    #[arg(long, env = "OPENHUB_GIT_BIN", default_value = "git")]
    pub git_bin: String,
}

impl Config {
    pub fn normalize(&mut self) {
        while self.public_url.ends_with('/') {
            self.public_url.pop();
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("openhub.db")
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.data_dir.join("repos")
    }

    pub fn host_key_path(&self) -> PathBuf {
        self.data_dir.join("ssh_host_ed25519_key")
    }

    pub fn repo_path(&self, repo_id: i64) -> PathBuf {
        self.repos_dir().join(format!("{repo_id}.git"))
    }

    pub fn secure_cookies(&self) -> bool {
        self.public_url.starts_with("https://")
    }

    fn public_host(&self) -> String {
        let rest = self
            .public_url
            .split_once("://")
            .map(|(_, r)| r)
            .unwrap_or(&self.public_url);
        let authority = rest.split('/').next().unwrap_or(rest);
        // Strip a port, taking care not to break bracketed IPv6 literals.
        match authority.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
                host.to_string()
            }
            _ => authority.to_string(),
        }
    }

    pub fn ssh_display_host(&self) -> String {
        self.ssh_host.clone().unwrap_or_else(|| self.public_host())
    }

    pub fn ssh_display_port(&self) -> u16 {
        self.ssh_port.unwrap_or(self.ssh_addr.port())
    }

    pub fn ssh_clone_url(&self, owner: &str, repo: &str) -> String {
        let host = self.ssh_display_host();
        match self.ssh_display_port() {
            22 => format!("git@{host}:{owner}/{repo}.git"),
            port => format!("ssh://git@{host}:{port}/{owner}/{repo}.git"),
        }
    }

    pub fn http_clone_url(&self, owner: &str, repo: &str) -> String {
        format!("{}/{owner}/{repo}.git", self.public_url)
    }

    /// The command users can run to check their SSH key is accepted.
    pub fn ssh_test_command(&self) -> String {
        let host = self.ssh_display_host();
        match self.ssh_display_port() {
            22 => format!("ssh -T git@{host}"),
            port => format!("ssh -T -p {port} git@{host}"),
        }
    }
}
