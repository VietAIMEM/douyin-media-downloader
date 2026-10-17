//! Live check that the browser renderer can read a Douyin player.
//!
//! Ignored by default: it needs an installed browser and live network access.
//!
//! ```text
//! cargo test --test live_browser -- --ignored --nocapture
//! ```

use std::sync::Arc;

use douyin_downloader_lib::errors::AppError;
use douyin_downloader_lib::network::chromium::{is_source_redirect, ChromiumRenderer};
use douyin_downloader_lib::network::ReqwestHttpClient;
use douyin_downloader_lib::network::{FetchRequest, HttpClient, PageRenderer, RenderRequest};
use douyin_downloader_lib::resolvers::douyin::player::{
    COLLECT_PLAYER_MEDIA, PLAYER_READY, VIDEO_METADATA_READY,
};

const VIDEO_URL: &str = "https://v.douyin.com/7yyhouNSzvA/";

#[tokio::test]
#[ignore = "requires an installed browser, live DNS, and network access"]
async fn the_browser_cannot_reach_this_machine_through_a_public_name() {
    // The residual risk the Phase 06 report recorded: the URL-level policy
    // checks a *name*, and Chromium resolves names with its own stack. This is
    // that case, end to end, against the real browser.
    //
    // `127.0.0.1.sslip.io` is an ordinary public name whose *answer* is this
    // machine, so every URL-level check passes — the policy has nothing to
    // refuse. Only the address the browser would dial is a refusal, and only a
    // network boundary that sees the resolved address can catch it.
    //
    // The server is proved reachable first, so a pass cannot be an artifact of
    // the name failing to resolve or the server not running.

    const MARKER: &str = "REACHED-THE-HOST";
    const REBINDING_NAME: &str = "127.0.0.1.sslip.io";

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("a loopback server");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut scratch = [0u8; 2048];
                let _ = stream.read(&mut scratch).await;
                let body = format!("<html><body>{MARKER}</body></html>");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });

    let url = format!("http://{REBINDING_NAME}:{port}/");

    // Does the name even resolve here? Without this, a "the browser could not
    // reach it" result would be indistinguishable from "the name was not
    // resolvable today".
    let resolves = tokio::net::lookup_host((REBINDING_NAME, port))
        .await
        .map(|addrs| addrs.count())
        .unwrap_or(0);
    if resolves == 0 {
        println!(
            "NOTE: {REBINDING_NAME} does not resolve on this network, so the rebinding \
             case could not be exercised end to end here."
        );
        return;
    }

    // And is the server actually serving that name? A plain client must reach
    // it, or the browser failing to would prove nothing.
    let permissive =
        ReqwestHttpClient::with_policy(10, douyin_downloader_lib::network::DestinationPolicy::Any)
            .expect("a permissive client");
    let direct = permissive.fetch_text(&FetchRequest::new(&url)).await;
    match &direct {
        Ok(body) if body.contains(MARKER) => {
            println!("control: a plain client reached the loopback server at {REBINDING_NAME}");
        }
        other => {
            println!(
                "NOTE: the control fetch did not reach the server ({other:?}); the rebinding \
                 case could not be exercised end to end here."
            );
            return;
        }
    }

    let Some(renderer) = ChromiumRenderer::discover(Arc::new(ReqwestHttpClient::new(30).unwrap()))
    else {
        panic!("no supported browser found");
    };

    // The page is only "ready" once the marker is in it, so a browser that
    // reached the loopback server is a success and the guard having stopped it
    // is a ready-timeout.
    let ready = format!("document.body && document.body.innerText.indexOf('{MARKER}') >= 0");
    let request = RenderRequest::new(&url, &ready, "({})")
        .with_ready_timeout(std::time::Duration::from_secs(8))
        .with_total_timeout(std::time::Duration::from_secs(25));

    match renderer.render(&request).await {
        Ok(page) => panic!(
            "the browser reached this machine through a public name (title {:?}); the \
             destination policy is not being enforced for the browser",
            page.title
        ),
        Err(err) => {
            println!("the browser was stopped from reaching this machine: {err}");
        }
    }

    // And the application's own HTTP path refuses it too, for the same reason —
    // this name is a loopback destination, whatever it is called.
    let strict = ReqwestHttpClient::new(10).expect("a strict client");
    let refused = strict.fetch_text(&FetchRequest::new(&url)).await;
    assert!(
        refused.is_err(),
        "the HTTP client must refuse a name that resolves to loopback"
    );
}

/// Show enough of a signed URL to identify the route, without reproducing the
/// signature.
fn short(url: &str) -> String {
    match url.split_once('?') {
        Some((path, query)) => format!("{path}?<{} chars of query>", query.len()),
        None => url.to_string(),
    }
}

/// Report, and step past, a live fixture the source will not serve.
///
/// Measured repeatedly: Douyin answers the shared video fixture by sending the
/// browser to its own front page. That is the source's answer about the *link*,
/// not a fault in the application — and it is not a pass either, so it is
/// printed as exactly what it is and the caller returns rather than failing on
/// it or pretending it succeeded. A red suite must never mean "browser recovery
/// broke" when what happened was "Douyin would not serve this link today".
///
/// Only this one error is treated this way. Any other failure still fails, so
/// nothing about the application is excused by this.
fn source_would_not_serve(err: &AppError) -> bool {
    if !is_source_redirect(err) {
        return false;
    }
    println!(
        "\n\
         ENVIRONMENT: Douyin answered {VIDEO_URL} with a page it was not asked for.\n\
         Cause:      {}\n\
         Meaning:    the source refused this fixture link. Nothing about the application\n\
         \x20           can be observed from this run, and nothing is claimed about it.\n\
         Pinned by:  network::chromium::tests::a_link_the_source_will_not_serve_is_\n\
         \x20           recognised_from_where_it_landed (deterministic, no network).\n",
        err.message()
    );
    true
}

#[tokio::test]
#[ignore = "requires an installed browser and live network access"]
async fn the_renderer_reads_the_players_media_list() {
    let Some(renderer) = ChromiumRenderer::discover(Arc::new(ReqwestHttpClient::new(30).unwrap()))
    else {
        panic!("no supported browser found");
    };
    println!("browser: {}", renderer.executable().display());
    println!("renderer: {}", renderer.name());

    let request = RenderRequest::new(VIDEO_URL, PLAYER_READY, COLLECT_PLAYER_MEDIA)
        .settle_on(VIDEO_METADATA_READY);
    let page = match renderer.render(&request).await {
        Ok(page) => page,
        Err(err) if source_would_not_serve(&err) => return,
        Err(err) => panic!("the page must render and expose a player: {err}"),
    };

    println!("final url: {}", short(&page.final_url));
    println!("title: {:?}", page.title);
    println!("media sources: {}", page.media.len());
    for source in &page.media {
        println!(
            "  {:?} {} | {}x{} | {:?}s | {} kbps | labels={:?}",
            source.kind,
            short(&source.url),
            source.width.unwrap_or(0),
            source.height.unwrap_or(0),
            source.duration_seconds,
            source.bitrate_kbps.unwrap_or(0),
            source.labels
        );
    }

    assert!(
        !page.media.is_empty(),
        "the player must expose at least one source"
    );
    assert!(
        page.media
            .iter()
            .all(|source| source.url.starts_with("https://")),
        "every source must be a fetchable https URL"
    );
    assert!(
        page.final_url.contains("douyin.com"),
        "the browser should have landed on douyin.com, got {}",
        page.final_url
    );
}

#[tokio::test]
#[ignore = "requires an installed browser and live network access"]
async fn the_renderer_shuts_its_browser_down_afterwards() {
    let Some(renderer) = ChromiumRenderer::discover(Arc::new(ReqwestHttpClient::new(30).unwrap()))
    else {
        panic!("no supported browser found");
    };

    assert_eq!(renderer.launches(), 0);
    assert!(
        renderer.session_profile().is_none(),
        "there is no profile before there is a browser"
    );

    let request = RenderRequest::new(VIDEO_URL, PLAYER_READY, COLLECT_PLAYER_MEDIA)
        .settle_on(VIDEO_METADATA_READY);

    // A usable page first. A render that fails has *already* retired the browser
    // it could not use — which is the right behaviour but leaves nothing to
    // observe about teardown — so the page is requested again, and a run that
    // never produced one says so rather than passing for the wrong reason.
    let mut outcome = renderer.render(&request).await;
    if outcome.is_err() {
        outcome = renderer.render(&request).await;
    }
    outcome.expect(
        "the live page did not produce a page on either attempt, so what happens to \
         the browser afterwards could not be observed",
    );

    // The browser is deliberately kept alive between renders, so its profile is
    // still there while the session is. What must not happen is it outliving the
    // renderer.
    //
    // Asserted about *this* renderer's browser, by the path it was given — not
    // by counting the profile directories in the temp folder, which is a
    // statement about every browser on the machine, including whatever another
    // test is running at the same moment.
    let profile = renderer
        .session_profile()
        .expect("a render that produced a page keeps the browser it used");
    println!("the app's browser profile: {}", profile.display());
    assert!(
        profile.starts_with(std::env::temp_dir()),
        "the profile must live in the temp directory, got {}",
        profile.display()
    );
    assert!(profile.is_dir(), "and it exists while the browser does");

    drop(renderer);

    // Teardown runs on drop. It waits for the browser to exit and then removes
    // the profile, so this polls rather than sleeping a guessed interval — the
    // guarantee is "removed", not "removed within any particular number of
    // milliseconds".
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while profile.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    assert!(
        !profile.exists(),
        "the browser profile outlived the renderer that owned it: {}",
        profile.display()
    );
}

#[tokio::test]
#[ignore = "requires an installed browser and live network access"]
async fn several_renders_reuse_one_browser() {
    // This is the whole point of the shared session: reading a profile page, the
    // pages of a listing, and each post resolved for download must not each cost
    // a browser.
    //
    // What is asserted here is the invariant this application owns — a render
    // that *succeeds* reuses the browser already in hand, and a render that fails
    // costs at most one browser. The page itself is Douyin's, and it does
    // intermittently serve the short link the home feed instead of the video
    // (the renderer reports that as "the page never became ready"), so a round
    // that produced no page is recorded and printed rather than being taken for
    // a browser-lifecycle defect. The lifecycle itself is pinned deterministically
    // in `network::chromium::tests::lifecycle`, against a fake browser.

    let Some(renderer) = ChromiumRenderer::discover(Arc::new(ReqwestHttpClient::new(30).unwrap()))
    else {
        panic!("no supported browser found");
    };
    let renderer = Arc::new(renderer);
    assert_eq!(
        renderer.launches(),
        0,
        "nothing launches before the first render"
    );

    let mut succeeded = 0;
    let mut failed: Vec<douyin_downloader_lib::errors::AppError> = Vec::new();
    let mut previous_succeeded = false;

    for round in 1..=3 {
        let before = renderer.launches();
        let request = RenderRequest::new(VIDEO_URL, PLAYER_READY, COLLECT_PLAYER_MEDIA)
            .settle_on(VIDEO_METADATA_READY);
        let outcome = renderer.render(&request).await;
        let spent = renderer.launches() - before;

        // The invariant the application owns, stated where it actually lives:
        // **one render starts at most one browser.**
        //
        // A render that finds a session running starts nothing; a render that
        // finds none starts one, and if that one fails it may be replaced exactly
        // once — but the browser it replaced was counted by the render that
        // started it, not by this one. So a render can never add more than one.
        //
        // A running total cannot express this, which is why this test no longer
        // tries: a render that fails retires the browser, so the *next* render
        // legitimately starts one. Counting that against the failure made a
        // correct run look like a browser storm — measured, with three renders
        // launched for one reported failure, every one of them spending exactly
        // one.
        assert!(
            spent <= 1,
            "render {round} started {spent} browsers; a render may start one and replace \
             it at most once, and the one it replaced belongs to the render that started it"
        );

        match outcome {
            Ok(page) => {
                println!("render {round}: {} media sources", page.media.len());
                assert!(!page.media.is_empty(), "render {round} must expose media");
                if previous_succeeded {
                    // With a usable browser already in hand, the next render must
                    // use it. Asserted only between two *consecutive* successes,
                    // because a page that failed legitimately retires the browser
                    // it could not use.
                    assert_eq!(
                        spent, 0,
                        "render {round} succeeded but started another browser instead of \
                         reusing the one it had"
                    );
                }
                succeeded += 1;
                previous_succeeded = true;
            }
            Err(err) => {
                println!("render {round}: the live page was not usable: {err}");
                failed.push(err);
                previous_succeeded = false;
            }
        }
    }

    assert!(
        succeeded > 0,
        "not one of three live renders produced a page, so nothing about reuse could \
         be observed: {failed:?}"
    );
    assert!(renderer.has_session(), "the session should still be alive");
    println!(
        "three renders, {succeeded} usable, {} browser launches",
        renderer.launches()
    );
}

#[tokio::test]
#[ignore = "requires an installed browser and live network access"]
async fn concurrent_renders_do_not_start_a_browser_each() {
    // Two callers arriving at once must queue behind the same browser rather
    // than race to start their own.

    let Some(renderer) = ChromiumRenderer::discover(Arc::new(ReqwestHttpClient::new(30).unwrap()))
    else {
        panic!("no supported browser found");
    };
    let renderer = Arc::new(renderer);

    let mut tasks = Vec::new();
    for round in 1..=2 {
        let renderer = Arc::clone(&renderer);
        tasks.push(tokio::spawn(async move {
            let request = RenderRequest::new(VIDEO_URL, PLAYER_READY, COLLECT_PLAYER_MEDIA)
                .settle_on(VIDEO_METADATA_READY);
            let result = renderer.render(&request).await;
            (round, result.map(|page| page.media.len()))
        }));
    }

    let mut succeeded = 0;
    let mut failed = 0;
    let mut refusals = 0;
    for task in tasks {
        let (round, outcome) = task.await.expect("the render task must not panic");
        match outcome {
            Ok(sources) => {
                println!("concurrent render {round}: {sources} media sources");
                assert!(sources > 0, "a successful render must expose media");
                succeeded += 1;
            }
            Err(err) => {
                println!("concurrent render {round} failed: {err}");
                if is_source_redirect(&err) {
                    refusals += 1;
                }
                failed += 1;
            }
        }
    }

    if succeeded == 0 && refusals == failed {
        // Every render was refused by the source, which says nothing at all about
        // this application. Reported as the environment; nothing is claimed.
        println!(
            "
ENVIRONMENT: Douyin refused {VIDEO_URL} for both concurrent renders, so              nothing about sharing a browser could be observed. Nothing is claimed              about the application from this run.
"
        );
        return;
    }
    assert!(
        succeeded > 0,
        "neither concurrent render produced a page, so nothing about sharing a browser \
         could be observed"
    );
    assert!(
        renderer.launches() <= 1 + failed,
        "concurrent renders must share one browser, not start one each: {} browsers \
         were started for {failed} failed renders",
        renderer.launches()
    );
}

#[tokio::test]
#[ignore = "requires an installed browser and live network access"]
async fn a_renderer_recovers_after_its_browser_is_killed() {
    // Recovery from a crashed or disconnected browser: the next render must
    // start a new one rather than driving a corpse.

    let Some(renderer) = ChromiumRenderer::discover(Arc::new(ReqwestHttpClient::new(30).unwrap()))
    else {
        panic!("no supported browser found");
    };

    let request = RenderRequest::new(VIDEO_URL, PLAYER_READY, COLLECT_PLAYER_MEDIA)
        .settle_on(VIDEO_METADATA_READY);
    match renderer.render(&request).await {
        Ok(_) => {}
        Err(err) if source_would_not_serve(&err) => return,
        Err(err) => panic!("the first render: {err}"),
    }
    assert_eq!(renderer.launches(), 1);

    // Kill the browser this renderer owns — by its own process id, never by
    // image name: the machine may be running the user's own Chrome, and that is
    // not ours to touch.
    let pid = renderer.session_pid().expect("the renderer owns a browser");
    println!("killing the app's browser, pid {pid}");
    #[cfg(windows)]
    {
        let killed = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        println!("killed the app's browser: {killed:?}");
    }
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // The next render either reuses the dead session and has to recover, or the
    // dead session is replaced; either way it must end with a working page from
    // a *new* browser rather than an error the caller cannot get past.
    let page = match renderer.render(&request).await {
        Ok(page) => page,
        Err(err) if source_would_not_serve(&err) => return,
        Err(err) => panic!("the renderer must recover from a killed browser: {err}"),
    };
    println!(
        "recovered: {} media sources, {} launches",
        page.media.len(),
        renderer.launches()
    );
    assert!(!page.media.is_empty());
    assert!(
        renderer.launches() >= 2,
        "recovery has to involve a new browser"
    );
}

/// Whether a process with this id is still running.
///
/// Asked of the operating system rather than inferred from the renderer's own
/// bookkeeping: "the renderer forgot its session" and "the process is gone" are
/// different statements, and the reported bug is the second one.
fn process_is_alive(pid: u32) -> bool {
    use douyin_downloader_lib::process::NoConsoleWindow;

    let mut command = std::process::Command::new("tasklist");
    command.args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]);
    command.no_console_window();
    let output = command.output().expect("tasklist must run");
    String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
}

#[tokio::test]
#[ignore = "requires an installed browser and live network access"]
async fn an_idle_browser_is_closed_by_its_own_timer() {
    // The reported symptom, measured: chrome.exe still in Task Manager after
    // the work finished. A browser is kept between renders on purpose, so
    // "finished" is not the moment it should die — but nothing should be able
    // to say that a browser this application started is still running long
    // after the last thing it was asked to do.
    let Some(renderer) = ChromiumRenderer::discover(Arc::new(ReqwestHttpClient::new(30).unwrap()))
    else {
        panic!("no supported browser found");
    };

    // Shortened so the check does not sit here for the production minute and a
    // half. The value is read at each tick, so it applies to the timer that is
    // already running.
    renderer.set_idle_timeout(std::time::Duration::from_secs(3));

    let request = RenderRequest::new(VIDEO_URL, PLAYER_READY, COLLECT_PLAYER_MEDIA)
        .settle_on(VIDEO_METADATA_READY);

    // The live page occasionally serves the home feed instead; a failed render
    // has already retired its browser, so there would be nothing to watch.
    let mut outcome = renderer.render(&request).await;
    if outcome.is_err() {
        outcome = renderer.render(&request).await;
    }
    if let Err(err) = &outcome {
        if source_would_not_serve(err) {
            return;
        }
    }
    outcome.expect(
        "the live page produced nothing on either attempt, so the idle timer could not be observed",
    );

    let pid = renderer
        .session_pid()
        .expect("the renderer is holding a browser");
    println!("idle browser pid {pid}");
    assert!(
        process_is_alive(pid),
        "the browser is running before the wait"
    );

    // No render, no navigation, nothing: just longer than the timeout.
    tokio::time::sleep(std::time::Duration::from_secs(9)).await;

    assert!(
        !renderer.has_session(),
        "an idle browser must be released rather than held for a next render that may never come"
    );
    assert!(
        !process_is_alive(pid),
        "the browser process must be gone, not merely forgotten: pid {pid}"
    );
    println!("the idle browser was closed");
}
