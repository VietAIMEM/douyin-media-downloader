//! Douyin profile enumeration.
//!
//! # Why a profile needs a browser
//!
//! A Douyin profile page is assembled by the page's own JavaScript, and the
//! listing request it makes (`/aweme/v1/web/aweme/post/`) is rejected for an
//! anonymous visitor with `403 Blocked by ArgusSecurityPlugin Sign Invalid`.
//! That is a signature check on the request the *page itself* issues, so a
//! plain HTTP client cannot reproduce it and forging the signature would be
//! defeating an access control. The same is true of the plain share page: it
//! serves an anti-bot shell with no state in it at all.
//!
//! What remains is the method the post resolver already uses — let a real
//! browser open the same public page a person would, and read only what that
//! page is given. This module is the platform half of that: it supplies the
//! JavaScript and turns the result into domain models. The renderer supplies
//! the browser, the protocol, and the cleanup.
//!
//! # Why the filtering lives here and not in the page
//!
//! The collection script is deliberately dumb: it reports every post-shaped
//! anchor it can see, together with the structural facts needed to judge it
//! (which region it sits in, whether it carries an image, what `source` query
//! parameter it arrived with). Deciding which of those are *this creator's
//! posts* is platform knowledge, and keeping it in Rust means it is pure,
//! reviewable, and tested against recorded payloads rather than against a live
//! page that changes shape without warning.
//!
//! A profile page also carries site navigation, a footer of unrelated
//! recommendations, and legal links — all of which contain `/video/…` anchors.
//! Taking every anchor on the page as a post is the mistake this split exists
//! to prevent.

use serde_json::Value;

use crate::models::{ContentStatus, Creator, MediaType, Platform, Post};

/// Metadata key recording that a post came from a profile listing.
pub const LISTING_KEY: &str = "listing";

/// Longest identifier accepted from a page.
const MAX_ID_LEN: usize = 32;

/// Post ids are Douyin's numeric aweme ids.
fn valid_post_id(id: &str) -> bool {
    id.len() >= 8
        && id.len() <= MAX_ID_LEN
        && id.chars().all(|c| c.is_ascii_digit())
}

/// JavaScript predicate: the profile header has been rendered.
///
/// Deliberately about *identity* rather than about the post grid, because the
/// two arrive at different times and the grid is what the platform refuses most
/// often. A page whose header is present has told us who the creator is even
/// when it declines to list their posts, and that is worth reporting.
pub const PROFILE_READY: &str = concat!(
    "(function(){try{",
    "var d=document;",
    "if(d.querySelector('h1'))return true;",
    "if(/\\/user\\//.test(location.pathname)&&d.body.innerText.length>200)return true;",
    "return false;}catch(e){return false}})()"
);

/// JavaScript: collect the creator's identity and every post-shaped anchor.
///
/// Reports structure, not judgement — see the module note. Every field is
/// optional and a missing one stays missing.
pub const COLLECT_PROFILE: &str = r#"(function(){
  try {
    var d = document, out = { creator: {}, posts: [] };

    function textOf(el) { return el ? (el.innerText || el.textContent || '').trim() : ''; }

    // --- identity ---------------------------------------------------------
    // The page marks its own header with `data-e2e="user-info"`, and every
    // field below is read from inside it. Site-wide text is never consulted for
    // a fact about this creator: the footer carries other people's numbers and
    // other people's badges.
    var header = d.querySelector('[data-e2e="user-info"]');
    var headerText = textOf(header);
    var body = d.body ? d.body.innerText || '' : '';

    var h1 = d.querySelector('h1');
    out.creator.name = textOf(h1) || null;

    // The handle line, and the line under it, which is where the profile keeps
    // its signature.
    var handleLine = null;
    if (header) {
      var ps = header.querySelectorAll('p');
      for (var i = 0; i < ps.length; i++) {
        if (textOf(ps[i]).indexOf('抖音号') >= 0) { handleLine = ps[i]; break; }
      }
    }
    var handleText = handleLine ? textOf(handleLine) : (body.match(/抖音号[：:][^\n]{0,40}/) || [''])[0];
    var handle = handleText.match(/抖音号[：:]\s*([A-Za-z0-9_.\-]+)/);
    out.creator.handle = handle ? handle[1] : null;

    var sig = handleLine && handleLine.nextElementSibling
      ? textOf(handleLine.nextElementSibling)
      : '';
    out.creator.signature = sig.length > 0 && sig.length < 300 ? sig : null;

    // A verification badge is a claim about a person, so it is only reported
    // when the header actually carries one — never inferred from anything else.
    out.creator.verified = headerText.indexOf('认证徽章') >= 0;

    // Counts come from the labelled rows the header publishes. The digit
    // abbreviations the page uses (72.0K, 1.2万) are expanded, and a row that is
    // absent stays absent rather than becoming zero.
    function countFrom(key) {
      var el = d.querySelector('[data-e2e="' + key + '"]');
      if (!el) return null;
      var nums = (el.innerText || el.textContent || '').match(/([0-9]+(?:\.[0-9]+)?)([KkMm万])?/);
      if (!nums) return null;
      var value = parseFloat(nums[1]);
      if (!isFinite(value)) return null;
      var unit = nums[2];
      if (unit === '万') value *= 10000;
      else if (unit === 'K' || unit === 'k') value *= 1000;
      else if (unit === 'M' || unit === 'm') value *= 1000000;
      return Math.round(value);
    }
    out.creator.followerCount = countFrom('user-info-fans');
    out.creator.likeCount = countFrom('user-info-like');

    // The works tab is the only place the page states how many posts exist.
    var tabbar = d.querySelector('#user-tabbar');
    var tabText = textOf(tabbar) || body;
    var works = tabText.match(/作品\s*\n?\s*([0-9]+(?:\.[0-9]+)?万?)/);
    out.creator.postCount = works
      ? (works[1].indexOf('万') >= 0
          ? Math.round(parseFloat(works[1]) * 10000)
          : parseInt(works[1], 10))
      : null;
    if (out.creator.postCount !== null && !isFinite(out.creator.postCount)) {
      out.creator.postCount = null;
    }

    // The avatar, scoped to the header so a footer or a card image cannot be
    // mistaken for the creator's own.
    var avatar = header ? header.querySelector('img[src*="aweme-avatar"], img[src*="avatar"]') : null;
    if (!avatar) avatar = d.querySelector('img[src*="aweme-avatar"]');
    out.creator.avatarUrl = avatar && /^https?:/.test(avatar.src) ? avatar.src : null;

    // --- posts ------------------------------------------------------------
    var seen = {};
    var anchors = d.querySelectorAll('a[href*="/video/"], a[href*="/note/"]');
    for (var i = 0; i < anchors.length; i++) {
      var a = anchors[i];
      var href = a.getAttribute('href') || '';
      var m = href.match(/\/(video|note)\/([0-9]{8,32})/);
      if (!m) continue;
      var id = m[2];
      if (seen[id]) continue;
      seen[id] = 1;

      // Structural facts only: where the anchor sits and what it carries.
      var el = a, inFooter = false, inNav = false;
      while (el) {
        var tag = el.tagName ? el.tagName.toLowerCase() : '';
        if (tag === 'footer') inFooter = true;
        if (tag === 'nav' || tag === 'header') inNav = true;
        el = el.parentElement;
      }

      // Whether this anchor sits inside the page's own post list, when the page
      // marks one. Reported rather than acted on: the collector takes every
      // post-shaped link on the document, and the difference between that set
      // and the creator's own works is a fact about the page that has to be
      // measured before anything is done about it. Dropping links on a
      // container guess would throw away real posts the day the guess is wrong.
      var inList = false;
      try {
        inList = !!(a.closest && a.closest(
          '[data-e2e="user-post-list"], [data-e2e="user-post-item"]'
        ));
      } catch (e) {}

      var source = null;
      try { source = new URL(a.href, location.href).searchParams.get('source'); } catch (e) {}

      var thumb = a.querySelector('img');
      var dur = a.querySelector('[class*="duration"], [class*="Duration"]');

      out.posts.push({
        id: id,
        kind: m[1],
        url: a.href,
        caption: textOf(a) || null,
        thumbnailUrl: thumb && /^https?:/.test(thumb.src) ? thumb.src : null,
        durationSeconds: dur ? parseFloat((textOf(dur) || '').replace(/[^0-9.]/g, '')) || null : null,
        inFooter: inFooter,
        inNav: inNav,
        inList: inList,
        hasImage: !!thumb,
        source: source
      });
    }

    // The page's own verdict on whether its listing request worked. It is a
    // *diagnostic*, not a result: the site shows this banner when the request
    // it made for the post list came back unusable, which is the same event the
    // empty grid reports from the other side. Capturing it means a maintainer
    // reading a log sees the source's own words rather than having to infer.
    var banner = (d.body ? d.body.innerText : '').match(/服务异常[^\n]{0,40}/);
    out.pageError = banner ? banner[0] : null;

    // Whether the page is asking this visitor to sign in.
    //
    // Reported separately from `pageError` because it is a different statement:
    // a service error says the site's own request failed, a sign-in prompt says
    // the site is waiting for an account before it will show this listing. A
    // sign-out visitor can hit either, and only the second is something the
    // user can do anything about. Matched on the site's own words for it, and
    // on the sign-in dialog the page renders, so it is evidence rather than a
    // guess from the grid being empty.
    var loginText = body.match(/登录后查看更多|登录后查看|请先登录|登录后可查看/);
    var loginDialog = d.querySelector(
      '[data-e2e="login-modal"], #login-panel-new, [class*="login-panel"], [class*="loginModal"]'
    );
    out.loginRequired = Boolean(loginText || loginDialog);

    // How many times the settle predicate pressed the page's refresh control.
    // Diagnostic only: it is what distinguishes "the grid never arrived" from
    // "we stopped asking too early".
    out.refreshes = window.__dyListingRefresh || 0;

    // Whether the page marked a post list at all. Without this, a run where
    // every anchor read as "outside the list" would be indistinguishable from a
    // page that does not mark one — and those call for opposite conclusions.
    out.postListFound = !!d.querySelector(
      '[data-e2e="user-post-list"], [data-e2e="user-post-item"]'
    );

    return JSON.stringify(out);
  } catch (e) {
    return JSON.stringify({ error: String(e) });
  }
})()"#;

/// JavaScript predicate: the post grid holds at least `n` cards.
///
/// Built rather than constant because the threshold depends on how much of the
/// listing has been read so far. It **scrolls** before counting, because a
/// profile grid is loaded lazily: asking a page how many cards it has without
/// moving it reports only the first batch forever. The renderer polls this
/// until it is true or its budget runs out, so repeated evaluation walks the
/// page down the same way a person browsing it would.
///
/// It is also how "is there another page?" is answered honestly: a caller asks
/// for one more card than it needs, so a grid that can supply it says yes and
/// one that cannot simply never reaches the threshold.
pub fn grid_at_least(n: usize) -> String {
    format!(
        concat!(
            "(function(){{try{{",
            "var seen={{}},k=0;",
            "var a=document.querySelectorAll('a[href*=\"/video/\"], a[href*=\"/note/\"]');",
            "for(var i=0;i<a.length;i++){{",
            "var m=(a[i].getAttribute('href')||'').match(/\\/(video|note)\\/([0-9]{{8,32}})/);",
            "if(m&&!seen[m[2]]){{seen[m[2]]=1;k++;}}}}",
            "if(k<{n}){{window.scrollBy(0, window.innerHeight);}}",
            "return k>={n};}}catch(e){{return false}}}})()"
        ),
        n = n
    )
}

/// JavaScript predicate for a profile listing that can be retried by the page.
///
/// This is [`grid_at_least`] plus the one recovery that actually works. A
/// signed-out profile page routinely answers its own first listing request with
/// an empty response and shows `服务异常，重新刷新拉取数据` — but the page puts a
/// clickable `刷新` control in that message, and clicking it makes the page ask
/// for the list again. Measured on a live profile, that click is what turns an
/// empty grid into the creator's real posts.
///
/// So the same predicate that waits for the grid also presses that control when
/// the grid has not arrived — at most `max_refreshes` times, because this is a
/// retry of the *page's own* request and unbounded retrying of a source that is
/// refusing would be traffic for nothing.
///
/// The click lives in a predicate rather than in the resolver so it happens
/// inside the one browser session the render already paid for, exactly as a
/// person would do it, and without a second browser launch.
pub fn listing_settle(n: usize, max_refreshes: usize) -> String {
    format!(
        concat!(
            "(function(){{try{{",
            "var seen={{}},k=0;",
            "var a=document.querySelectorAll('a[href*=\"/video/\"], a[href*=\"/note/\"]');",
            "for(var i=0;i<a.length;i++){{",
            "var el=a[i],bad=false;",
            "while(el){{var t=el.tagName?el.tagName.toLowerCase():'';",
            "if(t==='footer'||t==='nav')bad=true;el=el.parentElement;}}",
            "if(bad)continue;",
            "var m=(a[i].getAttribute('href')||'').match(/\\/(video|note)\\/([0-9]{{8,32}})/);",
            "if(!m||seen[m[2]])continue;",
            "var src=null;try{{src=new URL(a[i].href,location.href).searchParams.get('source');}}catch(e){{}}",
            "if(src&&src.toLowerCase().indexOf('baiduspider')===0)continue;",
            "seen[m[2]]=1;k++;}}",
            "if(k>={n})return true;",
            // The grid has not arrived. Ask the page to fetch it again, the way
            // its own error message invites, and no more often than allowed.
            "if(window.__dyListingRefresh===undefined)window.__dyListingRefresh=0;",
            "if(window.__dyListingRefresh<{max}){{",
            "var c=document.querySelectorAll('span,a,button');",
            "for(var j=0;j<c.length;j++){{",
            "var e=c[j];",
            "if(e.children.length===0&&(e.textContent||'').trim()==='刷新'){{",
            "try{{e.click();}}catch(err){{}}",
            "window.__dyListingRefresh=window.__dyListingRefresh+1;break;}}}}}}",
            "window.scrollBy(0, window.innerHeight);",
            "return false;}}catch(e){{return false}}}})()"
        ),
        n = n,
        max = max_refreshes
    )
}

/// Decode the value a collection script returned.
///
/// The renderer hands back whatever the script produced. Chromium returns a
/// string here because the script stringifies its own result (a bare object
/// would not survive `Runtime.evaluate`'s by-value serialization intact for
/// every value the page can hold), so both a JSON string and an already-parsed
/// object are accepted.
pub fn decode_payload(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) => serde_json::from_str(text).ok(),
        Value::Object(_) => Some(value.clone()),
        _ => None,
    }
}

/// The service-error banner the page was showing, when it was showing one.
///
/// Purely diagnostic: it says the site's own listing request came back
/// unusable, which corroborates an empty grid rather than changing what the
/// grid means. Reporting it rather than inferring a cause from an absence is
/// the difference between evidence and a guess.
pub fn page_error(payload: &Value) -> Option<String> {
    payload
        .get("pageError")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| text.chars().take(80).collect())
}

/// Whether the page was asking this visitor to sign in.
///
/// Evidence from the page rather than an inference from the grid being empty:
/// the site's own words, or its sign-in dialog, have to be there. An empty grid
/// on its own is not proof — it is also what a page that has not finished
/// building looks like, and telling those two apart is the whole point of
/// asking the question this way.
pub fn login_required(payload: &Value) -> bool {
    payload
        .get("loginRequired")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// How many times the page's own refresh control was pressed while waiting.
///
/// Diagnostic: it separates "the grid never arrived" from "the wait stopped
/// asking", which are different problems with different fixes.
pub fn refresh_count(payload: &Value) -> u64 {
    payload.get("refreshes").and_then(Value::as_u64).unwrap_or(0)
}

/// Build a [`Creator`] from a profile payload.
///
/// Returns `None` when the page yielded no usable identity at all, so a page
/// that rendered nothing is reported as such rather than turned into a
/// placeholder creator.
pub fn creator_from_payload(
    payload: &Value,
    platform_id: &str,
    profile_url: &str,
) -> Option<Creator> {
    let creator = payload.get("creator")?;
    let name = creator
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let handle = creator
        .get("handle")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| format!("@{text}"));
    let signature = creator
        .get("signature")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let avatar = creator
        .get("avatarUrl")
        .and_then(Value::as_str)
        .filter(|url| crate::network::renderer::is_fetchable(url))
        .map(str::to_string);
    let post_count = creator
        .get("postCount")
        .and_then(Value::as_u64)
        .and_then(|count| u32::try_from(count).ok());
    let verified = creator
        .get("verified")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // A page that gave us nothing but an empty object is not an identity.
    if name.is_none() && handle.is_none() && avatar.is_none() && post_count.is_none() {
        return None;
    }

    let mut out = Creator::new(
        Platform::Douyin,
        platform_id,
        // The page's stated name when it gave one; otherwise the platform id,
        // which is a real value rather than an invented display name.
        name.unwrap_or(platform_id),
        profile_url,
    );
    out.handle = handle;
    out.avatar_url = avatar;
    out.description = signature;
    out.verified = verified;
    out.post_count = post_count;
    Some(out)
}

/// Build the creator's posts from a profile payload.
///
/// Only anchors that could plausibly be *this creator's* posts survive; see the
/// module note. An anchor is kept when it names a well-formed Douyin post id,
/// sits outside the site's navigation and footer, carries a thumbnail or a
/// caption, and did not arrive from a search-engine feed. Order is preserved,
/// because a profile lists newest first.
/// Whether the page placed this post inside its own works list.
///
/// `None` means the page did not say — which is not the same answer as "outside
/// it", and is why this is an `Option` rather than a `bool`.
pub fn in_works_list(post: &Post) -> Option<bool> {
    post.metadata
        .get(LISTING_KEY)
        .and_then(|listing| listing.get("inWorksList"))
        .and_then(Value::as_bool)
}

/// Whether the page marked a post list at all.
pub fn post_list_found(payload: &Value) -> bool {
    payload
        .get("postListFound")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub fn posts_from_payload(
    payload: &Value,
    creator: &Creator,
    page_index: usize,
) -> Vec<Post> {
    let Some(items) = payload.get("posts").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut posts: Vec<Post> = Vec::new();
    for (position, item) in items.iter().enumerate() {
        let Some(post) = post_from_entry(item, creator, page_index, position) else {
            continue;
        };
        // A page can repeat a card; the listing must not.
        if posts
            .iter()
            .any(|existing| existing.platform_post_id == post.platform_post_id)
        {
            continue;
        }
        posts.push(post);
    }
    posts
}

/// Normalize one anchor entry into a [`Post`], or reject it.
fn post_from_entry(
    item: &Value,
    creator: &Creator,
    page_index: usize,
    position: usize,
) -> Option<Post> {
    let id = item.get("id").and_then(Value::as_str)?;
    if !valid_post_id(id) {
        return None;
    }

    // Site chrome is not content: the footer and the navigation both contain
    // post-shaped links that belong to someone else entirely.
    if item.get("inFooter").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    if item.get("inNav").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    // Search-engine feeds are appended by the site, not published by the
    // creator, and their cards are not the creator's work.
    if let Some(source) = item.get("source").and_then(Value::as_str) {
        if source.to_ascii_lowercase().starts_with("baiduspider") {
            return None;
        }
    }

    let url = item.get("url").and_then(Value::as_str).unwrap_or_default();
    let url_is_post = url.contains(&format!("/video/{id}")) || url.contains(&format!("/note/{id}"));
    if !url_is_post || !crate::network::renderer::is_fetchable(url) {
        // A card that does not actually point at the post it names cannot be
        // downloaded from, so it is not offered.
        return None;
    }

    let caption = item
        .get("caption")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let thumbnail = item
        .get("thumbnailUrl")
        .and_then(Value::as_str)
        .filter(|url| crate::network::renderer::is_fetchable(url))
        .map(str::to_string);
    let has_image = item.get("hasImage").and_then(Value::as_bool).unwrap_or(false);

    // A card with neither a picture nor a caption is navigation chrome that
    // happened to link at a post; offering it would be offering nothing.
    if thumbnail.is_none() && caption.is_none() && !has_image {
        return None;
    }

    // The route is the only statement the listing makes about the content type,
    // and it is recorded as provisional because the real answer arrives when
    // the post is resolved.
    let kind = item.get("kind").and_then(Value::as_str).unwrap_or("video");
    let media_type = if kind == "note" {
        MediaType::Carousel
    } else {
        MediaType::Video
    };

    // A grid card renders `mm:ss`; it is a convenience label rather than a
    // measurement, so it is kept only when it parses to a positive duration.
    let duration = item
        .get("durationSeconds")
        .and_then(Value::as_f64)
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0);

    let mut post = Post::new(Platform::Douyin, id, media_type, url);
    post.canonical_url = Some(format!("https://www.douyin.com/{kind}/{id}"));
    post.creator_id = Some(creator.id.clone());
    post.creator_handle = creator.handle.clone();
    post.caption = caption;
    post.thumbnail_url = thumbnail;
    // Discovered, not downloaded — the listing says nothing about local state,
    // which the application service determines from its own database.
    post.status = ContentStatus::New;
    post.metadata.insert(
        LISTING_KEY.into(),
        serde_json::json!({
            "page": page_index,
            "position": position,
            "mediaTypeProvisional": true,
            // Recorded, not filtered on. See the collector: this is the fact
            // that explains a collected count differing from the page's own.
            "inWorksList": item.get("inList").and_then(Value::as_bool).unwrap_or(false)
        }),
    );
    if let Some(seconds) = duration {
        post.metadata
            .insert("listing_duration_seconds".into(), serde_json::json!(seconds));
    }
    Some(post)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn creator() -> Creator {
        Creator::new(
            Platform::Douyin,
            "MS4wLjABAAAAexample",
            "郭虎",
            "https://www.douyin.com/user/MS4wLjABAAAAexample",
        )
    }

    fn anchor(id: &str, kind: &str, overrides: Value) -> Value {
        let mut base = json!({
            "id": id,
            "kind": kind,
            "url": format!("https://www.douyin.com/{kind}/{id}"),
            "caption": "a caption",
            "thumbnailUrl": "https://p3.douyinpic.com/x.jpeg",
            "hasImage": true,
            "inFooter": false,
            "inNav": false
        });
        for (key, value) in overrides.as_object().expect("object") {
            base[key] = value.clone();
        }
        base
    }

    #[test]
    fn a_rendered_profile_yields_its_identity() {
        let payload = json!({
            "creator": {
                "name": "郭虎",
                "handle": "42299195146",
                "signature": "导演",
                "avatarUrl": "https://p3.douyinpic.com/avatar.jpeg",
                "verified": true,
                "postCount": 50
            },
            "posts": []
        });

        let creator = creator_from_payload(&payload, "MS4wLjABAAAAexample", "https://www.douyin.com/user/x")
            .expect("creator");
        assert_eq!(creator.display_name, "郭虎");
        assert_eq!(creator.handle.as_deref(), Some("@42299195146"));
        assert_eq!(creator.description.as_deref(), Some("导演"));
        assert_eq!(creator.post_count, Some(50));
        assert!(creator.verified);
        assert_eq!(creator.platform_id, "MS4wLjABAAAAexample");
    }

    #[test]
    fn a_page_with_no_identity_is_not_turned_into_a_placeholder() {
        assert!(creator_from_payload(&json!({ "creator": {} }), "id", "url").is_none());
        assert!(creator_from_payload(&json!({}), "id", "url").is_none());
    }

    #[test]
    fn identity_falls_back_to_the_platform_id_rather_than_an_invented_name() {
        let payload = json!({ "creator": { "postCount": 3 } });
        let creator = creator_from_payload(&payload, "MS4wLjABAAAAexample", "url").unwrap();
        assert_eq!(creator.display_name, "MS4wLjABAAAAexample");
        assert_eq!(creator.post_count, Some(3));
    }

    #[test]
    fn a_non_fetchable_avatar_is_dropped_rather_than_shown_broken() {
        let payload = json!({
            "creator": { "name": "x", "avatarUrl": "blob:https://www.douyin.com/abc" }
        });
        let creator = creator_from_payload(&payload, "id", "url").unwrap();
        assert!(creator.avatar_url.is_none());
    }

    #[test]
    fn listing_keeps_only_the_creators_posts() {
        let payload = json!({
            "posts": [
                anchor("7091303675863321870", "video", json!({})),
                // The footer's recommendation feed.
                anchor("7656718025294351643", "video", json!({ "inFooter": true })),
                // The same feed tagged as a search-engine result.
                anchor("7324125021499198739", "video", json!({ "source": "Baiduspider" })),
                // Site navigation.
                anchor("7166248013101272323", "video", json!({ "inNav": true })),
                anchor("7473536022312144155", "note", json!({}))
            ]
        });

        let posts = posts_from_payload(&payload, &creator(), 0);
        let ids: Vec<&str> = posts.iter().map(|p| p.platform_post_id.as_str()).collect();
        assert_eq!(ids, vec!["7091303675863321870", "7473536022312144155"]);
    }

    #[test]
    fn a_note_route_is_an_image_post_and_is_marked_provisional() {
        let payload = json!({ "posts": [anchor("7473536022312144155", "note", json!({}))] });
        let posts = posts_from_payload(&payload, &creator(), 0);
        assert_eq!(posts[0].media_type, MediaType::Carousel);
        assert_eq!(posts[0].metadata["listing"]["mediaTypeProvisional"], json!(true));
        assert_eq!(posts[0].canonical_url.as_deref(), Some("https://www.douyin.com/note/7473536022312144155"));
    }

    #[test]
    fn a_listed_post_is_discovered_not_downloaded() {
        let payload = json!({ "posts": [anchor("7091303675863321870", "video", json!({}))] });
        let creator = creator();
        let posts = posts_from_payload(&payload, &creator, 0);
        assert_eq!(posts[0].status, ContentStatus::New);
        // Media is not invented: a listing makes no claim about downloadable
        // renditions, and the download path resolves them when they are needed.
        assert!(posts[0].media_items.is_empty());
        assert_eq!(posts[0].creator_id.as_deref(), Some(creator.id.as_str()));
    }

    #[test]
    fn duplicates_within_a_page_are_collapsed() {
        let payload = json!({
            "posts": [
                anchor("7091303675863321870", "video", json!({})),
                anchor("7091303675863321870", "video", json!({}))
            ]
        });
        assert_eq!(posts_from_payload(&payload, &creator(), 0).len(), 1);
    }

    #[test]
    fn malformed_entries_are_dropped_rather_than_guessed_at() {
        let payload = json!({
            "posts": [
                { "id": "not-a-number", "url": "https://www.douyin.com/video/abc", "caption": "x" },
                // Right id, but the link does not point at it.
                { "id": "7091303675863321870", "url": "https://www.douyin.com/video/1", "caption": "x" },
                // Right shape, but not fetchable.
                { "id": "7091303675863321871", "url": "blob:https://www.douyin.com/video/7091303675863321871", "caption": "x" },
                // A bare link with nothing on it.
                { "id": "7091303675863321872", "url": "https://www.douyin.com/video/7091303675863321872" }
            ]
        });
        assert!(posts_from_payload(&payload, &creator(), 0).is_empty());
    }

    #[test]
    fn a_page_without_a_post_array_yields_nothing() {
        assert!(posts_from_payload(&json!({}), &creator(), 0).is_empty());
        assert!(posts_from_payload(&json!({ "posts": "nope" }), &creator(), 0).is_empty());
    }

    #[test]
    fn full_urls_and_ids_survive_the_payload_verbatim() {
        let payload = json!({
            "posts": [anchor("7091303675863321870", "video", json!({ "caption": "拿什么来保住5.5%的目标？" }))]
        });
        let posts = posts_from_payload(&payload, &creator(), 2);
        assert_eq!(posts[0].url, "https://www.douyin.com/video/7091303675863321870");
        assert_eq!(posts[0].caption.as_deref(), Some("拿什么来保住5.5%的目标？"));
        assert_eq!(posts[0].metadata["listing"]["page"], json!(2));
    }

    #[test]
    fn a_payload_that_the_renderer_stringified_is_decoded() {
        let text = json!({ "creator": { "name": "x" }, "posts": [] }).to_string();
        let decoded = decode_payload(&Value::String(text)).expect("decoded");
        assert!(creator_from_payload(&decoded, "id", "url").is_some());
        // An object passes through unchanged.
        assert!(decode_payload(&json!({ "creator": { "name": "x" } })).is_some());
        // Anything else is not a payload.
        assert!(decode_payload(&json!(42)).is_none());
    }

    // --- the page the live source actually serves ----------------------------

    /// The payload a real Douyin profile page actually produced, captured
    /// from a browser rendering the public profile of a well-known creator.
    ///
    /// The page carried the creator's header and **none of their posts**:
    /// every card on it belonged to the site footer's search-engine feed, and
    /// the collector said so. It was also showing its own service-error banner,
    /// because the listing request it made for itself had failed. This is the
    /// shape that must never be mistaken for a creator's work — or for a
    /// creator with nothing published — so it is kept as a fixture.
    fn live_profile_page_payload() -> serde_json::Value {
        serde_json::json!({
        "pageError": "服务异常，重新刷新拉取数据",
        "creator": {
            "name": "郭虎",
            "handle": "42299195146",
            "signature": "代表作：《莲花楼》《周生如故》《沉香如屑》《书卷一梦》",
            "verified": true,
            "postCount": 50,
            "avatarUrl": "https://p3-pc.douyinpic.com/img/aweme-avatar/tos-cn-i-0813c001_oob82kAGeAWED6RWCAIA3mNffM8AUFEDAm0Ied~c5_300x300.jpeg",
        },
        "posts": [
            {
                "id": "7694658348163594155",
                "kind": "note",
                "url": "https://www.douyin.com/note/7694658348163594155",
                "caption": "📢广东成考｜10月 9日打印准考证啦！原定10月12日打印准考证，现在已经可以打印啦！ 考试准考证+身份证缺一不可✅ A4打印，最好打印两份备份 考试10.17-10.18，祝大家顺利上岸🎓 #打印准考证 #成人高考 #考试时间 #上岸",
                "thumbnailUrl": null,
                "durationSeconds": null,
                "inFooter": true,
                "inNav": false,
                "hasImage": false,
                "source": null,
            },
            {
                "id": "7660368186910444800",
                "kind": "video",
                "url": "https://www.douyin.com/video/7660368186910444800?source=Baiduspider",
                "caption": "西安高温红色预警 最高温将破40℃",
                "thumbnailUrl": null,
                "durationSeconds": null,
                "inFooter": true,
                "inNav": false,
                "hasImage": false,
                "source": "Baiduspider",
            },
            {
                "id": "7278876926419291452",
                "kind": "video",
                "url": "https://www.douyin.com/video/7278876926419291452?source=Baiduspider",
                "caption": "57.第五单元《方向与位置》--买文具 #北师大版数学 #北师大版数学课本同步讲解",
                "thumbnailUrl": null,
                "durationSeconds": null,
                "inFooter": true,
                "inNav": false,
                "hasImage": false,
                "source": "Baiduspider",
            },
            {
                "id": "7287495360589696310",
                "kind": "video",
                "url": "https://www.douyin.com/video/7287495360589696310?source=Baiduspider",
                "caption": "换车是为了要更大的空间长期自驾走下去，不是为了上天😅我的需求是啥呢？#旅行大玩家 #自驾游 #环游中国 #诚信服务五大承诺 #房车旅行",
                "thumbnailUrl": null,
                "durationSeconds": null,
                "inFooter": true,
                "inNav": false,
                "hasImage": false,
                "source": "Baiduspider",
            },
            {
                "id": "7579946635573767537",
                "kind": "video",
                "url": "https://www.douyin.com/video/7579946635573767537?source=Baiduspider-sdc",
                "caption": "谢谢你的关注",
                "thumbnailUrl": null,
                "durationSeconds": null,
                "inFooter": true,
                "inNav": false,
                "hasImage": false,
                "source": "Baiduspider-sdc",
            },
            {
                "id": "7115776740902849822",
                "kind": "video",
                "url": "https://www.douyin.com/video/7115776740902849822?source=Baiduspider",
                "caption": "【斯科尔斯—重炮轰门集锦】现在的曼联有个他会怎样？ #曼联#精彩进球 #唯有足球不可辜负 #斯科尔斯重炮",
                "thumbnailUrl": null,
                "durationSeconds": null,
                "inFooter": true,
                "inNav": false,
                "hasImage": false,
                "source": "Baiduspider",
            },
            {
                "id": "7410262036451446067",
                "kind": "video",
                "url": "https://www.douyin.com/video/7410262036451446067?source=Baiduspider",
                "caption": "秋天的第一杯奶茶，你们喝了吗？",
                "thumbnailUrl": null,
                "durationSeconds": null,
                "inFooter": true,
                "inNav": false,
                "hasImage": false,
                "source": "Baiduspider",
            },
            {
                "id": "7375064705594854719",
                "kind": "video",
                "url": "https://www.douyin.com/video/7375064705594854719?source=Baiduspider-sdc",
                "caption": "民国时一辆汽车多少钱 普通人别说买车 油价都能吓死你 #历史讲解 #汽车历史",
                "thumbnailUrl": null,
                "durationSeconds": null,
                "inFooter": true,
                "inNav": false,
                "hasImage": false,
                "source": "Baiduspider-sdc",
            },
        ],
    })
    }

    #[test]
    fn the_live_profile_page_yields_its_creator() {
        let payload = live_profile_page_payload();
        let creator = creator_from_payload(
            &payload,
            "MS4wLjABAAAA76ZXrCqzqz3-8744EBeOoVccpaLzvqYY6140890v9qA",
            "https://www.douyin.com/user/MS4wLjABAAAA76ZXrCqzqz3-8744EBeOoVccpaLzvqYY6140890v9qA",
        )
        .expect("the live page names its creator");

        assert_eq!(creator.display_name, "郭虎");
        assert_eq!(creator.handle.as_deref(), Some("@42299195146"));
        assert_eq!(creator.post_count, Some(50));
        assert!(creator.verified);
        assert!(creator.description.is_some());
        assert!(creator.avatar_url.is_some());
    }

    /// The trap this fixture exists for.
    ///
    /// A signed-out Douyin profile page is not empty: it carries the creator's
    /// header and then a footer full of unrelated videos pulled from a
    /// search-engine feed. Each of those is a real, well-formed post link with a
    /// caption — everything a naive collector would happily hand back as the
    /// creator's work. None of them is.
    #[test]
    fn the_live_profile_page_contributes_no_creator_posts() {
        let payload = live_profile_page_payload();
        let cards = payload["posts"].as_array().expect("cards present");
        assert!(
            !cards.is_empty(),
            "the live page really does carry post-shaped links; that is the point"
        );

        let creator =
            creator_from_payload(&payload, "MS4wLjABAAAAexample", "https://www.douyin.com/user/x")
                .expect("creator");
        let posts = posts_from_payload(&payload, &creator, 0);

        assert!(
            posts.is_empty(),
            "the footer's feed must never be offered as the creator's posts, got {:?}",
            posts.iter().map(|p| p.platform_post_id.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn every_card_on_the_live_page_is_rejected_for_a_recorded_reason() {
        // Each rejection is one of the structural rules, so a later change to
        // those rules cannot quietly start accepting this page.
        let payload = live_profile_page_payload();
        let cards = payload["posts"].as_array().expect("cards");
        let footer = cards
            .iter()
            .filter(|card| card["inFooter"].as_bool() == Some(true))
            .count();
        let spider = cards
            .iter()
            .filter(|card| {
                card["source"]
                    .as_str()
                    .is_some_and(|s| s.to_ascii_lowercase().starts_with("baiduspider"))
            })
            .count();

        assert_eq!(footer, cards.len(), "every card sat in the site footer");
        assert_eq!(spider, cards.len() - 1, "all but one also came from a spider feed");
    }

    /// A page whose header resolved but whose grid never arrived is a refusal,
    /// not a creator with nothing published.
    #[test]
    fn the_live_page_has_a_creator_but_no_listing() {
        let payload = live_profile_page_payload();
        let creator =
            creator_from_payload(&payload, "MS4wLjABAAAAexample", "https://www.douyin.com/user/x")
                .expect("creator");
        // The declared count is real, so an empty grid cannot be read as zero.
        assert_eq!(creator.post_count, Some(50));
        assert!(posts_from_payload(&payload, &creator, 0).is_empty());
        // And the page said out loud that its own request had failed.
        assert!(page_error(&payload).is_some());
    }

    #[test]
    fn whether_the_page_listed_a_post_in_its_own_works_is_recorded() {
        // A live run collected 54 posts where the profile's own label said 50.
        // This flag is what settled which set the collector had actually taken
        // — every card was inside the page's own works list — so it has to
        // survive the trip from the page into the stored post. It is recorded,
        // never filtered on: dropping cards on a container guess would throw
        // away real posts the day the guess is wrong.
        let payload = json!({
            "creator": { "postCount": 50 },
            "posts": [
                anchor("7091303675863321870", "video", json!({ "inList": true })),
                anchor("7091303675863321871", "video", json!({}))
            ],
            "postListFound": true
        });

        let posts = posts_from_payload(&payload, &creator(), 0);

        assert_eq!(posts.len(), 2, "the flag must not drop a post");
        assert_eq!(in_works_list(&posts[0]), Some(true));
        // An anchor the page did not place in the list reads as outside it,
        // which is a real answer and not the same as "the page said nothing".
        assert_eq!(in_works_list(&posts[1]), Some(false));
        assert!(post_list_found(&payload));
        assert!(!post_list_found(&json!({ "posts": [] })));
    }

    #[test]
    fn post_id_validation_is_strict() {
        assert!(valid_post_id("7091303675863321870"));
        assert!(!valid_post_id("1234"));
        assert!(!valid_post_id("7091303675863321870x"));
        assert!(!valid_post_id(""));
    }

    #[test]
    fn the_scroll_predicate_counts_the_threshold_it_was_given() {
        assert!(grid_at_least(18).contains("k>=18"));
    }

    #[test]
    fn the_listing_predicate_asks_the_page_to_refresh_but_only_so_often() {
        let settle = listing_settle(19, 2);
        // It still waits for the grid...
        assert!(settle.contains("k>=19"));
        // ...and it presses the page's own control, which is the recovery that
        // actually works on a live profile.
        assert!(settle.contains("刷新"));
        assert!(settle.contains(".click()"));
        // ...but no more often than it was told to.
        assert!(settle.contains("__dyListingRefresh<2"));
    }

    #[test]
    fn a_listing_predicate_with_no_refreshes_allowed_never_clicks() {
        let settle = listing_settle(5, 0);
        assert!(settle.contains("__dyListingRefresh<0"));
    }

    #[test]
    fn the_pages_own_error_banner_is_reported_when_present() {
        // A page that had given up on its own listing request says so.
        let payload = json!({ "creator": {}, "posts": [], "pageError": "\u{670d}\u{52a1}\u{5f02}\u{5e38}，\u{91cd}\u{65b0}\u{5237}\u{65b0}\u{62c9}\u{53d6}\u{6570}\u{636e}" });
        assert!(page_error(&payload).is_some());

        // A page that simply had nothing carries no such claim.
        assert!(page_error(&json!({ "creator": {}, "posts": [] })).is_none());
        assert!(page_error(&json!({ "pageError": null })).is_none());
        assert!(page_error(&json!({ "pageError": "   " })).is_none());
    }

    #[test]
    fn a_long_error_banner_is_capped_rather_than_quoted_whole() {
        let payload = json!({ "pageError": "x".repeat(500) });
        assert_eq!(page_error(&payload).map(|t| t.chars().count()), Some(80));
    }

    #[test]
    fn the_refresh_count_is_reported_when_the_page_recorded_one() {
        assert_eq!(refresh_count(&json!({ "refreshes": 2 })), 2);
        assert_eq!(refresh_count(&json!({ "refreshes": 0 })), 0);
        // A payload from a page that never had the predicate applied says
        // nothing, which is different from having pressed it zero times.
        assert_eq!(refresh_count(&json!({})), 0);
    }
}
