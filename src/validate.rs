//! Input validation for names and keys.

use russh::keys::ssh_key::{HashAlg, PublicKey};

/// Usernames that would collide with top-level routes.
const RESERVED: &[&str] = &[
    "admin",
    "api",
    "assets",
    "explore",
    "favicon.svg",
    "git",
    "help",
    "login",
    "logout",
    "new",
    "register",
    "root",
    "settings",
    "signin",
    "signup",
    "static",
    "system",
];

pub fn username(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > 39 {
        return Err("Username must be between 1 and 39 characters.");
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("Username may only contain letters, numbers and hyphens.");
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return Err("Username cannot start or end with a hyphen or contain consecutive hyphens.");
    }
    if RESERVED.contains(&name.to_ascii_lowercase().as_str()) {
        return Err("That username is reserved.");
    }
    Ok(())
}

pub fn repo_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > 100 {
        return Err("Repository name must be between 1 and 100 characters.");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err("Repository name may only contain letters, numbers, '-', '_' and '.'.");
    }
    if name.starts_with('.') || name.to_ascii_lowercase().ends_with(".git") {
        return Err("Repository name cannot start with '.' or end with '.git'.");
    }
    Ok(())
}

pub fn email(email: &str) -> Result<(), &'static str> {
    let ok = email.len() <= 254
        && email
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
        && !email.chars().any(char::is_whitespace);
    if ok {
        Ok(())
    } else {
        Err("Please enter a valid email address.")
    }
}

pub fn password(password: &str) -> Result<(), &'static str> {
    if password.chars().count() < 8 {
        return Err("Password must be at least 8 characters.");
    }
    if password.len() > 256 {
        return Err("Password is too long.");
    }
    Ok(())
}

pub struct ParsedKey {
    pub normalized: String,
    pub fingerprint: String,
    pub key_type: String,
    pub comment: String,
}

/// Parses an OpenSSH public key line, e.g. `ssh-ed25519 AAAA... me@laptop`.
pub fn ssh_public_key(input: &str) -> Result<ParsedKey, &'static str> {
    let line = input.trim();
    if line.contains('\n') {
        return Err("Please paste a single public key.");
    }
    if line.contains("PRIVATE KEY") {
        return Err("That looks like a private key. Paste the contents of the .pub file instead.");
    }
    let key = PublicKey::from_openssh(line)
        .map_err(|_| "Key is invalid. Paste an OpenSSH public key such as the contents of ~/.ssh/id_ed25519.pub.")?;
    let comment = key.comment().as_str_lossy().to_string();
    let mut bare = key.clone();
    bare.set_comment("");
    let normalized = bare.to_openssh().map_err(|_| "Key could not be encoded.")?;
    Ok(ParsedKey {
        normalized: normalized.trim().to_string(),
        fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
        key_type: key.algorithm().as_str().to_string(),
        comment,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames() {
        assert!(username("alice").is_ok());
        assert!(username("a-b-c").is_ok());
        assert!(username("-alice").is_err());
        assert!(username("al--ice").is_err());
        assert!(username("settings").is_err());
        assert!(username("ali ce").is_err());
    }

    #[test]
    fn repo_names() {
        assert!(repo_name("openhub").is_ok());
        assert!(repo_name("my_repo.rs").is_ok());
        assert!(repo_name("..").is_err());
        assert!(repo_name("x.git").is_err());
        assert!(repo_name("a/b").is_err());
    }

    #[test]
    fn parses_ed25519_key() {
        let k = ssh_public_key(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMys5dTzxVmfRH761T646Bpe3nX9Hx86atVkOotoyxWQ me@host",
        )
        .unwrap();
        assert!(k.fingerprint.starts_with("SHA256:"));
        assert_eq!(k.key_type, "ssh-ed25519");
        assert_eq!(k.comment, "me@host");
        assert!(ssh_public_key("not a key").is_err());
    }
}
