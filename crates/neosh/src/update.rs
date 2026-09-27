//! Finding out there is a newer neosh, and becoming it.
//!
//! Two halves that look like one feature and are not. *Is there something newer* is a question
//! about a registry. *How does this file get replaced* is a question about the machine, and it is
//! the one that decides everything: a binary under a Homebrew prefix belongs to Homebrew, and
//! writing over it leaves `brew` describing a version that is not on disk — `brew upgrade` then
//! "succeeds" by putting back the one we replaced. Same for a binary inside `node_modules`, same
//! for one under `~/.cargo/bin`. Only a binary nobody else is managing is swapped by hand; a
//! managed one is updated by **running its manager** — `brew update && brew upgrade neosh`, `npm
//! install -g`, `cargo install --force` — with its output streamed onto the bus, and then checked:
//! a manager that returned success and left the binary unchanged (a tap that has not caught up)
//! is a failure with the command in it, never an update. It used to stop short of this and print
//! the command instead, on the theory that driving somebody's package manager is how a machine
//! ends up in a state its owner cannot explain — and what that meant from the keyboard was that
//! `/update` on the most common install of all did not update, told you to go and type something
//! elsewhere, and then wanted a second `/update` to restart into what you had typed. An update is
//! one verb, and the manager's own log is on screen while it runs, which is more than a shell
//! would have shown.
//!
//! The check is a plain HTTP GET against the releases API and is never done on the loop: it is
//! network, it can hang, and nothing on screen should wait for it. What it produces is kept, so
//! the answer to "is there an update" is instant and the *checking* is what happens in the
//! background.
//!
//! And there is a third question underneath both, which is the one people actually get stuck on:
//! *is the binary on disk the binary I am running*. It is not the same as either. Most installs
//! are managed, so most updates happen in another terminal — `brew upgrade neosh` finishes, says
//! it succeeded, and the workspace goes on executing the inode it started with. Nothing had
//! noticed and nothing said so, so the version you had just installed was not the version you were
//! using, for as long as that workspace lived. It is answered by a `stat` rather than by a
//! registry: the executable is stamped at startup and compared afterwards, which is true for a
//! `brew upgrade`, an `npm install -g`, a `cargo install --force`, a re-run of `install.sh` and
//! our own rename alike — and needs nothing from the network, so it is still true on a train.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use neosh_proto::{InstallMethod, UpdateOutcome, UpdateProgress, UpdateStatus};
use tokio::sync::RwLock;

/// Where an update says how it is getting on. The plugin draws a progress row from these.
pub const UPDATE_EVENT: &str = "neosh.update.progress";

/// A listener for [`UpdateProgress`], handed in by whoever owns the bus.
///
/// `Arc<dyn Fn>` rather than a generic because it is shared with the tasks that read a child's
/// two pipes, and both of them have to be able to say something.
pub type Progress = Arc<dyn Fn(UpdateProgress) + Send + Sync>;

/// How long one step of a package manager may take before it is killed.
///
/// Generous, because `cargo install` is a compile of the whole tree on whatever machine this is,
/// and a build that was going to finish in twenty-two minutes is not one to stop at twenty. What
/// this guards against is a step that is *never* going to finish — a prompt on a closed stdin, a
/// lock held by another `brew` — which without a ceiling is a progress row that spins forever and
/// an `applying` flag that never clears.
const STEP_TIMEOUT: Duration = Duration::from_secs(45 * 60);

/// Where releases are published. The API rather than the HTML, and `latest` rather than a list,
/// because a pre-release is not something to offer somebody who did not ask for one.
const LATEST: &str = "https://api.github.com/repos/neoswarm/neosh/releases/latest";
const DOWNLOAD: &str = "https://github.com/neoswarm/neosh/releases/download";

/// How long a check is good for.
///
/// Six hours rather than the day it was. The number is a floor on how long a machine can be wrong
/// about the world, and a workspace here runs for weeks — so a day meant a release could be out
/// for most of a working day with the panel saying nothing. Four unauthenticated requests a day
/// against a rate limit of sixty an hour is not a cost worth defending.
const FRESH_FOR: Duration = Duration::from_secs(60 * 60 * 6);

/// How long to give the binary on disk to say what version it is.
///
/// It is `--version` on a binary we shipped, so it prints and exits — but it is still an exec of a
/// file somebody else just wrote, and a workspace must not be able to sit on one. The answer is
/// decoration on a row that is already correct without it.
const VERSION_TIMEOUT: Duration = Duration::from_secs(3);

/// The Rust target triple this binary was built for, which is what names the release asset.
///
/// `cfg!` rather than a runtime probe: the answer is a property of the build, and a binary that
/// guessed would download one for the wrong architecture and fail at the first exec.
const fn target_triple() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("aarch64-unknown-linux-gnu")
    } else {
        None
    }
}

/// Work out who owns the file this process is running from.
///
/// Path-shaped, because that is the only evidence there is — nothing writes a receipt. Ordered
/// most specific first: a cargo install inside a checkout is still a checkout.
pub fn install_method(exe: &Path) -> InstallMethod {
    let s = exe.to_string_lossy();
    // A `target/debug` or `target/release` binary is somebody's build, and the newest neosh is the
    // one they are about to compile. Checked first: a worktree can live anywhere, including under
    // a directory that looks like any of the others.
    if s.contains("/target/debug/") || s.contains("/target/release/") {
        return InstallMethod::Development;
    }
    if s.contains("/node_modules/") {
        return InstallMethod::Npm;
    }
    // Homebrew is `/opt/homebrew` on Apple silicon and `/usr/local` on Intel, and both put the
    // real file under `Cellar` with a symlink into `bin` — `current_exe` resolves the symlink, so
    // Cellar is what is actually seen.
    if s.contains("/Cellar/") || s.starts_with("/opt/homebrew/") || s.contains("/linuxbrew/") {
        return InstallMethod::Homebrew;
    }
    if s.contains("/.cargo/bin/") || s.contains("/.rustup/") {
        return InstallMethod::Cargo;
    }
    InstallMethod::Standalone
}

/// Compare two dotted versions numerically.
///
/// Not a string compare: `0.10.0` sorts before `0.9.0` as text, which would tell everybody running
/// 0.10 to downgrade. Anything unparseable compares as absent rather than as zero, so a tag nobody
/// anticipated cannot manufacture an update.
/// What a development build says when asked to update: which build it is, what is released, and
/// the two ways forward — because neither is something neosh can do to a binary cargo owns.
pub fn development_sentence(current: &str, latest: Option<&str>) -> String {
    match latest {
        Some(v) if is_newer(v, current) => format!(
            "development build {current} — neosh {v} is released. Rebuild, or `neosh stop` and run \
             the installed release"
        ),
        Some(_) => format!(
            "development build {current}, the newest release — rebuild from your checkout to update it"
        ),
        None => format!("development build {current} — rebuild from your checkout to update it"),
    }
}

fn is_newer(latest: &str, current: &str) -> bool {
    fn parts(v: &str) -> Option<Vec<u64>> {
        // A `-rc.1` suffix makes the last field unparseable, and an unparseable version is one we
        // decline to reason about rather than one we round.
        v.trim_start_matches('v')
            .split('.')
            .map(|p| p.parse::<u64>().ok())
            .collect()
    }
    match (parts(latest), parts(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// Enough of a file to tell it from the one that replaced it.
///
/// The inode is what carries this: every way neosh is actually updated ends in a rename or a fresh
/// directory, so the number changes even when a package manager has preserved the modification
/// time. Length and mtime are belt and braces for a filesystem with no meaningful inodes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    len: u64,
    mtime: u64,
    #[cfg(unix)]
    ino: u64,
}

impl Stamp {
    fn of(path: &Path) -> Option<Self> {
        let m = std::fs::metadata(path).ok()?;
        if !m.is_file() {
            return None;
        }
        Some(Self {
            len: m.len(),
            mtime: m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
            #[cfg(unix)]
            ino: {
                use std::os::unix::fs::MetadataExt;
                m.ino()
            },
        })
    }
}

/// The last answer, and when it was given.
#[derive(Default)]
struct Cached {
    status: Option<UpdateStatus>,
    at: Option<SystemTime>,
    /// The version *we* put on disk, if we have. Not the whole of `restart_pending` — most
    /// installs are replaced by somebody else's package manager and never come through here — but
    /// still worth keeping, because it is the one case where we know without looking.
    staged: Option<String>,
    /// The version found on disk once a replacement was noticed, so `neosh --version` is exec'd
    /// once rather than on every redraw of a panel.
    found: Option<Option<String>>,
}

/// The update checker. One per workspace.
#[derive(Clone)]
pub struct Updater {
    inner: Arc<RwLock<Cached>>,
    exe: PathBuf,
    /// The unresolved path this process was started from — see [`crate::build::launch_path`].
    launch: Option<PathBuf>,
    current: String,
    /// What the executable looked like at startup. `None` when it could not be stat'd at all, in
    /// which case nothing here claims anything: an unknown stamp compares equal to everything, and
    /// the failure of guessing is a workspace that offers a restart on every redraw.
    stamp: Option<Stamp>,
    /// Whether an [`apply`](Self::apply) is in flight. One at a time: two `brew upgrade`s racing
    /// for one lock is a second one that fails on the lock, and a second download racing the
    /// first for one rename is worse.
    applying: Arc<AtomicBool>,
}

impl Updater {
    /// `exe`, `launch` and `version` all come from [`crate::build`] — read at startup, because a
    /// workspace outlives the file it was loaded from and `current_exe` goes stale.
    ///
    /// Both paths are passed in rather than one of them being fetched here, because they are two
    /// different facts — [`crate::build::exe_path`] has followed the symlinks and
    /// [`crate::build::launch_path`] deliberately has not — and a constructor that reached for a
    /// process-wide one behind the caller's back would be a type nothing could test without being
    /// the neosh binary.
    pub fn new(exe: PathBuf, launch: Option<PathBuf>, current: String) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Cached::default())),
            stamp: Stamp::of(&exe),
            launch,
            exe,
            current,
            applying: Arc::new(AtomicBool::new(false)),
        }
    }

    fn method(&self) -> InstallMethod {
        install_method(&self.exe)
    }

    /// The binary that `neosh` would run *now*, and how it differs from the one running.
    ///
    /// Three candidates in order, and the order is the whole of it:
    ///
    /// 1. **The launch path, resolved afresh.** A managed install is a symlink into a versioned
    ///    directory, and upgrading it repoints the link without touching the file we are executing.
    ///    Following the link at startup and remembering the answer is exactly how that swap becomes
    ///    invisible.
    /// 2. **The resolved path from startup.** An install that is a real file — `install.sh`,
    ///    `cargo install`, our own rename — is overwritten in place, and this is where.
    /// 3. **`neosh` on `PATH`.** The case the first two miss: Homebrew removes the old keg, so on
    ///    Linux, where `/proc/self/exe` is already fully resolved and there is no link left to
    ///    follow, both paths are simply gone. What would run then is whatever the shell finds,
    ///    which is also what the terminal is about to exec.
    ///
    /// `None` when nothing runnable can be found, and that is deliberate: a restart we cannot
    /// finish is worse than no offer at all — it takes a working workspace away and does not bring
    /// one back.
    fn on_disk(&self) -> Option<(PathBuf, Stamp)> {
        let candidates = self
            .launch
            .iter()
            .cloned()
            .chain(std::iter::once(self.exe.clone()))
            .chain(which_neosh());
        for p in candidates {
            if let Ok(real) = p.canonicalize()
                && let Some(s) = Stamp::of(&real)
            {
                return Some((real, s));
            }
        }
        None
    }

    /// Whether the binary on disk is not the one this process is running, and where it is.
    fn replaced(&self) -> Option<PathBuf> {
        // A checkout is excluded here rather than at the row that draws it, because in one the
        // answer is *yes, constantly*: `cargo run` unlinks `target/debug/neosh` and writes another
        // one every build. It is not that the fact is uninteresting there — it is the single most
        // interesting fact in a checkout — it is that it already has an answer, said by the
        // terminal on attach out of `BuildId`, and which names `neosh stop`. Two notices about one
        // thing is one notice nobody reads.
        if self.method() == InstallMethod::Development {
            return None;
        }
        let (path, now) = self.on_disk()?;
        match (self.stamp, now) {
            (Some(then), now) if then != now => Some(path),
            _ => None,
        }
    }

    /// Fill in the half of a status that is about this machine rather than about a registry.
    ///
    /// Applied on the way out of every answer, including a cached one — the network half is good
    /// for hours and this half can change between two redraws, so serving them from one timestamp
    /// is how a workspace goes on saying "up to date" for a day after being upgraded under it.
    async fn with_local(&self, mut status: UpdateStatus) -> UpdateStatus {
        let staged = self.inner.read().await.staged.clone();
        match self.replaced() {
            Some(path) => {
                status.restart_pending = true;
                status.restart_version = self.version_on_disk(&path).await.or(staged);
            }
            // Nothing on disk differs from what started. Our own download is not taken on trust
            // against that: if we wrote a file and the stamp still matches, whatever we wrote is
            // not there any more and a restart would come back to the very binary it left. The one
            // exception is having had no stamp to begin with, where there is nothing to compare and
            // our own word is the only evidence there is.
            None => {
                status.restart_pending = staged.is_some() && self.stamp.is_none();
                status.restart_version = status.restart_pending.then_some(staged).flatten();
            }
        }
        status
    }

    /// Ask the binary that is waiting what it calls itself, once.
    ///
    /// Asked of the file rather than assumed to be [`UpdateStatus::latest`]: somebody who ran
    /// `brew upgrade` got whatever their tap had, which is not always the newest release, and a row
    /// naming a version that is not the one about to start is a row that teaches you not to read
    /// it. A failure is `None` and the row says "restart" without a number, which is still the one
    /// thing there is to do.
    async fn version_on_disk(&self, path: &Path) -> Option<String> {
        if let Some(found) = &self.inner.read().await.found {
            return found.clone();
        }
        let out = tokio::time::timeout(
            VERSION_TIMEOUT,
            tokio::process::Command::new(path)
                .arg("--version")
                .stdin(std::process::Stdio::null())
                .output(),
        )
        .await;
        let found = match out {
            Ok(Ok(o)) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().last())
                .map(|v| v.trim_start_matches('v').to_string())
                .filter(|v| !v.is_empty()),
            _ => None,
        };
        self.inner.write().await.found = Some(found.clone());
        found
    }

    /// A status with no network in it, for the shape of the answer before any check has run.
    fn blank(&self) -> UpdateStatus {
        let method = self.method();
        UpdateStatus {
            current: self.current.clone(),
            latest: None,
            behind: false,
            method,
            self_updatable: method.self_updatable(),
            upgrade_command: method.upgrade_command(),
            error: None,
            restart_pending: false,
            restart_version: None,
        }
    }

    /// What is known now, checking the network only if the last answer has gone stale or `force`.
    ///
    /// `local` never touches the network at all, which is what makes "has something replaced my
    /// binary" answerable on a tick: it is a `stat`, and a panel asking it should not be the reason
    /// a workspace makes an HTTP request.
    pub async fn check(&self, force: bool, local: bool) -> UpdateStatus {
        {
            let c = self.inner.read().await;
            let fresh = c
                .at
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .is_some_and(|age| age < FRESH_FOR);
            if local || (!force && fresh) {
                let known = c.status.clone();
                drop(c);
                return self.with_local(known.unwrap_or_else(|| self.blank())).await;
            }
        }

        let mut status = self.blank();

        match fetch_latest().await {
            Ok(tag) => {
                // A checkout is never *told* to update — a notice would fire on every developer's
                // every start, about a binary that is theirs to rebuild — so `behind` stays false.
                // It is still told what the newest release *is*: "nothing published is newer than
                // what is about to be compiled" is only true of a checkout that has been pulled,
                // and a `target/debug/neosh` left serving a workspace for a week answered `/update`
                // with "0.4.11 is the newest" the morning 0.5.0 came out.
                status.behind = status.method != InstallMethod::Development
                    && is_newer(&tag, &self.current);
                status.latest = Some(tag.trim_start_matches('v').to_string());
            }
            // Kept rather than swallowed: a failed check that reads as "up to date" is a machine
            // that never updates and never says why.
            Err(e) => status.error = Some(e),
        }

        {
            let mut c = self.inner.write().await;
            c.status = Some(status.clone());
            c.at = Some(SystemTime::now());
        }
        self.with_local(status).await
    }

    /// Update, by whichever route this install takes, saying how it is going on `progress`.
    ///
    /// One at a time, workspace-wide. A second caller while one is running — `/update` pressed
    /// twice, or a script and a person — is told so rather than started, because two package
    /// managers on one prefix contend for a lock and two downloads contend for one rename.
    pub async fn apply(&self, progress: Progress) -> UpdateOutcome {
        if self.applying.swap(true, Ordering::SeqCst) {
            return UpdateOutcome::Failed { reason: "an update is already running".into() };
        }
        let outcome = self.apply_inner(&progress).await;
        self.applying.store(false, Ordering::SeqCst);
        // The last event says it is the last, whatever happened, so a row drawn from these has a
        // moment to stop at. A `Failed` outcome reaches the caller; the row is not the caller.
        progress(UpdateProgress {
            phase: match &outcome {
                UpdateOutcome::Applied { version, .. } => format!("neosh {version} installed"),
                UpdateOutcome::UpToDate { .. } => "up to date".into(),
                UpdateOutcome::Failed { .. } => "failed".into(),
            },
            line: None,
            done: true,
        });
        outcome
    }

    async fn apply_inner(&self, progress: &Progress) -> UpdateOutcome {
        progress(UpdateProgress { phase: "checking".into(), line: None, done: false });
        let status = self.check(true, false).await;
        let method = status.method;

        // Nothing here can replace a binary cargo builds, and "up to date" would be the one wrong
        // answer: it is what a checkout left behind main was told. Said with what to do instead.
        if method == InstallMethod::Development {
            return UpdateOutcome::Failed { reason: development_sentence(&self.current, status.latest.as_deref()) };
        }
        // Finish what is already half done before starting anything new. A binary already waiting
        // on disk — ours, or one `brew upgrade` put there — is not a workspace that needs another
        // download, and saying so here rather than only in the plugin means every caller of the API
        // gets it, including `neosh agent run update`.
        if status.restart_pending {
            return UpdateOutcome::Applied {
                version: status.restart_version.unwrap_or_else(|| self.current.clone()),
                restart_required: true,
            };
        }
        if let Some(err) = status.error {
            return UpdateOutcome::Failed { reason: format!("could not check for updates: {err}") };
        }
        let Some(latest) = status.latest else {
            return UpdateOutcome::Failed { reason: "no published release to compare against".into() };
        };
        if !status.behind {
            return UpdateOutcome::UpToDate { version: self.current.clone() };
        }

        let result = if method.self_updatable() {
            self.replace_binary(&latest, progress).await
        } else if !method.upgrade_steps().is_empty() {
            self.run_manager(method, &latest, progress).await
        } else {
            return UpdateOutcome::Failed {
                reason: "this install is not one neosh can replace".into(),
            };
        };

        match result {
            Ok(()) => {
                let mut c = self.inner.write().await;
                c.staged = Some(latest.clone());
                // The file changed under us, so anything read off the old one is not about the new
                // one. Cleared rather than left, or a second update in one workspace's life reports
                // the version of the first.
                c.found = None;
                drop(c);
                // Asked of the file rather than assumed to be `latest`: a package manager installs
                // what *its* catalogue has, and a tap can be a release behind the page that said
                // there was one. The row that follows names what will actually start.
                let version = match self.replaced() {
                    Some(path) => self.version_on_disk(&path).await.unwrap_or(latest),
                    None => latest,
                };
                UpdateOutcome::Applied { version, restart_required: true }
            }
            Err(reason) => UpdateOutcome::Failed { reason },
        }
    }

    /// Update a managed install by running the manager that owns it, then checking it did.
    ///
    /// Checked, because a package manager's exit status answers "did the command run" and not
    /// "is there a new neosh": `brew upgrade` on a tap that has not caught up exits 0 having
    /// printed `already installed`, and `npm install -g` of a version the registry has not seen
    /// yet is the same. What this is *for* is a different binary on disk, so that is what is
    /// asserted — the same `stat` that notices an upgrade run in another terminal.
    async fn run_manager(
        &self,
        method: InstallMethod,
        latest: &str,
        progress: &Progress,
    ) -> Result<(), String> {
        let by_hand = method.upgrade_command().unwrap_or_default();
        let steps = method.upgrade_steps();
        let Some(tool) = steps.first().and_then(|s| s.first()) else {
            return Err("this install has no package manager to run".into());
        };
        let Some(tool_path) = find_tool(tool, &self.exe) else {
            return Err(format!(
                "`{tool}` is not on this workspace's PATH — in a shell, run: {by_hand}"
            ));
        };
        for step in steps {
            let (_, args) = step.split_first().unwrap_or((&"", &[]));
            let phase = step.join(" ");
            progress(UpdateProgress { phase: phase.clone(), line: None, done: false });
            run_step(&tool_path, args, method, &phase, progress).await.map_err(|e| {
                format!("`{phase}` failed: {e} — in a shell, run: {by_hand}")
            })?;
        }
        // `found` is about the binary this process started under; the manager may have just put
        // a different one there, and the check below must read that one.
        self.inner.write().await.found = None;
        if self.replaced().is_none() {
            return Err(format!(
                "{by_hand} finished but the binary did not change — the package index may not \
                 have {latest} yet; try again later, or in a shell run: {by_hand}"
            ));
        }
        Ok(())
    }

    /// Download the release for this platform and swap it in.
    ///
    /// The swap is a rename onto the running path, which on Unix is atomic and is allowed while
    /// the old binary is executing — the running process keeps the inode it started with, which is
    /// exactly why the restart is a separate question. A copy-in-place would be the version of
    /// this that can leave a half-written binary behind.
    async fn replace_binary(&self, version: &str, progress: &Progress) -> Result<(), String> {
        let Some(triple) = target_triple() else {
            return Err("no published binary for this platform".into());
        };
        let name = format!("neosh-{triple}");
        let base = format!("{DOWNLOAD}/v{version}/{name}.tar.gz");

        progress(UpdateProgress { phase: format!("downloading {name}"), line: None, done: false });
        let tar = http_bytes(&base).await.map_err(|e| format!("downloading {name}: {e}"))?;
        progress(UpdateProgress { phase: "verifying".into(), line: None, done: false });
        let want = http_text(&format!("{base}.sha256"))
            .await
            .map_err(|e| format!("downloading the checksum: {e}"))?;
        let want = want.split_whitespace().next().unwrap_or_default().to_string();

        let got = sha256_hex(&tar);
        if got != want {
            // Refused rather than installed: this is the one check standing between a release and
            // whatever else answered that URL.
            return Err(format!("checksum mismatch (expected {want}, got {got})"));
        }

        progress(UpdateProgress { phase: "installing".into(), line: None, done: false });
        // Next to the target, because a rename across filesystems is not a rename — and /tmp is
        // very often a different filesystem from a home directory or /usr/local.
        let dir = self.exe.parent().ok_or("the running binary has no directory")?;
        let staged = dir.join(format!(".neosh-update-{version}"));
        unpack_neosh(&tar, &staged).map_err(|e| format!("unpacking: {e}"))?;

        std::fs::rename(&staged, &self.exe).map_err(|e| {
            let _ = std::fs::remove_file(&staged);
            format!("replacing {}: {e}", self.exe.display())
        })
    }
}

/// `neosh` as the shell would find it, or nothing.
///
/// The last resort in [`Updater::on_disk`], and it exists for one shape: Homebrew removes the old
/// keg on upgrade, so on a platform where the launch path is already fully resolved there is no
/// link left to follow and no file left to stat — both of the specific answers are gone, and the
/// only remaining true statement about what would run is "whatever is on `PATH`", which is also
/// exactly what the terminal is about to exec.
///
/// Walked by hand rather than through a crate: it is one `split` and an `is_file`, and a
/// dependency for that is a dependency to keep updated.
pub fn which_neosh() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("neosh"))
        .find(|p| p.is_file() && is_executable(p))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

/// Where a package manager's executable is, or nothing.
///
/// `PATH` first, because that is what a shell would run. Then the prefix the *binary* is under:
/// a workspace started from a launcher, or a shell whose profile never added `/opt/homebrew/bin`,
/// has a `PATH` without `brew` on it and a neosh that `brew` plainly installed — the prefix is
/// three directories above the keg, and the tool that owns a keg lives in that prefix's `bin`.
/// Same shape for npm and cargo: `<prefix>/lib/node_modules/…` and `~/.cargo/bin/neosh` both
/// keep their tool beside what they installed.
fn find_tool(tool: &str, exe: &Path) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        if let Some(p) = std::env::split_paths(&path)
            .map(|d| d.join(tool))
            .find(|p| p.is_file() && is_executable(p))
        {
            return Some(p);
        }
    }
    let s = exe.to_string_lossy();
    let prefix = ["/Cellar/", "/lib/node_modules/", "/.cargo/bin/"]
        .iter()
        .find_map(|m| s.find(m).map(|i| PathBuf::from(&s[..i])));
    let candidates = prefix
        .into_iter()
        .flat_map(|p| [p.join("bin").join(tool), p.join(tool)])
        .chain(
            ["/opt/homebrew/bin", "/usr/local/bin", "/home/linuxbrew/.linuxbrew/bin"]
                .iter()
                .map(|d| Path::new(d).join(tool)),
        );
    candidates.into_iter().find(|p| p.is_file() && is_executable(p))
}

/// Run one step of a package manager, streaming what it prints onto `progress`.
///
/// Both pipes are read for the life of the child, on tasks of their own, because a manager whose
/// stderr nobody drains stops mid-download when the pipe fills. `\r` counts as a line ending as
/// well as `\n` — brew and cargo redraw one status line in place, and a reader that only honours
/// the second sees one line per step, at the end, having reported nothing anybody could watch.
///
/// Stdin is closed and every prompt is turned off up front: a package manager that asks a
/// question on a pipe nobody answers is a hang with no visible cause, and the answer to *any*
/// question it might ask is "no, do it the way `neosh` was installed".
async fn run_step(
    tool: &Path,
    args: &[&str],
    method: InstallMethod,
    phase: &str,
    progress: &Progress,
) -> Result<(), String> {
    use tokio::io::AsyncReadExt;

    let mut cmd = tokio::process::Command::new(tool);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env_remove("SUDO_ASKPASS")
        .kill_on_drop(true);
    if method == InstallMethod::Homebrew {
        // The refresh is the step before this one, so a second one inside `upgrade` is a second
        // clone of the tap for nothing — and it is what puts the "auto-update" sentence in the log.
        cmd.env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .env("HOMEBREW_NO_ENV_HINTS", "1")
            .env("HOMEBREW_NO_EMOJI", "1")
            .env("HOMEBREW_COLOR", "0")
            .env("NONINTERACTIVE", "1");
    }
    if method == InstallMethod::Npm {
        cmd.env("npm_config_progress", "false").env("CI", "1");
    }
    if method == InstallMethod::Cargo {
        cmd.env("CARGO_TERM_COLOR", "never").env("CARGO_TERM_PROGRESS_WHEN", "never");
    }

    let mut child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", tool.display()))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    /// Everything a pipe said, bounded, and the callback fed one line at a time.
    async fn drain(
        mut pipe: impl tokio::io::AsyncRead + Unpin,
        phase: String,
        progress: Progress,
    ) -> String {
        let mut said = String::new();
        let mut buf = [0u8; 4096];
        let mut pending = String::new();
        loop {
            let n = match pipe.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            pending.push_str(&String::from_utf8_lossy(&buf[..n]));
            while let Some(at) = pending.find(['\r', '\n']) {
                let line: String = pending.drain(..=at).collect();
                let line = line.trim_end_matches(['\r', '\n']).trim();
                if line.is_empty() {
                    continue;
                }
                if said.len() < 16 * 1024 {
                    said.push_str(line);
                    said.push('\n');
                }
                progress(UpdateProgress {
                    phase: phase.clone(),
                    line: Some(line.to_string()),
                    done: false,
                });
            }
        }
        if !pending.trim().is_empty() {
            said.push_str(pending.trim());
            said.push('\n');
        }
        said
    }

    let out = stdout.map(|p| tokio::spawn(drain(p, phase.to_string(), progress.clone())));
    let err = stderr.map(|p| tokio::spawn(drain(p, phase.to_string(), progress.clone())));

    let status = match tokio::time::timeout(STEP_TIMEOUT, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("waiting for {phase}: {e}")),
        Err(_) => {
            let _ = child.kill().await;
            return Err(format!("gave up after {} minutes", STEP_TIMEOUT.as_secs() / 60));
        }
    };
    let mut said = String::new();
    for task in [out, err].into_iter().flatten() {
        if let Ok(s) = task.await {
            said.push_str(&s);
        }
    }
    if status.success() {
        return Ok(());
    }
    // The tool's own last word — `Error: neosh not installed`, `EACCES`, `failed to compile` —
    // is the one that says what to do next. An exit code flattens all of them into "failed".
    let last = said
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .next_back()
        .unwrap_or("no output");
    Err(format!("exit {}: {last}", status.code().unwrap_or(-1)))
}

#[cfg(not(unix))]
fn is_executable(_p: &Path) -> bool {
    true
}

async fn fetch_latest() -> Result<String, String> {
    let body = http_text(LATEST).await?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    v.get("tag_name")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| "the releases API returned no tag_name".to_string())
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        // GitHub refuses a request with no user agent, and a timeout because this runs on a
        // background task that must not be able to sit forever.
        .user_agent(concat!("neosh/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())
}

async fn http_text(url: &str) -> Result<String, String> {
    let r = client()?.get(url).send().await.map_err(|e| e.to_string())?;
    if !r.status().is_success() {
        return Err(format!("{} returned {}", url, r.status()));
    }
    r.text().await.map_err(|e| e.to_string())
}

async fn http_bytes(url: &str) -> Result<Vec<u8>, String> {
    let r = client()?.get(url).send().await.map_err(|e| e.to_string())?;
    if !r.status().is_success() {
        return Err(format!("{} returned {}", url, r.status()));
    }
    Ok(r.bytes().await.map_err(|e| e.to_string())?.to_vec())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Pull the `neosh` binary out of a release tarball and write it executable at `dest`.
///
/// Through the system `tar` rather than two more crates in the tree: it is on both platforms neosh
/// ships to, `install.sh` already depends on it for exactly this, and a decompressor is a large
/// thing to carry for one call. The archive is `neosh-<triple>/neosh` plus a README and a licence,
/// and `--strip-components=1` is what turns that into a flat extract.
fn unpack_neosh(tar_gz: &[u8], dest: &Path) -> std::io::Result<()> {
    use std::io::Write;

    let dir = tempdir_beside(dest)?;
    let archive = dir.join("neosh.tar.gz");
    std::fs::write(&archive, tar_gz)?;

    let out = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("--strip-components=1")
        .arg("-C")
        .arg(&dir)
        .output()?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(std::io::Error::other(format!(
            "tar failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    let extracted = dir.join("neosh");
    if !extracted.is_file() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no neosh binary in the archive",
        ));
    }

    // Copied rather than renamed out: the caller renames `dest` onto the running binary, and that
    // rename has to stay on one filesystem.
    let mut src = std::fs::File::open(&extracted)?;
    let mut out = std::fs::File::create(dest)?;
    std::io::copy(&mut src, &mut out)?;
    out.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))?;
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// A scratch directory next to `near`, so everything stays on one filesystem.
fn tempdir_beside(near: &Path) -> std::io::Result<PathBuf> {
    let parent = near.parent().unwrap_or(Path::new("."));
    let dir = parent.join(format!(".neosh-unpack-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Seconds since the epoch, for stamping a check.
#[allow(dead_code)]
fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_install_is_recognised_by_where_it_lives() {
        let cases = [
            ("/opt/homebrew/Cellar/neosh/0.1.0/bin/neosh", InstallMethod::Homebrew),
            ("/usr/local/Cellar/neosh/0.1.0/bin/neosh", InstallMethod::Homebrew),
            ("/home/x/.npm-global/lib/node_modules/@neosh/cli-linux-x64/bin/neosh", InstallMethod::Npm),
            ("/home/x/.cargo/bin/neosh", InstallMethod::Cargo),
            ("/home/x/.local/bin/neosh", InstallMethod::Standalone),
            ("/usr/local/bin/neosh", InstallMethod::Standalone),
            ("/home/x/src/neosh/target/debug/neosh", InstallMethod::Development),
            ("/home/x/src/neosh/target/release/neosh", InstallMethod::Development),
        ];
        for (path, want) in cases {
            assert_eq!(install_method(Path::new(path)), want, "{path}");
        }
    }

    #[test]
    fn a_checkout_wins_over_every_other_shape() {
        // A worktree can live anywhere, including under a path that looks like a managed install.
        // Getting this backwards means offering to update somebody's working copy.
        let p = Path::new("/home/x/.cargo/src/neosh/target/debug/neosh");
        assert_eq!(install_method(p), InstallMethod::Development);
    }

    #[test]
    fn versions_compare_as_numbers_not_as_text() {
        assert!(is_newer("0.10.0", "0.9.0"), "0.10 is newer than 0.9 despite sorting before it");
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn an_unparseable_version_never_manufactures_an_update() {
        // A tag nobody anticipated is declined rather than rounded — the failure mode of guessing
        // is telling every user there is an update that does not exist.
        assert!(!is_newer("0.2.0-rc.1", "0.1.0"));
        assert!(!is_newer("nightly", "0.1.0"));
        assert!(!is_newer("0.2.0", "some-git-build"));
    }

    /// A scratch directory of this test's own, with a fake `neosh` in it.
    fn sandbox(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("neosh-update-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).expect("a scratch directory");
        dir
    }

    /// Write `body` at `path`, executable, the way an installer would.
    fn install(path: &Path, body: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).expect("a directory to install into");
        }
        std::fs::write(path, body).expect("a binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("+x");
        }
    }

    fn updater(exe: &Path, launch: &Path) -> Updater {
        Updater::new(
            exe.canonicalize().expect("a real path"),
            Some(launch.to_path_buf()),
            "0.4.1".into(),
        )
    }

    /// The case the whole feature exists for: `brew upgrade` in the next terminal along.
    ///
    /// Homebrew never touches the file we are executing. It writes a new keg and repoints
    /// `bin/neosh` at it — so a workspace that resolved the symlink at startup and remembered the
    /// answer sees nothing at all, which is exactly what used to happen and why a machine ran the
    /// old build for days after being told it had been updated.
    #[test]
    #[cfg(unix)]
    fn a_repointed_symlink_is_a_binary_that_was_replaced() {
        let dir = sandbox("brew");
        let old = dir.join("Cellar/neosh/0.4.1/bin/neosh");
        let new = dir.join("Cellar/neosh/0.4.2/bin/neosh");
        let link = dir.join("bin/neosh");
        install(&old, "i am 0.4.1");
        std::os::unix::fs::symlink(&old, &link).expect("a link into the keg");

        // Started as `bin/neosh`, which is what a shell on `PATH` does.
        let u = updater(&link, &link);
        assert!(u.replaced().is_none(), "nothing has happened yet");

        // `brew upgrade`: a new keg, the link moved, and the old keg removed on cleanup.
        install(&new, "i am 0.4.2 and longer");
        std::fs::remove_file(&link).expect("the old link");
        std::os::unix::fs::symlink(&new, &link).expect("the new link");
        std::fs::remove_dir_all(dir.join("Cellar/neosh/0.4.1")).expect("brew cleanup");

        assert_eq!(
            u.replaced(),
            Some(new.canonicalize().expect("the new keg")),
            "following the link afresh is the only way to see this"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An install that is a plain file — `install.sh`, `cargo install`, our own rename.
    #[test]
    fn a_file_overwritten_in_place_is_a_binary_that_was_replaced() {
        let dir = sandbox("inplace");
        let exe = dir.join("bin/neosh");
        install(&exe, "i am 0.4.1");

        let u = updater(&exe, &exe);
        assert!(u.replaced().is_none());

        // Written beside and renamed on, which is what every safe replacement does — and what
        // changes the inode even when a package manager has preserved the modification time.
        let staged = dir.join("bin/.neosh-new");
        install(&staged, "i am 0.4.2 and a different length");
        std::fs::rename(&staged, &exe).expect("the swap");

        assert_eq!(u.replaced(), Some(exe.canonicalize().expect("the new file")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A restart is a promise to come back, and one that cannot be kept is worse than silence.
    ///
    /// With every candidate gone there is no binary to restart onto: saying "restart for the
    /// update" would take a working workspace away and put nothing in its place.
    #[test]
    fn nothing_left_to_run_is_never_a_restart_worth_offering() {
        let dir = sandbox("gone");
        let exe = dir.join("bin/neosh");
        install(&exe, "i am 0.4.1");
        let u = updater(&exe, &exe);

        std::fs::remove_file(&exe).expect("an uninstall");
        // `which_neosh` may still find a real neosh on this machine's PATH, which would be a
        // truthful answer. What must never happen is a claim with nothing behind it.
        if let Some(found) = u.replaced() {
            assert!(found.is_file(), "offered a restart onto {found:?}, which is not there");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A checkout is the one place where "the binary changed" is true on nearly every build, and
    /// it already has an answer — the terminal says it on attach, out of `BuildId`, and names
    /// `neosh stop`. Two notices about one thing is one notice nobody reads.
    #[test]
    fn a_checkout_is_never_told_to_restart() {
        let dir = sandbox("checkout");
        let exe = dir.join("target/debug/neosh");
        install(&exe, "a build");
        let u = updater(&exe, &exe);
        install(&exe, "a rebuild, longer than the last one");

        assert_eq!(u.method(), InstallMethod::Development);
        assert!(u.replaced().is_none(), "the BuildId notice already covers a checkout");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unknown stamp compares equal to everything rather than to nothing.
    ///
    /// The failure of guessing here is a workspace that offers a restart on every redraw, forever,
    /// having never seen the file it is talking about.
    #[test]
    fn an_executable_that_could_not_be_stat_d_claims_nothing() {
        let dir = sandbox("nostat");
        let exe = dir.join("bin/neosh");
        // Never created. `Updater::new` finds no stamp, and nothing may be inferred from that.
        let u = Updater::new(exe.clone(), Some(exe), "0.4.1".into());
        assert!(u.stamp.is_none());
        assert!(u.replaced().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A development build asked to update names what is released instead of calling itself the
    /// newest — which is what a `target/debug/neosh` left serving a workspace used to answer.
    #[test]
    fn a_development_build_says_what_is_released() {
        let behind = development_sentence("0.4.11", Some("0.5.0"));
        assert!(behind.contains("development build 0.4.11"), "{behind}");
        assert!(behind.contains("0.5.0 is released"), "{behind}");
        assert!(behind.contains("neosh stop"), "{behind}");
        let level = development_sentence("0.5.0", Some("0.5.0"));
        assert!(level.contains("the newest release"), "{level}");
        let offline = development_sentence("0.4.11", None);
        assert!(
            !offline.contains("released") && !offline.contains("newest"),
            "nothing claimed about a release it never saw: {offline}"
        );
    }

    #[test]
    fn only_a_standalone_binary_is_ours_to_replace() {
        assert!(InstallMethod::Standalone.self_updatable());
        assert!(InstallMethod::Standalone.upgrade_steps().is_empty());
        for m in [InstallMethod::Homebrew, InstallMethod::Npm, InstallMethod::Cargo] {
            assert!(!m.self_updatable(), "{m:?} belongs to something else");
            assert!(!m.upgrade_steps().is_empty(), "{m:?} must have a manager to run");
            assert!(m.upgrade_command().is_some(), "{m:?} must name the command for a person");
        }
        // A checkout has neither: nothing to run, and nothing to replace.
        assert!(!InstallMethod::Development.self_updatable());
        assert!(InstallMethod::Development.upgrade_steps().is_empty());
        assert!(InstallMethod::Development.upgrade_command().is_none());
    }

    /// What a person is told to type is exactly what the host runs — one list, two spellings.
    #[test]
    fn the_command_for_a_person_is_the_steps_the_host_runs() {
        for m in [InstallMethod::Homebrew, InstallMethod::Npm, InstallMethod::Cargo] {
            let typed = m.upgrade_command().expect("names a command");
            let ran: Vec<String> = m.upgrade_steps().iter().map(|s| s.join(" ")).collect();
            assert_eq!(typed, ran.join(" && "), "{m:?}");
        }
    }

    /// A tool's own last line is the failure sentence, and its exit code is not.
    ///
    /// Through `sh` because that is a package manager as far as `run_step` is concerned: a child
    /// with two pipes and an exit status. What is asserted is the contract every real one relies
    /// on — output streamed as it happens, `\r`-redrawn lines counted as lines, and the last thing
    /// said being what a failure reports.
    #[tokio::test]
    async fn a_step_streams_what_it_prints_and_fails_with_its_last_word() {
        use std::sync::Mutex;
        let seen: Arc<Mutex<Vec<UpdateProgress>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let progress: Progress = Arc::new(move |p| sink.lock().expect("no poisoning").push(p));

        let ok = run_step(
            Path::new("sh"),
            &["-c", "printf 'one\\ntwo\\rthree\\n'; echo err >&2"],
            InstallMethod::Standalone,
            "sh",
            &progress,
        )
        .await;
        assert_eq!(ok, Ok(()));
        let mut lines: Vec<String> = seen
            .lock()
            .expect("no poisoning")
            .iter()
            .filter_map(|p| p.line.clone())
            .collect();
        lines.sort();
        assert_eq!(lines, ["err", "one", "three", "two"], "both pipes, and `\\r` is a line end");

        let failed = run_step(
            Path::new("sh"),
            &["-c", "echo starting; echo 'Error: no such formula' >&2; exit 3"],
            InstallMethod::Standalone,
            "sh",
            &progress,
        )
        .await;
        let reason = failed.expect_err("a non-zero exit is a failure");
        assert!(reason.contains("exit 3"), "{reason}");
        assert!(reason.contains("no such formula"), "the tool's last word, not ours: {reason}");
    }

    /// A tool that is not there is a sentence, not a panic and not a hang.
    #[tokio::test]
    async fn a_missing_tool_is_reported_by_name() {
        let progress: Progress = Arc::new(|_| {});
        let err = run_step(
            Path::new("/nonexistent/definitely-not-brew"),
            &["update"],
            InstallMethod::Homebrew,
            "brew update",
            &progress,
        )
        .await
        .expect_err("cannot start");
        assert!(err.contains("could not start"), "{err}");
    }

    /// The manager lives in the prefix that owns the binary, and `PATH` is not the only witness.
    ///
    /// A workspace started from a launcher, or a shell whose profile never added the prefix, has
    /// a `PATH` without `brew` on it and a neosh that `brew` plainly installed. The keg names the
    /// prefix; the tool is in its `bin`.
    #[test]
    fn a_manager_is_found_beside_what_it_installed() {
        let dir = sandbox("prefix");
        let exe = dir.join("Cellar/neosh/0.4.1/bin/neosh");
        let brew = dir.join("bin/brew");
        install(&exe, "a keg");
        install(&brew, "#!/bin/sh\n");
        // A tool name nothing on this machine's PATH will have, so only the prefix can answer.
        let name = brew.file_name().and_then(|n| n.to_str()).expect("a name");
        let found = find_tool(name, &exe);
        // `brew` may genuinely be on PATH here; either answer must be an executable file.
        let found = found.expect("found somewhere");
        assert!(found.is_file() && is_executable(&found), "{found:?}");
        if !which_on_path(name) {
            assert_eq!(found, brew, "the prefix's own bin, derived from the keg");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn which_on_path(tool: &str) -> bool {
        std::env::var_os("PATH").is_some_and(|p| {
            std::env::split_paths(&p).any(|d| d.join(tool).is_file())
        })
    }

    /// Two updates at once is one update and one refusal.
    #[tokio::test]
    async fn a_second_apply_while_one_is_running_is_refused() {
        let dir = sandbox("twice");
        let exe = dir.join("bin/neosh");
        install(&exe, "i am 0.4.1");
        let u = updater(&exe, &exe);
        u.applying.store(true, Ordering::SeqCst);
        let progress: Progress = Arc::new(|_| {});
        match u.apply(progress).await {
            UpdateOutcome::Failed { reason } => assert!(reason.contains("already running"), "{reason}"),
            other => panic!("{other:?}"),
        }
        // And the refusal did not clear the flag the running one owns.
        assert!(u.applying.load(Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The command we print has to be one that actually updates.
    ///
    /// Homebrew resolves formulae out of a git clone of the tap **on this disk** and only refreshes
    /// it every `HOMEBREW_AUTO_UPDATE_SECS` — 24 hours by default. So for the first day of a
    /// release, which is precisely when somebody has been told there is one, a bare
    /// `brew upgrade neosh` reads a stale clone and reports that the version they already have is
    /// the newest published. That happened: the tap had 0.4.3, brew said 0.4.2 was current, and the
    /// only thing wrong was the string in this file — `docs/releasing.md` had described the caching
    /// all along.
    ///
    /// Asserted on the *content* rather than on equality, so rewording the command is free and
    /// dropping the refresh is not.
    #[test]
    fn the_homebrew_command_refreshes_the_tap_before_it_upgrades() {
        let brew = InstallMethod::Homebrew.upgrade_command().expect("homebrew names a command");
        assert!(brew.contains("brew update"), "must refresh the tap clone first: {brew}");
        assert!(brew.contains("brew upgrade"), "and then upgrade: {brew}");
        assert!(
            brew.find("brew update") < brew.find("brew upgrade"),
            "and in that order, or the upgrade still reads the stale clone: {brew}"
        );
        // The other two resolve against the network on every run — npm against the registry for
        // `@latest`, cargo against its index as part of `install` — so neither needs a refresh
        // step, and adding one would be cargo-culting this fix onto tools that never had the bug.
        for m in [InstallMethod::Npm, InstallMethod::Cargo] {
            let c = m.upgrade_command().expect("names a command");
            assert!(!c.contains("&&"), "{m:?} needs no refresh step: {c}");
        }
    }
}
