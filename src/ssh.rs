//! Built-in SSH server for `git push` / `git fetch`.
//!
//! Users authenticate with the public keys registered in their settings.
//! The only commands accepted are `git-upload-pack` (fetch) and
//! `git-receive-pack` (push); each is mapped to a hosted repository after
//! checking permissions and run with the system `git`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rand::RngCore;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::ssh_key::{HashAlg, LineEnding, PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet, Pty, SshId};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::db::{self, Repo, User};
use crate::git;
use crate::state::AppState;
use crate::sync;

pub async fn serve(state: AppState) -> Result<()> {
    let key = load_or_create_host_key(&state.config.host_key_path())?;
    tracing::info!(
        "SSH host key fingerprint: {}",
        key.public_key().fingerprint(HashAlg::Sha256)
    );
    let mut methods = MethodSet::empty();
    methods.push(MethodKind::PublicKey);
    let config = Arc::new(server::Config {
        server_id: SshId::Standard("SSH-2.0-Openhub".into()),
        methods,
        auth_rejection_time: Duration::from_millis(300),
        auth_rejection_time_initial: Some(Duration::ZERO),
        keys: vec![key],
        inactivity_timeout: Some(Duration::from_secs(3600)),
        keepalive_interval: Some(Duration::from_secs(60)),
        nodelay: true,
        ..Default::default()
    });
    let addr = state.config.ssh_addr;
    tracing::info!("SSH server listening on {addr}");
    let mut server = SshServer { state };
    server.run_on_address(config, addr).await?;
    Ok(())
}

fn load_or_create_host_key(path: &Path) -> Result<PrivateKey> {
    if path.exists() {
        return PrivateKey::read_openssh_file(path)
            .with_context(|| format!("reading SSH host key {}", path.display()));
    }
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    let key = PrivateKey::from(Ed25519Keypair::from_seed(&seed));
    let pem = key.to_openssh(LineEnding::LF)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    std::io::Write::write_all(&mut opts.open(path)?, pem.as_bytes())?;
    tracing::info!("generated new SSH host key at {}", path.display());
    Ok(key)
}

struct SshServer {
    state: AppState,
}

impl server::Server for SshServer {
    type Handler = SshSession;

    fn new_client(&mut self, peer: Option<SocketAddr>) -> SshSession {
        SshSession {
            state: self.state.clone(),
            peer,
            user: None,
            channels: HashMap::new(),
        }
    }

    fn handle_session_error(&mut self, error: anyhow::Error) {
        tracing::debug!("ssh session ended with error: {error:#}");
    }
}

#[derive(Default)]
struct ChannelState {
    /// Value of GIT_PROTOCOL sent by the client (enables protocol v2).
    protocol: Option<String>,
    /// Feeds client data to the running git process. Dropping it closes stdin.
    stdin: Option<mpsc::UnboundedSender<Vec<u8>>>,
}

struct SshSession {
    state: AppState,
    peer: Option<SocketAddr>,
    user: Option<User>,
    channels: HashMap<ChannelId, ChannelState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Service {
    UploadPack,
    ReceivePack,
}

impl Service {
    fn subcommand(self) -> &'static str {
        match self {
            Service::UploadPack => "upload-pack",
            Service::ReceivePack => "receive-pack",
        }
    }
}

/// Parses `git-upload-pack '/owner/repo.git'` and similar into the service
/// and the (owner, repo) pair.
fn parse_command(command: &str) -> Option<(Service, String, String)> {
    let command = command.trim();
    let (verb, rest) = command.split_once(' ')?;
    let (service, arg) = match verb {
        "git-upload-pack" => (Service::UploadPack, rest),
        "git-receive-pack" => (Service::ReceivePack, rest),
        "git" => {
            let (sub, arg) = rest.trim_start().split_once(' ')?;
            match sub {
                "upload-pack" => (Service::UploadPack, arg),
                "receive-pack" => (Service::ReceivePack, arg),
                _ => return None,
            }
        }
        _ => return None,
    };
    let arg = arg.trim();
    let unquoted = arg
        .strip_prefix('\'')
        .and_then(|a| a.strip_suffix('\''))
        .or_else(|| arg.strip_prefix('"').and_then(|a| a.strip_suffix('"')))
        .unwrap_or(arg);
    if unquoted.contains(['\'', '"', '\\', '\0']) {
        return None;
    }
    let path = unquoted.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some((service, owner.to_string(), name.to_string()))
}

fn branch_tips(path: &Path) -> HashMap<String, String> {
    let Ok(repo) = git::open(path) else {
        return HashMap::new();
    };
    git::branch_names(&repo)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|b| git::branch_tip(&repo, &b).map(|t| (b, t.to_string())))
        .collect()
}

async fn forward<R, F, Fut>(mut reader: R, mut send: F)
where
    R: AsyncRead + Unpin,
    F: FnMut(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if !send(buf[..n].to_vec()).await {
                    break;
                }
            }
        }
    }
}

impl SshSession {
    fn reject(session: &mut Session, channel: ChannelId, message: &str) -> Result<()> {
        session.channel_success(channel)?;
        session.extended_data(channel, 1, format!("ERROR: {message}\n").into_bytes())?;
        session.exit_status_request(channel, 1)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

/// Lines shown after a push, pointing at pull requests for pushed branches.
async fn push_hints(
    state: &AppState,
    repo: &Repo,
    before: &HashMap<String, String>,
    after: &HashMap<String, String>,
) -> Vec<String> {
    let base = format!("{}/{}/{}", state.config.public_url, repo.owner, repo.name);
    let default = db::repo_by_name(&state.db, &repo.owner, &repo.name)
        .await
        .ok()
        .flatten()
        .map(|r| r.default_branch)
        .unwrap_or_else(|| repo.default_branch.clone());
    let open = db::list_pulls(&state.db, repo.id, true)
        .await
        .unwrap_or_default();
    let mut lines = Vec::new();
    let mut updated: Vec<&String> = after
        .iter()
        .filter(|(b, tip)| before.get(*b) != Some(*tip) && **b != default)
        .map(|(b, _)| b)
        .collect();
    updated.sort();
    for branch in updated {
        lines.push(String::new());
        match open.iter().find(|p| &p.head_branch == branch) {
            Some(pr) => {
                lines.push(format!("View pull request for '{branch}':"));
                lines.push(format!("     {base}/pull/{}", pr.number));
            }
            None => {
                lines.push(format!(
                    "Create a pull request for '{branch}' on Openhub by visiting:"
                ));
                lines.push(format!(
                    "     {base}/compare/{}...{}",
                    crate::web::fmt::urlpath(&default),
                    crate::web::fmt::urlpath(branch)
                ));
            }
        }
    }
    if !lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

impl server::Handler for SshSession {
    type Error = anyhow::Error;

    async fn auth_publickey_offered(&mut self, _user: &str, key: &PublicKey) -> Result<Auth> {
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        Ok(
            if db::fingerprint_exists(&self.state.db, &fingerprint).await? {
                Auth::Accept
            } else {
                Auth::reject()
            },
        )
    }

    async fn auth_publickey(&mut self, _user: &str, key: &PublicKey) -> Result<Auth> {
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        match db::user_by_key_fingerprint(&self.state.db, &fingerprint).await? {
            Some((user, key_id)) => {
                let _ = db::touch_key(&self.state.db, key_id).await;
                tracing::info!(
                    "ssh: {} authenticated with key {fingerprint} from {:?}",
                    user.username,
                    self.peer
                );
                self.user = Some(user);
                Ok(Auth::Accept)
            }
            None => Ok(Auth::reject()),
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<()> {
        self.channels.insert(channel.id(), ChannelState::default());
        // Data arrives through the `data` callback; dropping the channel
        // handle keeps russh from also buffering it there.
        drop(channel);
        reply.accept().await;
        Ok(())
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        value: &str,
        session: &mut Session,
    ) -> Result<()> {
        if name == "GIT_PROTOCOL" {
            if let Some(state) = self.channels.get_mut(&channel) {
                state.protocol = Some(value.to_string());
            }
            session.channel_success(channel)?;
        } else {
            session.channel_failure(channel)?;
        }
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<()> {
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<()> {
        let name = self
            .user
            .as_ref()
            .map(|u| u.username.as_str())
            .unwrap_or("there");
        session.channel_success(channel)?;
        let msg = format!(
            "Hi {name}! You've successfully authenticated, but Openhub does not provide shell access.\r\n"
        );
        session.extended_data(channel, 1, msg.into_bytes())?;
        session.exit_status_request(channel, 1)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<()> {
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<()> {
        let Some(user) = self.user.clone() else {
            return Self::reject(session, channel, "Not authenticated.");
        };
        let command = String::from_utf8_lossy(data).to_string();
        let Some((service, owner, name)) = parse_command(&command) else {
            tracing::info!(
                "ssh: {} sent unsupported command {command:?}",
                user.username
            );
            return Self::reject(
                session,
                channel,
                "Openhub only supports git fetch and push over SSH.",
            );
        };
        let repo = db::repo_by_name(&self.state.db, &owner, &name).await?;
        let access = match &repo {
            Some(r) => self.state.access(r, Some(&user)).await?,
            None => Default::default(),
        };
        let Some(repo) = repo.filter(|_| access.read) else {
            return Self::reject(session, channel, "Repository not found.");
        };
        if service == Service::ReceivePack && !access.write {
            let msg = format!(
                "You do not have permission to push to {}/{}.",
                repo.owner, repo.name
            );
            return Self::reject(session, channel, &msg);
        }

        let path = self.state.config.repo_path(repo.id);
        let before = if service == Service::ReceivePack {
            let p = path.clone();
            tokio::task::spawn_blocking(move || branch_tips(&p)).await?
        } else {
            HashMap::new()
        };

        let mut cmd = Command::new(&self.state.config.git_bin);
        cmd.arg(service.subcommand())
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(protocol) = self.channels.get(&channel).and_then(|c| c.protocol.clone()) {
            cmd.env("GIT_PROTOCOL", protocol);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("could not start git: {e}");
                return Self::reject(session, channel, "Internal server error.");
            }
        };
        tracing::info!(
            "ssh: {} running {} on {}/{}",
            user.username,
            service.subcommand(),
            repo.owner,
            repo.name
        );
        session.channel_success(channel)?;

        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        self.channels.entry(channel).or_default().stdin = Some(tx);
        let mut stdin = child.stdin.take().context("git stdin")?;
        let stdout = child.stdout.take().context("git stdout")?;
        let stderr = child.stderr.take().context("git stderr")?;

        tokio::spawn(async move {
            while let Some(buf) = rx.recv().await {
                if stdin.write_all(&buf).await.is_err() {
                    break;
                }
            }
        });

        let handle = session.handle();
        let state = self.state.clone();
        tokio::spawn(async move {
            let out = forward(stdout, |buf| {
                let h = handle.clone();
                async move { h.data(channel, buf).await.is_ok() }
            });
            let err = forward(stderr, |buf| {
                let h = handle.clone();
                async move { h.extended_data(channel, 1, buf).await.is_ok() }
            });
            tokio::join!(out, err);
            let code = match child.wait().await {
                Ok(status) => status.code().unwrap_or(1),
                Err(_) => 1,
            };
            if service == Service::ReceivePack && code == 0 {
                if let Err(e) = sync::after_push(&state, &repo, user.id).await {
                    tracing::error!(
                        "post-push sync failed for {}/{}: {e:#}",
                        repo.owner,
                        repo.name
                    );
                }
                let p = path.clone();
                let after = tokio::task::spawn_blocking(move || branch_tips(&p))
                    .await
                    .unwrap_or_default();
                for line in push_hints(&state, &repo, &before, &after).await {
                    let text = format!("remote: {line}\n");
                    let _ = handle.extended_data(channel, 1, text.into_bytes()).await;
                }
            }
            let _ = handle.exit_status_request(channel, code as u32).await;
            let _ = handle.eof(channel).await;
            let _ = handle.close(channel).await;
        });
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<()> {
        if let Some(tx) = self.channels.get(&channel).and_then(|c| c.stdin.as_ref()) {
            let _ = tx.send(data.to_vec());
        }
        Ok(())
    }

    async fn channel_eof(&mut self, channel: ChannelId, _session: &mut Session) -> Result<()> {
        if let Some(state) = self.channels.get_mut(&channel) {
            state.stdin = None;
        }
        Ok(())
    }

    async fn channel_close(&mut self, channel: ChannelId, _session: &mut Session) -> Result<()> {
        self.channels.remove(&channel);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_commands() {
        assert_eq!(
            parse_command("git-upload-pack '/alice/project.git'"),
            Some((Service::UploadPack, "alice".into(), "project".into()))
        );
        assert_eq!(
            parse_command("git-receive-pack 'alice/project.git'"),
            Some((Service::ReceivePack, "alice".into(), "project".into()))
        );
        assert_eq!(
            parse_command("git upload-pack 'alice/project'"),
            Some((Service::UploadPack, "alice".into(), "project".into()))
        );
        assert_eq!(
            parse_command("git-upload-archive 'alice/project.git'"),
            None
        );
        assert_eq!(parse_command("rm -rf /"), None);
        assert_eq!(parse_command("git-upload-pack 'a/b/c.git'"), None);
        assert_eq!(parse_command("git-upload-pack 'a/b'; ls'"), None);
    }
}
