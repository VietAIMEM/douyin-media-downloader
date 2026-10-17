//! The browser's own network boundary.
//!
//! [`GuardedResolver`](crate::network::resolver::GuardedResolver) pins the
//! address an *HTTP client* dials to the address the destination policy
//! approved. The browser does not use that client. Chromium resolves DNS with
//! its own stack, follows its own redirects, and fetches its own subresources,
//! so checking the navigation URL bounds where a render can be *pointed* and
//! nothing more: a page that redirects to, or fetches from, a loopback or
//! private address is a request this application never sees.
//!
//! # How this closes it
//!
//! Chromium is launched with every request routed through an HTTP proxy this
//! application owns, on a port it chose, on loopback. Nothing else about the
//! browser changes; what changes is that every destination it dials is named to
//! this module first. The proxy applies **the same policy and the same
//! resolver** the HTTP client uses, so the browser ends up with the same
//! guarantee: the address dialled is an address that passed classification, and
//! a name whose answer is mixed or non-public is refused rather than dialled.
//!
//! Two shapes arrive, and both are checked:
//!
//! * `CONNECT host:port` — an `https` destination. The name is classified,
//!   resolved once, and *every* address in the answer classified; a refusal
//!   answers `403` and the tunnel is never opened. Otherwise the connection is
//!   dialled to one of the approved addresses and the bytes are relayed
//!   untouched, so TLS stays end-to-end and unaffected.
//! * An absolute-URI request (`GET http://host/…`) — a plain `http`
//!   destination. The URL is checked and the name resolved exactly as above
//!   before anything is forwarded.
//!
//! `--proxy-bypass-list=<-loopback>` is passed alongside `--proxy-server`, which
//! removes Chromium's *implicit* bypass for loopback and link-local
//! destinations. Without it the one class of destination this exists to refuse
//! would never reach the proxy at all.
//!
//! # What this does not cover
//!
//! A browser is not only the renderer: WebRTC can open UDP flows that no HTTP
//! proxy carries, and Chromium's own background services are outside this too.
//! The renderer already launches with `--disable-background-networking`, and
//! nothing in this application asks a page for a WebRTC channel — but the honest
//! statement is that this bounds *HTTP(S) requests the page makes*, which is
//! where a rebinding or subresource attack lives, and not literally every socket
//! the browser can open.
//!
//! The control channel to the browser is deliberately *not* proxied: the
//! DevTools endpoint is on loopback, on a port this application chose, and it is
//! this process — not Chromium — that connects to it.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::Resolve;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::errors::{AppError, AppResult};
use crate::network::policy::DestinationPolicy;
use crate::network::resolver::GuardedResolver;
use crate::logging::url_host;

/// Longest request head this proxy will read before giving up on a connection.
///
/// Generous for cookies and a long URL, small enough that a client cannot make
/// the proxy hold memory by never finishing a line.
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// How long a connection may spend delivering its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a single approved destination may take to accept a connection.
///
/// The same budget the HTTP client gives a connection attempt, so a page's
/// request fails at roughly the time the rest of the application would give up
/// on the same host.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a forwarded plain-`http` exchange may take in total.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(120);

/// An HTTP proxy that refuses every destination the policy refuses.
///
/// Bound to loopback on a port the OS chose, so nothing outside this machine can
/// reach it, and it is torn down with the renderer that owns it.
pub struct GuardedProxy {
    port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl GuardedProxy {
    /// Start a proxy enforcing `policy`.
    pub async fn start(policy: DestinationPolicy) -> AppResult<Self> {
        Self::start_with(Arc::new(GuardedResolver::system(policy)), policy).await
    }

    /// Start a proxy over a specific resolver.
    ///
    /// The seam a test uses to present a crafted DNS answer — the rebinding case
    /// — without needing a name that really lies.
    pub async fn start_with(
        resolver: Arc<GuardedResolver>,
        policy: DestinationPolicy,
    ) -> AppResult<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|err| {
            AppError::browser_unavailable("the browser's network guard could not start")
                .with_technical_detail(err.to_string())
        })?;
        let port = listener
            .local_addr()
            .map_err(|err| {
                AppError::browser_unavailable("the browser's network guard has no address")
                    .with_technical_detail(err.to_string())
            })?
            .port();

        let task = tokio::spawn(accept_loop(listener, resolver, policy));

        tracing::info!(target: "browser", port, "browser network guard listening");
        Ok(Self { port, task })
    }

    /// The loopback port Chromium must be told to use.
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for GuardedProxy {
    fn drop(&mut self) {
        // The listener goes with the task; nothing is left to accept on.
        self.task.abort();
    }
}

/// Accept connections and serve each on its own task.
async fn accept_loop(
    listener: TcpListener,
    resolver: Arc<GuardedResolver>,
    policy: DestinationPolicy,
) {
    loop {
        let Ok((client, _peer)) = listener.accept().await else {
            // The listener is gone; so is this loop.
            return;
        };
        let resolver = Arc::clone(&resolver);
        tokio::spawn(async move {
            match serve(client, resolver, policy).await {
                Ok(()) => {}
                Err(err) => {
                    // A browser abandoning a connection is ordinary; a proxy
                    // failure is not, and neither is worth taking the render
                    // down for — the browser reports the request as failed.
                    tracing::debug!(target: "browser", error = %err, "a guarded connection ended");
                }
            }
        });
    }
}

/// One proxied connection.
async fn serve(
    mut client: TcpStream,
    resolver: Arc<GuardedResolver>,
    policy: DestinationPolicy,
) -> std::io::Result<()> {
    let Some(raw) = read_head(&mut client).await? else {
        return Ok(());
    };
    let Some(request) = RequestHead::parse(&raw) else {
        return refuse(&mut client, "the proxy could not read a request").await;
    };

    if request.method.eq_ignore_ascii_case("CONNECT") {
        return connect_tunnel(&mut client, &request, resolver).await;
    }
    forward_plain(&mut client, &request, resolver, policy).await
}

/// A parsed request head, with the bytes that followed it kept intact.
struct RequestHead {
    method: String,
    target: String,
    version: String,
    headers: Vec<(String, String)>,
    /// Everything read past the blank line — a body, or the start of one.
    ///
    /// Kept because it has already been taken off the socket: whatever the read
    /// pulled in belongs to the request and has to be forwarded with it.
    leftover: Vec<u8>,
}

impl RequestHead {
    fn parse(raw: &[u8]) -> Option<Self> {
        let end = find_head_end(raw)?;
        let head = std::str::from_utf8(&raw[..end.0]).ok()?;
        let mut lines = head.split("\r\n");
        let request_line = lines.next()?;
        let mut parts = request_line.split(' ');
        let method = parts.next()?.to_string();
        let target = parts.next()?.to_string();
        let version = parts.next().unwrap_or("HTTP/1.1").to_string();

        let headers = lines
            .filter(|line| !line.is_empty())
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.trim().to_string(), value.trim().to_string()))
            })
            .collect();

        Some(Self {
            method,
            target,
            version,
            headers,
            leftover: raw[end.1..].to_vec(),
        })
    }

    /// The `host:port` this request names, with the default port applied.
    fn destination(&self) -> Option<(String, u16)> {
        if self.method.eq_ignore_ascii_case("CONNECT") {
            return split_host_port(&self.target);
        }
        // Absolute form: the request line itself carries the authority.
        let url = url::Url::parse(&self.target).ok()?;
        let host = url.host_str()?.to_string();
        let port = url.port_or_known_default()?;
        Some((host, port))
    }

    /// The request line rewritten for the origin, which is what an origin server
    /// expects.
    fn origin_target(&self) -> String {
        match url::Url::parse(&self.target) {
            Ok(url) => {
                let mut path = url.path().to_string();
                if let Some(query) = url.query() {
                    path.push('?');
                    path.push_str(query);
                }
                path
            }
            // Not an absolute URI after all; forward it as it stands rather than
            // inventing a path.
            Err(_) => self.target.clone(),
        }
    }
}

/// Read up to and including the blank line that ends a request head.
///
/// Returns `None` when the peer closed before sending anything, which is the
/// ordinary way a browser drops an idle connection.
async fn read_head(stream: &mut TcpStream) -> std::io::Result<Option<Vec<u8>>> {
    let mut buffer: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];

    let read = tokio::time::timeout(HEAD_TIMEOUT, async {
        loop {
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                return Ok::<usize, std::io::Error>(0);
            }
            buffer.extend_from_slice(&chunk[..read]);
            if find_head_end(&buffer).is_some() || buffer.len() > MAX_HEAD_BYTES {
                return Ok(read);
            }
        }
    })
    .await;

    match read {
        Ok(Ok(0)) => Ok(if buffer.is_empty() { None } else { Some(buffer) }),
        Ok(Ok(_)) => Ok(Some(buffer)),
        Ok(Err(err)) => Err(err),
        Err(_) => Ok(None),
    }
}

/// Where a request head ends: the index of the blank line, and of the byte after
/// it. Both `\r\n\r\n` and the bare `\n\n` an unusual client might send.
fn find_head_end(raw: &[u8]) -> Option<(usize, usize)> {
    raw.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, index + 4))
        .or_else(|| {
            raw.windows(2)
                .position(|window| window == b"\n\n")
                .map(|index| (index, index + 2))
        })
}

/// Split `host:port`, tolerating a bracketed IPv6 literal.
fn split_host_port(target: &str) -> Option<(String, u16)> {
    // A bracketed literal: split after the closing bracket, not at its colons.
    if let Some(rest) = target.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = tail.strip_prefix(':')?.parse::<u16>().ok()?;
        return Some((host.to_string(), port));
    }
    let (host, port) = target.rsplit_once(':')?;
    Some((host.to_string(), port.parse::<u16>().ok()?))
}

/// Resolve `host` under the policy and report the addresses that may be dialled.
///
/// This is the whole point of the module: the classification happens here, and
/// what it returns is what gets dialled. `GuardedResolver` refuses a reserved
/// name without a lookup, refuses an answer that is empty or entirely
/// non-public, and refuses a *mixed* answer whole — so "some member is public"
/// never becomes "connect to whichever member the connector picked".
async fn approved_addresses(
    resolver: &GuardedResolver,
    host: &str,
) -> Result<Vec<std::net::SocketAddr>, String> {
    let name = reqwest::dns::Name::from_str(host).map_err(|_| "unusable host".to_string())?;
    match resolver.resolve(name).await {
        Ok(addresses) => Ok(addresses.collect()),
        Err(err) => Err(err.to_string()),
    }
}

/// Answer a refused request. The host is named; the URL is not.
async fn refuse<S: AsyncWriteExt + Unpin>(stream: &mut S, reason: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\nX-Guard: {}\r\n\r\n",
        reason.replace(['\r', '\n'], " ")
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

/// `CONNECT host:port` — check the destination, then relay the tunnel.
async fn connect_tunnel(
    client: &mut TcpStream,
    request: &RequestHead,
    resolver: Arc<GuardedResolver>,
) -> std::io::Result<()> {
    let Some((host, port)) = request.destination() else {
        return refuse(client, "malformed connect target").await;
    };

    let upstream = match dial(&resolver, &host, port).await {
        Ok(stream) => stream,
        Err(Dial::Refused(reason)) => {
            tracing::warn!(
                target: "browser",
                host = %host,
                port,
                reason = %reason,
                "the browser's network guard refused a destination"
            );
            return refuse(client, "destination not permitted").await;
        }
        Err(Dial::Unreachable(reason)) => {
            tracing::debug!(
                target: "browser",
                host = %host,
                port,
                reason = %reason,
                "an approved destination could not be reached"
            );
            return unreachable(client).await;
        }
    };

    tracing::debug!(target: "browser", host = %host, port, "browser tunnel opened");
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    client.flush().await?;

    let mut upstream = upstream;
    let _ = tokio::io::copy_bidirectional(client, &mut upstream).await;
    Ok(())
}

/// An absolute-URI plain-`http` request: check the destination, then forward it
/// once.
///
/// `Connection: close` is forced in both directions, so a connection carries
/// exactly one request. That is what makes "every destination was checked" true
/// without a response parser: a second request on the same connection could not
/// be recognized, so none is allowed.
async fn forward_plain(
    client: &mut TcpStream,
    request: &RequestHead,
    resolver: Arc<GuardedResolver>,
    policy: DestinationPolicy,
) -> std::io::Result<()> {
    if !policy.allows(&request.target) {
        return refuse(client, "destination not permitted").await;
    }
    let Some((host, port)) = request.destination() else {
        return refuse(client, "malformed request target").await;
    };

    let mut upstream = match dial(&resolver, &host, port).await {
        Ok(stream) => stream,
        Err(Dial::Refused(reason)) => {
            tracing::warn!(
                target: "browser",
                host = %url_host(&request.target),
                reason = %reason,
                "the browser's network guard refused a destination"
            );
            return refuse(client, "destination not permitted").await;
        }
        Err(Dial::Unreachable(reason)) => {
            tracing::debug!(
                target: "browser",
                host = %url_host(&request.target),
                reason = %reason,
                "an approved destination could not be reached"
            );
            return unreachable(client).await;
        }
    };

    let mut head = format!(
        "{} {} {}\r\n",
        request.method,
        request.origin_target(),
        request.version
    );
    for (name, value) in &request.headers {
        // Hop-by-hop headers belong to the client-to-proxy hop; `Connection` is
        // replaced below so the origin closes when it is done.
        if name.eq_ignore_ascii_case("proxy-connection")
            || name.eq_ignore_ascii_case("connection")
        {
            continue;
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");

    upstream.write_all(head.as_bytes()).await?;
    if !request.leftover.is_empty() {
        upstream.write_all(&request.leftover).await?;
    }
    upstream.flush().await?;

    let relay = async {
        let _ = tokio::io::copy_bidirectional(client, &mut upstream).await;
    };
    // Bounded: a server that ignores `Connection: close` must not hold the
    // proxy's task forever.
    let _ = tokio::time::timeout(FORWARD_TIMEOUT, relay).await;
    Ok(())
}

/// Why a destination could not be dialled.
///
/// The two are not the same answer and must not be given the same one. A
/// *refusal* is this application's decision: the destination is one the policy
/// will not contact, and a `403` says so. An *unreachable* destination passed
/// classification and simply did not connect — that is a network condition, and
/// answering `403` would tell the page it was forbidden rather than that the
/// network failed, and would tell a log reader the policy had refused something
/// it had approved.
#[derive(Debug)]
enum Dial {
    /// The policy refused the destination, or the name did not resolve.
    Refused(String),
    /// The destination was approved but could not be reached.
    Unreachable(String),
}

/// Resolve under the policy and connect to an approved address.
///
/// Every address returned has already passed classification, so dialling one is
/// dialling something that was checked. The first that accepts wins; a public
/// host with several addresses is not refused because one of them is
/// unreachable.
async fn dial(resolver: &GuardedResolver, host: &str, port: u16) -> Result<TcpStream, Dial> {
    let addresses = approved_addresses(resolver, host)
        .await
        .map_err(Dial::Refused)?;
    if addresses.is_empty() {
        return Err(Dial::Refused("no resolved addresses".to_string()));
    }

    let mut last: Option<String> = None;
    for address in addresses {
        let target = std::net::SocketAddr::new(address.ip(), port);
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(target)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(err)) => last = Some(err.to_string()),
            Err(_) => last = Some("connection timed out".to_string()),
        }
    }
    Err(Dial::Unreachable(
        last.unwrap_or_else(|| "could not connect".to_string()),
    ))
}

/// Answer a destination that was approved but could not be reached.
///
/// `502` is what an origin-side failure looks like through a proxy, and a page
/// treats it as a failed request rather than as a refusal.
async fn unreachable<S: AsyncWriteExt + Unpin>(stream: &mut S) -> std::io::Result<()> {
    let response = "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    // --- request parsing -----------------------------------------------------

    #[test]
    fn a_connect_head_is_parsed() {
        let raw = b"CONNECT www.douyin.com:443 HTTP/1.1\r\nHost: www.douyin.com:443\r\n\r\n";
        let head = RequestHead::parse(raw).expect("parseable");
        assert_eq!(head.method, "CONNECT");
        assert_eq!(
            head.destination(),
            Some(("www.douyin.com".to_string(), 443))
        );
    }

    #[test]
    fn an_absolute_uri_head_carries_its_authority_and_its_body() {
        let raw = b"GET http://example.test/a?b=1 HTTP/1.1\r\nHost: example.test\r\n\r\nbody-bytes";
        let head = RequestHead::parse(raw).expect("parseable");
        assert_eq!(head.destination(), Some(("example.test".to_string(), 80)));
        assert_eq!(head.origin_target(), "/a?b=1");
        // Bytes read past the head belong to the request, not to nothing.
        assert_eq!(head.leftover, b"body-bytes");
    }

    #[test]
    fn a_bracketed_ipv6_connect_target_is_split_at_its_own_colons() {
        let head = RequestHead::parse(b"CONNECT [::1]:8443 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(head.destination(), Some(("::1".to_string(), 8443)));
    }

    #[test]
    fn a_head_without_its_terminator_is_not_parsed() {
        assert!(RequestHead::parse(b"CONNECT a:443 HTTP/1.1\r\nHost: a").is_none());
    }

    #[test]
    fn an_https_absolute_uri_gets_the_default_port() {
        let head = RequestHead::parse(b"GET https://example.test/x HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(head.destination(), Some(("example.test".to_string(), 443)));
    }

    // --- the policy gate -----------------------------------------------------
    //
    // What matters is not that the classifier refuses — `policy`'s own tests
    // cover that — but that the *proxy* refuses, and that the decision is made
    // on the resolved answer rather than on the name.

    #[tokio::test]
    async fn a_loopback_destination_is_refused() {
        let resolver = GuardedResolver::system(DestinationPolicy::PublicOnly);
        let err = approved_addresses(&resolver, "localhost").await;
        assert!(err.is_err(), "localhost must not be dialled by the browser");
        assert!(
            err.unwrap_err().contains("special-name"),
            "the refusal travels with its category"
        );
    }

    #[tokio::test]
    async fn a_metadata_address_is_refused_however_it_is_spelled() {
        let resolver = GuardedResolver::system(DestinationPolicy::PublicOnly);
        for host in ["169.254.169.254", "127.0.0.1", "10.0.0.1", "::1"] {
            assert!(
                approved_addresses(&resolver, host).await.is_err(),
                "{host} must be refused"
            );
        }
    }

    #[tokio::test]
    async fn a_local_only_name_is_refused_without_a_lookup() {
        let resolver = GuardedResolver::system(DestinationPolicy::PublicOnly);
        for host in ["printer.local", "nas.internal", "router.home.arpa"] {
            let err = approved_addresses(&resolver, host).await.unwrap_err();
            assert!(err.contains("special-name"), "{host}: {err}");
        }
    }

    #[tokio::test]
    async fn the_permissive_policy_dials_what_the_strict_one_refuses() {
        // The seam the tests that drive their own loopback server rely on: the
        // check is real, and it is the policy that decides.
        let resolver = GuardedResolver::system(DestinationPolicy::Any);
        let addresses = approved_addresses(&resolver, "localhost").await.unwrap();
        assert!(!addresses.is_empty());
    }

    // --- the wire behaviour --------------------------------------------------
    //
    // End to end over a real loopback socket: what a browser gets back when it
    // asks the guard for something it must not have.

    async fn start_proxy(policy: DestinationPolicy) -> GuardedProxy {
        GuardedProxy::start(policy).await.expect("the proxy starts")
    }

    #[tokio::test]
    async fn a_connect_to_a_refused_destination_is_answered_403() {
        let proxy = start_proxy(DestinationPolicy::PublicOnly).await;
        let mut client = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        client
            .write_all(b"CONNECT 127.0.0.1:9 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut response))
            .await
            .expect("the proxy must answer")
            .unwrap();
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 403"), "got {text}");
        assert!(!text.contains("200 Connection Established"));
    }

    #[tokio::test]
    async fn a_plain_http_request_to_a_refused_destination_is_answered_403() {
        let proxy = start_proxy(DestinationPolicy::PublicOnly).await;
        let mut client = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        client
            .write_all(b"GET http://169.254.169.254/latest/meta-data/ HTTP/1.1\r\nHost: 169.254.169.254\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut response))
            .await
            .expect("the proxy must answer")
            .unwrap();
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 403"), "got {text}");
        // And the refusal says nothing about what was asked for.
        assert!(!text.contains("meta-data"), "the path must not be echoed");
    }

    #[tokio::test]
    async fn a_connect_to_an_allowed_destination_is_tunnelled() {
        // A loopback echo server stands in for a public origin, with the
        // permissive policy — the point is that the tunnel is opened and the
        // bytes flow untouched, which is what TLS needs.
        let echo = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = echo.accept().await {
                tokio::spawn(async move {
                    let mut buffer = [0u8; 64];
                    if let Ok(read) = stream.read(&mut buffer).await {
                        let _ = stream.write_all(&buffer[..read]).await;
                    }
                });
            }
        });

        let proxy = start_proxy(DestinationPolicy::Any).await;
        let mut client = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        client
            .write_all(
                format!("CONNECT 127.0.0.1:{echo_port} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes(),
            )
            .await
            .unwrap();

        // The tunnel's own `200` head, then the echoed payload through it.
        let mut established = vec![0u8; 39];
        tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut established))
            .await
            .expect("the tunnel must open")
            .unwrap();
        assert!(
            String::from_utf8_lossy(&established).starts_with("HTTP/1.1 200"),
            "got {:?}",
            String::from_utf8_lossy(&established)
        );

        client.write_all(b"through-the-tunnel").await.unwrap();
        let mut echoed = vec![0u8; 18];
        tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut echoed))
            .await
            .expect("bytes must be relayed")
            .unwrap();
        assert_eq!(&echoed, b"through-the-tunnel");
    }

    #[tokio::test]
    async fn a_plain_http_request_is_forwarded_in_origin_form() {
        // The origin server must see a request for `/a?b=1`, not the absolute
        // form the proxy received.
        let origin = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let origin_port = origin.local_addr().unwrap().port();
        let seen = Arc::new(parking_lot::Mutex::new(String::new()));
        let seen_by_server = Arc::clone(&seen);
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = origin.accept().await {
                let mut buffer = vec![0u8; 4096];
                let read = stream.read(&mut buffer).await.unwrap_or(0);
                *seen_by_server.lock() = String::from_utf8_lossy(&buffer[..read]).into_owned();
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                    .await;
            }
        });

        let proxy = start_proxy(DestinationPolicy::Any).await;
        let mut client = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        client
            .write_all(
                format!(
                    "GET http://127.0.0.1:{origin_port}/a?b=1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut response))
            .await
            .expect("the proxy must answer")
            .unwrap();
        assert!(String::from_utf8_lossy(&response).contains("200 OK"));

        let request_line = seen.lock().lines().next().unwrap_or_default().to_string();
        assert_eq!(request_line, "GET /a?b=1 HTTP/1.1");
        assert!(
            !seen.lock().contains("Connection: close\r\nConnection"),
            "hop-by-hop headers must not be duplicated"
        );
    }

    #[tokio::test]
    async fn a_connection_that_sends_nothing_is_dropped_quietly() {
        let proxy = start_proxy(DestinationPolicy::PublicOnly).await;
        let client = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        drop(client);
        // Nothing to assert but that the proxy is still serving afterwards.
        let mut second = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        second
            .write_all(b"CONNECT localhost:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), second.read_to_end(&mut response))
            .await
            .expect("the proxy must answer")
            .unwrap();
        assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 403"));
    }

    #[tokio::test]
    async fn the_proxy_listens_only_on_loopback() {
        let proxy = start_proxy(DestinationPolicy::PublicOnly).await;
        let local: SocketAddr = format!("127.0.0.1:{}", proxy.port()).parse().unwrap();
        assert!(TcpStream::connect(local).await.is_ok());
    }

    // --- the rebinding case, at the wire -------------------------------------

    /// A resolver whose answer a test controls. `policy`'s own tests cover what
    /// classification *decides*; these cover what the proxy *does* with it.
    struct FixedResolver {
        answers: std::collections::HashMap<String, Vec<SocketAddr>>,
    }

    impl Resolve for FixedResolver {
        fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
            let answer = self.answers.get(name.as_str()).cloned().unwrap_or_default();
            Box::pin(async move {
                Ok(Box::new(answer.into_iter()) as reqwest::dns::Addrs)
            })
        }
    }

    fn resolver_answering(
        name: &str,
        answers: &[&str],
        policy: DestinationPolicy,
    ) -> Arc<GuardedResolver> {
        let mut map = std::collections::HashMap::new();
        map.insert(
            name.to_string(),
            answers.iter().map(|addr| addr.parse().unwrap()).collect(),
        );
        Arc::new(GuardedResolver::wrapping(
            Arc::new(FixedResolver { answers: map }),
            policy,
        ))
    }

    /// Issue a `CONNECT` through `proxy` and return the status line it answers.
    async fn connect_through(proxy: &GuardedProxy, target: &str) -> String {
        let mut client = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        client
            .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
            .await
            .unwrap();

        let mut response = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), client.read_to_end(&mut response))
            .await;
        String::from_utf8_lossy(&response)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string()
    }

    #[tokio::test]
    async fn a_public_name_that_answers_with_loopback_is_refused_at_the_wire() {
        // The rebinding case in full: `cdn.example.com` is an ordinary public
        // name, so every URL-level check passes, and its *answer* is this
        // machine. The browser gets a refusal and no tunnel is opened.
        let resolver = resolver_answering(
            "cdn.example.com",
            &["127.0.0.1:443"],
            DestinationPolicy::PublicOnly,
        );
        let proxy = GuardedProxy::start_with(resolver, DestinationPolicy::PublicOnly)
            .await
            .unwrap();

        let status = connect_through(&proxy, "cdn.example.com:443").await;
        assert!(status.starts_with("HTTP/1.1 403"), "got {status:?}");
    }

    #[tokio::test]
    async fn a_mixed_answer_is_refused_whole_at_the_wire() {
        // One public address and one loopback: the answer a per-address filter
        // would survive by keeping the public member. It is refused, because the
        // client would dial whichever member the connector happened to pick.
        let resolver = resolver_answering(
            "split.example.com",
            &["203.0.113.10:443", "127.0.0.1:443"],
            DestinationPolicy::PublicOnly,
        );
        let proxy = GuardedProxy::start_with(resolver, DestinationPolicy::PublicOnly)
            .await
            .unwrap();

        let status = connect_through(&proxy, "split.example.com:443").await;
        assert!(status.starts_with("HTTP/1.1 403"), "got {status:?}");
    }

    #[tokio::test]
    async fn a_metadata_address_behind_a_public_name_is_refused_at_the_wire() {
        // The cloud metadata endpoint is the one that turns a fetch into a
        // credential leak, and a name is all a page needs to aim at it.
        let resolver = resolver_answering(
            "metadata.example.com",
            &["169.254.169.254:80"],
            DestinationPolicy::PublicOnly,
        );
        let proxy = GuardedProxy::start_with(resolver, DestinationPolicy::PublicOnly)
            .await
            .unwrap();

        let status = connect_through(&proxy, "metadata.example.com:80").await;
        assert!(status.starts_with("HTTP/1.1 403"), "got {status:?}");
    }

    #[tokio::test]
    async fn an_approved_destination_that_will_not_connect_is_not_a_refusal() {
        // The distinction that matters for the product: a host the policy
        // approves and the network cannot reach is a *network* failure. Saying
        // "403 Forbidden" there would blame the policy for a timeout — and would
        // tell the user their content was refused when it was merely offline.
        let resolver = GuardedResolver::system(DestinationPolicy::Any);
        match dial(&resolver, "127.0.0.1", 1).await {
            Err(Dial::Unreachable(_)) => {}
            other => panic!("an approved but dead destination is not a refusal: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_refused_destination_is_a_refusal_not_a_connect_failure() {
        let resolver = GuardedResolver::system(DestinationPolicy::PublicOnly);
        match dial(&resolver, "localhost", 9).await {
            Err(Dial::Refused(reason)) => assert!(reason.contains("special-name"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}
