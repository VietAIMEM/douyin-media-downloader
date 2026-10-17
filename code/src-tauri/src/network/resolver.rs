//! Binding an HTTP connection to an address the destination policy approved.
//!
//! [`super::policy`] decides which destinations may be contacted, but a URL only
//! carries a *name*. Which address that name turns into is decided later, by
//! whoever does the lookup — and if that lookup is an ordinary system resolution
//! performed by the HTTP client, the address that gets dialled is not the one
//! that was checked. Checking a name and then resolving it again is not a check
//! at all: a name whose answer changes between the two (DNS rebinding) walks
//! straight through.
//!
//! # How this closes it
//!
//! `reqwest` resolves through a [`Resolve`] implementation, and the addresses it
//! returns are the addresses the connector dials — `hyper-util`'s `HttpConnector`
//! asks the resolver for a name and uses what it gets, with no second lookup.
//! [`GuardedResolver`] therefore *is* the resolution step: it classifies the name,
//! asks the real resolver, classifies the whole answer, and hands back only
//! addresses that passed. There is no resolution between the check and the
//! connection for anything to interfere with.
//!
//! The one case that does not come through here is a URL that is *already* an IP
//! literal: `hyper-util` recognises one and dials it without consulting the
//! resolver at all. Those are refused earlier, by `DestinationPolicy::check` on
//! the URL, which classifies a literal host before the request is built. Between
//! them the two cover every outbound request; neither is sufficient alone, and
//! `ReqwestHttpClient` applies both.
//!
//! # What is preserved
//!
//! Nothing about the request itself changes. The connector still dials the
//! connection with the URL's hostname for TLS: SNI and certificate verification
//! use the name from the URL, not the address, so pinning the address does not
//! weaken or alter TLS. The `Host` header likewise comes from the URL.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use super::policy::{DestinationPolicy, Refusal};

/// Prefix of the message a policy refusal carries through the transport stack.
///
/// The refusal happens inside the resolver, several layers below the code that
/// builds an application error, and `reqwest` reports it as an ordinary connect
/// failure — which the error mapping would otherwise make *retryable*, so a
/// blocked destination would be retried until the attempt budget ran out. This
/// marker is what lets the mapping recognise it. It is checked by walking the
/// error's source chain, so it survives however many wrappers `hyper` and
/// `reqwest` put around it.
const REFUSAL_MARKER: &str = "blocked-destination: ";

/// A hostname was not allowed to be contacted, or did not resolve to an address
/// that may be.
#[derive(Debug, Clone)]
pub struct RefusedDestination {
    /// The name that was being resolved. A name, never a full URL: the URL may
    /// carry a signed query string.
    pub host: String,
    pub refusal: Refusal,
}

impl std::fmt::Display for RefusedDestination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Starts with the marker so the error mapping can find it through the
        // transport stack's wrappers. The category travels as its tag: the prose
        // is for a human, and prose is not recoverable.
        write!(
            f,
            "{REFUSAL_MARKER}{host} ({tag})",
            host = self.host,
            tag = self.refusal.tag()
        )
    }
}

impl std::error::Error for RefusedDestination {}

/// Whether a transport failure was the destination policy refusing an address.
///
/// Walks the error's source chain rather than downcasting, because the refusal
/// is a boxed `dyn Error` several wrappers down and each layer chooses whether
/// to expose it as a `source`. The `Debug` rendering is checked too: it prints
/// the chain, and one of the two always carries the marker.
pub fn policy_refusal(err: &(dyn std::error::Error + 'static)) -> Option<RefusedDestination> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(error) = current {
        if let Some(host) = parse_refusal(&error.to_string()) {
            return Some(host);
        }
        current = error.source();
    }
    parse_refusal(&format!("{err:?}"))
}

/// Read a refusal back out of a message that starts with the marker.
fn parse_refusal(text: &str) -> Option<RefusedDestination> {
    let start = text.find(REFUSAL_MARKER)? + REFUSAL_MARKER.len();
    let rest = &text[start..];
    let (host, tag) = match rest.split_once(" (") {
        Some((host, tag)) => (host, tag.trim_end_matches(')')),
        None => (rest, ""),
    };
    let refusal = Refusal::from_tag(tag).or(Some(Refusal::MixedAnswer))?;
    Some(RefusedDestination {
        host: host.to_string(),
        refusal,
    })
}

/// A [`Resolve`] that only ever returns addresses the destination policy allows.
pub struct GuardedResolver {
    /// The real resolver. Behind `Arc` because `resolve` takes `&self` and the
    /// future it returns outlives the borrow.
    inner: Arc<dyn Resolve>,
    policy: DestinationPolicy,
}

impl GuardedResolver {
    /// Guard the system resolver — what the application ships with.
    pub fn system(policy: DestinationPolicy) -> Self {
        Self::wrapping(Arc::new(SystemResolver), policy)
    }

    /// Guard an arbitrary resolver. How the tests inject crafted answers.
    pub fn wrapping(inner: Arc<dyn Resolve>, policy: DestinationPolicy) -> Self {
        Self { inner, policy }
    }
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let inner = Arc::clone(&self.inner);
        let policy = self.policy;
        let host = name.as_str().to_ascii_lowercase();

        Box::pin(async move {
            // The name first, so a reserved one is refused without being looked
            // up. `localhost` and friends need no resolver to know about.
            if let Some(refusal) = policy.classify_name(&host) {
                tracing::warn!(
                    target: "network",
                    host = %host,
                    reason = %refusal.describe(),
                    "refusing to resolve a non-public destination"
                );
                return Err(Box::new(RefusedDestination { host, refusal })
                    as Box<dyn std::error::Error + Send + Sync>);
            }

            let addrs: Vec<SocketAddr> = inner.resolve(name).await?.collect();

            if let Some(refusal) = policy.classify_answer(&addrs) {
                tracing::warn!(
                    target: "network",
                    host = %host,
                    addresses = addrs.len(),
                    reason = %refusal.describe(),
                    "refusing a destination whose resolved addresses are not public"
                );
                return Err(Box::new(RefusedDestination { host, refusal })
                    as Box<dyn std::error::Error + Send + Sync>);
            }

            tracing::debug!(
                target: "network",
                host = %host,
                addresses = addrs.len(),
                "resolved a destination; the connection will use one of these"
            );
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// The system resolver, reached through `getaddrinfo`.
///
/// `reqwest`'s own `GaiResolver` is crate-private, so this is the same call made
/// directly. It is blocking, so it runs on a blocking thread rather than stalling
/// an async worker — which matters here because a name that does not answer holds
/// the resolver for as long as the OS's own timeout.
struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<SocketAddr>> {
                use std::net::ToSocketAddrs;
                // Port 0: the connector sets the real port from the URL, and
                // asking for one here would only be overwritten.
                Ok((host.as_str(), 0).to_socket_addrs()?.collect())
            })
            .await
            .map_err(|err| {
                Box::new(std::io::Error::other(format!("resolver task failed: {err}")))
                    as Box<dyn std::error::Error + Send + Sync>
            })??;

            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn a_refusal_is_readable_back_out_of_the_message() {
        let refusal = RefusedDestination {
            host: "evil.example.com".into(),
            refusal: Refusal::Loopback,
        };
        let text = refusal.to_string();
        assert!(text.starts_with(REFUSAL_MARKER), "the marker leads: {text}");

        let found = policy_refusal(&refusal).expect("recognised");
        assert_eq!(found.host, "evil.example.com");
    }

    #[test]
    fn a_refusal_survives_being_wrapped_by_the_transport_stack() {
        // `hyper` and `reqwest` wrap the resolver's error in their own; the
        // marker has to survive that or the refusal is treated as a retryable
        // connect failure.
        #[derive(Debug)]
        struct Wrapper(RefusedDestination);
        impl std::fmt::Display for Wrapper {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "connect error")
            }
        }
        impl std::error::Error for Wrapper {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let wrapped = Wrapper(RefusedDestination {
            host: "metadata.internal.example".into(),
            refusal: Refusal::LinkLocal,
        });
        let found = policy_refusal(&wrapped).expect("found through the chain");
        assert_eq!(found.host, "metadata.internal.example");
    }

    #[test]
    fn an_ordinary_failure_is_not_mistaken_for_a_refusal() {
        let err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        assert!(policy_refusal(&err).is_none());
    }

    // --- deterministic connection-pinning tests ------------------------------
    //
    // These use an injected resolver, which is the same seam the guarded client
    // itself resolves through. What they prove is not "the classifier returns X"
    // — that is `policy`'s tests — but that the resolution step refuses what the
    // policy refuses, and that what it returns is *all* it can return: the
    // connector dials exactly these addresses, so an address that is not here is
    // an address that cannot be dialled.

    /// A resolver that answers with a fixed address set — the shape of a DNS
    /// answer a rebinding attack controls.
    struct FixedResolver {
        answers: std::collections::HashMap<String, Vec<SocketAddr>>,
    }

    impl FixedResolver {
        fn answering(name: &str, addrs: &[SocketAddr]) -> Self {
            let mut answers = std::collections::HashMap::new();
            answers.insert(name.to_string(), addrs.to_vec());
            Self { answers }
        }
    }

    impl Resolve for FixedResolver {
        fn resolve(&self, name: Name) -> Resolving {
            let answer = self.answers.get(name.as_str()).cloned().unwrap_or_default();
            Box::pin(async move { Ok(Box::new(answer.into_iter()) as Addrs) })
        }
    }

    #[derive(Debug)]
    enum Outcome {
        Allowed(Vec<SocketAddr>),
        Refused(RefusedDestination),
        OtherFailure(#[allow(dead_code)] String),
    }

    async fn resolved(name: &str, resolver: &GuardedResolver) -> Outcome {
        let name = Name::from_str(name).expect("a valid name");
        match resolver.resolve(name).await {
            Ok(addrs) => Outcome::Allowed(addrs.collect()),
            Err(err) => {
                // Walk the source chain, exactly as the error mapping does.
                match policy_refusal(err.as_ref()) {
                    Some(refusal) => Outcome::Refused(refusal),
                    None => Outcome::OtherFailure(format!("{err}")),
                }
            }
        }
    }

    fn socket(ip: std::net::IpAddr) -> SocketAddr {
        SocketAddr::new(ip, 443)
    }

    #[tokio::test]
    async fn a_public_name_resolving_to_a_public_address_is_returned() {
        let guarded = GuardedResolver::wrapping(
            Arc::new(FixedResolver::answering(
                "media.example.com",
                &[socket("203.0.113.10".parse().unwrap())],
            )),
            DestinationPolicy::PublicOnly,
        );
        match resolved("media.example.com", &guarded).await {
            Outcome::Allowed(addrs) => {
                assert_eq!(addrs.len(), 1);
                assert_eq!(
                    addrs[0].ip(),
                    "203.0.113.10".parse::<std::net::IpAddr>().unwrap()
                );
            }
            other => panic!("expected an allowed answer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_public_name_resolving_to_loopback_is_refused() {
        // The rebinding case in one line: the *name* looks public, the answer
        // is not.
        let guarded = GuardedResolver::wrapping(
            Arc::new(FixedResolver::answering(
                "cdn.example.com",
                &[socket("127.0.0.1".parse().unwrap())],
            )),
            DestinationPolicy::PublicOnly,
        );
        match resolved("cdn.example.com", &guarded).await {
            Outcome::Refused(refusal) => {
                assert_eq!(refusal.host, "cdn.example.com");
                assert_eq!(refusal.refusal, Refusal::Loopback);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn every_private_shape_is_refused_at_resolution() {
        for (answer, expected) in [
            ("10.1.2.3", Refusal::Private),
            ("192.168.0.9", Refusal::Private),
            ("172.16.4.4", Refusal::Private),
            ("169.254.169.254", Refusal::LinkLocal),
            ("0.0.0.0", Refusal::Unspecified),
            ("224.0.0.5", Refusal::Multicast),
        ] {
            let name = format!("rebind-{answer}.example.com");
            let guarded = GuardedResolver::wrapping(
                Arc::new(FixedResolver::answering(
                    &name,
                    &[socket(answer.parse().unwrap())],
                )),
                DestinationPolicy::PublicOnly,
            );
            match resolved(&name, &guarded).await {
                Outcome::Refused(refusal) => {
                    assert_eq!(refusal.refusal, expected, "for {answer}")
                }
                other => panic!("expected a refusal for {answer}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn ipv6_loopback_and_unique_local_are_refused() {
        for (answer, expected) in [
            ("::1", Refusal::Loopback),
            ("fc00::1", Refusal::Private),
            ("fd12::1", Refusal::Private),
            ("fe80::1", Refusal::LinkLocal),
            // The mapped form: an IPv4 loopback wearing a v6 hat.
            ("::ffff:127.0.0.1", Refusal::Loopback),
        ] {
            let name = format!("v6-{answer}.example.com");
            let guarded = GuardedResolver::wrapping(
                Arc::new(FixedResolver::answering(
                    &name,
                    &[socket(answer.parse().unwrap())],
                )),
                DestinationPolicy::PublicOnly,
            );
            match resolved(&name, &guarded).await {
                Outcome::Refused(refusal) => {
                    assert_eq!(refusal.refusal, expected, "for {answer}")
                }
                other => panic!("expected a refusal for {answer}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_mixed_answer_is_refused_whole() {
        // One public address and one loopback: the rebinding case that a
        // per-address filter would survive by keeping the public member. The
        // answer is refused entirely, because "some member is public" is not
        // the property that matters — the client would still dial whichever
        // member the connector handed it.
        let guarded = GuardedResolver::wrapping(
            Arc::new(FixedResolver::answering(
                "split-horizon.example.com",
                &[
                    socket("203.0.113.10".parse().unwrap()),
                    socket("127.0.0.1".parse().unwrap()),
                ],
            )),
            DestinationPolicy::PublicOnly,
        );
        match resolved("split-horizon.example.com", &guarded).await {
            Outcome::Refused(refusal) => assert_eq!(refusal.refusal, Refusal::MixedAnswer),
            other => panic!("expected a mixed-answer refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_empty_answer_is_refused_rather_than_passed_on() {
        let guarded = GuardedResolver::wrapping(
            Arc::new(FixedResolver::answering("gone.example.com", &[])),
            DestinationPolicy::PublicOnly,
        );
        match resolved("gone.example.com", &guarded).await {
            Outcome::Refused(refusal) => assert_eq!(refusal.refusal, Refusal::EmptyAnswer),
            other => panic!("expected an empty-answer refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_dns_failure_is_reported_as_a_failure_not_a_refusal() {
        struct DeadResolver;
        impl Resolve for DeadResolver {
            fn resolve(&self, _name: Name) -> Resolving {
                Box::pin(async {
                    Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "resolver timed out",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>)
                })
            }
        }

        let guarded =
            GuardedResolver::wrapping(Arc::new(DeadResolver), DestinationPolicy::PublicOnly);
        match resolved("unreachable.example.com", &guarded).await {
            Outcome::OtherFailure(_) => {}
            other => panic!("expected an ordinary failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_rebinding_second_lookup_meets_the_same_gate() {
        // The attack: the first lookup is public (so the URL check passes), the
        // second — the one that actually connects — is loopback. The guarded
        // resolver classifies *every* lookup, so the second one is refused the
        // same way the first would have been had it lied.
        struct RebindingResolver {
            calls: std::sync::atomic::AtomicUsize,
        }
        impl Resolve for RebindingResolver {
            fn resolve(&self, _name: Name) -> Resolving {
                let call = self
                    .calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move {
                    let ip: std::net::IpAddr = if call == 0 {
                        "203.0.113.10".parse().unwrap()
                    } else {
                        "127.0.0.1".parse().unwrap()
                    };
                    Ok(Box::new(vec![socket(ip)].into_iter()) as Addrs)
                })
            }
        }

        let inner = Arc::new(RebindingResolver {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let guarded = GuardedResolver::wrapping(
            inner.clone() as Arc<dyn Resolve>,
            DestinationPolicy::PublicOnly,
        );

        // First lookup: public, allowed through.
        match resolved("rebind.example.com", &guarded).await {
            Outcome::Allowed(addrs) => assert_eq!(
                addrs[0].ip(),
                "203.0.113.10".parse::<std::net::IpAddr>().unwrap()
            ),
            other => panic!("the first lookup should be allowed, got {other:?}"),
        }

        // Second lookup of the same name — the rebinding. The gate is the same
        // one; there is no path that returns the loopback address.
        match resolved("rebind.example.com", &guarded).await {
            Outcome::Refused(refusal) => assert_eq!(refusal.refusal, Refusal::Loopback),
            other => panic!("the second lookup should be refused, got {other:?}"),
        }
        assert_eq!(inner.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_reserved_name_is_refused_without_a_lookup() {
        // `localhost` never needs a resolver, and never gets one: the name is
        // classified before the inner resolver is asked.
        struct CountingResolver;
        impl Resolve for CountingResolver {
            fn resolve(&self, _name: Name) -> Resolving {
                Box::pin(async {
                    Ok(Box::new(
                        vec![socket("203.0.113.99".parse().unwrap())].into_iter(),
                    ) as Addrs)
                })
            }
        }
        let guarded =
            GuardedResolver::wrapping(Arc::new(CountingResolver), DestinationPolicy::PublicOnly);
        match resolved("localhost", &guarded).await {
            Outcome::Refused(refusal) => assert_eq!(refusal.refusal, Refusal::SpecialName),
            other => panic!("localhost should be refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_permissive_policy_answers_with_whatever_it_is_given() {
        // What the loopback test-servers rely on: `Any` returns the answer
        // untouched, including private addresses.
        let guarded = GuardedResolver::wrapping(
            Arc::new(FixedResolver::answering(
                "test.local",
                &[socket("127.0.0.1".parse().unwrap())],
            )),
            DestinationPolicy::Any,
        );
        match resolved("test.local", &guarded).await {
            Outcome::Allowed(addrs) => assert_eq!(addrs.len(), 1),
            other => panic!("expected an allowed answer, got {other:?}"),
        }
    }

    #[test]
    fn a_refusal_never_carries_a_url() {
        // Only the host is kept, so a signed query string cannot ride along.
        let refusal = RefusedDestination {
            host: "v3-cdn.douyin.com".into(),
            refusal: Refusal::MixedAnswer,
        };
        let text = refusal.to_string();
        assert!(!text.contains("token"));
        assert!(!text.contains("?"));
    }
}
