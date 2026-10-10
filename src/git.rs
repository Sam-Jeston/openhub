//! Read and write access to bare repositories via libgit2.
//!
//! Everything here is synchronous; web handlers call into it from
//! `spawn_blocking`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use git2::{
    BranchType, Commit, DiffFindOptions, DiffOptions, ObjectType, Oid, Patch, Repository,
    RepositoryInitOptions, Signature, Sort, Tree,
};

use crate::highlight;

// ---------------------------------------------------------------------------
// Repository lifecycle
// ---------------------------------------------------------------------------

pub fn init_bare(path: &Path, default_branch: &str) -> Result<Repository> {
    let mut opts = RepositoryInitOptions::new();
    opts.bare(true).initial_head(default_branch);
    let repo = Repository::init_opts(path, &opts)
        .with_context(|| format!("initializing repository at {}", path.display()))?;
    configure(&repo)?;
    Ok(repo)
}

/// Server-side configuration applied to every hosted repository.
fn configure(repo: &Repository) -> Result<()> {
    let mut cfg = repo.config()?;
    // refs/pull/* is managed by Openhub; clients may fetch but never push to it.
    cfg.set_str("receive.hideRefs", "refs/pull")?;
    Ok(())
}

pub fn open(path: &Path) -> Result<Repository> {
    Repository::open_bare(path).with_context(|| format!("opening repository {}", path.display()))
}

/// Creates the first commit of an empty repository with the given files.
pub fn initial_commit(
    repo: &Repository,
    branch: &str,
    files: &[(&str, &[u8])],
    sig: &Signature,
) -> Result<Oid> {
    let mut builder = repo.treebuilder(None)?;
    for (name, content) in files {
        let blob = repo.blob(content)?;
        builder.insert(name, blob, 0o100644)?;
    }
    let tree = repo.find_tree(builder.write()?)?;
    let oid = repo.commit(
        Some(&format!("refs/heads/{branch}")),
        sig,
        sig,
        "Initial commit",
        &tree,
        &[],
    )?;
    Ok(oid)
}

pub fn is_empty(repo: &Repository) -> bool {
    repo.branches(Some(BranchType::Local))
        .map(|mut b| b.next().is_none())
        .unwrap_or(true)
}

pub fn branch_exists(repo: &Repository, name: &str) -> bool {
    repo.find_branch(name, BranchType::Local).is_ok()
}

pub fn branch_tip(repo: &Repository, name: &str) -> Option<Oid> {
    repo.find_branch(name, BranchType::Local)
        .ok()
        .and_then(|b| b.get().target())
}

pub fn set_head(repo: &Repository, branch: &str) -> Result<()> {
    repo.set_head(&format!("refs/heads/{branch}"))?;
    Ok(())
}

/// Makes sure HEAD points at an existing branch, preferring `preferred`.
/// Returns the branch HEAD points at afterwards, or `None` if the repository
/// has no branches yet.
pub fn ensure_head(repo: &Repository, preferred: &str) -> Result<Option<String>> {
    let names = branch_names(repo)?;
    if names.is_empty() {
        return Ok(None);
    }
    let pick = [preferred, "main", "master"]
        .into_iter()
        .find(|c| names.iter().any(|n| n == c))
        .map(str::to_string)
        .unwrap_or_else(|| names[0].clone());
    let current = repo
        .find_reference("HEAD")
        .ok()
        .and_then(|r| r.symbolic_target().ok().flatten().map(str::to_string));
    if current.as_deref() != Some(&format!("refs/heads/{pick}")) {
        set_head(repo, &pick)?;
    }
    Ok(Some(pick))
}

pub fn delete_branch(repo: &Repository, name: &str) -> Result<()> {
    let mut branch = repo.find_branch(name, BranchType::Local)?;
    branch.delete()?;
    Ok(())
}

/// Points `refs/pull/<number>/head` at the given commit so the pull request's
/// history survives even if its branch is deleted.
pub fn set_pull_ref(repo: &Repository, number: i64, oid: Oid) -> Result<()> {
    repo.reference(
        &format!("refs/pull/{number}/head"),
        oid,
        true,
        "openhub: update pull request head",
    )?;
    Ok(())
}

pub fn pull_ref(repo: &Repository, number: i64) -> Option<Oid> {
    repo.refname_to_id(&format!("refs/pull/{number}/head")).ok()
}

// ---------------------------------------------------------------------------
// Commits and refs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub id: String,
    pub short_id: String,
    pub summary: String,
    pub body: String,
    pub author_name: String,
    pub author_email: String,
    pub author_time: i64,
    pub committer_name: String,
    pub committer_time: i64,
    pub parents: Vec<String>,
}

impl CommitInfo {
    pub fn from_commit(c: &Commit) -> Self {
        let message = String::from_utf8_lossy(c.message_bytes()).to_string();
        let mut parts = message.splitn(2, '\n');
        let summary = parts.next().unwrap_or("").trim().to_string();
        let body = parts.next().unwrap_or("").trim().to_string();
        let author = c.author();
        let committer = c.committer();
        let id = c.id().to_string();
        CommitInfo {
            short_id: id[..7].to_string(),
            id,
            summary,
            body,
            author_name: String::from_utf8_lossy(author.name_bytes()).to_string(),
            author_email: String::from_utf8_lossy(author.email_bytes()).to_string(),
            author_time: author.when().seconds(),
            committer_name: String::from_utf8_lossy(committer.name_bytes()).to_string(),
            committer_time: committer.when().seconds(),
            parents: c.parent_ids().map(|p| p.to_string()).collect(),
        }
    }

    pub fn short_parents(&self) -> Vec<(String, String)> {
        self.parents
            .iter()
            .map(|p| (p.clone(), p[..7].to_string()))
            .collect()
    }
}

pub fn commit_info(repo: &Repository, oid: Oid) -> Result<CommitInfo> {
    Ok(CommitInfo::from_commit(&repo.find_commit(oid)?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Branch,
    Tag,
    Commit,
}

/// A ref (branch, tag or commit) plus a path inside its tree, parsed from a
/// URL tail such as `feature/login/src/main.rs`.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub name: String,
    pub kind: RefKind,
    pub commit: Oid,
    pub path: String,
}

pub fn resolve(repo: &Repository, rest: &str) -> Option<Resolved> {
    let rest = rest.trim_matches('/');
    if rest.is_empty() {
        return None;
    }
    let segs: Vec<&str> = rest.split('/').collect();
    for i in 1..=segs.len() {
        let candidate = segs[..i].join("/");
        let path = segs[i..].join("/");
        for (prefix, kind) in [
            ("refs/heads/", RefKind::Branch),
            ("refs/tags/", RefKind::Tag),
        ] {
            if let Ok(r) = repo.find_reference(&format!("{prefix}{candidate}")) {
                if let Ok(c) = r.peel_to_commit() {
                    return Some(Resolved {
                        name: candidate,
                        kind,
                        commit: c.id(),
                        path,
                    });
                }
            }
        }
    }
    let first = segs[0];
    if (4..=40).contains(&first.len()) && first.chars().all(|c| c.is_ascii_hexdigit()) {
        if let Ok(c) = repo.revparse_single(first).and_then(|o| o.peel_to_commit()) {
            return Some(Resolved {
                name: first.to_string(),
                kind: RefKind::Commit,
                commit: c.id(),
                path: segs[1..].join("/"),
            });
        }
    }
    None
}

pub fn branch_names(repo: &Repository) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for b in repo.branches(Some(BranchType::Local))? {
        let (b, _) = b?;
        if let Some(name) = b.name()? {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

pub fn tag_names(repo: &Repository) -> Result<Vec<String>> {
    let mut names: Vec<String> = repo
        .tag_names(None)?
        .iter()
        .filter_map(|t| t.ok().flatten().map(String::from))
        .collect();
    names.sort_by(|a, b| natural_cmp(b, a));
    Ok(names)
}

/// Orders version-like names so that `v1.10` sorts after `v1.9`.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn chunks(s: &str) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for c in s.chars() {
            let digit = c.is_ascii_digit();
            match out.last_mut() {
                Some((d, buf)) if *d == digit => buf.push(c),
                _ => out.push((digit, c.to_string())),
            }
        }
        out
    }
    let (ca, cb) = (chunks(a), chunks(b));
    for (x, y) in ca.iter().zip(cb.iter()) {
        let ord = if x.0 && y.0 {
            let (nx, ny) = (x.1.trim_start_matches('0'), y.1.trim_start_matches('0'));
            nx.len().cmp(&ny.len()).then_with(|| nx.cmp(ny))
        } else {
            x.1.cmp(&y.1)
        };
        if ord.is_ne() {
            return ord;
        }
    }
    ca.len().cmp(&cb.len())
}

#[derive(Debug, Clone)]
pub struct BranchInfo {
    pub name: String,
    pub commit: CommitInfo,
    pub ahead: usize,
    pub behind: usize,
}

/// Branches sorted by most recent commit, with ahead/behind counts relative
/// to `default_branch`.
pub fn branches(repo: &Repository, default_branch: &str) -> Result<Vec<BranchInfo>> {
    let default_tip = branch_tip(repo, default_branch);
    let mut out = Vec::new();
    for b in repo.branches(Some(BranchType::Local))? {
        let (b, _) = b?;
        let Some(name) = b.name()?.map(str::to_string) else {
            continue;
        };
        let Ok(commit) = b.get().peel_to_commit() else {
            continue;
        };
        let (ahead, behind) = match default_tip {
            Some(base) if name != default_branch => {
                repo.graph_ahead_behind(commit.id(), base).unwrap_or((0, 0))
            }
            _ => (0, 0),
        };
        out.push(BranchInfo {
            name,
            commit: CommitInfo::from_commit(&commit),
            ahead,
            behind,
        });
    }
    out.sort_by(|a, b| b.commit.committer_time.cmp(&a.commit.committer_time));
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct TagInfo {
    pub name: String,
    pub commit: CommitInfo,
    pub message: Option<String>,
}

pub fn tags(repo: &Repository) -> Result<Vec<TagInfo>> {
    let mut out = Vec::new();
    for name in tag_names(repo)? {
        let Ok(r) = repo.find_reference(&format!("refs/tags/{name}")) else {
            continue;
        };
        let message = r
            .peel(ObjectType::Tag)
            .ok()
            .and_then(|o| o.into_tag().ok())
            .and_then(|t| t.message().ok().flatten().map(|m| m.trim().to_string()))
            .filter(|m| !m.is_empty());
        let Ok(commit) = r.peel_to_commit() else {
            continue;
        };
        out.push(TagInfo {
            name,
            commit: CommitInfo::from_commit(&commit),
            message,
        });
    }
    Ok(out)
}

fn entry_id_at(commit: &Commit, path: &str) -> Option<Oid> {
    let tree = commit.tree().ok()?;
    if path.is_empty() {
        return Some(tree.id());
    }
    tree.get_path(Path::new(path)).ok().map(|e| e.id())
}

/// True if `commit` changed `path` relative to all of its parents.
fn touches_path(commit: &Commit, path: &str) -> bool {
    let mine = entry_id_at(commit, path);
    if commit.parent_count() == 0 {
        return mine.is_some();
    }
    commit.parents().all(|p| entry_id_at(&p, path) != mine)
}

/// A page of history starting at `start`, optionally limited to commits
/// touching `path`. Returns the commits and whether more exist.
pub fn log(
    repo: &Repository,
    start: Oid,
    path: Option<&str>,
    skip: usize,
    limit: usize,
) -> Result<(Vec<CommitInfo>, bool)> {
    let mut walk = repo.revwalk()?;
    walk.set_sorting(Sort::TIME)?;
    walk.push(start)?;
    let mut out = Vec::new();
    let mut seen = 0usize;
    for oid in walk {
        let commit = repo.find_commit(oid?)?;
        if let Some(p) = path.filter(|p| !p.is_empty()) {
            if !touches_path(&commit, p) {
                continue;
            }
        }
        if seen >= skip + limit {
            return Ok((out, true));
        }
        if seen >= skip {
            out.push(CommitInfo::from_commit(&commit));
        }
        seen += 1;
    }
    Ok((out, false))
}

pub fn commit_count(repo: &Repository, start: Oid) -> Result<usize> {
    let mut walk = repo.revwalk()?;
    walk.push(start)?;
    Ok(walk.count())
}

/// Commits reachable from `head` but not from `base`, oldest first.
pub fn commits_between(repo: &Repository, base: Oid, head: Oid) -> Result<Vec<CommitInfo>> {
    let mut walk = repo.revwalk()?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
    walk.push(head)?;
    walk.hide(base)?;
    let mut out = Vec::new();
    for oid in walk {
        out.push(CommitInfo::from_commit(&repo.find_commit(oid?)?));
    }
    Ok(out)
}

pub fn merge_base(repo: &Repository, a: Oid, b: Oid) -> Option<Oid> {
    repo.merge_base(a, b).ok()
}

pub fn is_ancestor(repo: &Repository, ancestor: Oid, of: Oid) -> bool {
    ancestor == of || repo.graph_descendant_of(of, ancestor).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Trees and blobs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    Symlink,
    Submodule,
}

#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub name: String,
    pub path: String,
    pub kind: EntryKind,
    pub last_commit: Option<CommitInfo>,
}

impl TreeEntry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }
    pub fn is_submodule(&self) -> bool {
        self.kind == EntryKind::Submodule
    }
}

pub fn path_type(repo: &Repository, commit: Oid, path: &str) -> Result<Option<ObjectType>> {
    let tree = repo.find_commit(commit)?.tree()?;
    if path.is_empty() {
        return Ok(Some(ObjectType::Tree));
    }
    Ok(tree.get_path(Path::new(path)).ok().and_then(|e| e.kind()))
}

fn subtree<'r>(repo: &'r Repository, commit: &Commit<'r>, path: &str) -> Result<Option<Tree<'r>>> {
    let tree = commit.tree()?;
    if path.is_empty() {
        return Ok(Some(tree));
    }
    let Ok(entry) = tree.get_path(Path::new(path)) else {
        return Ok(None);
    };
    if entry.kind() != Some(ObjectType::Tree) {
        return Ok(None);
    }
    Ok(Some(repo.find_tree(entry.id())?))
}

/// Lists a directory, directories first, each with the last commit that touched it.
pub fn list_tree(repo: &Repository, commit_id: Oid, path: &str) -> Result<Option<Vec<TreeEntry>>> {
    let commit = repo.find_commit(commit_id)?;
    let Some(tree) = subtree(repo, &commit, path)? else {
        return Ok(None);
    };
    let mut entries: Vec<TreeEntry> = tree
        .iter()
        .map(|e| {
            let name = String::from_utf8_lossy(e.name_bytes()).to_string();
            let kind = match (e.kind(), e.filemode()) {
                (Some(ObjectType::Tree), _) => EntryKind::Dir,
                (Some(ObjectType::Commit), _) => EntryKind::Submodule,
                (_, 0o120000) => EntryKind::Symlink,
                _ => EntryKind::File,
            };
            TreeEntry {
                path: if path.is_empty() {
                    name.clone()
                } else {
                    format!("{path}/{name}")
                },
                name,
                kind,
                last_commit: None,
            }
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir()
            .cmp(&a.is_dir())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    let mut last = last_commits(repo, commit_id, path, &entries)?;
    for e in &mut entries {
        e.last_commit = last.remove(&e.name);
    }
    Ok(Some(entries))
}

/// Finds the most recent commit that changed each entry of a directory.
/// Bounded so very long histories cannot stall a page load.
fn last_commits(
    repo: &Repository,
    start: Oid,
    dir: &str,
    entries: &[TreeEntry],
) -> Result<HashMap<String, CommitInfo>> {
    const MAX_WALK: usize = 5000;
    let mut remaining: HashSet<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    let mut found = HashMap::new();
    let mut walk = repo.revwalk()?;
    walk.set_sorting(Sort::TIME)?;
    walk.push(start)?;

    let entry_ids = |c: &Commit| -> HashMap<String, Oid> {
        subtree(repo, c, dir)
            .ok()
            .flatten()
            .map(|t| {
                t.iter()
                    .map(|e| (String::from_utf8_lossy(e.name_bytes()).to_string(), e.id()))
                    .collect()
            })
            .unwrap_or_default()
    };

    for oid in walk.take(MAX_WALK) {
        if remaining.is_empty() {
            break;
        }
        let commit = repo.find_commit(oid?)?;
        let mine = entry_ids(&commit);
        let parents: Vec<HashMap<String, Oid>> = commit.parents().map(|p| entry_ids(&p)).collect();
        let changed: Vec<&str> = remaining
            .iter()
            .copied()
            .filter(|name| {
                let id = mine.get(*name);
                id.is_some() && parents.iter().all(|p| p.get(*name) != id)
            })
            .collect();
        if changed.is_empty() {
            continue;
        }
        let info = CommitInfo::from_commit(&commit);
        for name in changed {
            remaining.remove(name);
            found.insert(name.to_string(), info.clone());
        }
    }
    Ok(found)
}

pub struct Blob {
    pub data: Vec<u8>,
    pub size: usize,
    pub is_binary: bool,
}

pub fn read_blob(repo: &Repository, commit: Oid, path: &str) -> Result<Option<Blob>> {
    let tree = repo.find_commit(commit)?.tree()?;
    let Ok(entry) = tree.get_path(Path::new(path)) else {
        return Ok(None);
    };
    if entry.kind() != Some(ObjectType::Blob) {
        return Ok(None);
    }
    let blob = repo.find_blob(entry.id())?;
    Ok(Some(Blob {
        size: blob.size(),
        is_binary: blob.is_binary(),
        data: blob.content().to_vec(),
    }))
}

/// Finds a README in the given directory, returning its file name and contents.
pub fn find_readme(repo: &Repository, commit: Oid, dir: &str) -> Result<Option<(String, String)>> {
    let c = repo.find_commit(commit)?;
    let Some(tree) = subtree(repo, &c, dir)? else {
        return Ok(None);
    };
    let candidates = [
        "readme.md",
        "readme.markdown",
        "readme",
        "readme.txt",
        "readme.rst",
    ];
    let found = candidates.iter().find_map(|cand| {
        tree.iter().find(|e| {
            e.kind() == Some(ObjectType::Blob)
                && e.name().is_ok_and(|n| n.eq_ignore_ascii_case(cand))
        })
    });
    let Some(entry) = found else { return Ok(None) };
    let blob = repo.find_blob(entry.id())?;
    if blob.is_binary() || blob.size() > 1024 * 1024 {
        return Ok(None);
    }
    Ok(Some((
        entry.name().unwrap_or("README").to_string(),
        String::from_utf8_lossy(blob.content()).to_string(),
    )))
}

// ---------------------------------------------------------------------------
// Diffs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Add,
    Del,
    Context,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub html: String,
}

impl DiffLine {
    pub fn class(&self) -> &'static str {
        match self.kind {
            LineKind::Add => "add",
            LineKind::Del => "del",
            LineKind::Context => "ctx",
        }
    }
    pub fn marker(&self) -> &'static str {
        match self.kind {
            LineKind::Add => "+",
            LineKind::Del => "-",
            LineKind::Context => " ",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub index: usize,
    pub old_path: String,
    pub new_path: String,
    pub status: &'static str,
    pub additions: usize,
    pub deletions: usize,
    pub binary: bool,
    pub too_large: bool,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    pub fn display_path(&self) -> String {
        if self.status == "renamed" && self.old_path != self.new_path {
            format!("{} → {}", self.old_path, self.new_path)
        } else {
            self.new_path.clone()
        }
    }
    /// Five-square summary bar like `+++--`.
    pub fn stat_blocks(&self) -> Vec<&'static str> {
        let total = self.additions + self.deletions;
        if total == 0 {
            return vec!["neutral"; 5];
        }
        let share = |n: usize| {
            let blocks = ((n as f64 / total as f64) * 5.0).round() as usize;
            if n > 0 { blocks.max(1) } else { 0 }
        };
        let (mut adds, mut dels) = (share(self.additions), share(self.deletions));
        while adds + dels > 5 {
            if adds > dels { adds -= 1 } else { dels -= 1 }
        }
        let mut v = vec!["add"; adds];
        v.extend(std::iter::repeat_n("del", dels));
        v.resize(5, "neutral");
        v
    }
}

#[derive(Debug, Clone, Default)]
pub struct DiffSummary {
    pub files: Vec<FileDiff>,
    pub additions: usize,
    pub deletions: usize,
}

const MAX_FILE_LINES: usize = 3000;
const MAX_TOTAL_LINES: usize = 20000;

pub fn diff_trees(
    repo: &Repository,
    old: Option<&Tree>,
    new: Option<&Tree>,
) -> Result<DiffSummary> {
    let mut opts = DiffOptions::new();
    opts.context_lines(3).ignore_submodules(true);
    let mut diff = repo.diff_tree_to_tree(old, new, Some(&mut opts))?;
    let mut find = DiffFindOptions::new();
    find.renames(true);
    diff.find_similar(Some(&mut find))?;

    let mut summary = DiffSummary::default();
    let mut rendered_lines = 0usize;
    for idx in 0..diff.deltas().len() {
        let Some(patch) = Patch::from_diff(&diff, idx)? else {
            continue;
        };
        let delta = patch.delta();
        let status = match delta.status() {
            git2::Delta::Added => "added",
            git2::Delta::Deleted => "deleted",
            git2::Delta::Renamed => "renamed",
            git2::Delta::Copied => "copied",
            _ => "modified",
        };
        let path_of = |f: git2::DiffFile| {
            f.path()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        };
        let old_path = path_of(delta.old_file());
        let new_path = path_of(delta.new_file());
        let binary = delta.flags().is_binary();
        let (_, additions, deletions) = patch.line_stats()?;
        let too_large = additions + deletions > MAX_FILE_LINES
            || rendered_lines + additions + deletions > MAX_TOTAL_LINES;

        let mut hunks = Vec::new();
        if !binary && !too_large {
            let lang_path = if new_path.is_empty() {
                &old_path
            } else {
                &new_path
            };
            for h in 0..patch.num_hunks() {
                let (hunk, n) = patch.hunk(h)?;
                let mut raw = Vec::with_capacity(n);
                for l in 0..n {
                    let line = patch.line_in_hunk(h, l)?;
                    let kind = match line.origin() {
                        '+' => LineKind::Add,
                        '-' => LineKind::Del,
                        ' ' => LineKind::Context,
                        _ => continue, // "\ No newline at end of file" markers
                    };
                    let text = String::from_utf8_lossy(line.content());
                    let text = text.trim_end_matches(['\n', '\r']).to_string();
                    raw.push((kind, line.old_lineno(), line.new_lineno(), text));
                }
                let texts: Vec<&str> = raw.iter().map(|r| r.3.as_str()).collect();
                let htmls = highlight::highlight_lines(lang_path, &texts);
                let lines = raw
                    .into_iter()
                    .zip(htmls)
                    .map(|((kind, old_no, new_no, _), html)| DiffLine {
                        kind,
                        old_no,
                        new_no,
                        html,
                    })
                    .collect();
                hunks.push(Hunk {
                    header: String::from_utf8_lossy(hunk.header())
                        .trim_end()
                        .to_string(),
                    lines,
                });
            }
            rendered_lines += additions + deletions;
        }
        summary.additions += additions;
        summary.deletions += deletions;
        summary.files.push(FileDiff {
            index: summary.files.len(),
            old_path,
            new_path,
            status,
            additions,
            deletions,
            binary,
            too_large,
            hunks,
        });
    }
    Ok(summary)
}

/// Diff introduced by a commit, against its first parent.
pub fn commit_diff(repo: &Repository, oid: Oid) -> Result<DiffSummary> {
    let commit = repo.find_commit(oid)?;
    let parent_tree = match commit.parent(0) {
        Ok(p) => Some(p.tree()?),
        Err(_) => None,
    };
    diff_trees(repo, parent_tree.as_ref(), Some(&commit.tree()?))
}

/// Files changed, additions and deletions between two commits, without
/// rendering the patch.
pub fn diff_stats(repo: &Repository, base: Oid, head: Oid) -> Result<(usize, usize, usize)> {
    let base_tree = repo.find_commit(base)?.tree()?;
    let head_tree = repo.find_commit(head)?.tree()?;
    let mut diff = repo.diff_tree_to_tree(Some(&base_tree), Some(&head_tree), None)?;
    diff.find_similar(Some(DiffFindOptions::new().renames(true)))?;
    let stats = diff.stats()?;
    Ok((stats.files_changed(), stats.insertions(), stats.deletions()))
}

/// Diff between two commits (`base..head` as trees).
pub fn range_diff(repo: &Repository, base: Oid, head: Oid) -> Result<DiffSummary> {
    let base_tree = repo.find_commit(base)?.tree()?;
    let head_tree = repo.find_commit(head)?.tree()?;
    diff_trees(repo, Some(&base_tree), Some(&head_tree))
}

// ---------------------------------------------------------------------------
// Merging
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeability {
    /// Can be merged without conflicts.
    Clean,
    /// A three-way merge produces conflicts.
    Conflicts,
    /// The head has no commits that are not already in the base.
    UpToDate,
}

pub fn mergeability(repo: &Repository, base: Oid, head: Oid) -> Result<Mergeability> {
    if is_ancestor(repo, head, base) {
        return Ok(Mergeability::UpToDate);
    }
    let ours = repo.find_commit(base)?;
    let theirs = repo.find_commit(head)?;
    let index = repo.merge_commits(&ours, &theirs, None)?;
    Ok(if index.has_conflicts() {
        Mergeability::Conflicts
    } else {
        Mergeability::Clean
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "merge" => Some(Self::Merge),
            "squash" => Some(Self::Squash),
            "rebase" => Some(Self::Rebase),
            _ => None,
        }
    }
}

pub struct MergeRequest<'a> {
    pub base_branch: &'a str,
    pub base: Oid,
    pub head: Oid,
    pub method: MergeMethod,
    pub message: &'a str,
    pub committer: &'a Signature<'a>,
}

/// Merges `head` into the base branch and returns the new tip of the base.
/// Fails without changing anything if the base branch moved or the merge conflicts.
pub fn merge(repo: &Repository, req: &MergeRequest) -> Result<Oid> {
    let base_commit = repo.find_commit(req.base)?;
    let head_commit = repo.find_commit(req.head)?;

    let new_tip = match req.method {
        MergeMethod::Merge | MergeMethod::Squash => {
            let mut index = repo.merge_commits(&base_commit, &head_commit, None)?;
            if index.has_conflicts() {
                bail!("This branch has conflicts that must be resolved");
            }
            let tree = repo.find_tree(index.write_tree_to(repo)?)?;
            let parents: Vec<&Commit> = if req.method == MergeMethod::Merge {
                vec![&base_commit, &head_commit]
            } else {
                vec![&base_commit]
            };
            let author = if req.method == MergeMethod::Squash {
                head_commit.author().to_owned()
            } else {
                req.committer.to_owned()
            };
            repo.commit(None, &author, req.committer, req.message, &tree, &parents)?
        }
        MergeMethod::Rebase => {
            let mut walk = repo.revwalk()?;
            walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
            walk.push(req.head)?;
            walk.hide(req.base)?;
            let mut onto = base_commit;
            for oid in walk {
                let commit = repo.find_commit(oid?)?;
                if commit.parent_count() > 1 {
                    continue; // merge commits are dropped, as with `git rebase`
                }
                let mut index = repo.cherrypick_commit(&commit, &onto, 0, None)?;
                if index.has_conflicts() {
                    bail!(
                        "Commit {} cannot be rebased cleanly",
                        &commit.id().to_string()[..7]
                    );
                }
                let tree_id = index.write_tree_to(repo)?;
                if tree_id == onto.tree_id() {
                    continue; // became empty
                }
                let tree = repo.find_tree(tree_id)?;
                let message = String::from_utf8_lossy(commit.message_raw_bytes()).to_string();
                let new = repo.commit(
                    None,
                    &commit.author(),
                    req.committer,
                    &message,
                    &tree,
                    &[&onto],
                )?;
                onto = repo.find_commit(new)?;
            }
            onto.id()
        }
    };

    repo.reference_matching(
        &format!("refs/heads/{}", req.base_branch),
        new_tip,
        true,
        req.base,
        "openhub: merge pull request",
    )
    .context("The base branch changed while merging; please try again")?;
    Ok(new_tip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_ordering() {
        let mut v = vec!["v1.9", "v1.10", "v1.2", "v0.9"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["v0.9", "v1.2", "v1.9", "v1.10"]);
    }

    #[test]
    fn stat_blocks_sum_to_five() {
        for (a, d) in [
            (0, 0),
            (1, 0),
            (0, 1),
            (1, 1),
            (10, 1),
            (1, 10),
            (3, 2),
            (100, 99),
        ] {
            let f = FileDiff {
                index: 0,
                old_path: String::new(),
                new_path: String::new(),
                status: "modified",
                additions: a,
                deletions: d,
                binary: false,
                too_large: false,
                hunks: vec![],
            };
            let b = f.stat_blocks();
            assert_eq!(b.len(), 5, "{a}/{d}");
            assert_eq!(b.contains(&"add"), a > 0, "{a}/{d}");
            assert_eq!(b.contains(&"del"), d > 0, "{a}/{d}");
        }
    }
}
