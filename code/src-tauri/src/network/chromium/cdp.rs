//! Minimal Chrome DevTools Protocol client.
//!
//! Only the commands this application needs are modelled: enable the Page,
//! Runtime, and Network domains, navigate, evaluate an expression, and collect
//! the network events that arrive while doing so. There is no session
//! multiplexing and no target management — every call is a request/response
//! round trip on one connection to one page target.
//!
//! Keeping it this small is deliberate. A general-purpose Chrome automation
//! layer would be a second system to maintain, and everything it would add is
//! something this application has decided not to do.
//!
//! # Events
//!
//! A page's traffic only arrives as protocol *events*, which a request/response
//! client has nothing to do with. Rather than grow an event router, the client
//! keeps a small bounded buffer of the events worth having — network activity,
//! and nothing else — which the caller drains when it wants to look. Events are
//! collected only while a command is being awaited, which is exactly when the
//! page is being driven, and the buffer is capped so a busy page cannot turn
//! "watch the network" into unbounded memory.

use std::collections::VecDeque;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::errors::{AppError, AppResult};

/// How many protocol events are kept before the oldest are discarded.
///
/// A media request happens early in a page's life; dropping the oldest keeps the
/// recent window, which is where a second track appears.
const MAX_BUFFERED_EVENTS: usize = 512;

/// Only these events are kept. Everything else — DOM, runtime, lifecycle — is
/// dropped at capture time so unrelated activity cannot grow the buffer.
const KEPT_EVENT_PREFIX: &str = "Network.";

/// A connection to one page target's DevTools endpoint.
pub struct CdpClient {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    events: VecDeque<Value>,
    dropped_events: u64,
}

impl CdpClient {
    /// Connect to a `ws://` DevTools endpoint.
    ///
    /// Failure here means the browser is not usable, which is reported as
    /// `BrowserUnavailable` so the caller can fall back to a typed error rather
    /// than a protocol dump.
    pub async fn connect(ws_url: &str, timeout: Duration) -> AppResult<Self> {
        let connected = tokio::time::timeout(timeout, connect_async(ws_url))
            .await
            .map_err(|_| {
                AppError::browser_unavailable("timed out connecting to the browser")
                    .with_technical_detail(ws_url.to_string())
            })?
            .map_err(|err| {
                AppError::browser_unavailable("could not connect to the browser")
                    .with_technical_detail(err.to_string())
            })?;

        Ok(Self {
            socket: connected.0,
            next_id: 0,
            events: VecDeque::new(),
            dropped_events: 0,
        })
    }

    /// Record a protocol event, keeping the buffer bounded.
    fn observe(&mut self, message: Value) {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return;
        };
        if !method.starts_with(KEPT_EVENT_PREFIX) {
            return;
        }
        if self.events.len() >= MAX_BUFFERED_EVENTS {
            self.events.pop_front();
            self.dropped_events += 1;
        }
        // Moved, not cloned. This runs for every event that arrives while a
        // command is in flight, and the reply to that command is read off the
        // same socket only after this backlog has been drained — so the cost per
        // event is the cost of the command's latency. A deep clone of the parsed
        // event was the bulk of it.
        self.events.push_back(message);
    }

    /// Take everything observed since the last drain.
    pub fn take_events(&mut self) -> Vec<Value> {
        self.dropped_events = 0;
        self.events.drain(..).collect()
    }

    /// Discard anything observed so far.
    ///
    /// Called before navigating, so what the caller collects afterwards belongs
    /// to the page it actually asked for rather than to `about:blank`.
    pub fn clear_events(&mut self) {
        self.events.clear();
        self.dropped_events = 0;
    }

    /// How many events were discarded because the buffer was full.
    pub fn dropped_events(&self) -> u64 {
        self.dropped_events
    }

    /// Send one command and wait for its reply.
    ///
    /// Events that arrive while waiting are buffered rather than answered; none
    /// of the commands used here depend on one being delivered mid-call.
    pub async fn call(&mut self, method: &str, params: Value, timeout: Duration) -> AppResult<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let payload = json!({ "id": id, "method": method, "params": params }).to_string();

        let exchange = async {
            self.socket
                .send(Message::Text(payload))
                .await
                .map_err(|err| protocol_error(method, err.to_string()))?;

            loop {
                match self.socket.next().await {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(message) = serde_json::from_str::<Value>(text.as_str()) else {
                            continue;
                        };
                        if message.get("id").and_then(Value::as_u64) != Some(id) {
                            // An event, not our reply.
                            self.observe(message);
                            continue;
                        }
                        if let Some(error) = message.get("error") {
                            let detail = error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown protocol error");
                            return Err(protocol_error(method, detail));
                        }
                        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        return Err(protocol_error(method, "the browser closed the connection"))
                    }
                    // Ping, Pong, and Binary frames are not part of this
                    // conversation.
                    Some(Ok(_)) => continue,
                    Some(Err(err)) => return Err(protocol_error(method, err.to_string())),
                }
            }
        };

        tokio::time::timeout(timeout, exchange).await.map_err(|_| {
            AppError::browser_unavailable(format!("the browser did not answer `{method}` in time"))
                .with_technical_detail(format!("timeout={}ms", timeout.as_millis()))
        })?
    }

    /// Enable the Page domain.
    pub async fn enable_page(&mut self, timeout: Duration) -> AppResult<()> {
        self.call("Page.enable", json!({}), timeout).await.map(|_| ())
    }

    /// Enable the Runtime domain.
    pub async fn enable_runtime(&mut self, timeout: Duration) -> AppResult<()> {
        self.call("Runtime.enable", json!({}), timeout).await.map(|_| ())
    }

    /// Enable the Network domain.
    ///
    /// No response bodies are requested — only the fact that a request happened
    /// and what the server called it — so enabling this costs a page's worth of
    /// small event records rather than its traffic.
    pub async fn enable_network(&mut self, timeout: Duration) -> AppResult<()> {
        self.call(
            "Network.enable",
            json!({
                "maxTotalBufferSize": 8 * 1024 * 1024,
                "maxResourceBufferSize": 1024 * 1024,
                "maxPostDataSize": 0,
            }),
            timeout,
        )
        .await
        .map(|_| ())
    }

    /// Navigate the page and wait for the navigation command to be accepted.
    ///
    /// This does not wait for the page to finish loading — the caller polls for
    /// the condition it actually cares about, which is more reliable than
    /// guessing at load events on a page that renders itself continuously.
    pub async fn navigate(&mut self, url: &str, timeout: Duration) -> AppResult<()> {
        self.call("Page.navigate", json!({ "url": url }), timeout)
            .await
            .map(|_| ())
    }

    /// Evaluate a JavaScript expression and return its value.
    ///
    /// Returns `Ok(None)` when the expression evaluated to `undefined` or threw —
    /// both mean "not available yet" to the polling caller, which is not an
    /// error worth propagating.
    pub async fn evaluate(&mut self, expression: &str, timeout: Duration) -> AppResult<Option<Value>> {
        let result = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                    // A page that throws should not abort the whole render.
                    "silent": true,
                }),
                timeout,
            )
            .await?;

        if result.get("exceptionDetails").is_some() {
            return Ok(None);
        }

        Ok(result
            .get("result")
            .and_then(|inner| inner.get("value"))
            .filter(|value| !value.is_null())
            .cloned())
    }

    /// The page's current URL, as the browser sees it.
    pub async fn current_url(&mut self, timeout: Duration) -> AppResult<Option<String>> {
        let value = self
            .evaluate("document.location.href", timeout)
            .await?;
        Ok(value.and_then(|value| value.as_str().map(str::to_string)))
    }

    /// Close the socket politely. Failure is not interesting — the process is
    /// about to be terminated regardless.
    pub async fn close(&mut self) {
        let _ = self.socket.close(None).await;
    }
}

fn protocol_error(method: &str, detail: impl Into<String>) -> AppError {
    AppError::unknown(format!("browser protocol error during `{method}`"))
        .with_technical_detail(detail.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connecting_to_a_dead_endpoint_reports_browser_unavailable() {
        // Nothing is listening on this port.
        let err = CdpClient::connect("ws://127.0.0.1:9/devtools/page/none", Duration::from_secs(3))
            .await
            .err()
            .expect("a dead endpoint must not connect");
        assert_eq!(err.kind(), crate::errors::AppErrorKind::BrowserUnavailable);
    }

    #[tokio::test]
    async fn a_malformed_endpoint_is_reported_rather_than_panicking() {
        let err = CdpClient::connect("not a url", Duration::from_secs(3))
            .await
            .err()
            .expect("a malformed endpoint must not connect");
        assert_eq!(err.kind(), crate::errors::AppErrorKind::BrowserUnavailable);
    }

    /// A throwaway DevTools endpoint that answers from a script.
    ///
    /// `respond` is given each command's id and returns the frames to send back
    /// in order — usually a burst of events followed by the reply. `None` means
    /// the socket is dropped instead, which is what a browser dying looks like
    /// from this side.
    async fn endpoint<F>(respond: F) -> String
    where
        F: Fn(u64) -> Option<Vec<Value>> + Send + Sync + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback listener");
        let addr = listener.local_addr().expect("a bound address");
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            while let Some(Ok(message)) = socket.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                let Some(id) = value.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let Some(frames) = respond(id) else {
                    // Dropping the socket is the point of this arm: the client
                    // sees the connection end with its command still pending.
                    return;
                };
                for frame in frames {
                    if socket.send(Message::Text(frame.to_string())).await.is_err() {
                        return;
                    }
                }
            }
        });
        format!("ws://{addr}/devtools/page/test")
    }

    #[tokio::test]
    async fn a_flood_of_events_does_not_lose_the_reply() {
        // The live failure this pins: a signed-in Douyin page emits far more
        // protocol events than an anonymous one, and a command's reply is read
        // off the same socket, only after everything queued ahead of it. The
        // reply must be found regardless of how deep that queue is — which is
        // exactly what correlation by id is for.
        let url = endpoint(|id| {
            let mut frames: Vec<Value> = (0..600)
                .map(|index| {
                    json!({
                        "method": "Network.requestWillBeSent",
                        "params": { "requestId": format!("r{index}") }
                    })
                })
                .collect();
            frames.push(json!({ "id": id, "result": { "ok": true } }));
            Some(frames)
        })
        .await;

        let mut client = CdpClient::connect(&url, Duration::from_secs(5))
            .await
            .expect("the endpoint accepts");
        let result = client.call("Page.enable", json!({}), Duration::from_secs(5)).await;
        assert!(
            result.is_ok(),
            "the reply must be found behind the event backlog"
        );

        // Bounded, and honest about the bound: the buffer does not grow with
        // the page's traffic. Read before draining, which resets the count.
        assert!(client.dropped_events() > 0, "the buffer must cap, not grow");
        assert_eq!(client.take_events().len(), MAX_BUFFERED_EVENTS);
    }

    #[tokio::test]
    async fn a_command_the_browser_never_answers_times_out_and_names_itself() {
        let url = endpoint(|_id| Some(vec![json!({ "method": "Network.loadingFinished" })]))
            .await;

        let mut client = CdpClient::connect(&url, Duration::from_secs(5))
            .await
            .expect("the endpoint accepts");
        let err = client
            .call("Page.navigate", json!({}), Duration::from_millis(250))
            .await
            .expect_err("an unanswered command must not hang");

        assert_eq!(err.kind(), crate::errors::AppErrorKind::BrowserUnavailable);
        assert!(
            err.message().contains("Page.navigate"),
            "the timeout must name the command that stalled, got: {}",
            err.message()
        );
        // The budget that actually applied, so a log says which one was used
        // rather than leaving it to be guessed at from the constant.
        assert!(
            err.technical_detail().unwrap_or_default().contains("timeout=250ms"),
            "got: {:?}",
            err.technical_detail()
        );
    }

    #[tokio::test]
    async fn a_connection_that_closes_mid_command_is_reported_rather_than_hanging() {
        // A browser dying while a command is in flight has to surface as an
        // error the caller can act on — the renderer retires the session on any
        // failure — and not as a wait that only ends when the command's budget
        // runs out. Without this, "the browser is gone" and "the command is
        // slow" are indistinguishable from the caller's side, and they call for
        // different responses.
        let url = endpoint(|_id| None).await;

        let mut client = CdpClient::connect(&url, Duration::from_secs(5))
            .await
            .expect("the endpoint accepts");
        let started = std::time::Instant::now();
        let err = client
            .call("Page.enable", json!({}), Duration::from_secs(30))
            .await
            .expect_err("a closed connection must not hang");

        assert!(
            err.message().contains("Page.enable"),
            "the failure must name the command, got: {}",
            err.message()
        );
        let detail = err.technical_detail().unwrap_or_default().to_lowercase();
        assert!(
            detail.contains("connection") || detail.contains("closed"),
            "a disconnect must be reported as one, got: {detail}"
        );
        assert!(
            !detail.contains("in time"),
            "a disconnect must not be reported as a timeout, got: {detail}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a disconnect must be noticed long before the 30s command budget expires"
        );
    }
}
