//! Chromium-backed page rendering.
//!
//! Launches an installed Chrome or Edge in headless mode against a throwaway
//! profile, drives it over the DevTools Protocol ([`cdp`]), and shuts it down
//! again. The browser exists only to render one page; it never touches media
//! bytes.
//!
//! # Isolation
//!
//! Every render gets a brand-new profile directory under the system temp
//! directory. The user's own browser profile is never opened, read, or copied,
//! so no real cookies, history, or saved sessions can reach the page. Nothing
//! about the session is persisted: the profile is deleted when the render ends,
//! on every path including failure and cancellation.
//!
//! # Bounds
//!
//! A render is capped three ways — start-up, the wait for the page's own
//! readiness signal, and the total budget — and the browser is terminated and
//! its profile removed whether the render succeeded, failed, timed out, or was
//! abandoned. A [`BrowserProcess`] guard enforces that even if the future is
//! dropped mid-flight.

pub mod cdp;
pub mod proxy;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Semaphore;

use crate::errors::{AppError, AppResult};
use crate::models::new_id;
use crate::process::NoConsoleWindow;

use super::renderer::{
    network_media_from_events, parse_media_sources, BrowserProfile, BrowserVisibility, PageRenderer,
    RenderRequest, RenderedPage,
};
use super::{FetchRequest, HttpClient};

/// Environment variable naming a browser executable to use.
///
/// The first entry in the discovery order, for a machine whose browser lives
/// somewhere unusual.
pub const BROWSER_ENV_VAR: &str = "DOUYIN_DOWNLOADER_BROWSER";

/// How long to wait for the browser to publish its DevTools endpoint.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a control-plane command may take.
///
/// Connecting, enabling a domain, reading the cookie jar — commands that talk to
/// the browser about itself rather than to a page. These answer promptly or the
/// browser is not usable at all, so the budget stays short: a dead browser must
/// be reported as dead, not waited on.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a command that runs *against a page* may take.
///
/// Navigation, evaluating a predicate, collecting the page, reading its URL and
/// title. These are the commands whose reply waits on the page itself: Chromium
/// runs them on the page's main thread, which is exactly the thread a heavy page
/// is busiest on, and their replies are read off the same socket as that page's
/// event stream.
///
/// Sized as one budget rather than raised a command at a time. A flat 15-second
/// budget produced the same failure three separate times on this path — a
/// signed-in profile page is served far more of by Douyin than an anonymous one,
/// and each fix that widened one command's budget only moved the failure to the
/// next command that talked to the same busy page. What the commands have in
/// common is the page, so the budget is attached to that.
///
/// This is not licence to wait forever: readiness is polled and the render as a
/// whole is bounded well below this, so a page that never answers still ends the
/// render with a reported reason.
const PAGE_TIMEOUT: Duration = Duration::from_secs(60);

/// How often to re-check a page's readiness predicate.
const READY_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How many readiness polls between asking the browser where it ended up.
///
/// Eight polls is about two seconds — often enough to catch a redirect that
/// happened immediately, rarely enough that a page taking its time is not
/// interrupted by an extra round-trip every quarter second.
const LANDING_CHECK_EVERY: u32 = 8;

/// How many times a render is attempted.
///
/// A public page occasionally serves one visitor a challenge or a partially
/// initialized shell and works on the next visit. Retrying the page is a
/// legitimate second look at the same public content — it is not an attempt to
/// defeat anything, and a genuinely refused page simply fails again, which is
/// reported as-is.
const RENDER_ATTEMPTS: usize = 2;

/// How long to wait before looking at a retryable page again, multiplied by the
/// attempt number.
///
/// A page that has not finished building needs time to finish, not another
/// reading taken immediately. This is not a backoff against a refusing server —
/// a refusal is not retried at all — it is a pause for a page that is still
/// arriving.
const RETRY_BACKOFF: Duration = Duration::from_millis(400);

/// How many times to retry removing the temporary profile, and how long between
/// tries.
///
/// A browser that has just been killed is still releasing its handles, and on
/// Windows a directory cannot be removed while one is open. A real profile is
/// thousands of files with memory-mapped caches, so the release takes a moment.
/// This is patience, not a retry policy for a real failure: the last attempt
/// logs and gives up, and [`sweep_stale_profiles`] reclaims what is left.
const PROFILE_REMOVE_ATTEMPTS: usize = 20;
const PROFILE_REMOVE_DELAY: Duration = Duration::from_millis(200);

/// The file a browser writes the DevTools port it chose into, inside its
/// profile directory.
const DEVTOOLS_PORT_FILE: &str = "DevToolsActivePort";

/// Name prefix of a throwaway browser profile in the system temp directory.
/// Name prefix of a throwaway render profile.
///
/// Public because it is the *whole* of what the cleanup matches on: anything
/// named like this, in the directory the sweep is pointed at, is a render
/// profile and may be reclaimed. The application's own sign-in profile is
/// deliberately not named like this, and a test asserts it.
pub const PROFILE_PREFIX: &str = "douyin-downloader-browser-";

/// How long an abandoned profile must sit untouched before it is reclaimed.
///
/// Age is what makes the sweep safe: a profile another instance is using is
/// always being written to, so no live one is ever an hour old. An hour is also
/// far longer than any render, so an instance that is merely idle between
/// renders keeps its own profile.
const STALE_PROFILE_AGE: Duration = Duration::from_secs(60 * 60);

/// The client used to read the browser's target list.
///
/// Deliberately permissive: the endpoint is `127.0.0.1` on a port this
/// application chose, and the shipped destination policy exists to keep
/// *content* requests away from loopback. A control channel to the
/// application's own child process is not that case, and wiring the content
/// policy in front of it would refuse every render.
fn devtools_client() -> Arc<dyn HttpClient> {
    Arc::new(
        super::ReqwestHttpClient::with_policy(30, super::DestinationPolicy::Any)
            .expect("the DevTools client is built once, with fixed arguments"),
    )
}

/// Starts a browser for one session.
///
/// A seam, not an abstraction for its own sake. Everything interesting about the
/// renderer is its *session lifecycle* — one browser reused across renders,
/// concurrent callers queueing behind the same one, a dead browser retired and
/// replaced — and none of it can be asserted against a real browser without the
/// live network and a real Douyin page, which makes a failure there
/// indistinguishable from a flaky service. Injecting the launcher lets a test
/// substitute a loopback CDP server and pin the lifecycle exactly, while the
/// shipped implementation stays [`Browsers`] and launches a real process.
#[async_trait]
trait BrowserLauncher: Send + Sync {
    async fn launch(
        &self,
        profile: &Path,
        visibility: BrowserVisibility,
        kind: BrowserProfile,
    ) -> AppResult<Box<dyn LaunchedBrowser>>;
}

/// One launched browser: enough of it for the renderer to drive and to clean up.
#[async_trait]
trait LaunchedBrowser: Send + Sync {
    /// Process id, when there is one. Used by diagnostics and by a test that
    /// simulates a crash by naming the process rather than matching an image.
    fn pid(&self) -> Option<u32>;

    /// The throwaway profile this browser was given, while it still has one.
    ///
    /// Named so that "the browser and everything it wrote are gone" can be a
    /// statement about *this* browser. Counting the profile directories in the
    /// temp folder instead is a statement about every browser on the machine,
    /// including the ones another test is running — which makes a passing test
    /// depend on what else happens to be going on.
    fn profile(&self) -> Option<&Path>;

    /// Wait for the browser's DevTools endpoint and report its port.
    async fn devtools_port(&self, timeout: Duration) -> AppResult<u16>;

    /// Terminate the browser and release anything it held.
    async fn shutdown(&mut self);

    /// Terminate the browser without awaiting anything.
    ///
    /// The exit path cannot await — the runtime is going away — and leaving the
    /// browser running is precisely the failure this exists to prevent. Killing
    /// is safe by identity rather than by name: the child handle this browser
    /// holds is what the process id is derived from, and Windows will not reuse
    /// an id while a handle to the process is open.
    fn terminate_now(&mut self);
}

/// The shipped launcher: a real Chrome or Edge process on a throwaway profile,
/// with every request it makes routed through [`proxy::GuardedProxy`].
struct Browsers {
    executable: PathBuf,
    /// Which destinations the browser may reach. The same policy the HTTP client
    /// holds content to.
    policy: super::DestinationPolicy,
    /// Started on the first launch and kept for the renderer's lifetime, so
    /// every browser this renderer starts shares one guard and one port.
    proxy: tokio::sync::OnceCell<Arc<proxy::GuardedProxy>>,
}

impl Browsers {
    fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            policy: super::DestinationPolicy::PublicOnly,
            proxy: tokio::sync::OnceCell::new(),
        }
    }

    /// The network guard, started if this is the first launch.
    ///
    /// A failure to start it fails the launch. That is deliberate: a browser
    /// launched without a guard has Chromium's own DNS and its own redirects and
    /// nothing checking them, and running one silently would be worse than not
    /// rendering at all.
    async fn guard(&self) -> AppResult<&Arc<proxy::GuardedProxy>> {
        self.proxy
            .get_or_try_init(|| async {
                Ok::<_, AppError>(Arc::new(proxy::GuardedProxy::start(self.policy).await?))
            })
            .await
    }
}

#[async_trait]
impl BrowserLauncher for Browsers {
    async fn launch(
        &self,
        profile: &Path,
        visibility: BrowserVisibility,
        kind: BrowserProfile,
    ) -> AppResult<Box<dyn LaunchedBrowser>> {
        let guard = self.guard().await?;
        Ok(Box::new(BrowserProcess::launch(
            &self.executable,
            profile,
            visibility,
            kind,
            Some(guard.port()),
        )?))
    }
}

#[async_trait]
impl LaunchedBrowser for BrowserProcess {
    fn pid(&self) -> Option<u32> {
        self.pid
    }

    fn profile(&self) -> Option<&Path> {
        // Cleared once the directory has been removed, so an emptied path means
        // "there is no profile any more" rather than "the working directory".
        (!self.profile.as_os_str().is_empty()).then_some(self.profile.as_path())
    }

    async fn devtools_port(&self, timeout: Duration) -> AppResult<u16> {
        wait_for_devtools_port(&self.profile, timeout).await
    }

    async fn shutdown(&mut self) {
        BrowserProcess::shutdown(self).await
    }

    fn terminate_now(&mut self) {
        BrowserProcess::terminate_now(self);
    }
}

/// Renders pages with an installed Chrome or Edge.
pub struct ChromiumRenderer {
    executable: PathBuf,
    /// Starts the browser. The real one except in tests.
    launcher: Arc<dyn BrowserLauncher>,
    /// Reads the browser's own target list, which lives on loopback by design —
    /// a port this application chose, on a browser it started. Built internally
    /// with the permissive policy: the destination policy refuses loopback, and
    /// rightly so for content, but the DevTools endpoint is not content — it is
    /// the control channel to a process this application owns. The render's
    /// *navigation* is still policy-checked in `render_inner`.
    http: Arc<dyn HttpClient>,
    /// The kept-alive browser and the rules for retiring it. Shared, because
    /// the idle watcher outlives any one call into this renderer.
    slot: Arc<SessionSlot>,
    /// Where the application's persistent browser profile lives, when the
    /// composition root knows.
    ///
    /// `None` in tests that drive the renderer around a fake launcher and never
    /// ask for an authenticated render; asking for one then fails with a clear
    /// message rather than quietly using a throwaway profile.
    auth_profile: Option<PathBuf>,
}

/// The browser this renderer keeps alive, and everything that decides its fate.
///
/// Shared behind an `Arc` so the idle watcher can close the browser on a timer.
/// `render` is handed `&self` and cannot reach an `Arc` of the renderer, but it
/// can clone this — which is the whole reason the session does not live directly
/// on the renderer.
struct SessionSlot {
    /// Serializes renders. A browser is heavy, and one at a time is plenty for
    /// this application; it is also what stops two concurrent requests from
    /// starting two browsers. The idle watcher takes it too, so a browser is
    /// never closed while a render is driving it.
    gate: Semaphore,
    /// The browser kept alive between renders.
    ///
    /// Starting a browser per render is what put a window on screen for every
    /// profile page, every page of a listing, and every post resolved for
    /// download. One browser, navigated as often as needed, does the same work
    /// with one window in total — and the probe that established this measured
    /// exactly that: a single process serving a profile listing, a post page,
    /// and the listing again.
    session: tokio::sync::Mutex<Option<BrowserSession>>,
    /// How many browsers this renderer has started. Diagnostics, and how a test
    /// proves reuse rather than assuming it.
    launches: std::sync::atomic::AtomicUsize,
    /// Milliseconds since `started` at the end of the last render. Ignored
    /// while a render is in progress, which the gate already excludes.
    last_used_ms: std::sync::atomic::AtomicU64,
    /// How long a browser may sit unused before it is closed. Read at each tick
    /// so a test can shorten it without rebuilding the renderer.
    idle_timeout_ms: std::sync::atomic::AtomicU64,
    /// Whether an idle watcher has been started, so only one ever runs.
    watching: std::sync::atomic::AtomicBool,
    /// Set when the application is shutting down, so the watcher stops rather
    /// than trying to close a browser that is already being closed.
    stopping: std::sync::atomic::AtomicBool,
    started: Instant,
}

impl SessionSlot {
    fn new() -> Self {
        Self {
            gate: Semaphore::new(1),
            session: tokio::sync::Mutex::new(None),
            launches: std::sync::atomic::AtomicUsize::new(0),
            last_used_ms: std::sync::atomic::AtomicU64::new(0),
            idle_timeout_ms: std::sync::atomic::AtomicU64::new(
                DEFAULT_IDLE_BROWSER_TIMEOUT.as_millis() as u64,
            ),
            watching: std::sync::atomic::AtomicBool::new(false),
            stopping: std::sync::atomic::AtomicBool::new(false),
            started: Instant::now(),
        }
    }

    fn note_use(&self) {
        let elapsed = self.started.elapsed().as_millis() as u64;
        self.last_used_ms
            .store(elapsed, std::sync::atomic::Ordering::SeqCst);
    }

    fn idle_for(&self) -> Duration {
        let elapsed = self.started.elapsed().as_millis() as u64;
        let last = self.last_used_ms.load(std::sync::atomic::Ordering::SeqCst);
        Duration::from_millis(elapsed.saturating_sub(last))
    }

    fn idle_timeout(&self) -> Duration {
        Duration::from_millis(
            self.idle_timeout_ms
                .load(std::sync::atomic::Ordering::SeqCst),
        )
    }

    /// Close the browser if nothing has used it for a whole timeout.
    ///
    /// One task for the renderer's life, started with the first browser and
    /// finished when the renderer is going away. It holds no state across ticks
    /// beyond the reading it is comparing against, so there is no cache to go
    /// stale and nothing to unsubscribe.
    ///
    /// The slot is held *weakly*. Holding it strongly would keep the renderer's
    /// session alive for as long as this task runs, so dropping the renderer
    /// would no longer drop the browser — which is a leak this very test suite
    /// caught the first time it was written the other way round.
    async fn watch_for_idle(weak: std::sync::Weak<SessionSlot>) {
        loop {
            let Some(slot) = weak.upgrade() else {
                // The renderer is gone; its `Drop` closed whatever was left.
                return;
            };
            let poll = slot.idle_timeout().max(Duration::from_millis(50)) / 4;
            drop(slot);
            tokio::time::sleep(poll).await;

            let Some(slot) = weak.upgrade() else {
                return;
            };

            if slot.stopping.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            if slot.idle_for() < slot.idle_timeout() {
                continue;
            }

            // Idle by our reckoning, but a render may have started or finished
            // in between. Taking the gate is what makes this race-free: it is
            // the same permit every render acquires, so holding it means no
            // render is running and none can start.
            let Ok(permit) = slot.gate.acquire().await else {
                return;
            };
            // A render that has just released the gate may not yet have
            // recorded that it finished — the two are separate lines, and on a
            // multi-threaded runtime another worker could look in between.
            // Waiting a whole interval before believing "idle" is what makes it
            // mean idle rather than "finished a moment ago", and it costs a
            // browser nothing: nothing is waiting on this.
            tokio::time::sleep(poll).await;

            if slot.stopping.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            if slot.idle_for() < slot.idle_timeout() {
                drop(permit);
                continue;
            }

            let mut guard = slot.session.lock().await;
            // A browser the user is signing in with is not idle, whatever the
            // clock says: nobody is typing into a login page every ninety
            // seconds, and closing it would throw away the sign-in they are
            // part-way through.
            if guard.as_ref().is_some_and(|session| session.interactive) {
                drop(guard);
                drop(permit);
                continue;
            }
            let Some(session) = guard.take() else {
                // Nothing to close. Keep watching: a browser started later
                // should be closed later too.
                drop(guard);
                drop(permit);
                continue;
            };
            let idle = slot.idle_for();
            tracing::info!(
                target: "browser",
                idle_seconds = idle.as_secs(),
                "closing the browser after it went unused"
            );
            session.shutdown().await;
            // One close per session is enough; a later render starts its own
            // browser and this loop keeps running to close that one.
            drop(guard);
            drop(permit);
        }
    }
}

/// How long a browser may sit unused before it is closed.
///
/// A browser is kept between renders because starting one costs a second or two
/// and puts a window on screen; keeping it forever costs a process the user can
/// see in Task Manager long after they finished. This is the compromise: long
/// enough that a profile listing's consecutive pages and the downloads that
/// follow them all reuse one browser, short enough that "I finished and the
/// browser is still there" stops being true.
const DEFAULT_IDLE_BROWSER_TIMEOUT: Duration = Duration::from_secs(90);

/// A browser kept alive between renders, with the client that drives it.
struct BrowserSession {
    browser: Box<dyn LaunchedBrowser>,
    client: cdp::CdpClient,
    /// The kind of browser this session is, so a render that needs a different
    /// kind gets one rather than being served the wrong sort.
    visibility: BrowserVisibility,
    /// Whose cookies this browser carries, and therefore whether it is a
    /// session an anonymous render may be served from.
    profile: BrowserProfile,
    /// Held while this browser has the persistent profile open.
    ///
    /// Dropped with the session, which is what makes the release automatic on
    /// every path that closes a browser — a failed render, the idle timer, the
    /// exit path — and by the operating system when the process ends without
    /// closing one. Never read: the handle's existence is the claim, and the
    /// leading underscore says so rather than leaving a reader to wonder.
    _profile_lock: Option<ProfileLock>,
    /// Set while the user is signing in by hand in this browser.
    ///
    /// Two things follow from it. The idle timer leaves the browser alone, so a
    /// window nobody is typing in is not closed while somebody is reading a
    /// login page. And the session is not handed to a render that would
    /// navigate away from the sign-in flow underneath the user.
    interactive: bool,
    /// Cleared for the duration of a render and set again only when the render
    /// *completed*.
    ///
    /// A session whose render was cancelled, timed out, or failed is not reused:
    /// nobody knows what state its page and its command stream were left in, and
    /// driving a browser in an unknown state is how a render silently collects
    /// the wrong page. Starting a fresh one is cheap next to that.
    clean: bool,
}

impl BrowserSession {
    /// Close the connection, then terminate the browser and remove its profile.
    ///
    /// Done explicitly rather than left to `Drop` so the socket is closed and
    /// the process is reaped in a known order, before a replacement starts.
    async fn shutdown(mut self) {
        self.client.close().await;
        self.browser.shutdown().await;
    }

    /// Terminate the browser without awaiting the socket close or the reap.
    ///
    /// The exit path's version of [`Self::shutdown`]. Closing the DevTools
    /// socket is an orderly courtesy to a process that is about to be killed
    /// anyway, and awaiting it is the one thing that path cannot do.
    fn terminate_now(mut self) {
        self.browser.terminate_now();
    }
}

impl ChromiumRenderer {
    /// Discover a browser and build a renderer, or `None` if there is none.
    /// Discover a browser and build a renderer around it.
    ///
    /// The `http` argument is kept for compatibility with the constructor the
    /// application's composition root used to pass; the target-list client is
    /// now built internally, with the permissive policy, because the DevTools
    /// endpoint lives on loopback and the shipped policy refuses loopback for
    /// content. Passing a client here no longer decides what the renderer talks
    /// to the browser with.
    pub fn discover(http: Arc<dyn HttpClient>) -> Option<Self> {
        let _ = http;
        let executable = discover_browser()?;
        Self::with_executable(executable, devtools_client()).ok()
    }

    /// Build a renderer around a specific executable.
    pub fn with_executable(
        executable: impl Into<PathBuf>,
        http: Arc<dyn HttpClient>,
    ) -> AppResult<Self> {
        let executable = executable.into();
        if !executable.is_file() {
            return Err(
                AppError::browser_unavailable("the configured browser does not exist")
                    .with_technical_detail(executable.to_string_lossy().into_owned()),
            );
        }
        // A renderer is built once, at startup, which makes this the natural
        // place to clear profiles an earlier run could not.
        sweep_stale_profiles(STALE_PROFILE_AGE);
        Ok(Self::around(
            executable.clone(),
            Arc::new(Browsers::new(executable)),
            http,
        ))
    }

    /// Build a renderer around a launcher the caller supplies.
    ///
    /// How the lifecycle tests drive the real session code with a browser that
    /// is a loopback CDP server rather than a process.
    fn around(
        executable: impl Into<PathBuf>,
        launcher: Arc<dyn BrowserLauncher>,
        http: Arc<dyn HttpClient>,
    ) -> Self {
        Self {
            executable: executable.into(),
            launcher,
            http,
            slot: Arc::new(SessionSlot::new()),
            auth_profile: None,
        }
    }

    /// Point the renderer at the application's persistent browser profile.
    ///
    /// Separate from the constructors because it is the composition root that
    /// knows the data directory, and because a renderer without one is still a
    /// perfectly good anonymous renderer.
    pub fn with_auth_profile(mut self, dir: impl Into<PathBuf>) -> Self {
        self.auth_profile = Some(dir.into());
        self
    }

    /// The persistent profile, or an error saying why there is none.
    ///
    /// Deliberately not a fallback to the throwaway profile: a caller asking
    /// for the signed-in profile and silently getting an anonymous one would
    /// see an empty listing and conclude the account had no posts.
    fn persistent_profile(&self) -> AppResult<PathBuf> {
        self.auth_profile.clone().ok_or_else(|| {
            AppError::browser_unavailable("this build has no sign-in profile configured")
                .with_user_message(
                    "The saved sign-in is not available in this build, so profiles are loaded \
                     as an anonymous visitor.",
                )
        })
    }

    /// The executable this renderer will launch.
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// How many browsers this renderer has started.
    ///
    /// One for a whole session's worth of work is the point of the shared
    /// session; a test asserts it rather than trusting the design.
    pub fn launches(&self) -> usize {
        self.slot.launches.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether a browser is currently being kept alive.
    pub fn has_session(&self) -> bool {
        self.slot
            .session
            .try_lock()
            .map(|guard| guard.is_some())
            .unwrap_or(true)
    }

    /// The process id of the browser this renderer owns, if it has one.
    ///
    /// The application owns exactly one browser and this is which one, so
    /// anything that needs to act on it — a diagnostic, a test that simulates a
    /// crash — can name it instead of reaching for every `chrome.exe` on the
    /// machine.
    pub fn session_pid(&self) -> Option<u32> {
        self.slot
            .session
            .try_lock()
            .ok()
            .and_then(|guard| guard.as_ref().and_then(|session| session.browser.pid()))
    }

    /// The throwaway profile of the browser this renderer owns, if it has one.
    ///
    /// Diagnostics: "that browser and its profile are gone" is a statement about
    /// the browser this application started, and this is how something outside
    /// it can name that browser rather than every one on the machine.
    pub fn session_profile(&self) -> Option<std::path::PathBuf> {
        self.slot.session.try_lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .and_then(|session| session.browser.profile().map(Path::to_path_buf))
        })
    }

    /// Start a browser for `visibility` and connect to its page.
    async fn start_session(
        &self,
        visibility: BrowserVisibility,
        which: BrowserProfile,
    ) -> AppResult<BrowserSession> {
        // The profile decides where the browser keeps its cookies, and the two
        // answers are not close to each other: one is a directory made for this
        // browser and thrown away with it, the other is the application's own
        // and holds a sign-in that has to survive everything.
        let mut profile_lock = None;
        let profile = match which {
            BrowserProfile::Ephemeral => temp_profile_dir()?,
            BrowserProfile::Persistent => {
                let dir = self.persistent_profile()?;
                // The lock lives exactly as long as the browser does: it is
                // moved into the session below and dropped when the session is,
                // so there is no path that closes the browser and forgets the
                // lock, or releases the lock while the browser is still up.
                profile_lock = Some(ProfileLock::acquire(&dir)?);
                dir
            }
        };
        let mut browser = self.launcher.launch(&profile, visibility, which).await?;

        // Everything after the launch has to clean up after itself: a failure
        // here would otherwise leave a browser running with nothing holding it.
        let setup = async {
            let port = browser.devtools_port(STARTUP_TIMEOUT).await?;
            let endpoint = self.page_endpoint(port).await?;
            cdp::CdpClient::connect(&endpoint, COMMAND_TIMEOUT).await
        }
        .await;

        match setup {
            Ok(client) => {
                self.slot
                    .launches
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.slot.note_use();
                // A browser is kept for the next render, not forever. The first
                // one starts the watcher — one task, for the renderer's life,
                // which closes whatever browser is idle when its turn comes.
                self.watch_for_idle();
                tracing::info!(
                    target: "browser",
                    executable = %self.executable.display(),
                    visibility = ?visibility,
                    profile = ?which,
                    "browser session started"
                );
                Ok(BrowserSession {
                    browser,
                    client,
                    visibility,
                    profile: which,
                    interactive: false,
                    _profile_lock: profile_lock,
                    clean: true,
                })
            }
            Err(err) => {
                // A browser that started and could not be driven is closed
                // again. The reason is logged because the caller only sees that
                // the session failed, and a launch that fails on every attempt
                // with no record of why is indistinguishable from a browser that
                // is simply not installed.
                tracing::warn!(
                    target: "browser",
                    profile = ?which,
                    visibility = ?visibility,
                    error = %err,
                    "the browser started but could not be driven; closing it"
                );
                browser.shutdown().await;
                Err(err)
            }
        }
    }

    /// Start the idle watcher, once.
    ///
    /// Started with the first browser rather than at construction because the
    /// renderer is not behind an `Arc` at that point, and never started twice
    /// because two watchers would race each other to close the same browser.
    fn watch_for_idle(&self) {
        if self
            .slot
            .watching
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let slot = Arc::downgrade(&self.slot);
        tokio::spawn(SessionSlot::watch_for_idle(slot));
    }

    /// How long a browser may sit unused before it is closed.
    ///
    /// Exposed so a test can shorten it; the value is read at each tick rather
    /// than captured, so changing it affects the watcher already running.
    pub fn set_idle_timeout(&self, timeout: Duration) {
        self.slot.idle_timeout_ms.store(
            timeout.as_millis() as u64,
            std::sync::atomic::Ordering::SeqCst,
        );
    }

    /// Close the browser now, without awaiting anything.
    ///
    /// For the application's exit path. Rust would close the browser when the
    /// renderer is dropped, but that only happens if the process leaves through
    /// a path that runs destructors — and the updater's restart does not, since
    /// it exits the process directly. This gives the exit path something it can
    /// call that does not depend on Drop being reached.
    ///
    /// Non-blocking on purpose: if a render holds the session then the browser
    /// is in use and killing it here would pull the page out from under that
    /// render. The exit path runs once, and the runtime is already going away.
    pub fn shutdown_now(&self) {
        self.slot
            .stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let Ok(mut guard) = self.slot.session.try_lock() else {
            tracing::warn!(
                target: "browser",
                "the application is exiting while a render holds the browser; \
                 terminating it through the process handle instead"
            );
            return;
        };
        if let Some(session) = guard.take() {
            session.terminate_now();
        }
    }

    async fn render_inner(&self, request: &RenderRequest<'_>) -> AppResult<RenderedPage> {
        // The browser is the other way this application makes an outbound
        // request, so the navigation URL is held to the same destination policy
        // the HTTP client applies.
        //
        // This is the *first* of two layers, not the only one. Checking the URL
        // bounds where a render can be pointed; it says nothing about where the
        // page then goes, because a page can redirect itself and its scripts and
        // subresources can fetch anything. That half is closed by the network
        // guard every launched browser is routed through — see
        // [`proxy::GuardedProxy`], which applies this same policy and this same
        // resolver to each destination the browser dials, so a rebinding redirect
        // or a subresource aimed at loopback is refused before a socket opens.
        crate::network::policy::ensure_allowed(request.url)?;

        // One render at a time. This is also what makes concurrent requests
        // share a browser instead of racing to start their own.
        let _permit = self.slot.gate.acquire().await.map_err(|_| {
            AppError::browser_unavailable("the browser was shut down before rendering started")
        })?;

        let mut guard = self.slot.session.lock().await;

        // Take the browser already here when it is one this render may drive. A
        // session left dirty by an interrupted render, or of the wrong kind, is
        // retired now, so every path below reasons about a browser this render
        // can actually use.
        let reused = guard.as_ref().is_some_and(|session| {
            session_is_reusable(
                session.visibility,
                session.profile,
                session.clean,
                request.visibility,
                request.profile,
            )
        });
        if !reused {
            if let Some(previous) = guard.take() {
                previous.shutdown().await;
            }
        }

        // A browser kept from an earlier render may have died — it crashed, or
        // something outside this application closed it. That is not the caller's
        // problem, so when such a session fails it is replaced, and the page is
        // given one more look on a browser this application just started. Only
        // ever once: a second browser that fails the same page is reporting the
        // page, not the browser.
        let mut may_replace = reused;

        // How many times the page may be looked at on the browser currently
        // held. Reset whenever a new browser is started, because a new browser
        // is a new chance at the page.
        let mut looks_left = RENDER_ATTEMPTS;

        loop {
            if guard.is_none() {
                *guard = Some(
                    self.start_session(request.visibility, request.profile)
                        .await?,
                );
                // Each browser gets its own budget of looks at the page.
                looks_left = RENDER_ATTEMPTS;
            }

            if let Some(session) = guard.as_mut() {
                session.clean = false;
            }
            let session = guard.as_mut().expect("a session was started above");
            match self.render_in_session(session, request).await {
                Ok(page) => {
                    if let Some(session) = guard.as_mut() {
                        session.clean = true;
                    }
                    return Ok(page);
                }
                Err(err) => {
                    looks_left = looks_left.saturating_sub(1);

                    // A page that did not become ready is a statement about the
                    // page, not about the browser: the URL may have been served
                    // a challenge or a half-built shell, while the browser itself
                    // is healthy and its command stream is in sync. Looking again
                    // costs one page load; retiring the browser would cost a
                    // whole browser start to re-open the same page — a second
                    // browser for a page that was merely slow is exactly the
                    // waste the shared session exists to avoid.
                    if err.is_retryable() && looks_left > 0 {
                        // A page that was half-built when it was read often
                        // finishes on its own, and looking again at the same
                        // instant spends the second look on the same moment
                        // that just failed. The pause grows with the attempt
                        // and is bounded, so it cannot outlast the render's own
                        // budget — the call that wraps this is still what ends
                        // it.
                        let attempt = RENDER_ATTEMPTS - looks_left;
                        let backoff = RETRY_BACKOFF * attempt as u32;
                        tracing::info!(
                            target: "browser",
                            url = request.url,
                            error = %err.message(),
                            backoff_ms = backoff.as_millis() as u64,
                            "the page was not usable; looking at it again on the same browser"
                        );
                        tokio::time::sleep(backoff).await;
                        continue;
                    }

                    // Anything else — a browser that would not answer, a
                    // navigation that never returned, a page that did not expose
                    // what was asked of it — leaves the session in a state nobody
                    // can vouch for, so it is retired rather than driven again.
                    if let Some(broken) = guard.take() {
                        broken.shutdown().await;
                    }
                    if may_replace {
                        may_replace = false;
                        tracing::warn!(
                            target: "browser",
                            error = %err.message(),
                            "the reused browser session failed; starting a new one"
                        );
                        continue;
                    }
                    tracing::warn!(
                        target: "browser",
                        error = %err.message(),
                        "discarding the browser session after a failed render"
                    );
                    return Err(err);
                }
            }
        }
    }

    /// Drive the session's browser through one render.
    async fn render_in_session(
        &self,
        session: &mut BrowserSession,
        request: &RenderRequest<'_>,
    ) -> AppResult<RenderedPage> {
        let client = &mut session.client;
        client.enable_page(COMMAND_TIMEOUT).await?;
        client.enable_runtime(COMMAND_TIMEOUT).await?;

        // Watching the network is how a second track is found: a player that
        // streams separate renditions often names only one of them in its
        // configuration and fetches the rest itself. A failure here costs
        // that discovery but not the render, so it is recorded rather than
        // propagated — the caller is told the network was not watched, and
        // reports what it found as unknown rather than absent.
        let network_enabled = match client.enable_network(COMMAND_TIMEOUT).await {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(
                    target: "browser",
                    error = %err.message(),
                    "network capture unavailable for this render"
                );
                false
            }
        };
        // Anything buffered so far belongs to the previous page, not the one
        // about to be opened.
        client.clear_events();

        client.navigate(request.url, PAGE_TIMEOUT).await?;

        wait_until_ready(client, request).await?;
        settle(client, request).await;

        let collected = client
            .evaluate(request.collect, PAGE_TIMEOUT)
            .await?
            .ok_or_else(|| {
                AppError::resolution("the page's player did not expose any media")
                    .with_user_message(
                        "The page loaded but did not make its media available in a form \
                         this app can read.",
                    )
                    .with_technical_detail("collection script returned nothing")
            })?;

        let media = parse_media_sources(&collected);
        let final_url = client
            .current_url(PAGE_TIMEOUT)
            .await?
            .unwrap_or_else(|| request.url.to_string());
        let title = client
            .evaluate("document.title || null", PAGE_TIMEOUT)
            .await?
            .and_then(|value| value.as_str().map(str::to_string))
            .filter(|title| !title.trim().is_empty());

        // Drained last, so everything the page fetched while it was being
        // driven — including the requests that only happen once playback
        // starts — is in the buffer.
        let events = client.take_events();
        let dropped = client.dropped_events();
        if dropped > 0 {
            tracing::debug!(target: "browser", dropped, "network event buffer overflowed");
        }
        let network_media = if network_enabled {
            network_media_from_events(&events)
        } else {
            Vec::new()
        };

        tracing::info!(
            target: "browser",
            url = %crate::logging::redact_url(&final_url),
            player_sources = media.len(),
            network_media = network_media.len(),
            network_captured = network_enabled,
            "render completed"
        );

        Ok(RenderedPage {
            final_url,
            title,
            media,
            network_media,
            network_captured: network_enabled,
            // The script's own result, kept alongside the normalized view of
            // it: a platform may ask the page for more than its media, and
            // interpreting that is the resolver's job, not this module's.
            payload: Some(collected),
        })
    }

    /// Find the page target's DevTools WebSocket endpoint.
    async fn page_endpoint(&self, port: u16) -> AppResult<String> {
        let url = format!("http://127.0.0.1:{port}/json/list");
        let body = self
            .http
            .fetch_text(&FetchRequest::new(&url))
            .await
            .map_err(|err| {
                AppError::browser_unavailable("could not read the browser's target list")
                    .with_technical_detail(err.message().to_string())
            })?;

        let targets: Vec<serde_json::Value> = serde_json::from_str(&body).map_err(|err| {
            AppError::browser_unavailable("the browser's target list was not readable")
                .with_technical_detail(err.to_string())
        })?;

        targets
            .iter()
            .find(|target| target.get("type").and_then(serde_json::Value::as_str) == Some("page"))
            .and_then(|target| target.get("webSocketDebuggerUrl"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| AppError::browser_unavailable("the browser exposed no page to drive"))
    }
}

#[async_trait]
impl PageRenderer for ChromiumRenderer {
    fn name(&self) -> &'static str {
        "chromium"
    }

    fn is_available(&self) -> bool {
        self.executable.is_file()
    }

    async fn render(&self, request: &RenderRequest<'_>) -> AppResult<RenderedPage> {
        // The budget covers everything: start-up, the wait for the page, and
        // teardown. Retrying a page that did not become ready is part of
        // `render_inner`, where the session that owns the browser makes the
        // decision — a retry there re-uses the browser it already has rather than
        // paying for another start-up.
        let outcome = match tokio::time::timeout(request.total_timeout, self.render_inner(request))
            .await
        {
            Ok(outcome) => outcome,
            Err(_) => {
                // The deadline dropped `render_inner` mid-flight, so whatever
                // the browser was doing was abandoned. Dropping the future
                // released the session lock, which makes this the moment the
                // browser can be closed cleanly — much better than leaving a
                // process nobody is driving for the idle timer to find.
                let mut guard = self.slot.session.lock().await;
                if let Some(abandoned) = guard.take() {
                    tracing::warn!(
                        target: "browser",
                        url = request.url,
                        "closing the browser after the render ran out of time"
                    );
                    abandoned.shutdown().await;
                }
                drop(guard);

                Err(
                    AppError::browser_unavailable("the browser took too long to render")
                        .with_user_message("Rendering this page took too long, so it was stopped.")
                        .with_technical_detail(format!(
                            "total_timeout={}ms",
                            request.total_timeout.as_millis()
                        )),
                )
            }
        };

        // Whether it worked or not, the browser was just used. This is what the
        // idle watcher measures against: a long download that keeps resolving,
        // page by page, must not have its browser closed underneath it.
        self.slot.note_use();
        outcome
    }

    /// Close the browser now, without awaiting.
    fn shutdown_now(&self) {
        ChromiumRenderer::shutdown_now(self);
    }
}

/// Poll the page until its readiness predicate is truthy.
async fn wait_until_ready(
    client: &mut cdp::CdpClient,
    request: &RenderRequest<'_>,
) -> AppResult<()> {
    let deadline = Instant::now() + request.ready_timeout;
    let mut polls: u32 = 0;
    loop {
        if let Some(value) = client.evaluate(request.ready_when, PAGE_TIMEOUT).await? {
            if is_truthy(&value) {
                return Ok(());
            }
        }

        // Every couple of seconds, ask where the browser actually is.
        //
        // Measured: Douyin answers a link it will not serve by sending the
        // browser to its front page. The page that arrives is a perfectly
        // healthy page, so every predicate about a post waits for something that
        // is never going to appear, and the caller is told "the page never
        // became ready" a full timeout later — which describes the symptom and
        // hides the cause, and in the live suite looked exactly like an
        // application failure. Noticed here, the same event is answered in about
        // two seconds and says what actually happened.
        polls += 1;
        if polls % LANDING_CHECK_EVERY == 0 {
            if let Some(err) = landed_somewhere_else(client, request.url).await? {
                return Err(err);
            }
        }

        if Instant::now() >= deadline {
            // Say what the page actually was. A ready-timeout on a page that
            // served a challenge looks identical to one on a page that merely
            // loaded slowly, and the two call for different responses.
            let diagnostic = describe_page(client).await;
            tracing::warn!(
                target: "browser",
                url = request.url,
                diagnostic = %diagnostic,
                "the page never became ready"
            );
            return Err(AppError::browser_unavailable(
                "the page never became ready in the browser",
            )
            .with_user_message("The page did not finish loading in the browser in time.")
            // A page that served a challenge or a half-built shell on the first
            // visit may serve the real thing on the next one.
            .with_retryable(true)
            .with_technical_detail(format!(
                "ready_timeout={}ms predicate={} | {diagnostic}",
                request.ready_timeout.as_millis(),
                request.ready_when
            )));
        }
        tokio::time::sleep(READY_POLL_INTERVAL).await;
    }
}

/// A marker that survives in an error's technical detail, so a caller — a test,
/// or a resolver deciding whether to try again — can tell *this* answer apart
/// from any other without matching on prose.
pub const SOURCE_REDIRECT_MARKER: &str = "source-sent-elsewhere";

/// Whether a failure was the source answering with a page it was not asked for.
///
/// Not retryable, and deliberately so: if Douyin will not serve this link to
/// this visitor, asking again gives the same answer, and a retry loop that kept
/// asking is exactly what must not happen.
pub fn is_source_redirect(err: &AppError) -> bool {
    err.technical_detail()
        .map(|detail| detail.contains(SOURCE_REDIRECT_MARKER))
        .unwrap_or(false)
}

/// The browser's current address, when it is somewhere unrelated to the request.
async fn landed_somewhere_else(
    client: &mut cdp::CdpClient,
    requested: &str,
) -> AppResult<Option<AppError>> {
    let Some(value) = client
        .evaluate("String(location.href)", PAGE_TIMEOUT)
        .await?
    else {
        return Ok(None);
    };
    let Some(landed) = value.as_str() else {
        return Ok(None);
    };
    let Some(reason) = unrelated_landing(requested, landed) else {
        return Ok(None);
    };

    // Host and path only, through the same redaction the rest of the browser
    // layer uses: a landing URL can carry a query.
    let requested = crate::logging::redact_url(requested);
    let landed = crate::logging::redact_url(landed);
    tracing::warn!(
        target: "browser",
        requested = %requested,
        landed = %landed,
        reason,
        "the source sent this link to a different page"
    );
    Ok(Some(
        AppError::unavailable("the source redirected this link to another page")
            .with_user_message(
                "Douyin answered this link with its own front page instead of the post. \
                 That is the source's answer about the link, not a fault in the application, \
                 and asking for the same link again is unlikely to change it.",
            )
            .with_technical_detail(format!(
                "{SOURCE_REDIRECT_MARKER} requested={requested} landed={landed} reason={reason}"
            )),
    ))
}

/// Whether a landing page has nothing to do with what was asked for.
///
/// Deliberately narrow, because a wrong answer here would fail a page that was
/// fine. It fires only when the browser is on Douyin's own front page — or on a
/// page that says in so many words that the link is a post the source no longer
/// has — *and* something else was asked for. A short link redirecting to the
/// post it names is the ordinary case and is not this.
fn unrelated_landing(requested: &str, landed: &str) -> Option<&'static str> {
    let landed = url::Url::parse(landed).ok()?;
    let host = landed.host_str()?.to_ascii_lowercase();
    if !host.ends_with("douyin.com") {
        return None;
    }

    // The source naming the reason itself, in the redirect it made.
    if landed
        .query()
        .map(|query| query.contains("previous_page=web_video_404_link"))
        .unwrap_or(false)
    {
        return Some("the source marked the link as a post it no longer serves");
    }

    let reached = landed.path().trim_end_matches('/');
    let front_page = matches!(reached, "/jingxuan" | "/discover" | "/recommend" | "/login");
    if !front_page {
        return None;
    }

    // Only when somewhere else was asked for. A request for the front page that
    // arrives at the front page is not a redirection.
    let requested = url::Url::parse(requested).ok()?;
    let asked_for = requested.path().trim_end_matches('/');
    if asked_for.is_empty() || asked_for == reached {
        return None;
    }

    Some("the source answered with its front page instead of the requested content")
}

/// Summarize what the page turned out to be.
///
/// Bounded and structural: it reports which landmarks exist, never the page's
/// content, so nothing from a page can end up quoted in a log line.
async fn describe_page(client: &mut cdp::CdpClient) -> String {
    const PROBE: &str = concat!(
        "(function(){try{",
        "var w=window,d=document;",
        "var v=d.querySelector('video');",
        "return JSON.stringify({",
        "href:String(location.href).slice(0,120),",
        "titleChars:d.title?d.title.length:0,",
        "player:typeof w.player,",
        "playerConfig:(w.player&&w.player.config)?Object.keys(w.player.config).length:-1,",
        "routerData:!!w._ROUTER_DATA,",
        "renderData:!!w.SSR_RENDER_DATA,",
        "videos:d.querySelectorAll('video').length,",
        "videoWidth:v?v.videoWidth:-1,",
        "bodyChars:d.body?d.body.innerHTML.length:-1",
        "});}catch(e){return 'probe failed: '+e}})()"
    );

    match client.evaluate(PROBE, PAGE_TIMEOUT).await {
        Ok(Some(serde_json::Value::String(text))) => text,
        Ok(Some(other)) => other.to_string(),
        Ok(None) => "no diagnostic available".to_string(),
        Err(err) => format!("diagnostic failed: {}", err.message()),
    }
}

/// Give the page a bounded moment to enrich itself once it is already usable.
///
/// Best-effort by design: facts this step is waiting for are ones the render can
/// report as absent, so a page that never supplies them must not fail the render
/// — it just yields less metadata.
async fn settle(client: &mut cdp::CdpClient, request: &RenderRequest<'_>) {
    let Some(predicate) = request.settle_when else {
        return;
    };

    let deadline = Instant::now() + request.settle_timeout;
    while Instant::now() < deadline {
        match client.evaluate(predicate, PAGE_TIMEOUT).await {
            Ok(Some(value)) if is_truthy(&value) => return,
            // A transport problem here is not worth failing the render over.
            Err(_) => return,
            Ok(_) => {}
        }
        tokio::time::sleep(READY_POLL_INTERVAL).await;
    }
}

/// Whether a predicate's value means "yes".
///
/// Anything non-empty counts, so a predicate may return the thing it found as
/// well as `true`.
fn is_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(flag) => *flag,
        serde_json::Value::Null => false,
        serde_json::Value::String(text) => !text.is_empty(),
        serde_json::Value::Number(number) => number.as_f64().map(|n| n != 0.0).unwrap_or(false),
        serde_json::Value::Array(items) => !items.is_empty(),
        serde_json::Value::Object(_) => true,
    }
}

/// Whether a kept browser session can serve another render.
///
/// Pure, so the rule is stated once and asserted without a browser. A session is
/// reused only when it is the kind of browser this render needs *and* the last
/// render on it ran to completion. Anything else — a cancelled render, a
/// failure, a different kind of browser — means the session is replaced, because
/// the alternative is driving a browser whose page and command stream are in a
/// state nobody can vouch for.
fn session_is_reusable(
    session_visibility: BrowserVisibility,
    session_profile: BrowserProfile,
    session_clean: bool,
    wanted: BrowserVisibility,
    wanted_profile: BrowserProfile,
) -> bool {
    // A browser is "ready and a window" or "ready and no window"; it is also
    // "signed out" or "signed in". Substituting one for the other is not a
    // smaller or larger version of the same thing — an anonymous render served
    // from the signed-in profile would quietly become an authenticated one, and
    // an authenticated render served from a throwaway profile would come back
    // empty with no way to tell why.
    session_clean && session_visibility == wanted && session_profile == wanted_profile
}

/// Environment variable that puts the managed browser window back on screen.
pub const WINDOW_VISIBLE_ENV_VAR: &str = "DOUYIN_DOWNLOADER_BROWSER_WINDOW";

/// Whether the windowed browser should be placed on screen rather than off it.
///
/// Off by default: the window exists because the site requires one, not because
/// the user asked to watch it. The override exists so that a machine where the
/// off-screen position behaves differently can be put back without a rebuild.
fn window_placement_is_visible() -> bool {
    matches!(
        std::env::var(WINDOW_VISIBLE_ENV_VAR).as_deref(),
        Ok("1") | Ok("visible") | Ok("true") | Ok("yes")
    )
}

/// Create a fresh, empty profile directory for one render.
/// What the application can say about a saved Douyin sign-in.
///
/// Answered from the page Douyin actually serves the browser, never from a flag
/// the application wrote down and never from the presence of a cookie. A cookie
/// says what the browser is *holding*; only the site can say whether it is still
/// honouring it, and the difference is exactly the one a user notices — a
/// profile that comes back empty while the screen insists they are signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LoginStatus {
    /// The page showed explicit evidence that it recognises this visitor.
    SignedIn,
    /// The page asked for a sign-in, or said the session is no longer good.
    SignedOut,
    /// The page could not be read well enough to say either way.
    ///
    /// A third answer, not a hedge, and deliberately not folded into
    /// [`Self::SignedOut`]. A timeout, a redirect, a page that did not finish
    /// rendering, and a page that rendered without saying anything are all "not
    /// knowable from here" — and reporting any of them as signed out would tell
    /// the user to sign in again when nothing was wrong with their session.
    Unknown,
}

/// The public Douyin page the sign-in flow starts from, and the page the
/// session check reads.
const DOUYIN_HOME_URL: &str = "https://www.douyin.com/";

/// JavaScript predicate: the home page has rendered something worth reading.
///
/// Deliberately weak — a body with real text in it — because the check is not
/// waiting for the page to be *finished*, only for there to be a page to read.
/// A predicate that demanded more would turn a slow load into a verdict, and the
/// verdict it would produce is the one thing this must not invent.
const SESSION_READY: &str = concat!(
    "(function(){try{",
    "var b=document.body?document.body.innerText:'';",
    "return b.length>200;}catch(e){return false}})()"
);

/// JavaScript: report what the page says about *this visitor's* session.
///
/// Every signal is scoped to the site's own header, which is where Douyin
/// speaks for the viewer. That scoping is the whole of the safety here: the home
/// feed is full of other people's avatars, so an avatar anywhere on the page
/// proves nothing about who is signed in, while one in the top bar is the site
/// saying "this is you".
///
/// Positive and negative evidence are reported separately and an absence is
/// never allowed to become a verdict. "There is no sign-in prompt" is what a
/// failed render, a redirect and a slow page all look like too.
pub const PROBE_SESSION: &str = r#"(function(){
  try {
    var d = document, body = d.body ? (d.body.innerText || '') : '';
    var header = d.querySelector('header');
    var out = {
      href: location.href,
      bodyChars: body.length,
      headerFound: !!header,
      signals: {}
    };

    // Where the probe looked, and what it found there. Counts rather than
    // booleans so that a page which changes its markup is diagnosable from a
    // log instead of only from a wrong answer.
    out.candidates = {
      userAvatar: header ? header.querySelectorAll('[data-e2e="user-avatar"]').length : -1,
      avatarImage: header ? header.querySelectorAll('img[src*="aweme-avatar"]').length : -1,
      loginButton: header ? header.querySelectorAll('[data-e2e="login-button"]').length : -1,
      userLink: header ? header.querySelectorAll('a[href*="/user/"]').length : -1
    };

    // The site naming the visitor: an avatar the top bar marks as the viewer's.
    out.signals.avatar = out.candidates.userAvatar > 0 || out.candidates.avatarImage > 0;
    // The site still offering a way in, which is a thing it only does for a
    // visitor it is not currently treating as signed in.
    out.signals.loginButton = out.candidates.loginButton > 0;

    // The site's own words, in both directions. Read from the page's text
    // rather than guessed at from the markup.
    var prompt = body.match(
      /登录后查看更多|登录后查看|请先登录|登录后可查看/
    );
    var rejected = body.match(
      /登录已过期|重新登录|账号异常/
    );
    out.signals.loginPrompt = !!prompt;
    out.signals.sessionRejected = !!rejected;
    out.wording = prompt ? prompt[0] : (rejected ? rejected[0] : null);

    return JSON.stringify(out);
  } catch (e) {
    return JSON.stringify({ error: String(e) });
  }
})()"#;

/// How long the sign-in navigation may take before it is called a failure.
const LOGIN_NAVIGATION_TIMEOUT: Duration = Duration::from_secs(45);

impl ChromiumRenderer {
    /// Whether this build knows where the persistent profile is.
    pub fn has_auth_profile(&self) -> bool {
        self.auth_profile.is_some()
    }

    /// Open the window the user signs in with, and leave it open.
    ///
    /// The window is an ordinary browser on Douyin's ordinary public page. This
    /// function navigates there and stops: it fills nothing in, attempts no
    /// challenge, and does not wait for the sign-in to finish. The user does
    /// that, in their own time, and says when they are done.
    ///
    /// The session is marked interactive, which is what keeps the idle timer
    /// from closing the window while somebody is reading it.
    pub async fn open_login_window(&self) -> AppResult<()> {
        let _permit = self.slot.gate.acquire().await.map_err(|_| {
            AppError::browser_unavailable("the browser was shut down before sign-in could start")
        })?;

        let mut guard = self.slot.session.lock().await;

        // Keep the window if it is already the one being signed in with;
        // replace anything else, because a sign-in window must not be a shared
        // anonymous browser with somebody else's page in it.
        let already_open = guard.as_ref().is_some_and(|session| {
            session.profile == BrowserProfile::Persistent
                && session.visibility == BrowserVisibility::Visible
        });
        if !already_open {
            if let Some(previous) = guard.take() {
                previous.shutdown().await;
            }
        }

        if guard.is_none() {
            *guard = Some(
                self.start_session(BrowserVisibility::Visible, BrowserProfile::Persistent)
                    .await?,
            );
        }

        let session = guard.as_mut().expect("a session was started above");
        if !already_open {
            session
                .client
                .navigate(DOUYIN_HOME_URL, LOGIN_NAVIGATION_TIMEOUT)
                .await?;
        }
        session.interactive = true;
        self.slot.note_use();

        tracing::info!(
            target: "browser",
            "the sign-in window is open; the user completes it there"
        );
        Ok(())
    }

    /// Ask the persistent profile what it is holding.
    ///
    /// Reuses a browser that already has the profile open — the window the user
    /// is signing in with — so that checking the status never pulls the sign-in
    /// page out from under them. Otherwise it opens a headless browser on the
    /// profile, asks, and closes it again; the profile itself is untouched
    /// either way.
    ///
    /// Deliberately never answered from a stored flag: a remembered "signed in"
    /// outlives the session it described, and would have the application promise
    /// something the browser can no longer honour.
    pub async fn login_status(&self) -> AppResult<LoginStatus> {
        let _permit = self.slot.gate.acquire().await.map_err(|_| {
            AppError::browser_unavailable("the browser was shut down before the check could run")
        })?;

        let mut guard = self.slot.session.lock().await;

        let already_open = guard
            .as_ref()
            .is_some_and(|session| session.profile == BrowserProfile::Persistent && session.clean);

        // A window the user is signing in with is theirs. Reading the page it
        // is already showing answers the question without touching it; taking
        // that same window and navigating it to the home page would throw away
        // a sign-in they were part-way through, which is the opposite of what
        // asking about the session should cost them.
        let signing_in = guard.as_ref().is_some_and(|session| {
            session.profile == BrowserProfile::Persistent
                && session.visibility == BrowserVisibility::Visible
        });

        if signing_in {
            let session = guard.as_mut().expect("checked immediately above");
            let status = match probe_open_page(&mut session.client).await {
                Ok(page) => session_status_from(page.as_ref()),
                Err(err) => {
                    tracing::warn!(
                        target: "browser",
                        error = %err.message(),
                        "the open sign-in window could not be read"
                    );
                    LoginStatus::Unknown
                }
            };
            tracing::info!(
                target: "browser",
                status = ?status,
                "read the sign-in state from the open sign-in window"
            );
            self.slot.note_use();
            return Ok(status);
        }

        let opened_for_this_check = if already_open {
            false
        } else {
            if let Some(previous) = guard.take() {
                previous.shutdown().await;
            }
            *guard = Some(
                self.start_session(BrowserVisibility::Windowed, BrowserProfile::Persistent)
                    .await?,
            );
            true
        };

        let session = guard.as_mut().expect("a session was started above");
        // Read the page Douyin serves this profile, rather than asking the
        // browser what it is holding. Only the site can say whether it still
        // honours the session, and it says so on the page it renders — served,
        // like every Douyin page this application reads, to an ordinary window.
        let request = RenderRequest::new(DOUYIN_HOME_URL, SESSION_READY, PROBE_SESSION)
            .in_a_window()
            .with_persistent_profile();
        let outcome = self.render_in_session(session, &request).await;

        let status = match outcome {
            Ok(rendered) => {
                let page = rendered.payload.as_ref().and_then(decode_json_object);
                let status = session_status_from(page.as_ref());
                tracing::info!(
                    target: "browser",
                    status = ?status,
                    page = %page.as_ref().map(describe_session_page).unwrap_or_else(|| "-".into()),
                    "read the sign-in state from the page"
                );
                status
            }
            Err(err) => {
                // A render that failed says nothing about the session. It is
                // reported as such rather than as a sign-out, because inventing
                // a sign-out would send the user to sign in again over a
                // network hiccup.
                tracing::warn!(
                    target: "browser",
                    error = %err.message(),
                    "the session check could not read the page"
                );
                LoginStatus::Unknown
            }
        };

        // A browser opened only to answer this question is closed again; the
        // profile stays on disk, which is the whole reason the question can be
        // asked again later. A browser the user is signing in with is left
        // exactly as it is — but a session whose render just failed is not
        // vouched for by anything, so it is retired either way.
        if opened_for_this_check || status == LoginStatus::Unknown {
            if let Some(spent) = guard.take() {
                spent.shutdown().await;
            }
        }
        self.slot.note_use();

        Ok(status)
    }

    /// Close the browser that has the persistent profile open.
    ///
    /// Called before the saved session is cleared, so that nothing is deleted
    /// while a browser still has the profile's files mapped. A browser on the
    /// throwaway profile is left alone: signing out is not a reason to interrupt
    /// a render that has nothing to do with the sign-in.
    pub async fn close_login_session(&self) -> AppResult<()> {
        let _permit = self.slot.gate.acquire().await.map_err(|_| {
            AppError::browser_unavailable("the browser was shut down before sign-out could finish")
        })?;

        let mut guard = self.slot.session.lock().await;
        if guard
            .as_ref()
            .is_some_and(|session| session.profile == BrowserProfile::Persistent)
        {
            if let Some(session) = guard.take() {
                session.shutdown().await;
            }
            tracing::info!(target: "browser", "the sign-in browser was closed");
        }
        Ok(())
    }
}

/// What the page said about the visitor's session.
///
/// Pure, and stated once rather than re-derived at the call site, so the rule
/// can be read and tested without a browser:
///
/// * the page named the visitor — an avatar in its own header — is signed in;
/// * the page asked for a sign-in, or said the session is no longer good, and
///   did not name the visitor, is signed out;
/// * anything else is unknown.
///
/// The order is the load-bearing part. Nothing is read until the page has
/// actually rendered, and rendered Douyin — a half-built page, a redirect and a
/// failed load are all "cannot tell", and a verdict drawn from one of them is
/// the one thing this must not invent. After that, positive evidence is checked
/// first, because it is the only evidence that stands on its own: a page can
/// carry a stale "sign in" control somewhere while still recognising the
/// visitor, and answering "signed out" there costs the user a sign-in they did
/// not need.
fn session_status_from(page: Option<&serde_json::Value>) -> LoginStatus {
    let Some(page) = page else {
        return LoginStatus::Unknown;
    };
    // A probe that threw reports only `error`, which is not evidence about a
    // session in either direction.
    if page.get("error").is_some() {
        return LoginStatus::Unknown;
    }

    // The page has to have actually rendered, and have rendered Douyin, before
    // anything on it means anything at all.
    let rendered = page
        .get("bodyChars")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|chars| chars > 200);
    let on_douyin = page
        .get("href")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|href| href.contains("douyin.com"));
    if !rendered || !on_douyin {
        return LoginStatus::Unknown;
    }

    let flag = |name: &str| {
        page.get("signals")
            .and_then(|signals| signals.get(name))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };
    if flag("avatar") {
        return LoginStatus::SignedIn;
    }

    if flag("loginPrompt") || flag("sessionRejected") || flag("loginButton") {
        return LoginStatus::SignedOut;
    }
    LoginStatus::Unknown
}

/// Ask the page that is already open what it says, without driving it.
///
/// No navigation, because the only page this is used on is one the user is
/// standing in front of. Enabling `Runtime` is what the render path does before
/// it evaluates anything, and doing it here keeps the two paths the same shape.
async fn probe_open_page(client: &mut cdp::CdpClient) -> AppResult<Option<serde_json::Value>> {
    client.enable_runtime(COMMAND_TIMEOUT).await?;
    let value = client.evaluate(PROBE_SESSION, PAGE_TIMEOUT).await?;
    Ok(value.as_ref().and_then(decode_json_object))
}

/// The probe's answer, whether the browser handed it back as text or as object.
fn decode_json_object(value: &serde_json::Value) -> Option<serde_json::Value> {
    match value {
        serde_json::Value::String(text) => serde_json::from_str(text).ok(),
        serde_json::Value::Object(_) => Some(value.clone()),
        _ => None,
    }
}

/// A one-line summary of what the probe saw, for the log.
///
/// Selector counts and the site's own wording — never a page's contents, and
/// nothing that could carry a credential.
fn describe_session_page(page: &serde_json::Value) -> String {
    let count = |path: &str| {
        page.pointer(path)
            .and_then(serde_json::Value::as_i64)
            .map(|value| value.to_string())
            .unwrap_or_else(|| "?".into())
    };
    format!(
        "header={} userAvatar={} avatarImage={} loginButton={} userLink={} wording={}",
        page.get("headerFound")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        count("/candidates/userAvatar"),
        count("/candidates/avatarImage"),
        count("/candidates/loginButton"),
        count("/candidates/userLink"),
        page.get("wording")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("-"),
    )
}

/// An exclusive claim on the persistent browser profile.
///
/// Two Chromium processes pointed at one user-data directory do not share it —
/// the second is either refused, or silently redirected to the first, which is
/// worse because it looks like it worked. The claim is therefore held for as
/// long as a browser has the profile open, and it is taken with a *deny-all
/// share mode* rather than a lock file recording a process id: the operating
/// system releases an open handle when the process ends, however it ends, so a
/// crash leaves no stale lock for the next launch to misread, and there is
/// nothing in the file for a reader to believe.
///
/// Nothing is ever written to the file. Its exclusivity is the whole of it.
#[derive(Debug)]
struct ProfileLock {
    /// The handle whose existence is the claim. Never read from.
    _handle: std::fs::File,
}

impl ProfileLock {
    /// The file inside the profile directory that carries the claim.
    const FILE_NAME: &'static str = "application.lock";

    fn acquire(dir: &Path) -> AppResult<Self> {
        std::fs::create_dir_all(dir).map_err(|err| {
            AppError::browser_unavailable("the sign-in profile could not be created")
                .with_technical_detail(format!("{}: {err}", dir.display()))
        })?;

        let path = dir.join(Self::FILE_NAME);
        let mut options = std::fs::OpenOptions::new();
        options.create(true).write(true).read(true);

        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // No sharing: while this handle is open no other process — and no
            // other part of this one — can open the file, so acquiring it is
            // the test of ownership rather than a formality after one.
            options.share_mode(0);
        }

        let handle = options.open(&path).map_err(|err| {
            AppError::browser_unavailable("the sign-in profile is already in use")
                .with_user_message(
                    "The saved sign-in is open in another window of this application. Close \
                     the other window and try again.",
                )
                .with_technical_detail(format!("{}: {err}", path.display()))
        })?;

        Ok(Self { _handle: handle })
    }
}

fn temp_profile_dir() -> AppResult<PathBuf> {
    let dir = std::env::temp_dir().join(format!("{PROFILE_PREFIX}{}", new_id()));
    std::fs::create_dir_all(&dir).map_err(|err| {
        AppError::browser_unavailable("could not create a temporary browser profile")
            .with_technical_detail(err.to_string())
    })?;
    Ok(dir)
}

/// Reclaim throwaway profiles left behind by an earlier run.
///
/// A profile is removed when the browser that owned it is torn down, but that
/// can fail: the files stay held for a while after the process is gone — an
/// antivirus scan of freshly written files is the usual reason — and no amount
/// of retrying *inside* the process that is finishing clears it. The directory
/// is in the system temp folder, so nothing breaks; but they accumulate, ~4 MB
/// each, and the next launch is the natural place to clear them.
///
/// Age-gated by [`STALE_PROFILE_AGE`], so a profile a running instance is using
/// is never touched. Best-effort throughout: a profile that will not go is left
/// for the next launch rather than failing anything.
fn sweep_stale_profiles(older_than: Duration) {
    sweep_profiles_in(&std::env::temp_dir(), older_than);
}

/// The sweep, over a directory the caller names.
///
/// Split out so it can be tested against a directory of the test's own: a sweep
/// of the real temp directory would be reaching into every other test's
/// profiles at the same time.
fn sweep_profiles_in(root: &Path, older_than: Duration) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };

    for entry in entries.filter_map(Result::ok) {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(PROFILE_PREFIX)
        {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        let old_enough = modified
            .elapsed()
            .map(|age| age >= older_than)
            .unwrap_or(false);
        if !old_enough {
            continue;
        }

        let path = entry.path();
        if std::fs::remove_dir_all(&path).is_ok() {
            tracing::info!(
                target: "browser",
                profile = %path.display(),
                "reclaimed an abandoned browser profile"
            );
        }
    }
}

/// Read the DevTools port the browser published in its profile directory.
async fn wait_for_devtools_port(profile: &Path, timeout: Duration) -> AppResult<u16> {
    let port_file = profile.join(DEVTOOLS_PORT_FILE);
    let deadline = Instant::now() + timeout;

    loop {
        if let Ok(contents) = std::fs::read_to_string(&port_file) {
            if let Some(port) = parse_devtools_active_port(&contents) {
                return Ok(port);
            }
        }
        if Instant::now() >= deadline {
            return Err(
                AppError::browser_unavailable("the browser did not start in time")
                    .with_user_message(
                        "A browser is needed to read this page, but it would not start.",
                    )
                    .with_technical_detail(format!(
                        "startup_timeout={}ms profile={}",
                        timeout.as_millis(),
                        profile.display()
                    )),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Parse the first line of a Chrome `DevToolsActivePort` file.
///
/// The file holds the listening port on the first line and the browser's
/// WebSocket path on the second. Only the port is needed, because the page's own
/// endpoint is discovered from the target list.
fn parse_devtools_active_port(contents: &str) -> Option<u16> {
    contents
        .lines()
        .next()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
}

/// Candidate browser executables, in preference order.
///
/// An explicit configuration wins; then Chrome (the more capable headless
/// implementation); then Edge, which is present on every supported Windows
/// install. Nothing here is specific to one machine or user.
pub fn browser_candidates(explicit: Option<PathBuf>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = explicit {
        candidates.push(path);
    }
    for variable in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(variable) {
            let base = PathBuf::from(base);
            candidates.push(base.join("Google/Chrome/Application/chrome.exe"));
        }
    }
    for variable in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(variable) {
            let base = PathBuf::from(base);
            candidates.push(base.join("Microsoft/Edge/Application/msedge.exe"));
        }
    }
    // A browser on PATH, for installs that are not in the usual places.
    for name in ["chrome", "chromium", "google-chrome", "msedge"] {
        if let Some(path) = find_on_path(name) {
            candidates.push(path);
        }
    }
    candidates
}

/// The first candidate that exists on disk.
pub fn discover_browser() -> Option<PathBuf> {
    let explicit = std::env::var_os(BROWSER_ENV_VAR)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty());

    browser_candidates(explicit)
        .into_iter()
        .find(|candidate| candidate.is_file())
}

/// Look for an executable on `PATH`.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".EXE;.CMD;.BAT".to_string())
        .split(';')
        .map(|ext| ext.to_ascii_lowercase())
        .collect();

    for directory in std::env::split_paths(&path) {
        let direct = directory.join(name);
        if direct.is_file() {
            return Some(direct);
        }
        for extension in &extensions {
            let candidate = directory.join(format!("{name}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// A launched browser process and its temporary profile.
///
/// The guard is what makes cleanup unconditional: it terminates the process and
/// removes the profile when dropped, which covers the failure, timeout, and
/// dropped-future paths as well as the happy one.
struct BrowserProcess {
    child: tokio::process::Child,
    pid: Option<u32>,
    profile: PathBuf,
    /// What the browser was given, which decides whether the profile is the
    /// application's to delete. A throwaway profile was made for this browser
    /// and goes with it; the sign-in profile outlives every browser that opens
    /// it.
    kind: BrowserProfile,
}

impl BrowserProcess {
    fn launch(
        executable: &Path,
        profile: &Path,
        visibility: BrowserVisibility,
        kind: BrowserProfile,
        proxy_port: Option<u16>,
    ) -> AppResult<Self> {
        let mut command = tokio::process::Command::new(executable);
        // Chrome and Edge are GUI programs, so they allocate no console of
        // their own — but the flag costs nothing and keeps every launch in this
        // application configured the same way, rather than leaving the next
        // person to work out which sites were considered.
        command.no_console_window();
        // Whether there is a window is the one thing this method varies: some
        // public pages serve their content to an ordinary window and refuse the
        // same request from a headless browser. Nothing else about the launch
        // changes, so no page can tell the two apart by anything the
        // application does beyond the absence of `--headless`.
        match visibility {
            BrowserVisibility::Headless => {
                command.arg("--headless=new");
            }
            BrowserVisibility::Windowed => {
                // A real window, placed off the visible desktop by default. It
                // has to be a real window — that is what the site serves — but
                // there is no reason for it to be in the user's way, and it is
                // not hidden from the page in any way: same window, same size,
                // same everything, one screen over.
                //
                // Chrome ignores `--start-minimized`, so minimizing is not an
                // option that actually works; this is. (Verified by asking the
                // browser for its own window state: `--start-minimized` reports
                // `normal`, whereas this position is honoured.)
                //
                // `DOUYIN_DOWNLOADER_BROWSER_WINDOW=visible` puts it back on
                // screen, for a machine or a page where the off-screen position
                // turns out to matter.
                if window_placement_is_visible() {
                    tracing::info!(
                        target: "browser",
                        "placing the browser window on screen, as configured"
                    );
                } else {
                    command.arg("--window-position=-32000,-32000");
                }
            }
            BrowserVisibility::Visible => {
                // The sign-in window. Nothing is set here: the default placement
                // is on screen, which is the entire point — the user has to be
                // able to see the page and type into it. It is an ordinary
                // browser window in every other respect, on the ordinary public
                // page, and this application never types anything into it.
            }
        }
        command
            .arg("--disable-gpu")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-extensions")
            .arg("--disable-background-networking")
            .arg("--disable-default-apps")
            .arg("--disable-sync")
            .arg("--disable-component-update")
            .arg("--mute-audio");

        // Every request the browser makes goes through this application's guard,
        // which applies the same destination policy and the same resolver the
        // HTTP client uses. Chromium would otherwise resolve DNS with its own
        // stack, follow its own redirects, and fetch its own subresources with
        // nothing checking where any of it went.
        //
        // `<-loopback>` removes Chromium's *implicit* bypass for loopback and
        // link-local destinations. Without it, the one class this exists to
        // refuse would never reach the proxy at all.
        if let Some(port) = proxy_port {
            command
                .arg(format!("--proxy-server=http://127.0.0.1:{port}"))
                .arg("--proxy-bypass-list=<-loopback>");
        }

        command
            // The player only publishes its media list once it has loaded a
            // source, which the page does on its own schedule.
            .arg("--autoplay-policy=no-user-gesture-required")
            // Port 0 lets the OS choose; the browser then publishes the port in
            // its profile directory, so there is no window in which another
            // process could take it first.
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--window-size=1280,900")
            .arg("about:blank")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);

        // The browser publishes the port it listens on in this file, and a
        // profile that outlives a browser still holds the *previous* browser's
        // answer. Clearing it first makes what the wait below reads necessarily
        // this browser's — a leftover file would otherwise hand the wait a dead
        // port the instant it looked, and the session would fail to start on
        // every attempt after the first. A throwaway profile has no such file,
        // so for the ordinary render this does nothing.
        let _ = std::fs::remove_file(profile.join(DEVTOOLS_PORT_FILE));

        let child = command.spawn().map_err(|err| {
            AppError::browser_unavailable("the browser could not be started")
                .with_user_message("A browser is needed to read this page, but it would not start.")
                .with_technical_detail(format!("{}: {err}", executable.display()))
        })?;
        let pid = child.id();

        Ok(Self {
            child,
            pid,
            profile: profile.to_path_buf(),
            kind,
        })
    }

    /// Terminate the browser and delete its profile. Safe to call twice.
    async fn shutdown(&mut self) {
        // The tree first, while the parent still exists. A browser is a parent
        // with renderer, GPU, and utility children, and `/T` works from a
        // *live* process id: kill the parent first and its children are
        // reparented, so the tree kill finds nothing, the children go on holding
        // the profile's files open, and the removal below fails with a sharing
        // violation — stranding a throwaway profile in the temp directory.
        self.kill_tree();
        let _ = self.child.start_kill();
        // Reap so the process handle does not outlive the render.
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
        self.remove_profile();
    }

    /// Terminate the browser and its children without awaiting.
    ///
    /// The profile is deliberately left behind: removing it retries for up to
    /// four seconds, and this runs on the exit path, where holding the
    /// application open that long to tidy a directory is the wrong trade. The
    /// next launch's sweep reclaims it — see `sweep_stale_profiles`.
    fn terminate_now(&mut self) {
        self.kill_tree();
        let _ = self.child.start_kill();
    }

    fn remove_profile(&mut self) {
        if self.profile.as_os_str().is_empty() {
            return;
        }
        if self.kind != BrowserProfile::Ephemeral {
            // Only a profile made for this browser is deleted with it. The
            // sign-in profile holds a session the user established by hand in a
            // window they typed into, and it outlives every browser that opens
            // it — so removing it here would throw that away as a side effect of
            // a render, or of a browser that failed to start. Clearing it is the
            // user's decision, and `clear_login_session` is where that happens.
            return;
        }
        for attempt in 0..PROFILE_REMOVE_ATTEMPTS {
            match std::fs::remove_dir_all(&self.profile) {
                Ok(()) => break,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
                // The browser may still be releasing its lock file.
                Err(_) if attempt + 1 < PROFILE_REMOVE_ATTEMPTS => {
                    std::thread::sleep(PROFILE_REMOVE_DELAY);
                }
                Err(err) => {
                    tracing::warn!(
                        target: "browser",
                        profile = %self.profile.display(),
                        error = %err,
                        "could not remove the temporary browser profile"
                    );
                }
            }
        }
        // Mark it handled so Drop does not try again.
        self.profile = PathBuf::new();
    }

    /// Force-kill the whole process tree.
    ///
    /// A browser is a parent process with renderer and GPU children. Killing the
    /// parent normally takes them with it, but a wedged child would otherwise
    /// linger, so the tree is terminated explicitly.
    fn kill_tree(&mut self) {
        if let Some(pid) = self.pid.take() {
            #[cfg(windows)]
            {
                // taskkill is the only way to take a whole tree down in one
                // step, but it is a console program and would flash a window —
                // on every browser teardown, which is every render.
                let mut command = std::process::Command::new("taskkill");
                command
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                command.no_console_window();
                let _ = command.status();
            }
            #[cfg(not(windows))]
            {
                let _ = pid;
            }
        }
    }
}

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        // Backstop for a cancelled future or an error that skipped `shutdown`.
        // The tree before the parent, for the reason `shutdown` gives.
        self.kill_tree();
        let _ = self.child.start_kill();

        // Wait for the process to actually be gone before touching its profile.
        // `shutdown` waits for the same reason; this path has no runtime to await
        // on, so it polls. Without it the directory is removed while the browser
        // is still writing to it — and a browser killed a moment ago can leave
        // files behind after a removal that appeared to succeed, stranding a
        // throwaway profile in the temp directory.
        //
        // Bounded: this is a cleanup guard, and a browser that will not die must
        // not hold the drop for ever.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                Err(_) => break,
            }
        }

        self.remove_profile();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::renderer::MediaSource;

    #[test]
    fn devtools_active_port_is_parsed_from_the_first_line() {
        assert_eq!(
            parse_devtools_active_port("9222\n/devtools/browser/abc"),
            Some(9222)
        );
        assert_eq!(parse_devtools_active_port(" 5000 \r\n"), Some(5000));
        assert_eq!(parse_devtools_active_port(""), None);
        assert_eq!(parse_devtools_active_port("not-a-port\nx"), None);
        assert_eq!(parse_devtools_active_port("0\nx"), None);
    }

    #[test]
    fn an_explicit_browser_is_preferred_over_discovered_ones() {
        let explicit = PathBuf::from(r"C:\custom\browser.exe");
        let candidates = browser_candidates(Some(explicit.clone()));
        assert_eq!(candidates.first(), Some(&explicit));
    }

    #[test]
    fn candidates_cover_both_chrome_and_edge_locations() {
        let candidates = browser_candidates(None);
        let joined: Vec<String> = candidates
            .iter()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect();
        assert!(joined
            .iter()
            .any(|path| path.ends_with("Chrome/Application/chrome.exe")));
        assert!(joined
            .iter()
            .any(|path| path.ends_with("Edge/Application/msedge.exe")));
    }

    #[test]
    fn a_missing_executable_is_reported_as_browser_unavailable() {
        let err = ChromiumRenderer::with_executable(
            r"C:\definitely\not\here.exe",
            Arc::new(crate::network::MockHttpClient::new()),
        )
        .err()
        .expect("a missing executable must be rejected");
        assert_eq!(err.kind(), crate::errors::AppErrorKind::BrowserUnavailable);
    }

    // --- reclaiming what a previous run could not remove ---------------------

    #[test]
    fn an_abandoned_profile_is_reclaimed_and_a_fresh_one_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let stale = root.path().join(format!("{PROFILE_PREFIX}stale"));
        let fresh = root.path().join(format!("{PROFILE_PREFIX}fresh"));
        let unrelated = root.path().join("something-else");
        for dir in [&stale, &fresh, &unrelated] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("marker"), b"x").unwrap();
        }

        // With the real age gate, nothing here is old enough — which is what
        // protects a profile a running instance is using.
        sweep_profiles_in(root.path(), STALE_PROFILE_AGE);
        assert!(
            stale.is_dir(),
            "a profile younger than the gate must survive"
        );
        assert!(fresh.is_dir());
        assert!(unrelated.is_dir());

        // With the gate opened, our profiles go and nothing else does.
        sweep_profiles_in(root.path(), Duration::ZERO);
        assert!(!stale.exists(), "an abandoned profile must be reclaimed");
        assert!(!fresh.exists());
        assert!(
            unrelated.is_dir(),
            "only this application's own profiles may be swept"
        );
    }

    #[test]
    fn a_temp_profile_lives_under_the_system_temp_directory() {
        let dir = temp_profile_dir().unwrap();
        assert!(dir.starts_with(std::env::temp_dir()));
        assert!(dir.is_dir());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn launching_clears_a_port_file_the_last_browser_left_behind() {
        // A throwaway profile is new every time, so this file is never there to
        // begin with. The sign-in profile is the same directory every time, and
        // a leftover answer would have the wait return a dead port immediately
        // rather than waiting for the browser that is starting — which is how a
        // reused profile fails to start on every attempt after the first.
        let profile = tempfile::tempdir().unwrap();
        let port_file = profile.path().join(DEVTOOLS_PORT_FILE);
        std::fs::write(&port_file, b"45123\n/devtools/browser/abcd\n").unwrap();

        let executable = std::env::current_exe().unwrap();
        let mut process = BrowserProcess::launch(
            &executable,
            profile.path(),
            BrowserVisibility::Headless,
            BrowserProfile::Persistent,
            None,
        )
        .ok()
        .unwrap();
        process.terminate_now();

        assert!(
            !port_file.exists(),
            "a port recorded by the previous browser must not survive the next launch"
        );
    }

    #[tokio::test]
    async fn shutting_down_removes_the_profile_directory() {
        let profile = temp_profile_dir().unwrap();
        std::fs::write(profile.join("marker"), b"x").unwrap();

        // A process that exits immediately stands in for a browser.
        let executable = std::env::current_exe().unwrap();
        let mut process = BrowserProcess::launch(
            &executable,
            &profile,
            BrowserVisibility::Headless,
            BrowserProfile::Ephemeral,
            None,
        )
        .ok()
        .unwrap();
        process.shutdown().await;

        assert!(!profile.exists(), "the profile must not be left behind");
    }

    #[tokio::test]
    async fn shutting_down_leaves_the_sign_in_profile_where_it_is() {
        // The whole of what makes a saved sign-in survive a render. A browser
        // that opened the sign-in profile and is closed again must put the
        // directory back exactly as it found it — the profile holds a session
        // the user established by hand, and no render, and no browser that
        // failed to start, may take it away with them.
        let profile = tempfile::tempdir().unwrap();
        let dir = profile.path().join("profile");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker"), b"x").unwrap();

        let executable = std::env::current_exe().unwrap();
        let mut process = BrowserProcess::launch(
            &executable,
            &dir,
            BrowserVisibility::Headless,
            BrowserProfile::Persistent,
            None,
        )
        .ok()
        .unwrap();
        process.shutdown().await;

        assert!(
            dir.join("marker").is_file(),
            "the sign-in profile must survive the browser that opened it"
        );
    }

    #[test]
    fn readiness_interprets_predicate_values_sensibly() {
        use serde_json::json;
        assert!(is_truthy(&json!(true)));
        assert!(!is_truthy(&json!(false)));
        assert!(!is_truthy(&json!(null)));
        assert!(!is_truthy(&json!("")));
        assert!(is_truthy(&json!("found")));
        assert!(!is_truthy(&json!(0)));
        assert!(is_truthy(&json!(3)));
        assert!(!is_truthy(&json!([])));
        assert!(is_truthy(&json!([1])));
        // A predicate may return the object it found.
        assert!(is_truthy(&json!({ "player": true })));
    }

    #[test]
    fn a_settle_condition_is_optional() {
        let plain = RenderRequest::new("https://example.test", "true", "({})");
        assert!(plain.settle_when.is_none());
        let enriched = plain.settle_on("window.ready");
        assert_eq!(enriched.settle_when, Some("window.ready"));
    }

    #[test]
    fn parse_media_sources_is_reused_from_the_renderer_module() {
        // Guards against the renderer and the parser drifting apart.
        let value =
            serde_json::json!({ "media": [{ "url": "https://cdn/a.mp4", "kind": "video" }] });
        let sources: Vec<MediaSource> = parse_media_sources(&value);
        assert_eq!(sources.len(), 1);
    }

    // --- the shared session --------------------------------------------------

    #[test]
    fn a_clean_session_of_the_right_kind_is_reused() {
        use BrowserVisibility::{Headless, Windowed};
        use BrowserProfile::Ephemeral;
        assert!(session_is_reusable(Windowed, Ephemeral, true, Windowed, Ephemeral));
        assert!(session_is_reusable(Headless, Ephemeral, true, Headless, Ephemeral));
    }

    #[test]
    fn a_session_left_dirty_by_an_interrupted_render_is_replaced() {
        // Cancelled, timed out, or failed: nobody knows what state the page and
        // the command stream were left in, so it is not driven again.
        assert!(!session_is_reusable(
            BrowserVisibility::Windowed,
            BrowserProfile::Ephemeral,
            false,
            BrowserVisibility::Windowed,
            BrowserProfile::Ephemeral
        ));
    }

    #[test]
    fn a_session_of_the_wrong_kind_is_replaced() {
        // A render that needs a window must not be served a headless browser,
        // and vice versa: the whole reason the window exists is that the site
        // treats the two differently.
        assert!(!session_is_reusable(
            BrowserVisibility::Headless,
            BrowserProfile::Ephemeral,
            true,
            BrowserVisibility::Windowed,
            BrowserProfile::Ephemeral
        ));
        assert!(!session_is_reusable(
            BrowserVisibility::Windowed,
            BrowserProfile::Ephemeral,
            true,
            BrowserVisibility::Headless,
            BrowserProfile::Ephemeral
        ));
    }

    #[test]
    fn a_renderer_starts_with_no_browser_and_none_running() {
        // The point of the shared session: nothing is launched until something
        // is actually rendered, and it is counted when it is.
        let renderer = ChromiumRenderer::with_executable(
            std::env::current_exe().unwrap(),
            Arc::new(crate::network::MockHttpClient::new()),
        )
        .expect("a renderer around an existing file");
        assert_eq!(renderer.launches(), 0);
        assert!(!renderer.has_session());
    }

    // --- the session lifecycle, against a browser that is not a browser ------    //
    // The live tests drive real Douyin through a real Chrome, which is the only
    // way to prove the *page* path works — and the only place a failure cannot be
    // attributed to this application, because the page, the redirect, and the
    // network are all outside it. Everything the renderer itself promises —
    // one browser for many renders, concurrent callers sharing that browser, a
    // failed page not costing a second browser, a dead browser being replaced,
    // teardown on drop — is an invariant of *this* code, so it is pinned here
    // against a launcher whose browser is a loopback CDP server. Deterministic,
    // no network, no Chrome.
    /// The application's own browser profile: where it lives, who may open it,
    /// and which cleanup is allowed to touch it.
    mod sign_in_profile {
        use super::*;
        use std::time::Duration;

        /// What the probe reports, as `PROBE_SESSION` shapes it. Only the
        /// fields the rule reads are filled in.
        fn probe_page(signals: &[&str], body_chars: u64, href: &str) -> serde_json::Value {
            let mut set = serde_json::Map::new();
            for name in signals {
                set.insert((*name).to_string(), serde_json::json!(true));
            }
            serde_json::json!({
                "href": href,
                "bodyChars": body_chars,
                "headerFound": true,
                "signals": set,
                "candidates": {
                    "userAvatar": if signals.contains(&"avatar") { 1 } else { 0 },
                    "loginButton": if signals.contains(&"loginButton") { 1 } else { 0 },
                }
            })
        }

        const DOUYIN_HOME: &str = "https://www.douyin.com/jingxuan";

        #[test]
        fn a_page_that_names_the_visitor_is_signed_in() {
            // The site showing the visitor their own avatar in its top bar is
            // the one signal that stands on its own: Douyin saying who it takes
            // this to be, rather than the application inferring it from a cookie
            // it is holding.
            assert_eq!(
                session_status_from(Some(&probe_page(&["avatar"], 9000, DOUYIN_HOME))),
                LoginStatus::SignedIn
            );
        }

        #[test]
        fn a_page_that_asks_for_a_sign_in_is_signed_out() {
            for signals in [
                vec!["loginPrompt"],
                vec!["sessionRejected"],
                vec!["loginButton"],
            ] {
                assert_eq!(
                    session_status_from(Some(&probe_page(&signals, 9000, DOUYIN_HOME))),
                    LoginStatus::SignedOut,
                    "for {signals:?}"
                );
            }
        }

        #[test]
        fn naming_the_visitor_outranks_a_sign_in_control_left_on_the_page() {
            // Both at once is a page that recognises the visitor and still
            // renders some sign-in affordance somewhere. Reading that as signed
            // out would cost them a sign-in they did not need.
            assert_eq!(
                session_status_from(Some(&probe_page(
                    &["avatar", "loginPrompt"],
                    9000,
                    DOUYIN_HOME
                ))),
                LoginStatus::SignedIn
            );
        }

        #[test]
        fn a_page_that_says_nothing_is_unknown_not_signed_out() {
            // The whole point of the third state. A page that rendered without
            // naming the visitor and without asking for a sign-in is exactly
            // what a slow load, a half-built page and a layout change look like,
            // and telling the user to sign in again on that evidence is telling
            // them to fix something that is not broken.
            assert_eq!(
                session_status_from(Some(&probe_page(&[], 9000, DOUYIN_HOME))),
                LoginStatus::Unknown
            );
        }

        #[test]
        fn a_page_that_never_rendered_is_unknown_whatever_it_contains() {
            // Too thin to have rendered anything. Checked before any signal is
            // believed, so a fragment that happens to carry an avatar cannot
            // produce a confident answer.
            assert_eq!(
                session_status_from(Some(&probe_page(&["avatar"], 12, DOUYIN_HOME))),
                LoginStatus::Unknown
            );
        }

        #[test]
        fn a_redirect_away_from_douyin_is_unknown() {
            // Landing somewhere else is not evidence about a Douyin session.
            assert_eq!(
                session_status_from(Some(&probe_page(&[], 9000, "https://example.invalid/oops"))),
                LoginStatus::Unknown
            );
        }

        #[test]
        fn a_probe_that_threw_is_unknown() {
            assert_eq!(
                session_status_from(Some(&serde_json::json!({ "error": "boom" }))),
                LoginStatus::Unknown
            );
            assert_eq!(session_status_from(None), LoginStatus::Unknown);
        }

        #[test]
        fn the_claim_on_the_profile_is_exclusive() {
            let temp = tempfile::tempdir().expect("a temporary directory");
            let dir = temp.path().join("profile");

            let held = ProfileLock::acquire(&dir).expect("the first claim is granted");
            let second = ProfileLock::acquire(&dir);

            let err = second.expect_err("a second claim on one profile must be refused");
            assert_eq!(err.kind(), crate::errors::AppErrorKind::BrowserUnavailable);
            // The message has to say what to do, because the user is the only
            // one who can do it.
            assert!(
                err.user_message().contains("another window"),
                "the refusal must say how to resolve it: {}",
                err.user_message()
            );

            drop(held);
            ProfileLock::acquire(&dir).expect("and released when the holder goes away");
        }

        #[test]
        fn a_refused_claim_does_not_disturb_what_is_already_there() {
            // A second instance being turned away must leave the first one's
            // profile exactly as it found it — nothing truncated, nothing
            // moved, nothing written.
            let temp = tempfile::tempdir().expect("a temporary directory");
            let dir = temp.path().join("profile");
            std::fs::create_dir_all(&dir).expect("the profile directory");
            let cookies = dir.join("Cookies");
            std::fs::write(&cookies, b"a session lives here").expect("the file");

            let held = ProfileLock::acquire(&dir).expect("the first claim");
            assert!(ProfileLock::acquire(&dir).is_err());

            assert_eq!(
                std::fs::read(&cookies).expect("readable"),
                b"a session lives here",
                "a refused claim must not touch the profile"
            );
            drop(held);
        }

        #[test]
        fn the_sweep_reclaims_render_profiles_and_leaves_the_sign_in_profile() {
            // Side by side under one root, so nothing but the name can save the
            // second one. This is the property that matters: the cleanup that
            // reclaims abandoned render profiles must never be able to take the
            // saved sign-in with it.
            let root = tempfile::tempdir().expect("a temporary directory");
            let abandoned = root.path().join(format!("{PROFILE_PREFIX}abandoned"));
            let sign_in = root.path().join("profile");
            std::fs::create_dir_all(&abandoned).expect("an abandoned render profile");
            std::fs::create_dir_all(&sign_in).expect("the sign-in profile");
            let cookies = sign_in.join("Cookies");
            std::fs::write(&cookies, b"a session lives here").expect("the file");

            // Sweeping everything regardless of age, which is stricter than the
            // application ever is at startup.
            sweep_profiles_in(root.path(), Duration::ZERO);

            assert!(!abandoned.exists(), "an abandoned render profile is reclaimed");
            assert!(
                cookies.is_file(),
                "the saved sign-in must survive every sweep"
            );
        }

        #[test]
        fn a_session_is_never_reused_across_profiles() {
            use BrowserProfile::{Ephemeral, Persistent};
            use BrowserVisibility::{Headless, Windowed};

            // The same browser is reusable for the same question...
            assert!(session_is_reusable(Headless, Ephemeral, true, Headless, Ephemeral));
            assert!(session_is_reusable(Windowed, Persistent, true, Windowed, Persistent));
            // ...and not across the two, in either direction. An anonymous
            // render served from the signed-in profile would silently become an
            // authenticated one; an authenticated render served from the
            // throwaway profile would come back empty with no way to tell why.
            assert!(!session_is_reusable(Headless, Ephemeral, true, Headless, Persistent));
            assert!(!session_is_reusable(Headless, Persistent, true, Headless, Ephemeral));
            // And a session that did not finish its last render is still
            // replaced, whichever profile it is on.
            assert!(!session_is_reusable(Headless, Persistent, false, Headless, Persistent));
        }
    }

    mod lifecycle {
        use super::*;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        use futures_util::{SinkExt, StreamExt};
        use serde_json::json;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio_tungstenite::tungstenite::Message;

        /// What the fake browser does when it is driven.
        #[derive(Default)]
        struct Behaviour {
            /// How many more times the readiness predicate answers `false`
            /// before it answers `true`. Large means "this page never becomes
            /// ready", which is the live flake this models.
            not_ready_for: AtomicUsize,
            /// Media the collection script reports.
            media: usize,
        }

        impl Behaviour {
            fn ready_now() -> Arc<Self> {
                Arc::new(Self {
                    media: 3,
                    ..Self::default()
                })
            }

            fn not_ready(times: usize) -> Arc<Self> {
                Arc::new(Self {
                    media: 3,
                    not_ready_for: AtomicUsize::new(times),
                })
            }

            fn reply(&self, method: &str, expression: &str) -> serde_json::Value {
                match method {
                    "Page.navigate" => json!({ "frameId": "F1" }),
                    "Runtime.evaluate" => {
                        if expression.contains("location.href") {
                            return json!({ "result": { "type": "string", "value": "https://www.douyin.com/video/1" } });
                        }
                        if expression.contains("document.title") {
                            return json!({ "result": { "type": "string", "value": "Fake page" } });
                        }
                        if expression.contains("isPlayEndpoint") {
                            let media: Vec<serde_json::Value> = (0..self.media)
                                .map(|index| {
                                    json!({ "url": format!("https://cdn.test/media-video-{index}.mp4"), "kind": "video" })
                                })
                                .collect();
                            return json!({ "result": { "type": "object", "value": { "media": media } } });
                        }
                        // The readiness and settle predicates.
                        let outstanding = self.not_ready_for.load(Ordering::SeqCst);
                        let ready = outstanding == 0;
                        if !ready {
                            let _ = self.not_ready_for.compare_exchange(
                                outstanding,
                                outstanding - 1,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            );
                        }
                        json!({ "result": { "type": "boolean", "value": ready } })
                    }
                    _ => json!({}),
                }
            }
        }

        /// The per-browser half of a fake launch: what the launcher and the test
        /// both need to observe, separate from the socket plumbing.
        #[derive(Default)]
        struct BrowserState {
            /// Set when the browser "dies": it stops answering and drops the
            /// socket, exactly as a killed process looks from here.
            dead: AtomicBool,
            /// Set once the browser has been torn down, so drop and an explicit
            /// shutdown count once between them — as a real one does, because
            /// `BrowserProcess::shutdown` clears what `Drop` would have used.
            closed: AtomicBool,
            shutdowns: Arc<AtomicUsize>,
        }

        impl BrowserState {
            fn kill(&self) {
                self.dead.store(true, Ordering::SeqCst);
            }
        }

        /// A browser that is a loopback CDP server, with its own lifecycle.
        struct FakeBrowser {
            profile: PathBuf,
            /// The port the target list is served on.
            port: u16,
            state: Arc<BrowserState>,
        }

        impl FakeBrowser {
            async fn start(
                profile: &Path,
                behaviour: Arc<Behaviour>,
                state: Arc<BrowserState>,
            ) -> Self {
                let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let ws_port = ws_listener.local_addr().unwrap().port();
                let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let port = http_listener.local_addr().unwrap().port();

                tokio::spawn(serve_target_list(http_listener, ws_port));
                tokio::spawn(serve_cdp(ws_listener, behaviour, Arc::clone(&state)));

                Self {
                    profile: profile.to_path_buf(),
                    port,
                    state,
                }
            }

            /// Tear down, once, however it is reached.
            fn close(&mut self) {
                if !self.state.closed.swap(true, Ordering::SeqCst) {
                    self.state.shutdowns.fetch_add(1, Ordering::SeqCst);
                    let _ = std::fs::remove_dir_all(&self.profile);
                }
            }
        }

        impl Drop for FakeBrowser {
            fn drop(&mut self) {
                // The real browser is cleaned up by its own guard; this is that
                // guard's stand-in, so a renderer that is dropped without an
                // explicit teardown is still held to the same promise.
                self.close();
            }
        }

        /// Answer `GET /json/list` with one page target pointing at the CDP port.
        async fn serve_target_list(listener: TcpListener, ws_port: u16) {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let body = json!([{
                    "type": "page",
                    "webSocketDebuggerUrl": format!("ws://127.0.0.1:{ws_port}/devtools/page/1"),
                }])
                .to_string();
                tokio::spawn(async move {
                    let mut scratch = [0u8; 4096];
                    let _ = stream.read(&mut scratch).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        }

        /// Speak just enough DevTools for one render.
        async fn serve_cdp(
            listener: TcpListener,
            behaviour: Arc<Behaviour>,
            state: Arc<BrowserState>,
        ) {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let behaviour = Arc::clone(&behaviour);
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    while let Some(Ok(message)) = socket.next().await {
                        // A dead browser answers nothing and drops the socket,
                        // which is what a killed process looks like from here.
                        if state.dead.load(Ordering::SeqCst) {
                            let _ = socket.close(None).await;
                            return;
                        }
                        let Message::Text(text) = message else {
                            continue;
                        };
                        let Ok(request) = serde_json::from_str::<serde_json::Value>(text.as_str())
                        else {
                            continue;
                        };
                        let method = request
                            .get("method")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default();
                        let expression = request
                            .get("params")
                            .and_then(|params| params.get("expression"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default();
                        let payload = json!({
                            "id": request.get("id").cloned().unwrap_or(serde_json::Value::Null),
                            "result": behaviour.reply(method, expression),
                        })
                        .to_string();
                        if socket.send(Message::Text(payload)).await.is_err() {
                            return;
                        }
                    }
                });
            }
        }

        #[async_trait]
        impl LaunchedBrowser for FakeBrowser {
            fn pid(&self) -> Option<u32> {
                None
            }

            fn profile(&self) -> Option<&Path> {
                Some(self.profile.as_path())
            }

            async fn devtools_port(&self, _timeout: Duration) -> AppResult<u16> {
                Ok(self.port)
            }

            async fn shutdown(&mut self) {
                self.close();
            }

            fn terminate_now(&mut self) {
                self.close();
            }
        }

        /// A launcher whose "browser" is [`FakeBrowser`], counting what it started.
        struct FakeLauncher {
            behaviour: Arc<Behaviour>,
            launches: AtomicUsize,
            shutdowns: Arc<AtomicUsize>,
            /// Every browser it has started, oldest first, so a test can kill a
            /// specific one rather than "the browser" in the abstract.
            browsers: parking_lot::Mutex<Vec<Arc<BrowserState>>>,
            /// The visibility each launch was asked for, in order.
            visibilities: parking_lot::Mutex<Vec<BrowserVisibility>>,
            /// The profile kind each launch was asked for, in order.
            kinds: parking_lot::Mutex<Vec<BrowserProfile>>,
        }

        impl FakeLauncher {
            fn new(behaviour: Arc<Behaviour>) -> Arc<Self> {
                Arc::new(Self {
                    behaviour,
                    launches: AtomicUsize::new(0),
                    shutdowns: Arc::new(AtomicUsize::new(0)),
                    browsers: parking_lot::Mutex::new(Vec::new()),
                    visibilities: parking_lot::Mutex::new(Vec::new()),
                    kinds: parking_lot::Mutex::new(Vec::new()),
                })
            }

            fn launches(&self) -> usize {
                self.launches.load(Ordering::SeqCst)
            }

            fn shutdowns(&self) -> usize {
                self.shutdowns.load(Ordering::SeqCst)
            }

            /// Kill the browser started by launch `index`, as an outside process
            /// would.
            fn kill(&self, index: usize) {
                if let Some(browser) = self.browsers.lock().get(index) {
                    browser.kill();
                }
            }
        }

        #[async_trait]
        impl BrowserLauncher for FakeLauncher {
            async fn launch(
                &self,
                profile: &Path,
                visibility: BrowserVisibility,
                kind: BrowserProfile,
            ) -> AppResult<Box<dyn LaunchedBrowser>> {
                let state = Arc::new(BrowserState {
                    shutdowns: Arc::clone(&self.shutdowns),
                    ..BrowserState::default()
                });
                self.browsers.lock().push(Arc::clone(&state));
                self.launches.fetch_add(1, Ordering::SeqCst);
                self.visibilities.lock().push(visibility);
                self.kinds.lock().push(kind);
                Ok(Box::new(
                    FakeBrowser::start(profile, Arc::clone(&self.behaviour), state).await,
                ))
            }
        }

        /// A renderer wired to a fake launcher.
        fn renderer(launcher: Arc<FakeLauncher>) -> Arc<ChromiumRenderer> {
            let http = Arc::new(
                crate::network::ReqwestHttpClient::with_policy(
                    5,
                    crate::network::DestinationPolicy::Any,
                )
                .expect("a permissive loopback client"),
            );
            Arc::new(ChromiumRenderer::around(
                std::env::current_exe().unwrap(),
                launcher,
                http,
            ))
        }

        /// The same, with somewhere for the sign-in profile to live.
        fn renderer_with_auth_profile(
            launcher: Arc<FakeLauncher>,
            profile: PathBuf,
        ) -> Arc<ChromiumRenderer> {
            let http = Arc::new(
                crate::network::ReqwestHttpClient::with_policy(
                    5,
                    crate::network::DestinationPolicy::Any,
                )
                .expect("a permissive loopback client"),
            );
            Arc::new(
                ChromiumRenderer::around(std::env::current_exe().unwrap(), launcher, http)
                    .with_auth_profile(profile),
            )
        }

        /// A request that fails fast, so a test that waits for a page which never
        /// becomes ready does not wait the production budget to find out.
        ///
        /// The predicates are the real Douyin ones, so the fake browser is being
        /// asked exactly what a real one is and the scripts cannot drift from
        /// what the resolver ships.
        fn quick_request() -> RenderRequest<'static> {
            use crate::resolvers::douyin::player::{
                COLLECT_PLAYER_MEDIA, PLAYER_READY, VIDEO_METADATA_READY,
            };
            RenderRequest::new(
                "https://www.douyin.com/video/1",
                PLAYER_READY,
                COLLECT_PLAYER_MEDIA,
            )
            .settle_on(VIDEO_METADATA_READY)
            .with_ready_timeout(Duration::from_millis(400))
            .with_settle_timeout(Duration::from_millis(200))
            .with_total_timeout(Duration::from_secs(20))
        }

        #[tokio::test]
        async fn several_renders_reuse_one_browser() {
            let launcher = FakeLauncher::new(Behaviour::ready_now());
            let renderer = renderer(Arc::clone(&launcher));

            for round in 1..=4 {
                let page = renderer
                    .render(&quick_request())
                    .await
                    .unwrap_or_else(|err| panic!("round {round}: {err}"));
                assert!(!page.media.is_empty(), "round {round} must yield media");
                assert_eq!(
                    launcher.launches(),
                    1,
                    "round {round} started another browser instead of reusing the one it had"
                );
            }
            assert!(renderer.has_session());
        }

        #[tokio::test]
        async fn concurrent_renders_do_not_start_a_browser_each() {
            let launcher = FakeLauncher::new(Behaviour::ready_now());
            let renderer = renderer(Arc::clone(&launcher));

            let mut tasks = Vec::new();
            for round in 1..=4 {
                let renderer = Arc::clone(&renderer);
                tasks.push(tokio::spawn(async move {
                    renderer
                        .render(&quick_request())
                        .await
                        .map(|page| page.media.len())
                        .map_err(|err| (round, err))
                }));
            }
            for task in tasks {
                task.await
                    .expect("the render task must not panic")
                    .expect("every concurrent render must produce a page");
            }

            assert_eq!(
                launcher.launches(),
                1,
                "concurrent renders must share one browser, not start one each"
            );
        }

        #[tokio::test]
        async fn a_page_that_never_becomes_ready_does_not_cost_a_second_browser() {
            // The live flake, modelled: Douyin intermittently serves the short
            // link a page that never exposes its player. The browser is healthy
            // throughout, so looking at the page again must not start another
            // one — a fresh browser for a slow page is pure waste.
            let launcher = FakeLauncher::new(Behaviour::not_ready(1_000));
            let renderer = renderer(Arc::clone(&launcher));

            let err = renderer
                .render(&quick_request())
                .await
                .expect_err("a page that never becomes ready must fail");
            assert!(err.is_retryable(), "the page, not the browser, failed");

            assert_eq!(
                launcher.launches(),
                1,
                "one browser for a page-level failure, however many times it was looked at"
            );
            // And the browser it could not use is not kept for the next caller.
            assert!(!renderer.has_session(), "a failed session is not kept");
            assert_eq!(launcher.shutdowns(), 1);
        }

        #[tokio::test]
        async fn a_page_that_becomes_ready_on_the_second_look_succeeds_on_one_browser() {
            // The same flake, but the second look works — which is what the retry
            // is for. The whole point is that the recovery costs a page load and
            // not a browser start.
            let launcher = FakeLauncher::new(Behaviour::not_ready(1));
            let renderer = renderer(Arc::clone(&launcher));

            let page = renderer
                .render(&quick_request())
                .await
                .expect("the second look must produce the page");
            assert!(!page.media.is_empty());
            assert_eq!(
                launcher.launches(),
                1,
                "the retry reused the browser it had"
            );
        }

        #[tokio::test]
        async fn a_browser_that_dies_is_replaced_and_the_render_recovers() {
            // A session kept between renders may have been killed from outside.
            // The next render must end with a working page from a *new* browser
            // rather than an error the caller cannot get past.
            let behaviour = Behaviour::ready_now();
            let launcher = FakeLauncher::new(Arc::clone(&behaviour));
            let renderer = renderer(Arc::clone(&launcher));

            renderer
                .render(&quick_request())
                .await
                .expect("the first render");
            assert_eq!(launcher.launches(), 1);

            // Kill the browser this renderer owns — the one it started — and
            // leave nothing else to die with it.
            launcher.kill(0);

            let page = renderer
                .render(&quick_request())
                .await
                .expect("the renderer must recover from a dead browser");
            assert!(!page.media.is_empty());
            assert_eq!(launcher.launches(), 2, "recovery means a new browser");
            assert_eq!(launcher.shutdowns(), 1, "the dead one was cleaned up");
        }

        #[tokio::test]
        async fn a_render_that_needs_a_window_is_not_served_a_headless_browser() {
            let launcher = FakeLauncher::new(Behaviour::ready_now());
            let renderer = renderer(Arc::clone(&launcher));

            renderer
                .render(&quick_request())
                .await
                .expect("headless render");
            let mut windowed = quick_request();
            windowed.visibility = BrowserVisibility::Windowed;
            renderer.render(&windowed).await.expect("windowed render");

            assert_eq!(
                launcher.launches(),
                2,
                "the wrong kind of browser is replaced"
            );
            assert_eq!(
                launcher.visibilities.lock().as_slice(),
                &[BrowserVisibility::Headless, BrowserVisibility::Windowed]
            );
        }

        #[tokio::test]
        async fn a_renderer_that_is_never_used_starts_no_browser() {
            let launcher = FakeLauncher::new(Behaviour::ready_now());
            let renderer = renderer(Arc::clone(&launcher));
            assert_eq!(renderer.launches(), 0);
            assert!(!renderer.has_session());
            assert_eq!(
                launcher.launches(),
                0,
                "nothing launches until something renders"
            );
        }

        #[tokio::test]
        async fn dropping_the_renderer_shuts_its_browser_down() {
            let launcher = FakeLauncher::new(Behaviour::ready_now());

            let profile = {
                let renderer = renderer(Arc::clone(&launcher));
                renderer.render(&quick_request()).await.expect("a render");
                assert!(renderer.has_session());
                assert_eq!(launcher.shutdowns(), 0, "still in use");

                let profile = renderer
                    .session_profile()
                    .expect("a live browser has a throwaway profile");
                assert!(profile.is_dir(), "and it exists while the browser does");
                profile
            };

            assert_eq!(
                launcher.shutdowns(),
                1,
                "the browser must not outlive its renderer"
            );
            assert!(!profile.exists(), "and its throwaway profile goes with it");
        }

        #[tokio::test]
        async fn a_browser_that_goes_unused_is_closed_without_waiting_for_the_renderer() {
            let launcher = FakeLauncher::new(Behaviour::ready_now());
            let renderer = renderer(Arc::clone(&launcher));
            renderer.set_idle_timeout(Duration::from_millis(200));

            renderer.render(&quick_request()).await.expect("a render");
            assert_eq!(launcher.launches(), 1);
            assert!(renderer.has_session(), "kept for the next render");

            // Long enough for the watcher to notice the browser is unused and
            // close it. The watcher checks every quarter of the timeout, so
            // this is several checks, not one race against a single tick.
            tokio::time::sleep(Duration::from_millis(1500)).await;

            assert_eq!(
                launcher.shutdowns(),
                1,
                "an unused browser must be closed rather than left in Task Manager"
            );
            assert!(!renderer.has_session(), "and no longer held");
            assert_eq!(launcher.launches(), 1, "closing it must not start another");
        }

        #[tokio::test]
        async fn a_browser_that_keeps_being_used_is_not_closed_by_the_idle_timer() {
            let launcher = FakeLauncher::new(Behaviour::ready_now());
            let renderer = renderer(Arc::clone(&launcher));
            renderer.set_idle_timeout(Duration::from_millis(300));

            // Renders closer together than the timeout, which is what paging
            // through a listing looks like: the browser is never unused for a
            // whole interval, so it is never closed underneath the work.
            for round in 1..=6 {
                renderer
                    .render(&quick_request())
                    .await
                    .unwrap_or_else(|err| panic!("round {round}: {err}"));
                tokio::time::sleep(Duration::from_millis(40)).await;
            }

            assert_eq!(
                launcher.shutdowns(),
                0,
                "a browser in use must not be closed by the idle timer"
            );
            assert_eq!(launcher.launches(), 1, "and it stayed the same browser");
            assert!(renderer.has_session());
        }

        #[tokio::test]
        async fn a_render_that_runs_out_of_time_does_not_leave_its_browser_behind() {
            use crate::resolvers::douyin::player::{COLLECT_PLAYER_MEDIA, PLAYER_READY};

            // A page that never becomes ready, and a budget far shorter than
            // the ready timeout — so the deadline is what ends this render,
            // while the browser is mid-wait and holding the session.
            let launcher = FakeLauncher::new(Behaviour::not_ready(10_000));
            let renderer = renderer(Arc::clone(&launcher));

            let request = RenderRequest::new(
                "https://www.douyin.com/video/1",
                PLAYER_READY,
                COLLECT_PLAYER_MEDIA,
            )
            .with_ready_timeout(Duration::from_secs(60))
            .with_total_timeout(Duration::from_millis(1500));

            assert!(
                renderer.render(&request).await.is_err(),
                "a page that never becomes ready must fail"
            );

            assert_eq!(launcher.launches(), 1, "it did start a browser");
            assert_eq!(
                launcher.shutdowns(),
                1,
                "the browser must be closed as the render gives up, not left running"
            );
            assert!(!renderer.has_session(), "and not held for a later render");
        }

        #[tokio::test]
        async fn the_exit_path_closes_the_browser_without_awaiting() {
            let launcher = FakeLauncher::new(Behaviour::ready_now());
            let renderer = renderer(Arc::clone(&launcher));

            renderer.render(&quick_request()).await.expect("a render");
            assert!(renderer.has_session());

            // Exactly what the application does on its way out, where there is
            // no runtime left to await on.
            renderer.shutdown_now();

            assert_eq!(launcher.shutdowns(), 1, "the browser must be terminated");
            assert!(!renderer.has_session(), "and not held afterwards");
        }

        #[tokio::test]
        async fn the_exit_path_is_harmless_with_no_browser_to_close() {
            let launcher = FakeLauncher::new(Behaviour::ready_now());
            let renderer = renderer(Arc::clone(&launcher));

            renderer.shutdown_now();

            assert_eq!(launcher.launches(), 0, "nothing was started");
            assert_eq!(launcher.shutdowns(), 0, "and so nothing was closed");
        }

        #[tokio::test]
        async fn a_render_that_asks_for_the_sign_in_profile_without_one_is_refused() {
            // A build with nowhere to keep a sign-in must say so. Falling back
            // to the throwaway profile would render the signed-*out* page and
            // return an empty listing, which reads as "this account has no
            // posts" rather than "there is no sign-in".
            let launcher = FakeLauncher::new(Behaviour::ready_now());
            let renderer = renderer(Arc::clone(&launcher));

            let err = renderer
                .render(&quick_request().with_persistent_profile())
                .await
                .expect_err("a build with no sign-in profile cannot serve one");

            assert!(
                err.user_message().to_lowercase().contains("anonymous"),
                "the message must say what it did instead: {}",
                err.user_message()
            );
            assert_eq!(
                launcher.launches(),
                0,
                "and no browser is started for a render that cannot happen"
            );
        }

        #[tokio::test]
        async fn a_signed_in_render_holds_the_profile_and_releases_it_with_the_browser() {
            let temp = tempfile::tempdir().expect("a temporary directory");
            let dir = temp.path().join("browser").join("profile");

            let launcher = FakeLauncher::new(Behaviour::ready_now());
            let renderer = renderer_with_auth_profile(Arc::clone(&launcher), dir.clone());

            renderer
                .render(&quick_request().with_persistent_profile())
                .await
                .expect("a signed-in render");
            assert_eq!(launcher.launches(), 1);

            // The profile is claimed for as long as a browser has it open, so
            // another instance cannot open the same user-data directory.
            assert!(
                ProfileLock::acquire(&dir).is_err(),
                "the profile must be held while a browser has it open"
            );

            // Closing that browser releases it. Closing the browser *before*
            // anything is deleted is the ordering that keeps a cleared session
            // cleared.
            renderer
                .close_login_session()
                .await
                .expect("the sign-in browser closes");
            assert_eq!(launcher.shutdowns(), 1, "the browser was actually closed");
            assert!(
                ProfileLock::acquire(&dir).is_ok(),
                "the claim is released with the browser that held it"
            );
        }

        #[tokio::test]
        async fn signing_out_leaves_an_anonymous_browser_alone() {
            // The sign-in screen's logout must not interrupt a render that has
            // nothing to do with the sign-in.
            let launcher = FakeLauncher::new(Behaviour::ready_now());
            let temp = tempfile::tempdir().expect("a temporary directory");
            let renderer =
                renderer_with_auth_profile(Arc::clone(&launcher), temp.path().join("profile"));

            renderer.render(&quick_request()).await.expect("an anonymous render");

            renderer
                .close_login_session()
                .await
                .expect("signing out is harmless");

            assert_eq!(
                launcher.shutdowns(),
                0,
                "a browser on the throwaway profile is none of the sign-in's business"
            );
            assert!(renderer.has_session(), "and it is still there to be reused");
        }

        #[tokio::test]
        async fn a_sign_in_window_is_not_closed_by_the_idle_timer() {
            let temp = tempfile::tempdir().expect("a temporary directory");
            let launcher = FakeLauncher::new(Behaviour::ready_now());
            let renderer = renderer_with_auth_profile(
                Arc::clone(&launcher),
                temp.path().join("profile"),
            );
            renderer.set_idle_timeout(Duration::from_millis(150));

            renderer.open_login_window().await.expect("a sign-in window");
            assert!(renderer.has_session(), "the window is open");

            // Long enough for the watcher to have closed an idle browser many
            // times over. Nobody types into a login page every 150ms, and the
            // window has to survive that.
            tokio::time::sleep(Duration::from_millis(1200)).await;

            assert_eq!(
                launcher.shutdowns(),
                0,
                "the window the user is signing in with must not be closed underneath them"
            );
            assert!(renderer.has_session());
        }
    }

    // --- a landing page that is not what was asked for ----------------------

    #[test]
    fn a_link_the_source_will_not_serve_is_recognised_from_where_it_landed() {
        // Measured, twice, in the live suite: this is what Douyin does with the
        // shared video fixture, and what the browser reported when it happened.
        assert_eq!(
            unrelated_landing("https://v.douyin.com/7yyhouNSzvA/", "https://www.douyin.com/jingxuan"),
            Some("the source answered with its front page instead of the requested content")
        );
        assert_eq!(
            unrelated_landing(
                "https://www.douyin.com/video/7281234567890123456",
                "https://www.douyin.com/jingxuan/"
            ),
            Some("the source answered with its front page instead of the requested content")
        );
    }

    #[test]
    fn the_source_saying_the_post_is_gone_is_recognised_as_itself() {
        // Douyin names the reason in the redirect it made.
        assert_eq!(
            unrelated_landing(
                "https://v.douyin.com/7yyhouNSzvA/",
                "https://www.douyin.com/jingxuan?previous_page=web_video_404_link"
            ),
            Some("the source marked the link as a post it no longer serves")
        );
    }

    #[test]
    fn the_ordinary_redirect_is_not_mistaken_for_a_refusal() {
        // A short link resolving to the post it names is the whole point of a
        // short link, and failing that would fail every working resolution.
        assert_eq!(
            unrelated_landing(
                "https://v.douyin.com/7yyhouNSzvA/",
                "https://www.douyin.com/video/7281234567890123456"
            ),
            None
        );
        assert_eq!(
            unrelated_landing(
                "https://www.douyin.com/video/7281234567890123456",
                "https://www.douyin.com/video/7281234567890123456"
            ),
            None
        );
        // A post link that lands on a *different post* is the source's business,
        // not a refusal — the page it serves may still be the one being read.
        assert_eq!(
            unrelated_landing(
                "https://www.douyin.com/note/7629996475679029850",
                "https://www.douyin.com/video/7281234567890123456"
            ),
            None
        );
    }

    #[test]
    fn asking_for_the_front_page_and_getting_it_is_not_a_redirection() {
        assert_eq!(
            unrelated_landing("https://www.douyin.com/jingxuan", "https://www.douyin.com/jingxuan"),
            None
        );
        assert_eq!(
            unrelated_landing("https://www.douyin.com/", "https://www.douyin.com/jingxuan"),
            None
        );
    }

    #[test]
    fn a_page_on_another_site_is_not_read_as_a_douyin_refusal() {
        // The direct host check: nothing outside douyin.com can be this, so a
        // page served from anywhere else never gets a verdict from here.
        assert_eq!(
            unrelated_landing("https://v.douyin.com/7yyhouNSzvA/", "https://example.test/jingxuan"),
            None
        );
        assert_eq!(
            unrelated_landing("https://v.douyin.com/7yyhouNSzvA/", "about:blank"),
            None
        );
    }

    #[test]
    fn a_refusal_is_not_offered_as_something_to_try_again() {
        // The other half of the decision: a link the source will not serve is
        // answered the same way however many times it is asked, so the error
        // must not be retryable and must be recognisable as this and not as
        // something else.
        let err = AppError::unavailable("the source redirected this link to another page")
            .with_technical_detail(format!("{SOURCE_REDIRECT_MARKER} requested=x landed=y"));
        assert!(!err.is_retryable());
        assert!(is_source_redirect(&err));

        // And an ordinary readiness failure is not claimed to be this one.
        let timeout = AppError::browser_unavailable("the page never became ready in the browser")
            .with_retryable(true)
            .with_technical_detail("ready_timeout=25000ms");
        assert!(!is_source_redirect(&timeout));
        assert!(timeout.is_retryable());
    }

}
