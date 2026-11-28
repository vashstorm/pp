use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{self, Mode, OFlags},
    io::Errno,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
};

pub const MAINTENANCE: &str = "When code changes make project descriptions, commands, paths, or workflows in AGENTS.md outdated or inconsistent, update AGENTS.md as part of the same task. Preserve valid project constraints; never rewrite rules merely to conceal a violation. Project instructions apply only within the specified directory and its descendants in the specified project root. Parent rules remain effective; deeper rules take precedence on conflicts within their scope. Rules from sibling directories or other projects do not apply outside their scope. All project rules remain subject to higher-priority instructions and the user's current request. Read target files before modifying them to discover applicable rules. If a tool reports rules were not included in the model request, reconsider the change using the updated rules and submit new tool calls; do not bypass the check using bash or external tools.";

pub const INIT_PROMPT: &str = "Create or update AGENTS.md in the startup working directory. First inspect the existing AGENTS.md, if present, and relevant project files using the available tools. Based only on observed evidence, document the project purpose, architecture, development commands, testing procedures, and necessary project constraints. If AGENTS.md already exists, preserve valid rules and user guidance while correcting outdated descriptions, commands, paths, and workflows. Keep the document concise and useful to future coding agents; do not invent facts or expose secrets. Do not change unrelated code. Use write or edit to save AGENTS.md, then briefly describe the changes actually made.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Document {
    pub path: PathBuf,
    pub root: PathBuf,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectedDocument {
    pub document: Document,
    pub sent: bool,
}

/// Observed fingerprint of a single rule file: device/inode/length/mtime (seconds + nanoseconds).
///
/// Same basis as the derived session index. mtime alone misses overwrites that keep both the
/// length and the same timestamp tick, so inode and length take part; the byte-exact edit/write
/// pre-gate still reads the whole file every time and skips this fingerprint (see [`State::check`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fingerprint {
    device: u64,
    inode: u64,
    len: u64,
    seconds: i64,
    nanoseconds: i64,
}

impl Fingerprint {
    /// stat only, never following symlinks; a missing file yields `None` and other errors propagate.
    fn read(path: &Path) -> Result<Option<Self>> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {} metadata", path.display()));
            }
        };
        // Same rule as `read_file`: symlinks, directories and other special files are not rule sources.
        ensure!(
            metadata.is_file(),
            "{} must be a regular file",
            path.display()
        );
        Ok(Some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            len: metadata.len(),
            seconds: metadata.mtime(),
            nanoseconds: metadata.mtime_nsec(),
        }))
    }
}

/// Candidate rule file on a directory chain and the project root it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RuleSource {
    path: PathBuf,
    root: PathBuf,
}

/// Body of the last successful read together with the fingerprint observed when it was read.
struct Cached {
    fingerprint: Fingerprint,
    content: String,
}

/// Each Agent keeps its own activated directories and the full rule snapshot of the last request.
pub struct State {
    cwd: PathBuf,
    directories: BTreeSet<PathBuf>,
    /// Current rule snapshot and its shared copy: reused whole while the sources are unchanged, without copying bodies.
    current: Arc<[Document]>,
    sent: Arc<[Document]>,
    roots: BTreeSet<PathBuf>,
    sent_roots: BTreeSet<PathBuf>,
    /// Candidate sources and observed fingerprints captured when `current` was built, used to detect disk drift.
    sources: Vec<RuleSource>,
    observed: Vec<Option<Fingerprint>>,
    /// Only successfully read bodies are stored: a read failure must not be cached as a missing rule.
    cache: BTreeMap<PathBuf, Cached>,
    blocked: Option<String>,
}

impl State {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_owned(),
            // Ancestor rules of the startup directory load before the first request; descendant directories still activate on access.
            directories: BTreeSet::from([cwd.to_owned()]),
            current: Arc::from(Vec::new()),
            sent: Arc::from(Vec::new()),
            roots: BTreeSet::new(),
            sent_roots: BTreeSet::new(),
            sources: Vec::new(),
            observed: Vec::new(),
            cache: BTreeMap::new(),
            blocked: None,
        }
    }

    fn root(&self, directory: &Path) -> Option<PathBuf> {
        directory
            .ancestors()
            .find(|parent| parent.join(".git").exists())
            .map(Path::to_owned)
            .or_else(|| directory.starts_with(&self.cwd).then(|| self.cwd.clone()))
    }

    /// Candidate rule files on one directory chain (project root first); empty when there is no implicit root.
    fn chain_sources(&self, directory: &Path) -> Vec<RuleSource> {
        let Some(root) = self.root(directory) else {
            return Vec::new();
        };
        let mut directories = Vec::new();
        for parent in directory.ancestors() {
            directories.push(parent);
            if parent == root {
                break;
            }
        }
        directories
            .into_iter()
            .rev()
            .map(|directory| RuleSource {
                path: directory.join("AGENTS.md"),
                root: root.clone(),
            })
            .collect()
    }

    /// Candidate rule files for the activated scope (deduped, sorted by depth then path) and the set of project roots.
    fn sources(&self) -> (Vec<RuleSource>, BTreeSet<PathBuf>) {
        let mut found = BTreeMap::new();
        let mut roots = BTreeSet::new();
        for directory in &self.directories {
            if let Some(root) = self.root(directory) {
                roots.insert(root);
            }
            for source in self.chain_sources(directory) {
                // When several activated chains cover the same file, the latter decides the owning root, matching the existing order.
                found.insert(source.path.clone(), source);
            }
        }
        let mut sources: Vec<RuleSource> = found.into_values().collect();
        sources.sort_by(|left, right| {
            left.path
                .components()
                .count()
                .cmp(&right.path.components().count())
                .then_with(|| left.path.cmp(&right.path))
        });
        (sources, roots)
    }

    /// stat candidate files only; `None` means it does not currently exist.
    fn observe(sources: &[RuleSource]) -> Result<Vec<Option<Fingerprint>>> {
        sources
            .iter()
            .map(|source| Fingerprint::read(&source.path))
            .collect()
    }

    /// Take the cached body on a fingerprint hit, otherwise read in full and record the current fingerprint.
    fn cached(&mut self, source: &RuleSource, fingerprint: Fingerprint) -> Result<Option<&str>> {
        if self
            .cache
            .get(&source.path)
            .is_none_or(|cached| cached.fingerprint != fingerprint)
        {
            let Some(content) = read_file(&source.path)? else {
                return Ok(None);
            };
            self.cache.insert(
                source.path.clone(),
                Cached {
                    fingerprint,
                    content,
                },
            );
        }
        Ok(self
            .cache
            .get(&source.path)
            .map(|cached| cached.content.as_str()))
    }

    /// Assemble the snapshot from the observations; files whose fingerprint did not change are not read again.
    fn load(
        &mut self,
        sources: &[RuleSource],
        observed: &[Option<Fingerprint>],
    ) -> Result<Vec<Document>> {
        let mut documents = Vec::new();
        for (source, fingerprint) in sources.iter().zip(observed) {
            let Some(fingerprint) = *fingerprint else {
                continue;
            };
            let Some(content) = self.cached(source, fingerprint)? else {
                continue;
            };
            // Missing and blank bodies are neither appended nor allowed to clear parent rules.
            if content.trim().is_empty() {
                continue;
            }
            documents.push(Document {
                path: source.path.clone(),
                root: source.root.clone(),
                content: content.to_owned(),
            });
        }
        Ok(documents)
    }

    /// Directory chain for the edit/write pre-gate: always read in full so the verdict matches the bytes on disk.
    fn chain_fresh(&self, directory: &Path) -> Result<Vec<Document>> {
        let mut documents = Vec::new();
        for source in self.chain_sources(directory) {
            if let Some(content) = read_file(&source.path)?
                && !content.trim().is_empty()
            {
                documents.push(Document {
                    path: source.path,
                    root: source.root,
                    content,
                });
            }
        }
        Ok(documents)
    }

    /// Read the current bodies of a directory chain (for activation validation); unchanged fingerprints reuse the cache.
    fn chain(&mut self, directory: &Path) -> Result<Vec<Document>> {
        let sources = self.chain_sources(directory);
        let observed = Self::observe(&sources)?;
        self.load(&sources, &observed)
    }

    pub fn activate(&mut self, directory: PathBuf) -> Result<()> {
        // Validate the whole chain first so a failed read never leaves half the rules marked as loaded.
        self.chain(&directory)?;
        self.directories.insert(directory);
        Ok(())
    }

    pub fn remember(&mut self, directory: PathBuf) {
        if directory.is_absolute() {
            self.directories.insert(directory);
        }
    }

    /// Current rule snapshot for each regular/summary request.
    ///
    /// When the activated scope, candidate sources and fingerprints are unchanged, share the previous snapshot
    /// directly (no disk reads, no body copies); only changed fingerprints are re-read, so disk changes show up next request.
    pub fn refresh_shared(&mut self) -> Result<Arc<[Document]>> {
        let (sources, roots) = self.sources();
        let observed = Self::observe(&sources)?;
        if roots == self.roots && sources == self.sources && observed == self.observed {
            return Ok(Arc::clone(&self.current));
        }
        let documents = self.load(&sources, &observed)?;
        self.current = Arc::from(documents);
        self.sources = sources;
        self.observed = observed;
        self.roots = roots;
        Ok(Arc::clone(&self.current))
    }

    /// For callers that need an owned list (standalone preview, existing tests); the request path uses [`State::refresh_shared`].
    pub fn refresh(&mut self) -> Result<Vec<Document>> {
        Ok(self.refresh_shared()?.to_vec())
    }

    pub fn inspect(&mut self) -> Result<(Vec<InspectedDocument>, String)> {
        let (sources, roots) = self.sources();
        let observed = Self::observe(&sources)?;
        let documents = self.load(&sources, &observed)?;
        let scope = scope_summary_for(&documents, &roots);
        let inspected = documents
            .into_iter()
            .map(|document| {
                let sent = self.sent.iter().any(|existing| existing == &document);
                InspectedDocument { document, sent }
            })
            .collect();
        Ok((inspected, scope))
    }

    pub fn sent(&mut self) {
        // Shares the same snapshot as `current`: each request no longer deep-copies every rule body.
        self.sent = Arc::clone(&self.current);
        self.sent_roots.clone_from(&self.roots);
        self.blocked = None;
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        self.current
            .iter()
            .map(|document| document.path.clone())
            .collect()
    }

    pub fn scope_summary(&self) -> String {
        scope_summary_for(&self.current, &self.roots)
    }

    pub fn blocked(&self) -> Option<&str> {
        self.blocked.as_deref()
    }

    pub fn block(&mut self, message: String) {
        self.blocked = Some(message);
    }

    pub fn check(&mut self, directory: PathBuf) -> Result<()> {
        self.directories.insert(directory.clone());
        // The gate judges by bytes on disk: an overwrite that keeps length and mtime cannot bypass the rule check.
        let documents = self.chain_fresh(&directory)?;
        let sent_root = directory
            .ancestors()
            .find(|parent| self.sent_roots.contains(*parent));
        let sent = self.sent.iter().filter(|document| {
            Some(document.root.as_path()) == sent_root
                && document
                    .path
                    .parent()
                    .is_some_and(|parent| directory.starts_with(parent))
        });
        if documents.iter().ne(sent) {
            self.blocked = Some("Not executed: applicable AGENTS.md rules changed or were not included in the model request. The next request will include the current rules; reconsider and submit new tool calls.".into());
        }
        if let Some(message) = &self.blocked {
            anyhow::bail!("{message}");
        }
        Ok(())
    }
}

fn scope_summary_for(documents: &[Document], roots: &BTreeSet<PathBuf>) -> String {
    let mut summary = String::new();
    for root in roots {
        if documents
            .iter()
            .any(|document| root != &document.root && root.starts_with(&document.root))
        {
            summary.push_str(&format!(
                "\n[Project root boundary: {}]\nAncestor project rules do not apply inside this project root.\n",
                root.display(),
            ));
        }
    }
    summary
}

pub fn render(documents: &[Document]) -> String {
    let mut text = String::new();
    for document in documents {
        text.push_str(&format!(
            "\n\n[Project instructions: {}]\nApplies to directory: {}\nProject root: {}\n\n{}\n[End project instructions]\n",
            document.path.display(), document.path.parent().unwrap_or(&document.root).display(),
            document.root.display(), document.content,
        ));
    }
    text
}

/// Read a plain UTF-8 instruction file; missing and empty bodies stay distinct so an explicit empty override is kept.
pub fn read_file(path: &Path) -> Result<Option<String>> {
    // Never follow symlinks; a non-blocking open keeps special files such as FIFOs from blocking the worker.
    let fd = match fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("loading {} (symlinks are forbidden)", path.display()));
        }
    };
    let mut file = File::from(fd);
    ensure!(
        file.metadata()
            .with_context(|| format!("reading {} metadata", path.display()))?
            .is_file(),
        "{} must be a regular file",
        path.display()
    );
    let mut content = String::new();
    file.read_to_string(&mut content)
        .with_context(|| format!("reading {} as UTF-8", path.display()))?;
    Ok(Some(content))
}

pub fn compose(base: &str, project: &str, skills: &str) -> String {
    let mut system = format!("{base}\n\n{MAINTENANCE}");
    if !project.is_empty() {
        system.push_str("\n\n");
    }
    system.push_str(project);
    system.push_str(skills);
    system
}

#[cfg(test)]
mod tests;
