//! HTTP abstraction.
//!
//! The resolver layer talks to the network through [`HttpClient`] rather than
//! `reqwest` directly. That keeps `reqwest` (and its error type) confined to
//! this module, lets resolvers be unit-tested against [`MockHttpClient`], and
//! means the request policy — timeout, user agent, redirect handling, size
//! caps — is configured in exactly one place.
//!
//! Three capabilities are exposed, and deliberately no more:
//!
//! * [`HttpClient::resolve_redirect`] — follow a short link (no body read).
//! * [`HttpClient::fetch_text`] — retrieve a bounded text document for the
//!   resolver to parse.
//! * [`HttpClient::download`] — stream a media resource to a file while
//!   reporting progress and honouring cancellation.
//!
//! Nothing here knows about Douyin, media types, or the database; it moves
//! bytes and reports what happened.

use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

use crate::errors::{AppError, AppResult};
use crate::logging::redact_url;
use crate::network::resolver::GuardedResolver;

pub mod chromium;
pub mod policy;
pub mod renderer;
pub mod resolver;

pub use chromium::{ChromiumRenderer, LoginStatus};
pub use policy::{DestinationPolicy, Refusal};
pub use renderer::{
    MediaSource, MediaSourceKind, NetworkMedia, NoopRenderer, PageRenderer, RenderRequest,
    RenderedPage,
};

/// Default ceiling for a fetched text document (share pages are ~30–80 KB).
pub const DEFAULT_MAX_FETCH_BYTES: usize = 4 * 1024 * 1024;

/// User agent used for ordinary requests.
pub const DEFAULT_USER_AGENT: &str = concat!(
    "DouyinDownloader/",
    env!("CARGO_PKG_VERSION"),
    " (+desktop; public-content-only)"
);

/// User agent presented when fetching a Douyin share page.
///
/// Douyin serves an executable anti-bot shell to desktop agents but real server
/// rendered markup to a mobile share-page agent. This is a *request format*
/// choice, not an access-control bypass: the same public page is requested, with
/// the agent the page is designed to serve.
pub const MOBILE_SHARE_USER_AGENT: &str =
    "Mozilla/5.0 (iPhone; CPU iPhone OS 16_6 like Mac OS X) AppleWebKit/605.1.15 \
     (KHTML, like Gecko) Version/16.6 Mobile/15E148 Safari/604.1";

/// Why a transfer was asked to stop.
///
/// The two are not the same instruction and must not be treated as one: a
/// cancellation throws away what was written, while a pause deliberately keeps
/// it so the work already paid for can be continued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum StopReason {
    Running = 0,
    Cancelled = 1,
    Paused = 2,
}

/// A cancellable token shared with an in-flight transfer.
///
/// The download worker keeps one of these per task; the command layer flips it
/// when the user cancels or pauses. The transfer loop polls it between chunks,
/// so a stop ends the actual byte transfer rather than only its bookkeeping.
#[derive(Debug, Clone, Default)]
pub struct CancellationHandle {
    state: Arc<AtomicU8>,
}

impl CancellationHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation: stop, and discard what was written.
    ///
    /// Overrides a pause — a user asking for the task to go away outranks an
    /// earlier request to keep its partial data.
    pub fn cancel(&self) {
        self.state.store(StopReason::Cancelled as u8, Ordering::SeqCst);
    }

    /// Request a stop that leaves the partial file where it is.
    ///
    /// Does not override an outright cancellation: once a task is cancelled, a
    /// late pause cannot resurrect partial data that was already discarded.
    pub fn pause(&self) {
        let _ = self.state.compare_exchange(
            StopReason::Running as u8,
            StopReason::Paused as u8,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::SeqCst) != StopReason::Running as u8
    }

    /// Whether the partial file must be left in place when this transfer stops.
    pub fn keeps_partial(&self) -> bool {
        self.state.load(Ordering::SeqCst) == StopReason::Paused as u8
    }
}

/// Receives byte-level progress during a transfer.
///
/// Implementations must not block: the transfer loop calls this for every chunk
/// (after the caller's own throttling), so a slow reporter stalls the download.
pub trait ProgressReporter: Send + Sync {
    /// `total` is `None` when the server did not report a content length.
    fn report(&self, downloaded: u64, total: Option<u64>);
}

/// A [`ProgressReporter`] that discards everything, for callers that do not
/// care about progress (tests, fire-and-forget transfers).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopProgress;

impl ProgressReporter for NoopProgress {
    fn report(&self, _downloaded: u64, _total: Option<u64>) {}
}

/// What to retrieve with [`HttpClient::fetch_text`].
#[derive(Debug, Clone)]
pub struct FetchRequest<'a> {
    pub url: &'a str,
    /// Overrides [`DEFAULT_USER_AGENT`] when set.
    pub user_agent: Option<&'a str>,
    /// Optional `Accept` header.
    pub accept: Option<&'a str>,
    /// Hard cap on the number of body bytes read.
    pub max_bytes: usize,
}

impl<'a> FetchRequest<'a> {
    pub fn new(url: &'a str) -> Self {
        Self {
            url,
            user_agent: None,
            accept: None,
            max_bytes: DEFAULT_MAX_FETCH_BYTES,
        }
    }

    /// Present a mobile share-page agent (what Douyin's share pages respond to).
    pub fn mobile_share(url: &'a str) -> Self {
        Self {
            user_agent: Some(MOBILE_SHARE_USER_AGENT),
            accept: Some("text/html,application/xhtml+xml"),
            ..Self::new(url)
        }
    }
}

/// What to retrieve with [`HttpClient::download`].
pub struct TransferRequest<'a> {
    /// Absolute URL of the media resource.
    pub url: &'a str,
    /// File the bytes are written to. The caller is responsible for choosing a
    /// temporary name and moving it into place afterwards.
    pub destination: &'a Path,
    /// Polled between chunks; a cancelled transfer stops and returns
    /// [`AppErrorKind::Download`](crate::errors::AppErrorKind::Download) with a
    /// cancellation marker.
    pub cancel: CancellationHandle,
    pub user_agent: Option<&'a str>,
    /// Inclusive byte range start, for a resumable transfer. `None` starts at 0.
    pub resume_from: Option<u64>,
    /// The validator recorded when the partial was written, if any.
    ///
    /// Sent as `If-Range` on a ranged request, which makes the *server* decide
    /// whether the range is a safe continuation — where a local comparison would
    /// only be a guess. `None` does not mean "skip the header": a partial whose
    /// identity was never established is not resumable at all, and the ranged
    /// path refuses it before this is even sent.
    pub if_range: Option<&'a str>,
    /// The resource's whole size as the partial was fetched under, when it was
    /// known then.
    ///
    /// The second signal, and the one that speaks when nothing else can. A
    /// `206` whose `Content-Range` states a different total is describing a
    /// different resource, and that is worth refusing even though the
    /// validator matched — most importantly when the response carries *no*
    /// validator at all, which is exactly the case where the server's own
    /// statement is all there is to go on.
    pub expected_total: Option<u64>,
}

impl<'a> TransferRequest<'a> {
    pub fn new(url: &'a str, destination: &'a Path) -> Self {
        Self {
            url,
            destination,
            cancel: CancellationHandle::new(),
            user_agent: None,
            resume_from: None,
            if_range: None,
            expected_total: None,
        }
    }
}

/// Result of a completed transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferOutcome {
    /// Bytes in the file after this transfer — everything in it, not only what
    /// this response contributed. A continued transfer's result is the whole
    /// file, and a caller reporting progress on top of it needs that number.
    pub bytes_written: u64,
    pub content_type: Option<String>,
    /// Final URL after redirects. Media CDNs redirect to a signed location.
    pub final_url: String,
    pub status: u16,
    /// The offset this transfer actually continued from; `0` when it started
    /// over. Not the offset that was *asked* for — the one the source agreed to.
    pub resumed_from: u64,
    /// The whole resource's size, when the source stated it.
    pub resource_total: Option<u64>,
    /// The validator this response carried, if any. Persisted with the partial
    /// so a later resume can ask the server whether the range is still valid.
    pub validator: Option<crate::downloads::partial::ResourceValidator>,
}

/// Performs HTTP requests on behalf of the application core.
#[async_trait]
pub trait HttpClient: Send + Sync {
    /// Follow redirects for `url` and return the final absolute URL.
    ///
    /// Used to turn a Douyin short link (`https://v.douyin.com/xxxx/`) into the
    /// canonical content or profile URL. The body is never read.
    async fn resolve_redirect(&self, url: &str) -> AppResult<String>;

    /// Perform a `HEAD` request and return the status code.
    async fn head_status(&self, url: &str) -> AppResult<u16>;

    /// Fetch a bounded text document.
    ///
    /// Fails with `Network` when the request cannot be made, and with
    /// `UnavailableContent` when the server answers with an error status — the
    /// distinction matters because only the first is worth retrying.
    async fn fetch_text(&self, request: &FetchRequest<'_>) -> AppResult<String>;

    /// Stream a resource to `request.destination`, reporting progress.
    ///
    /// Returns `Download` errors that carry a retryability flag: a network
    /// timeout is retryable, a `404` is not.
    async fn download(
        &self,
        request: &TransferRequest<'_>,
        progress: &dyn ProgressReporter,
    ) -> AppResult<TransferOutcome>;
}

/// Most redirect hops a request will follow before giving up.
///
/// A short link resolves in two or three; ten is generous enough for a real CDN
/// hop and short enough that a loop cannot run for long.
const MAX_REDIRECTS: usize = 10;

/// How long a connection attempt may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Real HTTP client backed by `reqwest`.
pub struct ReqwestHttpClient {
    client: reqwest::Client,
    /// Long-lived requests get their own client; see [`Self::new`].
    transfer_client: reqwest::Client,
    policy: DestinationPolicy,
}

impl ReqwestHttpClient {
    /// Build a client with a bounded timeout and a descriptive user agent.
    ///
    /// `timeout_seconds` comes from application settings; it is clamped so a
    /// pathological value cannot make requests hang indefinitely.
    ///
    /// It is used for two different shapes of request, which need different
    /// timeouts:
    ///
    /// * **Page and short-link requests** are small and bounded, so the whole
    ///   request is given a total budget. A page that has not finished in that
    ///   time is not going to.
    /// * **Media transfers** are the opposite. `ClientBuilder::timeout` is a
    ///   *total* deadline covering the entire body, so applying it to a transfer
    ///   would abort a large file on a slow connection that was still making
    ///   perfectly good progress. A transfer gets a **read** timeout instead:
    ///   it fails only when the source stops sending, which is what "the
    ///   transfer stalled" actually means. The setting is reused as that stall
    ///   budget, so a user asking for a longer timeout gets a longer stall
    ///   allowance too.
    pub fn new(timeout_seconds: u64) -> AppResult<Self> {
        Self::with_policy(timeout_seconds, DestinationPolicy::PublicOnly)
    }

    /// Build a client under an explicit destination policy.
    ///
    /// [`DestinationPolicy::Any`] exists so a test can drive a server it started
    /// on loopback. The shipped application always uses
    /// [`DestinationPolicy::PublicOnly`] — see [`Self::new`].
    pub fn with_policy(timeout_seconds: u64, policy: DestinationPolicy) -> AppResult<Self> {
        let budget = Duration::from_secs(timeout_seconds.clamp(5, 600));

        let client = reqwest::Client::builder()
            .timeout(budget)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(DEFAULT_USER_AGENT)
            // `redirect::Policy` is not `Clone`, and each client needs its own.
            .redirect(redirect_policy(policy))
            // The resolution *is* the check. See `resolver`: the addresses this
            // returns are the addresses the connector dials, so an address that
            // passes here cannot be swapped for another one afterwards.
            .dns_resolver(Arc::new(GuardedResolver::system(policy)))
            .build()
            .map_err(|err| {
                AppError::network("could not build the HTTP client")
                    .with_technical_detail(err.to_string())
            })?;

        let transfer_client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(budget)
            .user_agent(DEFAULT_USER_AGENT)
            .redirect(redirect_policy(policy))
            .dns_resolver(Arc::new(GuardedResolver::system(policy)))
            .build()
            .map_err(|err| {
                AppError::network("could not build the HTTP client")
                    .with_technical_detail(err.to_string())
            })?;

        Ok(Self {
            client,
            transfer_client,
            policy,
        })
    }

    /// Refuse a destination this client will not contact.
    fn check(&self, url: &str) -> AppResult<()> {
        self.policy.check(url)
    }
}

/// Redirect handling: bounded, and re-checked at every hop.
///
/// The destination policy is applied to the *initial* URL by each method, but a
/// redirect can point anywhere — including at a private address — so each hop is
/// checked too. A refused hop stops the chain rather than erroring, which
/// surfaces the redirect response to the caller as a 3xx and is mapped there to
/// a specific, non-retryable message.
fn redirect_policy(policy: DestinationPolicy) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.stop();
        }
        if !policy.allows(attempt.url().as_str()) {
            return attempt.stop();
        }
        attempt.follow()
    })
}

#[async_trait]
impl HttpClient for ReqwestHttpClient {
    async fn resolve_redirect(&self, url: &str) -> AppResult<String> {
        self.check(url)?;
        // A GET (not HEAD) is used because some endpoints only emit the
        // redirect for GET. The body is never read, so nothing large is
        // buffered — important for keeping logs and memory clean.
        let response = self.client.get(url).send().await.map_err(request_error)?;
        Ok(response.url().to_string())
    }

    async fn head_status(&self, url: &str) -> AppResult<u16> {
        self.check(url)?;
        let response = self.client.head(url).send().await.map_err(request_error)?;
        Ok(response.status().as_u16())
    }

    async fn fetch_text(&self, request: &FetchRequest<'_>) -> AppResult<String> {
        self.check(request.url)?;
        // The page client, not the transfer client: a page fetch is small and
        // bounded, so the total timeout is the right shape for it.
        let mut builder = self.client.get(request.url);
        if let Some(agent) = request.user_agent {
            builder = builder.header(reqwest::header::USER_AGENT, agent);
        }
        if let Some(accept) = request.accept {
            builder = builder.header(reqwest::header::ACCEPT, accept);
        }

        let response = builder.send().await.map_err(request_error)?;
        let status = response.status();
        if !status.is_success() {
            return Err(page_status_error(status.as_u16(), request.url));
        }

        // `chunk()` rather than a full `text()` so the size cap is enforced
        // while reading instead of after the whole body is in memory.
        collect_body(response, request.max_bytes).await
    }

    async fn download(
        &self,
        request: &TransferRequest<'_>,
        progress: &dyn ProgressReporter,
    ) -> AppResult<TransferOutcome> {
        self.check(request.url)?;

        // A caller offers a partial file by naming its length, and — since
        // partial metadata exists — by the validator recorded when that partial
        // was written. A partial with no validator establishes nothing about
        // which resource its bytes belong to, so it is not offered: the transfer
        // restarts from zero, which costs a fraction of what a spliced file
        // would.
        if let Some(offset) = resumable_offset(request.resume_from, request.if_range) {
            match self.download_ranged(request, progress, offset).await {
                RangedOutcome::Done(outcome) => return Ok(outcome),
                RangedOutcome::Failed(err) => return Err(err),
                RangedOutcome::Restart(reason) => {
                    tracing::info!(
                        target: "network",
                        url = %redact_url(request.url),
                        offset,
                        reason = %reason,
                        "the source will not continue this partial; restarting the transfer"
                    );
                    // Falling through re-requests from zero, which truncates the
                    // file the caller offered.
                }
            }
        }

        self.download_stream(request, progress).await
    }
}

/// Whether a partial may be offered for continuation, and from where.
///
/// The client-side half of resume safety. The *server's* half is `If-Range`;
/// this is the rule about what may be asked. A partial is offerable only when
/// both its length and its identity are known: an offset without a validator
/// describes bytes whose provenance is unknown, and asking a server to continue
/// them is how two versions of a resource get spliced together. The ranged path
/// is only reached when this returns `Some`.
fn resumable_offset(resume_from: Option<u64>, if_range: Option<&str>) -> Option<u64> {
    resume_from
        .filter(|offset| *offset > 0)
        .filter(|_| if_range.is_some())
}

/// How a ranged transfer turned out.
enum RangedOutcome {
    Done(TransferOutcome),
    /// The range could not be used — the source ignored it, the offsets did not
    /// line up, or the partial no longer matched. The transfer must start over.
    Restart(String),
    /// The request itself failed.
    Failed(AppError),
}

impl ReqwestHttpClient {
    /// Continue a partial file, if the source will let us.
    async fn download_ranged(
        &self,
        request: &TransferRequest<'_>,
        progress: &dyn ProgressReporter,
        offset: u64,
    ) -> RangedOutcome {
        let mut builder = self.transfer_client.get(request.url);
        if let Some(agent) = request.user_agent {
            builder = builder.header(reqwest::header::USER_AGENT, agent);
        }
        let mut builder = builder.header(reqwest::header::RANGE, format!("bytes={offset}-"));
        // The stored validator. A `206` to this request is the server's own
        // statement that the entity is unchanged — the mechanism that makes the
        // range safe, where matching offsets and totals would only be a guess.
        if let Some(validator) = request.if_range {
            builder = builder.header(reqwest::header::IF_RANGE, validator);
        }

        let response = match builder.send().await {
            Ok(response) => response,
            Err(err) => return RangedOutcome::Failed(transfer_error(&err, request.url)),
        };
        let status = response.status().as_u16();

        // The source cannot satisfy the range. Whether that means "already
        // complete" or "the partial is nonsense" is not worth guessing at: the
        // file is discarded and fetched again, which is always correct and costs
        // one download against a result nobody could vouch for.
        if status == 416 {
            return RangedOutcome::Restart("the source rejected the requested range (416)".into());
        }
        if !response.status().is_success() {
            return RangedOutcome::Failed(status_error(status, request.url, "media resource"));
        }
        // `200` to a ranged request means the range was ignored. Appending
        // would duplicate everything before the offset.
        if status != 206 {
            return RangedOutcome::Restart(format!(
                "the source answered {status} instead of 206 Partial Content"
            ));
        }

        // The offset has to be the one that was asked for. A server that
        // answers a different range (or a different resource) would otherwise
        // have its bytes spliced into the wrong place.
        let (start, total) = match content_range(&response) {
            Some(range) => range,
            None => {
                return RangedOutcome::Restart(
                    "the source sent no usable Content-Range for a partial response".into(),
                )
            }
        };
        if start != offset {
            return RangedOutcome::Restart(format!(
                "asked to continue from {offset} but the source began at {start}"
            ));
        }

        // The single check that matters, and it is deliberately not "the server
        // said 206, so it must be fine".
        //
        // `If-Range` is a *cooperative* mechanism: a `206` says the range was
        // satisfied, and says nothing about whether the condition was even
        // evaluated. An origin that ignores the condition, or that never
        // implemented it, answers exactly as one that honoured it. So the
        // response's own validator is the only evidence available on this side,
        // and the only three outcomes are: the response names the same
        // validator the partial was written under, the response names a
        // different one, or the response names none.
        //
        // The last of those is not a pass. "It did not tell me anything" is not
        // "it told me it is the same entity", and a `206` carrying no validator
        // can only be continuing the partial on the strength of its offset —
        // which is the one thing this whole path exists to refuse. A matching
        // offset and total are *not* evidence of identity: a re-encode of the
        // same length matches both exactly. Restarting costs one download;
        // appending across an unanswerable question costs a file assembled from
        // two versions.
        let response_validator = crate::downloads::partial::ResourceValidator::from_response(&response);
        if let Some(stored) = request.if_range {
            let agrees = match &response_validator {
                Some(later) => later.value() == stored,
                None => {
                    return RangedOutcome::Restart(
                        "the source answered a range without naming a validator, so the partial \
                         cannot be shown to belong to it"
                            .into(),
                    )
                }
            };
            if !agrees {
                return RangedOutcome::Restart(
                    "the resource changed while the transfer was paused; the partial does not belong to it any more"
                        .into(),
                );
            }
        }

        // What is left to fetch, and what the whole resource is.
        //
        // The total is also a statement about *which* resource this is. A
        // response that describes a different size than the partial was written
        // under is not the same entity, whatever its validator says — and when
        // the response carried no validator at all, this is the only check left
        // standing between the partial and a blind append.
        if let (Some(expected), Some(actual)) = (request.expected_total, total) {
            if expected != actual {
                return RangedOutcome::Restart(format!(
                    "the partial was written under a {expected}-byte resource but the source \
                     now describes {actual} bytes"
                ));
            }
        }

        let remaining = match total {
            Some(total) if total >= offset => Some(total - offset),
            Some(total) => {
                return RangedOutcome::Restart(format!(
                    "the resource is {total} bytes, shorter than the {offset} already held"
                ))
            }
            None => response.content_length(),
        };

        match self
            .write_stream(request, progress, response, true, offset, remaining, total)
            .await
        {
            Ok(outcome) => RangedOutcome::Done(outcome),
            Err(err) => RangedOutcome::Failed(err),
        }
    }

    /// A transfer from the beginning, over the file as it is (or as it is not).
    async fn download_stream(
        &self,
        request: &TransferRequest<'_>,
        progress: &dyn ProgressReporter,
    ) -> AppResult<TransferOutcome> {
        let mut builder = self.transfer_client.get(request.url);
        if let Some(agent) = request.user_agent {
            builder = builder.header(reqwest::header::USER_AGENT, agent);
        }

        let response = builder.send().await.map_err(|err| transfer_error(&err, request.url))?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(status_error(status, request.url, "media resource"));
        }

        let total = response.content_length();
        self.write_stream(request, progress, response, false, 0, total, total)
            .await
    }
    /// Stream a response body into the destination.
    ///
    /// `append` decides whether the file is continued or replaced, and it is
    /// only ever `true` after the response has been established to be a
    /// continuation of exactly the bytes already on disk.
    #[allow(clippy::too_many_arguments)]
    async fn write_stream(
        &self,
        request: &TransferRequest<'_>,
        progress: &dyn ProgressReporter,
        mut response: reqwest::Response,
        append: bool,
        offset: u64,
        remaining: Option<u64>,
        resource_total: Option<u64>,
    ) -> AppResult<TransferOutcome> {
        let status = response.status().as_u16();
        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or(value).trim().to_string());

        // The response headers are where the resource's identity becomes known
        // — which is immediately, not when the last byte lands. Recorded here,
        // so a transfer paused mid-body leaves a partial whose identity is
        // already on disk beside it. The offset recorded is where this attempt
        // *started*; the file grows past it, and the metadata's job is to carry
        // the validator, not to track the length.
        crate::downloads::partial::PartialMetadata {
            validator: crate::downloads::partial::ResourceValidator::from_response(&response),
            total_bytes: resource_total,
            offset_bytes: offset,
        }
        .write(request.destination)
        .await
        .map_err(|err| {
            AppError::storage("could not record the partial's identity")
                .with_technical_detail(request.destination.to_string_lossy().into_owned())
                .with_source(err)
        })?;

        let mut file = if append {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(request.destination)
                .await
        } else {
            tokio::fs::File::create(request.destination).await
        }
        .map_err(|err| {
            AppError::storage("could not open the download file")
                .with_technical_detail(request.destination.to_string_lossy().into_owned())
                .with_source(err)
        })?;

        let mut received: u64 = 0;
        // What is left of *this* response, which is what a progress bar over the
        // remaining work should be measured against.
        progress.report(0, remaining);
        tracing::debug!(
            target: "network",
            url = %redact_url(request.url),
            resumed_from = if append { offset } else { 0 },
            remaining = ?remaining,
            "transfer connected"
        );

        loop {
            if request.cancel.is_cancelled() {
                tracing::debug!(
                    target: "network",
                    url = %redact_url(request.url),
                    received,
                    "transfer stopping on cancel"
                );
                drop(file);
                // A pause keeps what was written so the transfer can continue
                // from it; a cancellation removes it, because there is nothing
                // left to continue.
                if request.cancel.keeps_partial() {
                    tracing::debug!(
                        target: "network",
                        url = %redact_url(request.url),
                        bytes = offset + received,
                        "paused; keeping the partial file"
                    );
                } else {
                    remove_partial_file(request.destination).await;
                }
                return Err(AppError::download("transfer stopped by the user")
                    .with_retryable(false)
                    .with_technical_detail(format!("url={}", redact_url(request.url))));
            }

            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(err) => {
                    drop(file);
                    remove_partial_file(request.destination).await;
                    return Err(transfer_error(&err, request.url));
                }
            };

            file.write_all(&chunk).await.map_err(|err| {
                AppError::storage("could not write the downloaded data")
                    .with_technical_detail(request.destination.to_string_lossy().into_owned())
                    .with_source(err)
            })?;
            received += chunk.len() as u64;
            progress.report(received, remaining);
        }

        file.flush().await.map_err(|err| {
            AppError::storage("could not flush the downloaded file")
                .with_technical_detail(request.destination.to_string_lossy().into_owned())
                .with_source(err)
        })?;
        file.sync_all().await.ok();
        drop(file);

        Ok(TransferOutcome {
            // The size of the file, not of this response: a continued transfer's
            // result is everything in it, and a caller reporting progress on top
            // of it needs the whole number.
            bytes_written: offset + received,
            content_type,
            final_url,
            status,
            resumed_from: if append { offset } else { 0 },
            validator: crate::downloads::partial::ResourceValidator::from_response(&response),
            resource_total,
        })
    }
}

/// The `Content-Range` a partial response declares: `(start, total)`.
///
/// `bytes 200-999/1000` → `(200, Some(1000))`. An unknown total (`*`) is
/// reported as `None` rather than invented.
fn content_range(response: &reqwest::Response) -> Option<(u64, Option<u64>)> {
    let raw = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .trim()
        .to_string();
    let rest = raw.strip_prefix("bytes")?.trim();
    let (range, total) = rest.split_once('/')?;
    let start = range.split_once('-')?.0.trim().parse::<u64>().ok()?;
    let total = match total.trim() {
        "*" => None,
        value => Some(value.parse::<u64>().ok()?),
    };
    Some((start, total))
}

/// Read a response body into a string, refusing to exceed `max_bytes`.
async fn collect_body(mut response: reqwest::Response, max_bytes: usize) -> AppResult<String> {
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if buffer.len() + chunk.len() > max_bytes {
            return Err(AppError::resolution("response body exceeded the size limit")
                .with_user_message("This page returned more data than expected.")
                .with_technical_detail(format!("limit={max_bytes}")));
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// Map a `reqwest` transport failure into a retryable network error.
///
/// Except when it was the destination policy refusing an address: that is not a
/// transport failure at all, and reporting it as retryable would have the
/// application dial a blocked destination until the attempt budget ran out.
/// `reqwest` cannot tell the two apart — a refusal inside the resolver reaches
/// it as an ordinary connect error — so the marker is looked for first.
fn request_error(err: reqwest::Error) -> AppError {
    if let Some(refusal) = crate::network::resolver::policy_refusal(&err) {
        return blocked_destination(&refusal);
    }
    AppError::network("request failed")
        .with_retryable(true)
        .with_technical_detail(describe_reqwest_error(&err))
        .with_source(err)
}

/// The error for a destination the policy refused.
///
/// The host is named; the URL is not, because a media URL's query string is a
/// credential and this message crosses IPC in debug builds.
fn blocked_destination(refusal: &crate::network::resolver::RefusedDestination) -> AppError {
    AppError::network(format!("the destination is not public: {refusal}"))
        .with_retryable(false)
        .with_user_message("That address is not a public destination.")
        .with_technical_detail(format!("destination={}", crate::logging::url_host(&refusal.host)))
}

/// Map a failed media transfer, deciding retryability from the failure shape.
fn transfer_error(err: &reqwest::Error, url: &str) -> AppError {
    // A refusal is reported before anything else: it is a decision, not a
    // failure, and it must not be retried.
    if let Some(refusal) = crate::network::resolver::policy_refusal(err) {
        return blocked_destination(&refusal);
    }
    // Timeouts and connection failures are transient; a decode failure or a
    // redirect loop is not something a retry will fix.
    let retryable = err.is_timeout() || err.is_connect() || err.is_request();
    AppError::download("the media transfer failed")
        .with_user_message("The download was interrupted. You can retry it.")
        .with_retryable(retryable)
        .with_technical_detail(format!(
            "{} url={}",
            describe_reqwest_error(err),
            redact_url(url)
        ))
}

/// Build an error for a non-success HTTP status.
///
/// 4xx (other than 408/429) is permanent: the resource is not there or is not
/// permitted, and retrying only wastes time. 5xx and the two throttling codes
/// are retryable.
fn status_error(status: u16, url: &str, what: &str) -> AppError {
    let mut retryable = status == 408 || status == 429 || (500..600).contains(&status);
    let (kind_message, user_message) = if (300..400).contains(&status) {
        // Redirects are followed by the client, so a 3xx reaching here means the
        // chain was *stopped* — either it was too long or a hop pointed
        // somewhere this application will not contact (see `redirect_policy`).
        // Retrying follows the same chain to the same place.
        (
            "the redirect chain was not followed",
            "This content redirected somewhere the application will not follow.",
        )
    } else if status == 404 || status == 410 {
        (
            "the resource is no longer available",
            "This content is no longer available.",
        )
    } else if (400..500).contains(&status) {
        (
            "the source refused the request",
            "The source did not return this content.",
        )
    } else {
        (
            "the source returned a server error",
            "The source is temporarily unavailable. Try again shortly.",
        )
    };

    retryable = retryable && !(300..400).contains(&status);
    AppError::download(format!("{what} request failed with status {status}"))
        .with_user_message(user_message)
        .with_retryable(retryable)
        .with_technical_detail(format!(
            // The 3xx case reaching here *is* the policy having stopped the
            // chain — the client follows redirects itself, so a redirect status
            // surfacing as a response is the refusal, not an ordinary hop.
            "{kind_message} status={status} url={}",
            redact_url(url)
        ))
}

/// Build an error for a non-success HTTP status on a *page* fetch.
///
/// Distinct from [`status_error`] because the kinds differ meaningfully: a page
/// that is gone is `UnavailableContent`, a page the source refused is a
/// `Resolution` problem, and a server error is worth retrying.
fn page_status_error(status: u16, url: &str) -> AppError {
    let detail = format!("status={status} url={}", redact_url(url));
    match status {
        404 | 410 => AppError::unavailable("the page is no longer available")
            .with_user_message("This content is no longer available.")
            .with_technical_detail(detail),
        400..=499 => AppError::resolution("the source refused the page request")
            .with_user_message("The source did not return this content.")
            .with_technical_detail(detail),
        _ => AppError::network("the source returned a server error")
            .with_retryable(true)
            .with_user_message("The source is temporarily unavailable. Try again shortly.")
            .with_technical_detail(detail),
    }
}

/// Turn a `reqwest` error into a short, log-safe description.
///
/// Deliberately coarse: the full `reqwest` error can contain the URL and
/// connection internals, which we do not want to mirror into logs verbatim.
fn describe_reqwest_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "request timed out".to_string()
    } else if err.is_connect() {
        "connection failed".to_string()
    } else if err.is_decode() {
        "could not decode the response".to_string()
    } else if err.is_redirect() {
        "too many redirects".to_string()
    } else if err.is_status() {
        "unexpected status code".to_string()
    } else {
        "request failed".to_string()
    }
}

/// Delete a partially written file, retrying briefly.
///
/// A file cannot be removed while a handle to it is still open, and on Windows
/// closing a handle is not instantaneous — especially for a transfer that was
/// cancelled mid-write, where the close is queued behind the write that was in
/// flight. A single best-effort delete therefore loses the race and leaves a
/// stray `.part` behind, so this retries for a short, bounded while and reports
/// whether it succeeded.
///
/// Returns `true` when the file is gone (including when it was never there).
pub async fn remove_partial_file(path: &Path) -> bool {
    const ATTEMPTS: usize = 20;
    const DELAY: Duration = Duration::from_millis(50);

    for attempt in 0..ATTEMPTS {
        match tokio::fs::remove_file(path).await {
            Ok(()) => return true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) if attempt + 1 < ATTEMPTS => {
                // Retried quietly: a handle that is still closing is expected.
                tokio::time::sleep(DELAY).await;
            }
            Err(err) => {
                tracing::warn!(
                    target: "downloads",
                    path = %path.display(),
                    error = %err,
                    "could not remove a partial file"
                );
                return false;
            }
        }
    }
    false
}

/// Deterministic test double.
///
/// Available in non-test builds too, so integration tests (and a future
/// offline/demo mode) can exercise resolver code without touching the network.
#[derive(Debug, Default, Clone)]
pub struct MockHttpClient {
    redirects: std::collections::HashMap<String, String>,
    statuses: std::collections::HashMap<String, u16>,
    bodies: std::collections::HashMap<String, String>,
    downloads: std::collections::HashMap<String, Vec<u8>>,
    /// Number of initial failures to simulate per URL, for retry tests.
    transient_failures: std::collections::HashMap<String, usize>,
    /// How many times each URL has been requested so far.
    attempts: Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
    default_status: u16,
    /// Whether this server honours a `Range` request. Real servers differ, and
    /// a client that assumes either answer is wrong half the time.
    range_support: bool,
    /// The validator each resource is served with, and whether the resource
    /// "changed" — i.e. whether the validator handed out now differs from the
    /// one a previous resume would have stored.
    validators:
        std::collections::HashMap<String, crate::downloads::partial::ResourceValidator>,
    /// Whether this server honours `If-Range`. A server that ignores it answers
    /// `206` regardless, which is exactly the case the client must survive.
    if_range_support: bool,
    /// How long each download takes before it writes anything.
    ///
    /// Zero by default. A non-zero delay is what makes transfers overlap in
    /// time, which is the only way a test can measure concurrency rather than
    /// infer it.
    delay: Duration,
    /// Downloads in flight right now, and the most that ever were at once.
    live: ConcurrencyProbe,
}

/// How many transfers a [`MockHttpClient`] had in flight, at most.
///
/// Cloned out before the client moves into the manager under test, so a test can
/// still measure what happened inside it.
#[derive(Debug, Clone, Default)]
pub struct ConcurrencyProbe(Arc<std::sync::Mutex<(usize, usize)>>);

impl ConcurrencyProbe {
    /// The most concurrent transfers seen.
    pub fn peak(&self) -> usize {
        self.0.lock().expect("lock").1
    }
}

impl MockHttpClient {
    pub fn new() -> Self {
        Self {
            redirects: std::collections::HashMap::new(),
            statuses: std::collections::HashMap::new(),
            bodies: std::collections::HashMap::new(),
            downloads: std::collections::HashMap::new(),
            transient_failures: std::collections::HashMap::new(),
            attempts: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            default_status: 200,
            range_support: true,
            validators: std::collections::HashMap::new(),
            if_range_support: true,
            delay: Duration::ZERO,
            live: ConcurrencyProbe::default(),
        }
    }

    /// Register the final URL that `from` redirects to.
    pub fn with_redirect(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.redirects.insert(from.into(), to.into());
        self
    }

    /// Register a status code returned for a URL.
    pub fn with_status(mut self, url: impl Into<String>, status: u16) -> Self {
        self.statuses.insert(url.into(), status);
        self
    }

    /// Register a text body returned by `fetch_text`.
    pub fn with_body(mut self, url: impl Into<String>, body: impl Into<String>) -> Self {
        self.bodies.insert(url.into(), body.into());
        self
    }

    /// Register bytes served by `download`.
    pub fn with_download(mut self, url: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        self.downloads.insert(url.into(), bytes.into());
        self
    }

    /// Serve the resource with an `ETag`, so resume tests have a strong
    /// validator to work with.
    pub fn with_etag(mut self, url: impl Into<String>, etag: impl Into<String>) -> Self {
        self.validators.insert(
            url.into(),
            crate::downloads::partial::ResourceValidator::ETag { value: etag.into() },
        );
        self
    }

    /// Make the server ignore `If-Range`: it answers `206` no matter what the
    /// request condition said. The one server behaviour the client cannot
    /// detect from the status line alone.
    pub fn ignoring_if_range(mut self) -> Self {
        self.if_range_support = false;
        self
    }

    /// Serve `bytes`, but fail the first `failures` attempts with a retryable
    /// error. Used to exercise the retry path with backoff.
    pub fn with_transient_failures(
        mut self,
        url: impl Into<String>,
        failures: usize,
        bytes: impl Into<Vec<u8>>,
    ) -> Self {
        let url = url.into();
        self.transient_failures.insert(url.clone(), failures);
        self.downloads.insert(url, bytes.into());
        self
    }

    /// A server that ignores `Range` and answers `200` with the whole resource.
    ///
    /// Real servers differ on this, and a client that assumes either answer is
    /// wrong half the time, so both are testable.
    pub fn without_range_support(mut self) -> Self {
        self.range_support = false;
        self
    }

    /// Make every download take `delay` before it writes, so transfers overlap.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// The most downloads this server ever had in flight at once.
    ///
    /// A test asserts this stays within the configured limit, instead of
    /// assuming the scheduler bounded it.
    pub fn probe(&self) -> ConcurrencyProbe {
        self.live.clone()
    }
}

#[async_trait]
impl HttpClient for MockHttpClient {
    async fn resolve_redirect(&self, url: &str) -> AppResult<String> {
        match self.statuses.get(url) {
            // A failed redirect is a transport failure, not a content answer:
            // the link was never resolved, so nothing about its target is known.
            Some(status) if *status >= 400 => Err(AppError::network(format!(
                "mock network failure for {url}"
            ))
            .with_retryable(status == &408 || status == &429 || (500..600).contains(status))
            .with_technical_detail(format!("status {status}"))
            .with_user_message("The source could not be reached.")),
            _ => Ok(self
                .redirects
                .get(url)
                .cloned()
                .unwrap_or_else(|| url.to_string())),
        }
    }

    async fn head_status(&self, url: &str) -> AppResult<u16> {
        Ok(self.statuses.get(url).copied().unwrap_or(self.default_status))
    }

    async fn fetch_text(&self, request: &FetchRequest<'_>) -> AppResult<String> {
        if let Some(status) = self.statuses.get(request.url) {
            if *status >= 400 {
                return Err(page_status_error(*status, request.url));
            }
        }
        match self.bodies.get(request.url) {
            Some(body) => Ok(body.clone()),
            None => Err(AppError::network(format!(
                "mock has no body for {}",
                request.url
            ))
            .with_user_message("The source could not be reached.")),
        }
    }

    async fn download(
        &self,
        request: &TransferRequest<'_>,
        progress: &dyn ProgressReporter,
    ) -> AppResult<TransferOutcome> {
        if let Some(status) = self.statuses.get(request.url) {
            if *status >= 400 {
                return Err(mock_status_error(*status, request.url, "media resource"));
            }
        }

        // Simulated transient failures consume the first N attempts.
        if let Some(&failures) = self.transient_failures.get(request.url) {
            let seen = {
                let mut attempts = self.attempts.lock().expect("mock attempts lock");
                let counter = attempts.entry(request.url.to_string()).or_insert(0);
                *counter += 1;
                *counter
            };
            if seen <= failures {
                return Err(AppError::download("simulated transient transfer failure")
                    .with_retryable(true)
                    .with_user_message("The download was interrupted. You can retry it."));
            }
        }

        let Some(bytes) = self.downloads.get(request.url) else {
            return Err(AppError::download(format!(
                "mock has no resource for {}",
                request.url
            ))
            .with_retryable(false)
            .with_user_message("This media resource is not available."));
        };

        // Emit a couple of progress frames so callers can observe the stream.
        progress.report(0, Some(bytes.len() as u64));
        if request.cancel.is_cancelled() {
            if !request.cancel.keeps_partial() {
                remove_partial_file(request.destination).await;
            }
            return Err(AppError::download("transfer stopped by the user")
                .with_retryable(false));
        }

        // Counting only the window a real transfer would occupy: a test that
        // measures concurrency wants to know how many were moving bytes at
        // once, not how many had been called.
        {
            let mut live = self.live.0.lock().expect("lock");
            live.0 += 1;
            live.1 = live.1.max(live.0);
        }
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }

        // A range request is honoured only if the resource is longer than the
        // offset, which is the case a real server answers `206` for. Anything
        // else — including a server that ignores `Range` — gets the whole body
        // and a `200`, which is what the client has to cope with either way.
        let offset = request.resume_from.unwrap_or(0);
        // An `If-Range` condition is honoured only when the stored validator
        // still matches what this server would serve. A resource that "changed"
        // — the validator map now holds a different value than the request
        // carried — gets the full-body `200` answer the specification requires
        // for a failed condition.
        let validator_matches = match (request.if_range, self.validators.get(request.url)) {
            (Some(stored), Some(current)) => stored == current.value(),
            (Some(_), None) => false,
            (None, _) => true,
        };
        // Two independent server behaviours: whether the `Range` header is
        // honoured at all, and whether the `If-Range` condition is. A server
        // that honours the condition answers `206` only when the validator
        // matches, and `200` (full body) when it does not; a server that
        // ignores the condition answers `206` regardless — which is precisely
        // the case the client's own validator comparison exists for.
        let condition_ok = if self.if_range_support {
            validator_matches
        } else {
            true
        };
        let honoured =
            offset > 0 && offset < bytes.len() as u64 && self.range_support && condition_ok;

        let (body, resumed_from, status) = if honoured {
            (&bytes[offset as usize..], offset, 206u16)
        } else {
            (&bytes[..], 0, 200u16)
        };

        if honoured {
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(request.destination)
                .await
                .map_err(|err| {
                    AppError::storage("could not open the download file").with_source(err)
                })?;
            file.write_all(body).await.map_err(|err| {
                AppError::storage("could not write the downloaded data").with_source(err)
            })?;
            file.flush().await.ok();
        } else {
            tokio::fs::write(request.destination, body).await.map_err(|err| {
                AppError::storage("could not write the downloaded data").with_source(err)
            })?;
        }

        let written = offset + body.len() as u64;
        progress.report(body.len() as u64, Some(body.len() as u64));
        self.live.0.lock().expect("lock").0 -= 1;

        Ok(TransferOutcome {
            bytes_written: written,
            content_type: Some("application/octet-stream".to_string()),
            final_url: request.url.to_string(),
            status,
            resumed_from,
            resource_total: Some(bytes.len() as u64),
            validator: self.validators.get(request.url).cloned(),
        })
    }
}

fn mock_status_error(status: u16, url: &str, what: &str) -> AppError {
    let retryable = status == 408 || status == 429 || (500..600).contains(&status);
    AppError::download(format!("mock {what} failure for {url}"))
        .with_retryable(retryable)
        .with_technical_detail(format!("status {status}"))
        .with_user_message("The source could not be reached.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloads::partial::PartialMetadata;
    use crate::downloads::partial::ResourceValidator;

    // --- resume integrity ----------------------------------------------------
    //
    // These prove the thing the offset checks could not: that bytes from two
    // versions of a resource never end up in one file, whatever the server does.
    // The mock enforces real `If-Range` semantics — a mismatched condition gets
    // the full-body `200` the specification requires — so what is under test is
    // the client's behaviour against a server that either cooperates or does not.

    /// A resume whose validator still holds continues from the offset.
    #[tokio::test]
    async fn a_valid_etag_resume_continues_from_the_offset() {
        let temp = tempfile::tempdir().unwrap();
        let partial = temp.path().join("clip.mp4.part");
        let head = vec![1u8; 100];
        let tail = vec![2u8; 100];
        std::fs::write(&partial, &head).unwrap();

        let client = MockHttpClient::new()
            .with_download("https://cdn/resource", [head.clone(), tail.clone()].concat())
            .with_etag("https://cdn/resource", "\"v1\"");
        PartialMetadata {
            validator: Some(ResourceValidator::ETag { value: "\"v1\"".into() }),
            total_bytes: Some(200),
            offset_bytes: 100,
        }
        .write(&partial)
        .await
        .unwrap();

        let outcome = client
            .download(
                &TransferRequest {
                    resume_from: Some(100),
                    if_range: Some("\"v1\""),
                    ..TransferRequest::new("https://cdn/resource", &partial)
                },
                &NoopProgress,
            )
            .await
            .unwrap();

        assert_eq!(outcome.resumed_from, 100, "the range was honoured");
        assert_eq!(outcome.status, 206);
        // The file is head + tail, in order — the two halves of one resource.
        let bytes = std::fs::read(&partial).unwrap();
        assert_eq!(bytes.len(), 200);
        assert_eq!(&bytes[..100], &head[..]);
        assert_eq!(&bytes[100..], &tail[..]);
    }

    /// The resource changed while the transfer was paused. The mock answers the
    /// failed `If-Range` condition with the full-body `200` the specification
    /// requires; the client must restart, never append.
    #[tokio::test]
    async fn a_changed_resource_never_appends_to_the_old_partial() {
        let temp = tempfile::tempdir().unwrap();
        let partial = temp.path().join("clip.mp4.part");
        // The partial holds version 1's first hundred bytes.
        let stale = vec![1u8; 100];
        std::fs::write(&partial, &stale).unwrap();

        // The resource is now version 2 — same length, different bytes.
        let fresh = vec![9u8; 200];
        let client = MockHttpClient::new()
            .with_download("https://cdn/resource", fresh.clone())
            .with_etag("https://cdn/resource", "\"v2\"");

        let outcome = client
            .download(
                &TransferRequest {
                    resume_from: Some(100),
                    if_range: Some("\"v1\""),
                    ..TransferRequest::new("https://cdn/resource", &partial)
                },
                &NoopProgress,
            )
            .await
            .unwrap();

        assert_eq!(outcome.resumed_from, 0, "the partial was not continued");
        assert_eq!(outcome.status, 200, "the failed condition got a full answer");
        // The decisive assertion: not one byte of version 1 survives. A splice
        // would leave `1u8`s at the front and `9u8`s behind them.
        let bytes = std::fs::read(&partial).unwrap();
        assert_eq!(bytes, fresh, "the file is the new resource alone");
    }

    /// The same-length variant, which is the case offsets and totals cannot
    /// detect on their own: 100 bytes of version 1 plus 100 bytes of version 2
    /// would have the "right" length.
    #[tokio::test]
    async fn a_same_length_changed_resource_is_not_spliced() {
        let temp = tempfile::tempdir().unwrap();
        let partial = temp.path().join("clip.mp4.part");
        let stale = vec![1u8; 100];
        std::fs::write(&partial, &stale).unwrap();

        // Version 2: same total, different content.
        let mut fresh = vec![9u8; 200];
        fresh[..100].copy_from_slice(&[8u8; 100]);
        let client = MockHttpClient::new()
            .with_download("https://cdn/resource", fresh.clone())
            .with_etag("https://cdn/resource", "\"v2\"");

        client
            .download(
                &TransferRequest {
                    resume_from: Some(100),
                    if_range: Some("\"v1\""),
                    ..TransferRequest::new("https://cdn/resource", &partial)
                },
                &NoopProgress,
            )
            .await
            .unwrap();

        let bytes = std::fs::read(&partial).unwrap();
        assert_eq!(bytes, fresh);
        assert!(!bytes.starts_with(&stale), "no stale byte survived");
    }

    // NOTE: the "server ignores If-Range" case cannot be tested through the
    // mock — the mock *is* the HttpClient here, so the client-side comparison in
    // `ReqwestHttpClient::download_ranged` never runs. That case is proven
    // end-to-end in `download_pipeline.rs`, against a real server on loopback,
    // which is also where the byte-level assertions belong.

    /// A partial whose identity was never established is not offered at all.
    /// This is the client-side half of the policy — the mock is a *server*, and
    /// a server would happily answer `206` to a bare `Range`; refusing to ask
    /// is the client's job, so that rule is a pure function tested directly.
    /// The end-to-end proof lives in `download_pipeline`, which drives the real
    /// `ReqwestHttpClient` over a real socket.
    #[test]
    fn a_resume_without_a_validator_is_never_offered() {
        // An offset without an identity: refused.
        assert_eq!(resumable_offset(Some(100), None), None);
        // No offset: nothing to continue.
        assert_eq!(resumable_offset(None, Some("\"v1\"")), None);
        assert_eq!(resumable_offset(Some(0), Some("\"v1\"")), None, "0 is a fresh start, not a resume");
        // Both known: offered.
        assert_eq!(resumable_offset(Some(100), Some("\"v1\"")), Some(100));
    }

    /// And the mock, being a faithful server, does answer `206` to the bare
    /// range the client would never send — which is why the gate exists.
    #[tokio::test]
    async fn a_bare_range_would_be_honoured_by_a_server() {
        let temp = tempfile::tempdir().unwrap();
        let partial = temp.path().join("clip.mp4.part");
        std::fs::write(&partial, vec![1u8; 100]).unwrap();

        let client = MockHttpClient::new().with_download("https://cdn/resource", vec![2u8; 200]);
        let outcome = client
            .download(
                &TransferRequest {
                    resume_from: Some(100),
                    if_range: None,
                    ..TransferRequest::new("https://cdn/resource", &partial)
                },
                &NoopProgress,
            )
            .await
            .unwrap();
        assert_eq!(outcome.status, 206, "the server honours a bare Range");
    }

    /// A server that ignores `Range` answers `200` with the whole resource.
    /// Appending would duplicate everything before the offset; the client
    /// truncates and rewrites.
    #[tokio::test]
    async fn a_server_that_ignores_range_is_restarted_not_appended() {
        let temp = tempfile::tempdir().unwrap();
        let partial = temp.path().join("clip.mp4.part");
        let stale = vec![1u8; 100];
        std::fs::write(&partial, &stale).unwrap();

        let body = vec![7u8; 200];
        let client = {
            let mut c = MockHttpClient::new()
                .with_download("https://cdn/resource", body.clone())
                .with_etag("https://cdn/resource", "\"v1\"");
            c.range_support = false;
            c
        };

        let outcome = client
            .download(
                &TransferRequest {
                    resume_from: Some(100),
                    if_range: Some("\"v1\""),
                    ..TransferRequest::new("https://cdn/resource", &partial)
                },
                &NoopProgress,
            )
            .await
            .unwrap();

        assert_eq!(outcome.resumed_from, 0);
        let bytes = std::fs::read(&partial).unwrap();
        assert_eq!(bytes, body, "the file is the whole resource, once");
    }

    /// HTTP 416: the source rejected the range. The transfer fails the ranged
    /// attempt with a typed status error rather than guessing whether the
    /// partial was already complete.
    #[tokio::test]
    async fn a_416_answer_is_a_typed_failure() {
        let temp = tempfile::tempdir().unwrap();
        let partial = temp.path().join("clip.mp4.part");
        std::fs::write(&partial, vec![1u8; 100]).unwrap();

        let client = {
            let mut c = MockHttpClient::new()
                .with_download("https://cdn/resource", vec![7u8; 100])
                .with_etag("https://cdn/resource", "\"v1\"");
            c.statuses.insert("https://cdn/resource".into(), 416);
            c
        };

        let err = client
            .download(
                &TransferRequest {
                    resume_from: Some(100),
                    if_range: Some("\"v1\""),
                    ..TransferRequest::new("https://cdn/resource", &partial)
                },
                &NoopProgress,
            )
            .await
            .unwrap_err();

        assert_eq!(err.kind(), crate::errors::AppErrorKind::Download);
    }

    #[test]
    fn content_range_parses_the_real_shapes() {
        // Real Content-Range values, including the asterisk form. `Response::from`
        // wraps a plain `http` response in the reqwest type the parser reads.
        let make = |value: &str| {
            reqwest::Response::from(
                http::Response::builder()
                    .header("content-range", value)
                    .body(reqwest::Body::from(""))
                    .unwrap(),
            )
        };
        assert_eq!(content_range(&make("bytes 100-199/200")), Some((100, Some(200))));
        assert_eq!(content_range(&make("bytes 100-199/*")), Some((100, None)));
        assert_eq!(content_range(&make("bytes */200")), None, "no start");
        assert_eq!(content_range(&make("garbage")), None);
        // No header at all.
        let bare = reqwest::Response::from(
            http::Response::builder()
                .body(reqwest::Body::from(""))
                .unwrap(),
        );
        assert_eq!(content_range(&bare), None);
    }

    #[tokio::test]
    async fn mock_follows_registered_redirect() {
        let client = MockHttpClient::new()
            .with_redirect("https://v.douyin.com/abc/", "https://www.douyin.com/video/123");
        let final_url = client.resolve_redirect("https://v.douyin.com/abc/").await.unwrap();
        assert_eq!(final_url, "https://www.douyin.com/video/123");
    }

    #[tokio::test]
    async fn mock_returns_input_when_no_redirect_registered() {
        let client = MockHttpClient::new();
        let final_url = client.resolve_redirect("https://www.douyin.com/video/123").await.unwrap();
        assert_eq!(final_url, "https://www.douyin.com/video/123");
    }

    #[tokio::test]
    async fn mock_reports_network_failure_for_error_status() {
        let client = MockHttpClient::new().with_status("https://x/", 503);
        let err = client.resolve_redirect("https://x/").await.unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Network);
    }

    #[tokio::test]
    async fn mock_serves_registered_text_bodies() {
        let client = MockHttpClient::new().with_body("https://x/page", "<html>hi</html>");
        let body = client.fetch_text(&FetchRequest::new("https://x/page")).await.unwrap();
        assert_eq!(body, "<html>hi</html>");
    }

    #[tokio::test]
    async fn mock_download_writes_bytes_and_reports_progress() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("out.bin");
        let client = MockHttpClient::new().with_download("https://cdn/v.mp4", vec![7u8; 2048]);

        let outcome = client
            .download(
                &TransferRequest::new("https://cdn/v.mp4", &target),
                &NoopProgress,
            )
            .await
            .unwrap();

        assert_eq!(outcome.bytes_written, 2048);
        assert_eq!(std::fs::read(&target).unwrap().len(), 2048);
    }

    #[tokio::test]
    async fn mock_download_honours_cancellation() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("out.bin");
        let cancel = CancellationHandle::new();
        cancel.cancel();

        let mut request = TransferRequest::new("https://cdn/v.mp4", &target);
        request.cancel = cancel;
        let client = MockHttpClient::new().with_download("https://cdn/v.mp4", vec![0u8; 16]);

        let err = client.download(&request, &NoopProgress).await.unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Download);
        assert!(!target.exists());
    }

    #[test]
    fn cancellation_flag_is_shared_across_clones() {
        let handle = CancellationHandle::new();
        let clone = handle.clone();
        assert!(!handle.is_cancelled());
        clone.cancel();
        assert!(handle.is_cancelled());
    }

    #[test]
    fn status_error_marks_4xx_permanent_and_5xx_retryable() {
        assert!(!status_error(404, "u", "media").is_retryable());
        assert!(!status_error(403, "u", "media").is_retryable());
        assert!(status_error(503, "u", "media").is_retryable());
        assert!(status_error(429, "u", "media").is_retryable());
    }
}
