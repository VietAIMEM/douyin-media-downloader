//! Reading a Douyin video page's player.
//!
//! When Douyin's share page does not server-render the post, the page's own
//! JavaScript fetches it and hands the result to an XGPlayer instance. That
//! player holds the signed media URLs in `player.config.url` — the only place
//! they appear, in the markup or in the page's embedded state.
//!
//! This module owns the two pieces of platform knowledge that requires: the
//! JavaScript that waits for the player and reads it, and the mapping from what
//! it returns into the application's own media model. The renderer that runs the
//! JavaScript knows nothing about Douyin; this module knows nothing about
//! browsers.
//!
//! # What it will not do
//!
//! It does not invent a URL, infer a quality tier the source did not declare, or
//! rewrite a watermarked URL into a clean one. Whatever the player exposes is
//! what appears — including a video-only and an audio-only track when that is
//! genuinely all the source offers.

use std::collections::HashMap;

use crate::models::{MediaItem, MediaType, MediaVariant, Post, TrackLayout, VariantKind};
use crate::network::{MediaSource, MediaSourceKind, NetworkMedia};

use super::extract::is_watermarked;

/// Which track a URL names, if it is one of Douyin's own track paths.
///
/// A strict allow-list. The network sees everything a page requested, and a
/// permissive rule would let an advertisement, a preview, or an unrelated clip
/// be offered as a rendition of this post. Only the source's own track paths
/// qualify, so nothing is promoted to "a track of your video" on a guess.
fn track_kind(url: &str) -> Option<MediaSourceKind> {
    let lower = url.to_ascii_lowercase();
    if lower.contains("media-audio") {
        Some(MediaSourceKind::Audio)
    } else if lower.contains("media-video") {
        Some(MediaSourceKind::Video)
    } else {
        None
    }
}

/// Tracks the page fetched that Douyin itself labelled.
///
/// This is what the player's configuration does not contain. A player that
/// streams separate renditions names some of them and pulls the rest itself, and
/// the audio is routinely in the second group.
pub fn sources_from_network(network: &[NetworkMedia]) -> Vec<MediaSource> {
    network
        .iter()
        .filter_map(|candidate| {
            let kind = track_kind(&candidate.url)?;
            Some(MediaSource {
                url: candidate.url.clone(),
                kind,
                mime_type: candidate.mime_type.clone(),
                width: None,
                height: None,
                duration_seconds: None,
                bitrate_kbps: None,
                // Recorded so a reader can tell how this source was found.
                labels: vec!["observed-in-browser".to_string()],
            })
        })
        .collect()
}

/// Combine the player's own source list with what the page fetched.
///
/// The player's list leads: the source ordered it, and it carries the geometry
/// the media element reported. Network-observed tracks are appended only when
/// they are not already present, so a track the player ranked is never displaced
/// by one the network merely saw a request for.
pub fn merge_sources(
    from_player: Vec<MediaSource>,
    network: &[NetworkMedia],
) -> Vec<MediaSource> {
    let mut merged = from_player;
    for source in sources_from_network(network) {
        if merged.iter().any(|existing| existing.url == source.url) {
            continue;
        }
        merged.push(source);
    }
    merged
}

/// What a render actually established about the page's tracks.
///
/// Watching the network and *seeing* a track are different facts. A browser that
/// never started playback requests nothing, so an empty observation set says
/// nothing about whether an audio track exists — it only says nobody looked at
/// the right moment. Conflating the two is how a silent download gets labelled
/// as a source that legitimately has no sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackEvidence {
    /// The page's traffic was not watched, so nothing was established.
    NotWatched,
    /// The traffic was watched, but the page requested no track at all. It never
    /// fetched its media, so nothing about its tracks was established.
    NoTracksRequested,
    /// The page requested at least one track. Which tracks appeared is then a
    /// finding about the content rather than an absence of looking.
    TracksRequested,
}

impl TrackEvidence {
    /// Classify a render from what it reported.
    ///
    /// `network_captured` is the renderer saying it watched; `observed_tracks` is
    /// how many of Douyin's own track requests it actually saw.
    pub fn from_render(network_captured: bool, observed_tracks: usize) -> Self {
        match (network_captured, observed_tracks) {
            (false, _) => Self::NotWatched,
            (true, 0) => Self::NoTracksRequested,
            (true, _) => Self::TracksRequested,
        }
    }

    /// Whether this evidence rules out a separate audio track.
    pub fn rules_out_audio(self) -> bool {
        matches!(self, Self::TracksRequested)
    }

    /// A short description for logs.
    pub fn describe(self) -> &'static str {
        match self {
            Self::NotWatched => "network-not-watched",
            Self::NoTracksRequested => "network-watched-no-track-requested",
            Self::TracksRequested => "tracks-requested",
        }
    }
}

/// State what tracks each item was found to have.
///
/// A video with no audio item is called `VideoOnly` only when the render produced
/// the evidence for it — see [`TrackEvidence`]. Every other case is `Unknown`,
/// because a silent download and an unexamined one are not the same thing and the
/// model must not present one as the other.
pub fn apply_track_layout(items: &mut [MediaItem], evidence: TrackEvidence) {
    let has_audio = items
        .iter()
        .any(|item| item.media_type == MediaType::Audio);

    for item in items.iter_mut() {
        let layout = match item.media_type {
            MediaType::Audio => TrackLayout::AudioOnly,
            MediaType::Video | MediaType::Carousel if has_audio => TrackLayout::AudioVideo,
            MediaType::Video | MediaType::Carousel if evidence.rules_out_audio() => {
                TrackLayout::VideoOnly
            }
            MediaType::Video | MediaType::Carousel => TrackLayout::Unknown,
            // An image is neither; saying so is better than borrowing a video's
            // vocabulary for it. A text post has no tracks at all.
            MediaType::Image | MediaType::Text => TrackLayout::Unknown,
        };
        item.set_track_layout(layout);
    }
}

/// JavaScript predicate that is truthy once the player has its source list.
///
/// Evaluated repeatedly by the renderer. It is written defensively: a page that
/// is still bootstrapping simply evaluates falsy rather than throwing.
pub const PLAYER_READY: &str = concat!(
    "(function(){",
    "try{",
    "var p=window.player;",
    "return !!(p&&p.config&&Array.isArray(p.config.url)&&p.config.url.length>0);",
    "}catch(e){return false}",
    "})()"
);

/// JavaScript predicate that is truthy once the media element knows its own size.
///
/// Used as a *settle* condition, not a readiness one: the player publishes its
/// source list first, and the element reports geometry a moment later. Waiting
/// for it is best-effort — a render that never sees it still succeeds, reporting
/// the dimensions as absent rather than inventing them.
pub const VIDEO_METADATA_READY: &str = concat!(
    "(function(){",
    "try{",
    "var v=document.querySelector('video');",
    "return !!(v&&v.videoWidth>0&&v.videoHeight>0);",
    "}catch(e){return false}",
    "})()"
);

/// JavaScript that reads the player's media list.
///
/// Returns `{ "media": [ ... ] }`. It accepts either bare URL strings or
/// `{src}` objects, because the player has used both shapes, and it reports only
/// what it actually found: a URL that is not `http(s)` (an MSE `blob:` handle,
/// for instance) is dropped rather than passed along.
pub const COLLECT_PLAYER_MEDIA: &str = r#"(function(){
  var media = [];
  var seen = {};
  function isPlayEndpoint(url){ return /aweme\/v1\/play/i.test(url); }
  function classify(url){
    if(isPlayEndpoint(url)) return "unknown";
    if(/media-audio|[\/._-]audio|mime_type=audio/i.test(url)) return "audio";
    if(/media-video|mime_type=video/i.test(url)) return "video";
    if(/\.mp4(\?|$)/i.test(url)) return "video";
    if(/\.m4a(\?|$)|\.mp3(\?|$)/i.test(url)) return "audio";
    return "unknown";
  }
  function add(url, labels){
    if(typeof url !== "string") return;
    if(!/^https?:\/\//i.test(url)) return;
    if(seen[url]) return;
    seen[url] = true;
    var tagged = (labels || []).slice();
    // A play endpoint is a redirector to a rendition, not a file. It is marked
    // so it is never presented as an original source.
    if(isPlayEndpoint(url)) tagged.push("play-endpoint");
    media.push({ url: url, kind: classify(url), labels: tagged });
  }
  function addList(list, labels){
    if(!Array.isArray(list)) return;
    for(var i=0;i<list.length;i++){
      var entry = list[i];
      if(typeof entry === "string") add(entry, labels);
      else if(entry && typeof entry.src === "string") add(entry.src, labels);
      else if(entry && typeof entry.url === "string") add(entry.url, labels);
    }
  }

  try{
    var p = window.player;
    if(p && p.config){
      addList(p.config.url, []);
      // Some builds keep additional renditions beside the main list.
      if(p.config.videoConfig) addList(p.config.videoConfig.url, []);
    }
    if(p && p.videoConfig) addList(p.videoConfig.url, []);
  }catch(e){}

  // --- images -------------------------------------------------------------
  //
  // A note/photo post renders its slides as images in the page, and nothing in
  // the player's list mentions them. The post's own data is frequently absent
  // from the page entirely, so the rendered gallery is the only evidence there
  // is — which is why photo posts used to fall through to whatever the player
  // happened to expose.
  //
  // The filter is deliberately about *size*, not about selectors: a slide is a
  // large picture, and the site's chrome — avatars, icons, badges — is small.
  // Selector names for the gallery change between builds; "big pictures, in DOM
  // order" does not.
  var images = [];
  var seenImage = {};
  var imgStats = { total: 0, skippedAvatar: 0, skippedSmall: 0, skippedScheme: 0 };

  // A picture big enough to be a slide rather than chrome. Two things the size
  // test has to get right, and both were measured live:
  //
  // Natural size, never displayed size — the viewer's layout has not settled
  // when this runs and a slide that has not been measured reports zero, which
  // filtered out every slide.
  //
  // Area, not a minimum edge — the viewer draws small thumbnails of its own
  // slides beside them, 248x372 next to 2160x3238, and a 200px edge let four of
  // them through as though they were slides.
  function isSlide(im){
    var w = im.naturalWidth || 0, h = im.naturalHeight || 0;
    return w * h >= 300000;
  }
  // The picture a URL serves, before the rendition suffix that names its size.
  // Two URLs with the same identity are the same slide shown twice.
  function slideKey(url){
    try{ var u = new URL(url); return u.pathname.split("~")[0]; }catch(e){ return url; }
  }
  function largeInside(root){
    var inner = root.getElementsByTagName("img");
    var count = 0;
    for(var q=0;q<inner.length;q++){
      var src = inner[q].currentSrc || inner[q].src || "";
      if(!/^https?:\/\//i.test(src)) continue;
      if(/avatar/i.test(src)) continue;
      if(!isSlide(inner[q])) continue;
      count++;
    }
    return count;
  }

  // The post's own viewer, tried in order of how specifically each name means
  // "the overlay this post is being shown in". The broadest match is not the
  // best one: on a live note page the widest match held 34 pictures and the
  // modal held 11, and taking the widest is how a recommendation strip ends up
  // counted as slides.
  var viewers = ['[role="dialog"]', '[class*="Modal"]', '[class*="modal"]',
                 '[data-e2e*="note"]', '[class*="swiper"]', '[data-e2e*="slide"]',
                 '[class*="gallery"]'];
  var containers = [];
  var viewer = null, viewerName = null, viewerCount = 0;
  try{
    for(var s=0;s<viewers.length && !viewer;s++){
      var candidates = document.querySelectorAll(viewers[s]);
      var best = null, bestCount = 0;
      for(var c=0;c<candidates.length;c++){
        var count = largeInside(candidates[c]);
        if(count > 0) containers.push(viewers[s] + ":" + count);
        if(count > bestCount){ best = candidates[c]; bestCount = count; }
      }
      if(bestCount >= 2){
        viewer = best;
        viewerName = viewers[s];
        viewerCount = bestCount;
      }
    }
  }catch(e){}

  try{
    // Pictures are read *only* from the post's own viewer, and a page that has
    // no viewer has no pictures to offer. There is deliberately no fallback to
    // the whole document: measured on a live video page, that fallback picked up
    // eleven pictures belonging to the feed behind the post — `byteimg.com`
    // cards that have nothing to do with it — and offered them as media for a
    // video. A page that does not mark a viewer has not said it holds pictures,
    // and finding none is the honest answer.
    var imgs = viewer ? viewer.getElementsByTagName("img") : [];
    imgStats.total = document.getElementsByTagName("img").length;
    for(var k=0;k<imgs.length;k++){
      var im = imgs[k];
      // The URL the browser actually chose, never a srcset member: those are
      // the same slide at another size, and counting them would invent slides.
      var src = im.currentSrc || im.src || "";
      if(!/^https?:\/\//i.test(src)){ imgStats.skippedScheme++; continue; }
      if(/avatar/i.test(src)){ imgStats.skippedAvatar++; continue; }
      if(!isSlide(im)){ imgStats.skippedSmall++; continue; }
      if(seenImage[src]) continue;
      seenImage[src] = true;
      images.push({ url: src, width: im.naturalWidth, height: im.naturalHeight, alt: im.alt || null });
    }
  }catch(e){}

  // --- the slide track ----------------------------------------------------
  //
  // The viewer lays its slides out side by side inside one container, so that
  // container's children *are* the slides. Measured live on a nine-slide post:
  // one div holding nine children, each holding a slide picture. Two things
  // come from reading it rather than the pictures alone — the creator's own
  // order, and a structural count that owes nothing to how many pictures the
  // page happened to have drawn.
  var track = null, trackCount = 0;
  if(viewer){
    try{
      var boxes = viewer.querySelectorAll("div,ul");
      for(var t=0;t<boxes.length;t++){
        var kids = boxes[t].children;
        if(kids.length < 2) continue;
        var holders = 0;
        for(var q=0;q<kids.length;q++){
          var gk = kids[q].getElementsByTagName("img");
          for(var gr=0;gr<gk.length;gr++){
            if(isSlide(gk[gr])){ holders++; break; }
          }
        }
        // Every child a slide, and the largest such container: an ancestor that
        // also qualifies is a wrapper, not the track.
        if(holders === kids.length && holders > trackCount){ track = boxes[t]; trackCount = holders; }
      }
    }catch(e){}
  }

  // Slides in the track's order, and only the track's slides.
  //
  // The track is the album. Anything else the viewer happens to be showing is
  // not, and a live run proved it: one collection returned an extra large
  // picture from a second CDN hostname that the track did not contain, while the
  // page's own counter still said nine. A picture is kept when its *identity* —
  // the object path, before any rendition suffix — is one the track holds. The
  // same slide served at another size therefore still counts, and a stranger
  // does not.
  //
  // How many were seen before the filter is reported as well, because "the track
  // excluded everything" and "the gallery had nothing in it yet" look identical
  // from the outside and call for completely different answers.
  var imagesBeforeTrack = images.length;
  var trackIncluded = null;
  if(track){
    try{
      var rank = {}, inTrack = {}, next = 0;
      var tk = track.children;
      for(var z=0;z<tk.length;z++){
        var ti = tk[z].getElementsByTagName("img");
        for(var ty=0;ty<ti.length;ty++){
          var ts = ti[ty].currentSrc || ti[ty].src || "";
          if(!ts || !isSlide(ti[ty])) continue;
          if(rank[ts] === undefined) rank[ts] = next++;
          inTrack[slideKey(ts)] = 1;
        }
      }
      var kept = [];
      for(var zz=0;zz<images.length;zz++){
        if(inTrack[slideKey(images[zz].url)]) kept.push(images[zz]);
      }
      // Only when the track actually accounts for the gallery. A track detected
      // on a page that keeps its pictures somewhere else must not silently empty
      // the collection.
      trackIncluded = kept.length;
      if(kept.length >= 2) images = kept;
      images.sort(function(a,b){
        var ra = rank[a.url] === undefined ? 1e9 : rank[a.url];
        var rb = rank[b.url] === undefined ? 1e9 : rank[b.url];
        return ra - rb;
      });
    }catch(e){}
  }

  // What the page itself says the album holds, when it says anything. A note
  // viewer prints "1/9" beside the gallery; that is the source stating its own
  // total, and it is the only thing here that can contradict a collection that
  // stopped early. Absent means absent — never inferred, never defaulted.
  var declaredTotal = null;
  try{
    var scope = viewer || document;
    var marks = scope.querySelectorAll("span,div,p,em,i");
    for(var m2=0;m2<marks.length;m2++){
      var txt = (marks[m2].innerText || "").trim();
      if(txt.length > 16) continue;
      var mm = /^(\d+)\s*[\/·]\s*(\d+)$/.exec(txt);
      if(!mm) continue;
      var total = parseInt(mm[2], 10);
      if(isFinite(total) && total > 0 && (declaredTotal === null || total > declaredTotal)){
        declaredTotal = total;
      }
    }
  }catch(e){}

  // When the creator published it, as the page states it. Read by its *shape*
  // rather than by a class name: the note sidebar prints `发布时间：2026-04-18
  // 15:23:11`, and the surrounding markup is CSS-module hashed and changes
  // between builds. A date the page did not state is left absent rather than
  // guessed at.
  var publishedAt = null;
  try{
    var scopeText = (viewer || document).innerText || "";
    var when = /发布(?:时间|于)\s*[:：]\s*(\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2})/.exec(scopeText);
    if(when) publishedAt = when[1].replace(" ", "T");
  }catch(e){}

  for(var m=0;m<images.length;m++){
    add(images[m].url, ["page-image"]);
    var item = media[media.length - 1];
    if(item && item.url === images[m].url){
      item.kind = "image";
      item.width = images[m].width;
      item.height = images[m].height;
    }
  }

  // Track geometry and duration live on the element, never on the source list.
  var el = document.querySelector("video");
  if(el){
    for(var i=0;i<media.length;i++){
      if(media[i].kind !== "video") continue;
      if(el.videoWidth) media[i].width = el.videoWidth;
      if(el.videoHeight) media[i].height = el.videoHeight;
      if(isFinite(el.duration) && el.duration > 0) media[i].durationSeconds = el.duration;
    }
  }

  // What was looked at, so a resolution that finds nothing can say *why* rather
  // than reporting a bare failure. Counts and hosts only — never a full URL.
  var hosts = {};
  for(var n=0;n<images.length;n++){
    try{ hosts[new URL(images[n].url).host] = 1; }catch(e){}
  }
  var og = document.querySelector('meta[property="og:image"]');
  var ogHost = null;
  if(og && og.content){ try{ ogHost = new URL(og.content).host; }catch(e){} }
  var caption = document.querySelector('meta[property="og:description"]');

  return {
    media: media,
    diagnostics: {
      imagesFound: images.length,
      imagesBeforeTrack: imagesBeforeTrack,
      trackIncluded: trackIncluded,
      imgTotal: imgStats.total,
      imgSkippedAvatar: imgStats.skippedAvatar,
      imgSkippedSmall: imgStats.skippedSmall,
      imgSkippedScheme: imgStats.skippedScheme,
      gallery: viewerName,
      galleryCount: viewerCount,
      slideTrack: track ? track.tagName : null,
      slideTrackCount: trackCount,
      declaredTotal: declaredTotal,
      publishedAt: publishedAt,
      containers: containers.slice(0, 12).join(","),
      imageHosts: Object.keys(hosts).join(","),
      ogImageHost: ogHost,
      captionChars: caption && caption.content ? caption.content.length : 0,
      videoElement: !!el
    }
  };
})()"#;

/// The identity of a picture, independent of which rendition it was served as.
///
/// Douyin serves one picture at many renditions from one path. The same object
/// arrives with a `~noop` suffix at its original size and with a `~tplv-…`
/// suffix cropped, resized and re-encoded, and the page uses both at once: a
/// slide in the gallery and the very same slide in a preview strip are two
/// `<img>` elements with two different URLs. Everything after the first `~`
/// names the rendition, so what identifies the picture is what comes before it.
///
/// The host is deliberately not part of the key. The same object is served from
/// several CDN hostnames, and which one a run happens to get is not a fact about
/// the picture. The path already carries the bucket and the object's own id.
pub fn logical_slide_key(url: &str) -> String {
    // The path alone: no scheme, no hostname, no query, no fragment.
    let path = match url::Url::parse(url) {
        Ok(parsed) => parsed.path().to_string(),
        Err(_) => url
            .split(['?', '#'])
            .next()
            .unwrap_or(url)
            .split_once("://")
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_else(|| url.to_string()),
    };
    match path.split_once('~') {
        Some((head, _)) => head.to_string(),
        None => path,
    }
}

/// Collapse the several renditions of one picture into one entry each.
///
/// Counting URLs counted slides twice. A live run of a nine-slide post produced
/// ten items: the gallery's nine slides, and then the first picture again at
/// full size, because the page's preview strip shows that same picture and
/// serves it at a different URL. The page's own counter said nine, and the page
/// was right.
///
/// Which rendition is kept is decided by the natural size the page reported —
/// the *largest* of them, so that identifying renditions with each other never
/// costs resolution. It is deliberately not a size *threshold*: a gallery
/// legitimately holds pictures of very different shapes, from 1080x1920 portraits
/// to 4096x1364 banners, and a threshold that kept the one would drop the other.
/// Renditions are only ever compared with the other renditions of their own
/// picture, never against each other.
///
/// Order is first appearance, so the slides keep the creator's order.
fn collapse_slide_renditions<'a>(sources: Vec<&'a MediaSource>) -> Vec<&'a MediaSource> {
    let mut order: Vec<String> = Vec::new();
    let mut best: HashMap<String, &'a MediaSource> = HashMap::new();
    let mut area: HashMap<String, u64> = HashMap::new();

    for source in sources {
        let key = logical_slide_key(&source.url);
        let size =
            u64::from(source.width.unwrap_or(0)) * u64::from(source.height.unwrap_or(0));
        match area.get(&key).copied() {
            None => {
                order.push(key.clone());
                best.insert(key.clone(), source);
                area.insert(key, size);
            }
            Some(existing) if size > existing => {
                best.insert(key.clone(), source);
                area.insert(key, size);
            }
            Some(_) => {}
        }
    }

    order.iter().filter_map(|key| best.get(key).copied()).collect()
}

/// One media item per picture the page displayed.
///
/// Each slide is a download in its own right: a carousel is a set of pictures,
/// not one picture with several candidate URLs, so the slides are separate items
/// and are never collapsed into one. Order is the page's own order — the order
/// the creator published — and each item records its slide number, so "the third
/// picture" keeps its identity across a re-resolution.
fn image_items(post: &Post, images: &[&MediaSource]) -> Vec<MediaItem> {
    images
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let mut item = MediaItem::new(post.id.clone(), MediaType::Image, source.url.clone());
            // A picture is its own thumbnail; there is nothing smaller to point at.
            item.thumbnail_url = Some(source.url.clone());
            item.width = source.width;
            item.height = source.height;
            item.mime_type = source.mime_type.clone();
            item.extension = Some(image_extension(&source.url).to_string());
            item.metadata
                .insert("slide".to_string(), serde_json::json!(index));
            item
        })
        .collect()
}

/// The extension to write a picture with, taken from the URL the source gave.
///
/// Provisional, like every extension before the bytes arrive: the download reads
/// the file's own signature, and that is what actually decides whether this was
/// an image at all. Douyin serves `.jpeg` and `.webp` here, and a URL carrying no
/// suffix is given the commonest of them rather than left without one.
fn image_extension(url: &str) -> &'static str {
    let lowered = url.to_ascii_lowercase();
    let path = lowered.split(['?', '#']).next().unwrap_or("");
    if path.ends_with(".png") {
        "png"
    } else if path.ends_with(".webp") {
        "webp"
    } else if path.ends_with(".gif") {
        "gif"
    } else if path.ends_with(".bmp") {
        "bmp"
    } else {
        "jpg"
    }
}

/// JavaScript predicate that is truthy once the post's pictures have stopped
/// arriving.
///
/// A note viewer renders its slides as they are needed, so the number of
/// pictures on the page at any instant is a fact about how far it has got, not
/// about the post. Reading it too early is why one run found three slides and
/// another found twenty-five for the same post.
///
/// Two things make the wait mean something. It counts *distinct pictures*, not
/// `<img>` elements — a viewer mid-transition draws the same slide in several
/// places, and a live run showed nineteen large images that were two slides. And
/// it wants eight polls without a new one, not three: a viewer between slides
/// looks exactly like a viewer that has finished, and the difference is only how
/// long you are willing to look.
///
/// It is a *settle* condition, so a gallery that never stops still ends the
/// render rather than hanging it — and the caller is told the count it saw.
pub const GALLERY_SETTLED: &str = concat!(
    "(function(){try{",
    "var d=document;",
    "var v=d.querySelector('[role=\"dialog\"]')||d.querySelector('[class*=\"Modal\"]')",
    "  ||d.querySelector('[data-e2e*=\"note\"]');",
    "if(!v)return false;",
    "var n=0,seen={},a=v.getElementsByTagName('img');",
    "for(var i=0;i<a.length;i++){",
    "var s=a[i].currentSrc||a[i].src||'';",
    "if(!s||seen[s])continue;",
    "if((a[i].naturalWidth||0)*(a[i].naturalHeight||0)>=300000){seen[s]=1;n++;}}",
    // A picture still loading reports naturalWidth 0, so a count that includes
    // one is not a count that has finished.
    "if(n===0)return false;",
    "if(window.__dyGalleryCount===n){",
    "window.__dyGalleryStable=(window.__dyGalleryStable||0)+1;",
    "}else{window.__dyGalleryCount=n;window.__dyGalleryStable=0;}",
    "return window.__dyGalleryStable>=8;",
    "}catch(e){return false}})()"
);

/// JavaScript predicate that is truthy once the post's viewer holds a picture.
///
/// A *readiness* condition, and it has to be one. A note page does not always
/// have a player — a live render reported `player: "undefined"` and a full page
/// body — so waiting only for `window.player.config.url` waits for ever on a
/// page that has nothing to play, and the post resolves as a failure.
pub const GALLERY_READY: &str = concat!(
    "(function(){try{",
    "var d=document;",
    "var v=d.querySelector('[role=\"dialog\"]')||d.querySelector('[class*=\"Modal\"]')",
    "  ||d.querySelector('[data-e2e*=\"note\"]');",
    "if(!v)return false;",
    "var a=v.getElementsByTagName('img');",
    "for(var i=0;i<a.length;i++){",
    "if((a[i].naturalWidth||0)*(a[i].naturalHeight||0)>=300000)return true;}",
    "return false;",
    "}catch(e){return false}})()"
);

/// JavaScript predicate that is truthy once a page with *only words* is loaded.
///
/// A readiness condition, and it has to exist. Every other predicate here waits
/// for something a media post has — the player's source list, or a picture big
/// enough to be a slide — so on a caption-only post they are all false for ever,
/// the render never becomes ready, and a post this application can perfectly well
/// read ends as "the page never became ready". That is the same trap
/// [`GALLERY_READY`] was written for, one step further on.
///
/// Deliberately narrow, because firing early is the expensive mistake — and it
/// was measured making it. A first version asked only for "text in the note's
/// scope and no media", and on a note page whose post the source no longer
/// serves it fired in two seconds on the loading shell: the render reported
/// success for a page with nothing on it, and the user was told the post had no
/// downloadable media instead of being told it could not be read.
///
/// Three things close that:
///
/// * **No `<video>` element at all.** A photo post draws its slides inside the
///   note's own player container and reports one, so a note post with pictures
///   can never satisfy this, and the gallery readiness that waits for a real
///   slide stays in charge of it.
/// * **A title the page set for itself.** Douyin puts the post's own words there
///   once its data has arrived — measured: `#凤不栖 开机大吉！顺顺利利！ - 抖音` on
///   the real post, and nothing at all on the shell. This is the difference
///   between "the container has drawn" and "the post is here".
/// * **Enough text to be a post.** A shell with a few characters is not content.
pub const TEXT_READY: &str = concat!(
    "(function(){try{",
    "var d=document;",
    "var v=d.querySelector('[role=\"dialog\"]')||d.querySelector('[class*=\"Modal\"]')",
    "  ||d.querySelector('[data-e2e*=\"note\"]');",
    "if(!v)return false;",
    // Pictures and video have their own readiness conditions and must wait for
    // them rather than being settled for by this one.
    "if(d.querySelector('video'))return false;",
    "var a=v.getElementsByTagName('img');",
    "for(var i=0;i<a.length;i++){",
    "if((a[i].naturalWidth||0)*(a[i].naturalHeight||0)>=300000)return false;}",
    // The page's own statement that it has loaded a post rather than a frame.
    "var t=(d.title||'').replace(/\\s+/g,' ').trim();",
    "if(t.length<2 || /^抖音$/.test(t))return false;",
    // And something worth reading under it.
    "var body=(v.innerText||'').replace(/\\s+/g,' ').trim();",
    "if(body.length<8)return false;",
    // Held for a moment before it counts. A player that has not been created yet
    // looks exactly like a post with no player, and the moment between the page
    // setting its title and its player appearing is precisely when that would be
    // believed. Four polls is about a second: short enough not to delay a real
    // text post, long enough that a page about to grow a video is not mistaken
    // for one. Every other condition here re-checks, so a page that changes its
    // mind simply stops counting.
    "window.__dyTextReady=(window.__dyTextReady||0)+1;",
    "return window.__dyTextReady>=4;",
    "}catch(e){return false}})()"
);

/// The player's list plus the page's pictures, as one settle condition.
///
/// A video page settles when the element knows its geometry; a note page has no
/// such moment and settles when its gallery stops growing. Either finishing ends
/// the wait.
pub fn post_settle_condition() -> String {
    format!("({VIDEO_METADATA_READY}) || ({GALLERY_SETTLED})")
}

/// What makes a post page ready to read.
///
/// Either kind of media page will do: a video page is ready when its player has
/// its source list, and a note page is ready when its viewer holds a picture.
/// Requiring both would fail every note page without a player, and requiring only
/// the player is what did exactly that.
///
/// A page with *only words* satisfies neither, and is deliberately not waited for
/// here — see [`text_post_ready_condition`], which is asked only after this one
/// has already given up. Adding the text case to this predicate was tried and
/// measured: it made every note page become ready on its title a second or two
/// in, before its gallery had drawn, and collections of the known carousel came
/// back empty or ragged. A condition that changes when a working page is read is
/// too expensive to be worth the case it adds.
pub fn post_ready_condition() -> String {
    format!("({PLAYER_READY}) || ({GALLERY_READY})")
}

/// What makes a page that carries no media at all ready to read.
///
/// Asked only as a second question, of a page that has already failed to become
/// a media page — so it can cost a working page nothing. A caption-only post
/// satisfies no media predicate at all, waits out the whole readiness budget and
/// ends as "the page never became ready"; this is the question that finds it.
pub fn text_post_ready_condition() -> String {
    TEXT_READY.to_string()
}

/// Turn the player's media sources into normalized media items.
///
/// Pictures become one item each, video sources become one video item carrying
/// every rendition the player exposed, and audio sources become their own item.
/// A page that offers only one of the three yields only that one.
pub fn sources_to_media(post: &Post, sources: &[MediaSource]) -> Vec<MediaItem> {
    // Defence in depth. The renderer already refuses anything that is not an
    // `http(s)` URL, but this is the last point before a URL becomes a download
    // target, so the rule is applied again rather than trusted across a boundary.
    let usable: Vec<&MediaSource> = sources
        .iter()
        .filter(|source| crate::network::renderer::is_fetchable(&source.url))
        .collect();

    let mut items = Vec::new();

    let video: Vec<&MediaSource> = usable
        .iter()
        .copied()
        .filter(|source| source.kind == MediaSourceKind::Video)
        .collect();
    let audio: Vec<&MediaSource> = usable
        .iter()
        .copied()
        .filter(|source| source.kind == MediaSourceKind::Audio)
        .collect();
    // A source whose kind could not be told apart is offered as another
    // rendition of the video rather than dropped.
    let undetermined: Vec<&MediaSource> = usable
        .iter()
        .copied()
        .filter(|source| source.kind == MediaSourceKind::Unknown)
        .collect();

    // A video is built from sources the page said *are* video, plus — when there
    // is nothing else — Douyin's own `/aweme/v1/play/` redirector, which exists
    // only to serve a video and so is evidence of one. An unlabelled source may
    // also join a video that exists, as another rendition of it.
    //
    // What may *not* create a video is a resource the page labelled nothing
    // about on any other URL. That is the reported symptom exactly: Douyin
    // serves a post's music through the same player slot a video uses, the page
    // labelled nothing about it, and an unlabelled aac stream on the static CDN
    // became a downloaded "video" holding no picture at all.
    let play_endpoint = undetermined.iter().copied().any(is_play_endpoint);
    let is_video_post = !video.is_empty() || play_endpoint;

    // Pictures are this post's media only when the page offered no video at all.
    // A page that served a video is a video post whatever else is on it — the
    // feed behind a post is full of pictures, and taking them as this post's
    // media is how a live video post came to offer eleven slides of other
    // people's content. A picture post is one whose viewer offered pictures and
    // no video, which is what is left when this branch is reached.
    if !is_video_post {
        let images: Vec<&MediaSource> = usable
            .iter()
            .copied()
            .filter(|source| source.kind == MediaSourceKind::Image)
            .collect();
        items.extend(image_items(post, &collapse_slide_renditions(images)));
    }

    if is_video_post {
        let mut ordered = video;
        ordered.extend(undetermined);
        items.push(video_item(post, &ordered));
    }

    if !audio.is_empty() {
        items.push(audio_item(post, &audio));
    }

    items
}

/// Whether a source is Douyin's play redirector rather than a media file.
fn is_play_endpoint(source: &MediaSource) -> bool {
    source
        .labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case("play-endpoint"))
}

fn video_item(post: &Post, sources: &[&MediaSource]) -> MediaItem {
    // Direct media files come first; a play endpoint is a redirector to one of
    // them, so it is offered as a fallback rather than as the default.
    let mut ordered: Vec<&MediaSource> = sources
        .iter()
        .copied()
        .filter(|source| !is_play_endpoint(source))
        .collect();
    ordered.extend(sources.iter().copied().filter(|source| is_play_endpoint(source)));

    let primary = ordered[0];
    let mut item = MediaItem::new(post.id.clone(), MediaType::Video, primary.url.clone());
    item.thumbnail_url = post.thumbnail_url.clone();
    item.mime_type = primary.mime_type.clone().or_else(|| Some("video/mp4".into()));
    item.extension = Some(extension_for(&primary.url, "mp4"));
    item.width = primary.width;
    item.height = primary.height;
    item.duration_seconds = primary.duration_seconds;
    item.file_size = None;

    item.variants = ordered
        .iter()
        .map(|source| video_variant(&item.id, source))
        .collect();

    item.metadata.insert(
        "source".into(),
        serde_json::json!("browser_player"),
    );
    // The track layout is stated once for the whole set by `apply_track_layout`,
    // which knows whether an audio source exists and whether the page's network
    // was actually watched.
    item
}

fn video_variant(media_item_id: &str, source: &MediaSource) -> MediaVariant {
    let watermarked = is_watermarked(&source.url);
    let resolution = match (source.width, source.height) {
        (Some(width), Some(height)) => Some(format!(" · {width}x{height}")),
        _ => None,
    };
    let resolution = resolution.unwrap_or_default();

    // The label is the honesty guarantee. A play endpoint is neither clean nor a
    // distinct rendition, so it is named for what it is; a watermarked file is
    // named as watermarked and never rewritten; only a file the source itself
    // serves unmarked is described as clean.
    let (kind, label, is_original) = if is_play_endpoint(source) {
        (
            VariantKind::Standard,
            "Play endpoint (video only)".to_string(),
            false,
        )
    } else if watermarked {
        (
            VariantKind::Standard,
            format!("Video (watermarked){resolution}"),
            false,
        )
    } else {
        (
            VariantKind::Original,
            format!("Original / Clean{resolution}"),
            true,
        )
    };

    let mut variant =
        MediaVariant::new(media_item_id, kind, label, source.url.clone());
    variant.is_original = is_original;
    variant.mime_type = source.mime_type.clone().or_else(|| Some("video/mp4".into()));
    variant.extension = Some(extension_for(&source.url, "mp4"));
    variant.width = source.width;
    variant.height = source.height;
    variant.bitrate_kbps = source.bitrate_kbps;
    variant.file_size = None;
    variant
}

fn audio_item(post: &Post, sources: &[&MediaSource]) -> MediaItem {
    let primary = sources[0];
    let mut item = MediaItem::new(post.id.clone(), MediaType::Audio, primary.url.clone());
    item.mime_type = primary.mime_type.clone().or_else(|| Some("audio/mp4".into()));
    item.extension = Some(extension_for(&primary.url, "m4a"));
    item.duration_seconds = primary.duration_seconds;

    item.variants = sources
        .iter()
        .map(|source| {
            let mut variant = MediaVariant::new(
                &item.id,
                VariantKind::AudioOnly,
                "Audio only",
                source.url.clone(),
            );
            variant.mime_type = source.mime_type.clone().or_else(|| Some("audio/mp4".into()));
            variant.extension = Some(extension_for(&source.url, "m4a"));
            variant.bitrate_kbps = source.bitrate_kbps;
            variant
        })
        .collect();

    item
}

/// Extension from a URL path, falling back when the URL carries none.
///
/// Douyin's signed CDN URLs frequently have no extension at all, which is why a
/// fallback is required rather than optional.
fn extension_for(url: &str, fallback: &str) -> String {
    url.split(['?', '#'])
        .next()
        .and_then(|path| path.rsplit('/').next())
        .and_then(|last| last.rsplit_once('.').map(|(_, ext)| ext))
        .map(|ext| ext.to_ascii_lowercase())
        .filter(|ext| (2..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| fallback.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Platform;

    fn post() -> Post {
        Post::new(
            Platform::Douyin,
            "7281234567890123456",
            MediaType::Video,
            "https://www.douyin.com/video/7281234567890123456",
        )
    }

    fn source(url: &str, kind: MediaSourceKind) -> MediaSource {
        MediaSource {
            url: url.to_string(),
            kind,
            mime_type: None,
            width: None,
            height: None,
            duration_seconds: None,
            bitrate_kbps: None,
            labels: Vec::new(),
        }
    }

    #[test]
    fn separate_video_and_audio_tracks_become_separate_items() {
        let post = post();
        let sources = vec![
            source("https://cdn.example/media-video-avc1/?x=1", MediaSourceKind::Video),
            source("https://cdn.example/media-audio-und-mp4a/?x=1", MediaSourceKind::Audio),
        ];

        let mut items = sources_to_media(&post, &sources);
        apply_track_layout(&mut items, TrackEvidence::TracksRequested);
        assert_eq!(items.len(), 2);

        let video = items.iter().find(|item| item.media_type == MediaType::Video).unwrap();
        let audio = items.iter().find(|item| item.media_type == MediaType::Audio).unwrap();

        // Both tracks exist, so the video item says so rather than presenting
        // itself as a finished file that happens to have no sound.
        assert_eq!(video.track_layout(), Some(TrackLayout::AudioVideo));
        assert_eq!(audio.track_layout(), Some(TrackLayout::AudioOnly));
        assert_eq!(audio.variants[0].kind, VariantKind::AudioOnly);
        assert_eq!(audio.extension.as_deref(), Some("m4a"));
    }

    #[test]
    fn a_video_without_audio_is_only_called_video_only_on_real_evidence() {
        let sources = vec![source(
            "https://cdn.example/media-video-avc1/",
            MediaSourceKind::Video,
        )];

        // Watched, and the page really did fetch a track: the absence of audio
        // among them is a finding about the content.
        let mut items = sources_to_media(&post(), &sources);
        apply_track_layout(&mut items, TrackEvidence::TracksRequested);
        assert_eq!(items[0].track_layout(), Some(TrackLayout::VideoOnly));
    }

    /// The regression this change exists for.
    ///
    /// A render that watched the network but saw no track request has established
    /// nothing about the page's tracks — the page simply never fetched its media.
    /// Reading that as "this source has no audio" is what let a silent download be
    /// presented as a complete one.
    #[test]
    fn an_empty_observation_set_does_not_become_a_video_only_claim() {
        let sources = vec![source(
            "https://cdn.example/media-video-avc1/",
            MediaSourceKind::Video,
        )];

        for evidence in [TrackEvidence::NotWatched, TrackEvidence::NoTracksRequested] {
            let mut items = sources_to_media(&post(), &sources);
            apply_track_layout(&mut items, evidence);
            assert_eq!(
                items[0].track_layout(),
                Some(TrackLayout::Unknown),
                "{evidence:?} is not evidence that the source lacks audio"
            );
        }
    }

    #[test]
    fn evidence_is_classified_from_what_the_render_reported() {
        // Nothing was watched, so nothing was established — whatever else is said.
        assert_eq!(TrackEvidence::from_render(false, 0), TrackEvidence::NotWatched);
        assert_eq!(TrackEvidence::from_render(false, 3), TrackEvidence::NotWatched);
        // Watched, and the page never fetched a track.
        assert_eq!(
            TrackEvidence::from_render(true, 0),
            TrackEvidence::NoTracksRequested
        );
        // Watched, and at least one track was requested.
        assert_eq!(
            TrackEvidence::from_render(true, 1),
            TrackEvidence::TracksRequested
        );

        // Only the last is enough to rule a separate audio track out.
        assert!(!TrackEvidence::NotWatched.rules_out_audio());
        assert!(!TrackEvidence::NoTracksRequested.rules_out_audio());
        assert!(TrackEvidence::TracksRequested.rules_out_audio());
    }

    #[test]
    fn an_audio_only_post_is_audio_only_whatever_the_evidence() {
        let sources = vec![source(
            "https://cdn.example/media-audio-und-mp4a/?x=1",
            MediaSourceKind::Audio,
        )];

        let mut items = sources_to_media(&post(), &sources);
        assert_eq!(items.len(), 1, "no video item is invented for an audio post");
        apply_track_layout(&mut items, TrackEvidence::NoTracksRequested);
        assert_eq!(items[0].track_layout(), Some(TrackLayout::AudioOnly));
    }

    /// The whole decision, from a render's shape to the normalized items.
    #[test]
    fn a_track_the_page_fetched_is_kept_even_when_the_player_named_only_a_video() {
        let from_player = vec![source(
            "https://cdn.example/media-video-avc1/?x=1",
            MediaSourceKind::Video,
        )];
        let network = vec![
            NetworkMedia::test("https://cdn.example/media-video-avc1/?x=1", "video/mp4"),
            NetworkMedia::test("https://cdn.example/media-audio-und-mp4a/?x=1", "audio/mp4"),
        ];

        let merged = merge_sources(from_player, &network);
        let observed = sources_from_network(&network).len();
        let mut items = sources_to_media(&post(), &merged);
        apply_track_layout(&mut items, TrackEvidence::from_render(true, observed));

        let video = items.iter().find(|i| i.media_type == MediaType::Video).unwrap();
        let audio = items.iter().find(|i| i.media_type == MediaType::Audio).unwrap();
        assert_eq!(video.track_layout(), Some(TrackLayout::AudioVideo));
        assert_eq!(audio.track_layout(), Some(TrackLayout::AudioOnly));
    }

    /// Conflicting evidence is resolved by what was actually observed.
    #[test]
    fn an_audio_track_named_only_by_the_player_still_counts() {
        // The player's own list carries the audio; the network saw nothing.
        let from_player = vec![
            source("https://cdn.example/media-video-avc1/?x=1", MediaSourceKind::Video),
            source("https://cdn.example/media-audio-und-mp4a/?x=1", MediaSourceKind::Audio),
        ];

        let merged = merge_sources(from_player, &[]);
        let mut items = sources_to_media(&post(), &merged);
        apply_track_layout(&mut items, TrackEvidence::from_render(true, 0));

        let video = items.iter().find(|i| i.media_type == MediaType::Video).unwrap();
        assert_eq!(
            video.track_layout(),
            Some(TrackLayout::AudioVideo),
            "an audio item exists, so the video is not video-only"
        );
    }

    #[test]
    fn media_from_the_network_is_kept_only_when_the_source_labelled_it_a_track() {
        let network = vec![
            NetworkMedia::test("https://cdn.example/media-audio-und-mp4a/?x=1", "video/mp4"),
            NetworkMedia::test("https://cdn.example/media-video-avc1/?x=1", "video/mp4"),
            // Traffic that carries media but is not a track of this post.
            NetworkMedia::test("https://ad.example/promo.mp4", "video/mp4"),
            NetworkMedia::test("https://cdn.example/poster.jpg", "image/jpeg"),
        ];

        let sources = sources_from_network(&network);
        assert_eq!(sources.len(), 2, "only the source's own track paths qualify");
        assert_eq!(sources[0].kind, MediaSourceKind::Audio);
        assert_eq!(sources[1].kind, MediaSourceKind::Video);
        assert!(sources[0].labels.contains(&"observed-in-browser".to_string()));
    }

    #[test]
    fn merging_keeps_the_players_order_and_adds_only_what_is_missing() {
        let from_player = vec![
            source("https://cdn.example/media-video-avc1/?x=1", MediaSourceKind::Video),
            source("https://cdn.example/media-video-hevc/?x=1", MediaSourceKind::Video),
        ];
        let network = vec![
            // Already present, and must not be duplicated or promoted.
            NetworkMedia::test("https://cdn.example/media-video-avc1/?x=1", "video/mp4"),
            NetworkMedia::test("https://cdn.example/media-audio-und-mp4a/?x=1", "video/mp4"),
        ];

        let merged = merge_sources(from_player, &network);
        assert_eq!(merged.len(), 3);
        // The player's own first entry still leads, so the primary is unchanged.
        assert!(merged[0].url.contains("media-video-avc1"));
        assert_eq!(merged[2].kind, MediaSourceKind::Audio);
    }

    #[test]
    fn merging_an_empty_network_capture_changes_nothing() {
        let from_player = vec![source(
            "https://cdn.example/media-video-avc1/",
            MediaSourceKind::Video,
        )];
        let merged = merge_sources(from_player.clone(), &[]);
        assert_eq!(merged, from_player);
    }

    #[test]
    fn a_play_endpoint_is_named_for_what_it_is_and_sorted_last() {
        let mut endpoint = source("https://www.douyin.com/aweme/v1/play/dash/?is_play_url=1", MediaSourceKind::Unknown);
        endpoint.labels = vec!["play-endpoint".to_string()];

        let items = sources_to_media(
            &post(),
            &[
                source("https://cdn.example/media-video-avc1/", MediaSourceKind::Video),
                endpoint,
            ],
        );

        assert_eq!(items.len(), 1);
        let variants = &items[0].variants;

        // The real media file leads, and is the item's primary source.
        assert_eq!(variants[0].kind, VariantKind::Original);
        assert!(variants[0].source_url.contains("media-video"));

        // The redirector is offered, but never described as a clean original.
        let play = &variants[1];
        assert_eq!(play.kind, VariantKind::Standard);
        assert!(!play.is_original);
        assert!(play.label.contains("Play endpoint"));
        assert!(!play.label.contains("Clean"));
    }

    #[test]
    fn a_play_endpoint_alone_is_still_offered_but_not_as_clean() {
        let mut endpoint = source("https://www.douyin.com/aweme/v1/play/?x=1", MediaSourceKind::Unknown);
        endpoint.labels = vec!["play-endpoint".to_string()];

        let items = sources_to_media(&post(), &[endpoint]);
        assert_eq!(items.len(), 1);
        assert!(!items[0].variants[0].is_original);
        assert_eq!(items[0].variants[0].kind, VariantKind::Standard);
    }

    #[test]
    fn an_unlabelled_resource_is_not_a_reason_to_call_a_post_a_video() {
        // The reported symptom, at the layer that caused it. Douyin serves a
        // post's music through the same player slot a video uses, the page
        // labels nothing about it, and a file holding one aac stream and no
        // picture was downloaded as a video and marked complete. An unlabelled
        // resource is not evidence of anything, so no item is claimed from it.
        let items = sources_to_media(
            &post(),
            &[source(
                "https://sf3-sign.douyinstatic.com/obj/audio/quiet.m4a",
                MediaSourceKind::Unknown,
            )],
        );
        assert!(
            items.is_empty(),
            "an unlabelled resource must not become a video item"
        );
    }

    #[test]
    fn an_unlabelled_resource_may_still_join_a_video_that_exists() {
        // The other half: once something really is a video, an unlabelled URL is
        // a plausible rendition of it and dropping it would lose a mirror.
        let items = sources_to_media(
            &post(),
            &[
                source("https://cdn.example/media-video-avc1/", MediaSourceKind::Video),
                source("https://cdn.example/unlabelled-1/", MediaSourceKind::Unknown),
            ],
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].media_type, MediaType::Video);
        assert_eq!(items[0].variants.len(), 2, "both URLs stay as candidates");
    }

    /// A picture source, as the page collector reports one.
    fn picture(url: &str, width: u32, height: u32) -> MediaSource {
        let mut source = source(url, MediaSourceKind::Image);
        source.width = Some(width);
        source.height = Some(height);
        source
    }

    #[test]
    fn every_picture_on_the_page_becomes_its_own_item() {
        // A carousel is a set of pictures, not one picture with several
        // candidates: each slide is a download in its own right and must not be
        // collapsed into another, however alike the URLs look. The page's order
        // is the order the creator published in.
        let items = sources_to_media(
            &post(),
            &[
                picture("https://p3-cdn.douyinpic.com/one.jpeg", 1080, 1440),
                picture("https://p3-cdn.douyinpic.com/two.jpeg", 1080, 1440),
                picture("https://p9-cdn.douyinpic.com/three.jpeg", 1080, 1440),
            ],
        );

        assert_eq!(items.len(), 3);
        assert!(items.iter().all(|item| item.media_type == MediaType::Image));
        assert_eq!(items[0].metadata["slide"], serde_json::json!(0));
        assert_eq!(items[2].metadata["slide"], serde_json::json!(2));
        assert_eq!(items[1].extension.as_deref(), Some("jpg"));
        assert_eq!(items[1].width, Some(1080));
        assert!(items[0].source_url.ends_with("one.jpeg"));
        assert!(items[2].source_url.ends_with("three.jpeg"));
    }

    #[test]
    fn renditions_of_one_slide_collapse_into_one_image() {
        // The live shape of the double-count, reproduced exactly. A nine-slide
        // post produced ten items: the gallery's slide, and then the same slide
        // again at its original size, because the page's preview strip shows that
        // picture too and serves it with a `~noop` suffix instead of the
        // `~tplv-…` one the gallery used. The page's own counter said nine.
        let items = sources_to_media(
            &post(),
            &[
                picture(
                    "https://p3-pc-sign.douyinpic.com/tos-cn-i-0813c001/\
                     oMTHEAiWA3NaOccjAABAIi83eO0gMCDAAZiHfd~tplv-dy-aweme-images-v2:1440:2158:q75.webp",
                    1440,
                    2158,
                ),
                picture(
                    "https://p3-pc-sign.douyinpic.com/tos-cn-i-0813c001/\
                     oMTHEAiWA3NaOccjAABAIi83eO0gMCDAAZiHfd~noop.jpeg",
                    2160,
                    3238,
                ),
            ],
        );

        assert_eq!(items.len(), 1, "one picture, however many ways it is served");
        // And the surviving one is the larger, so merging costs no resolution.
        assert!(items[0].source_url.ends_with("~noop.jpeg"));
        assert_eq!(items[0].width, Some(2160));
        assert_eq!(items[0].height, Some(3238));
    }

    #[test]
    fn a_slide_served_from_two_hostnames_is_one_slide() {
        // Which CDN hostname answers is not a fact about the picture. Measured
        // live, the same post's slides came back from `p3-pc-sign` and from
        // `p3-sign` on different runs.
        let items = sources_to_media(
            &post(),
            &[
                picture("https://p3-pc-sign.douyinpic.com/tos-cn-i-0813c001/abc~noop.jpeg", 2160, 3238),
                picture("https://p3-sign.douyinpic.com/tos-cn-i-0813c001/abc~noop.jpeg", 2160, 3238),
            ],
        );
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn distinct_slides_that_look_alike_stay_distinct() {
        // The other direction, and the one that would lose a slide if identity
        // were drawn too loosely: two pictures whose URLs differ only in the
        // object id, and two served from different buckets of the same account.
        let items = sources_to_media(
            &post(),
            &[
                picture("https://p3-sign.douyinpic.com/tos-cn-i-0813c001/aaa~noop.jpeg", 1080, 1920),
                picture("https://p3-sign.douyinpic.com/tos-cn-i-0813c001/aab~noop.jpeg", 1080, 1920),
                picture("https://p3-sign.douyinpic.com/tos-cn-i-0813/aaa~noop.jpeg", 1080, 1920),
            ],
        );
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].metadata["slide"], serde_json::json!(0));
        assert_eq!(items[1].metadata["slide"], serde_json::json!(1));
        assert_eq!(items[2].metadata["slide"], serde_json::json!(2));
    }

    #[test]
    fn a_smaller_slide_of_a_different_shape_is_still_a_slide() {
        // Measured on the live nine-slide post: its pictures range from
        // 1080x1920 portraits to 4096x1364 banners. A rule that kept slides by
        // size would have dropped the banners; a rule that kept them by identity
        // keeps every one of them.
        let items = sources_to_media(
            &post(),
            &[
                picture("https://p3-sign.douyinpic.com/b/portrait~noop.jpeg", 1080, 1920),
                picture("https://p3-sign.douyinpic.com/b/banner~noop.jpeg", 4096, 1364),
                picture("https://p3-sign.douyinpic.com/b/thumb~248x332.jpeg", 248, 332),
            ],
        );
        assert_eq!(items.len(), 3);
        assert!(items.iter().all(|item| item.media_type == MediaType::Image));
    }

    #[test]
    fn a_repeated_slide_that_arrives_last_keeps_the_creators_order() {
        // The exact live ordering: nine slides in the track, and then the first
        // picture once more, at the end, from the preview strip. The merge must
        // leave nine items in their original order rather than moving the
        // repeated one to the front or to the end.
        let mut sources = Vec::new();
        for id in ["s1", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9"] {
            sources.push(picture(
                &format!("https://p3-pc-sign.douyinpic.com/tos-cn-i-0813c001/{id}~tplv-dy.webp"),
                1440,
                2158,
            ));
        }
        sources.push(picture(
            "https://p3-pc-sign.douyinpic.com/tos-cn-i-0813c001/s1~noop.jpeg",
            2160,
            3238,
        ));

        let items = sources_to_media(&post(), &sources);

        assert_eq!(items.len(), 9);
        assert!(items[0].source_url.ends_with("s1~noop.jpeg"), "the first slide is still first");
        assert!(items[8].source_url.ends_with("s9~tplv-dy.webp"));
        for (index, item) in items.iter().enumerate() {
            assert_eq!(item.metadata["slide"], serde_json::json!(index));
        }
    }

    #[test]
    fn an_identity_is_the_picture_and_not_the_way_it_was_served() {
        // The rule itself, stated where it can be read.
        assert_eq!(
            logical_slide_key("https://p3-sign.douyinpic.com/tos-cn-i-0813c001/abc~noop.jpeg"),
            logical_slide_key(
                "https://p3-sign.douyinpic.com/tos-cn-i-0813c001/abc~tplv-x:1:2:q75.webp"
            )
        );
        assert_eq!(
            logical_slide_key("https://p3-sign.douyinpic.com/tos-cn-i-0813c001/abc~noop.jpeg?a=b"),
            "/tos-cn-i-0813c001/abc"
        );
        // No rendition suffix at all is still a picture of its own.
        assert_eq!(
            logical_slide_key("https://p3-sign.douyinpic.com/plain.jpg"),
            "/plain.jpg"
        );
        assert_ne!(
            logical_slide_key("https://p3-sign.douyinpic.com/abc.jpg"),
            logical_slide_key("https://p3-sign.douyinpic.com/abd.jpg")
        );
    }

    #[test]
    fn the_post_takes_the_type_its_media_actually_is() {
        // Neither the route it was reached by nor the player decides this. One
        // picture is a picture post; several are a carousel; a real video track
        // is still a video.
        let one = sources_to_media(
            &post(),
            &[picture("https://p3-cdn.douyinpic.com/a.jpeg", 900, 900)],
        );
        assert_eq!(super::super::dominant_media_type(&one), MediaType::Image);

        let many = sources_to_media(
            &post(),
            &[
                picture("https://p3-cdn.douyinpic.com/a.jpeg", 900, 900),
                picture("https://p3-cdn.douyinpic.com/b.jpeg", 900, 900),
            ],
        );
        assert_eq!(super::super::dominant_media_type(&many), MediaType::Carousel);

        let video = sources_to_media(
            &post(),
            &[source("https://cdn.example/media-video-avc1/", MediaSourceKind::Video)],
        );
        assert_eq!(super::super::dominant_media_type(&video), MediaType::Video);
    }

    #[test]
    fn music_beside_pictures_does_not_make_the_post_audio() {
        // The reported shape: a photo post whose player was playing its music.
        // The pictures stay pictures, the music stays an item of its own, and
        // the post is a picture post rather than an audio one.
        let items = sources_to_media(
            &post(),
            &[
                picture("https://p3-cdn.douyinpic.com/a.jpeg", 1080, 1440),
                source(
                    "https://sf3-sign.douyinstatic.com/obj/music.mp3",
                    MediaSourceKind::Audio,
                ),
            ],
        );

        assert_eq!(items.len(), 2);
        assert_eq!(super::super::dominant_media_type(&items), MediaType::Image);
        assert!(items.iter().any(|item| item.media_type == MediaType::Image));
        assert!(items.iter().any(|item| item.media_type == MediaType::Audio));
    }

    #[test]
    fn a_pictures_extension_comes_from_the_url_it_was_served_at() {
        for (url, expected) in [
            ("https://p3-cdn.douyinpic.com/a.jpeg", "jpg"),
            ("https://p3-cdn.douyinpic.com/a.webp", "webp"),
            ("https://p3-cdn.douyinpic.com/a.png", "png"),
            ("https://p3-cdn.douyinpic.com/a.jpeg?x=1&y=2", "jpg"),
            ("https://p3-cdn.douyinpic.com/no-suffix", "jpg"),
        ] {
            let items = sources_to_media(&post(), &[picture(url, 500, 500)]);
            assert_eq!(items[0].extension.as_deref(), Some(expected), "for {url}");
        }
    }

    #[test]
    fn a_video_post_never_carries_the_feeds_pictures() {
        // Measured on a live video page: the collector picked up eleven pictures
        // belonging to the feed behind the post and offered them as that post's
        // media. A page that served a video is a video post, whatever else was
        // on it.
        let items = sources_to_media(
            &post(),
            &[
                source("https://cdn.example/media-video-avc1/", MediaSourceKind::Video),
                picture("https://p3-cdn.douyinpic.com/feed-a.jpeg", 900, 900),
                picture("https://p3-cdn.douyinpic.com/feed-b.jpeg", 900, 900),
            ],
        );

        assert_eq!(items.len(), 1, "the video, and nothing else");
        assert_eq!(items[0].media_type, MediaType::Video);
        assert_eq!(super::super::dominant_media_type(&items), MediaType::Video);
    }

    #[test]
    fn a_video_only_page_yields_only_a_video_item() {
        let items = sources_to_media(
            &post(),
            &[source("https://cdn.example/media-video-avc1/", MediaSourceKind::Video)],
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].media_type, MediaType::Video);
        // Nothing claims there is audio.
        assert!(items.iter().all(|item| item.media_type != MediaType::Audio));
    }

    #[test]
    fn an_unwatermarked_track_is_offered_as_clean() {
        let items = sources_to_media(
            &post(),
            &[source("https://cdn.example/media-video-avc1/", MediaSourceKind::Video)],
        );
        let variant = &items[0].variants[0];
        assert_eq!(variant.kind, VariantKind::Original);
        assert!(variant.is_original);
        assert!(variant.label.starts_with("Original / Clean"));
    }

    #[test]
    fn a_watermarked_track_is_labelled_and_never_rewritten() {
        let url = "https://cdn.example/aweme/v1/playwm/?video_id=abc";
        let items = sources_to_media(&post(), &[source(url, MediaSourceKind::Video)]);

        let variant = &items[0].variants[0];
        assert!(!variant.is_original);
        assert_eq!(variant.kind, VariantKind::Standard);
        assert!(variant.label.contains("watermarked"));
        // The URL is exactly what the source gave us.
        assert_eq!(variant.source_url, url);
    }

    #[test]
    fn dimensions_are_reflected_in_the_variant_label_and_fields() {
        let mut video = source("https://cdn.example/media-video-avc1/", MediaSourceKind::Video);
        video.width = Some(1280);
        video.height = Some(720);
        video.duration_seconds = Some(72.0);
        video.bitrate_kbps = Some(641);

        let items = sources_to_media(&post(), &[video]);
        let item = &items[0];
        assert_eq!(item.width, Some(1280));
        assert_eq!(item.height, Some(720));
        assert_eq!(item.duration_seconds, Some(72.0));
        assert!(item.variants[0].label.contains("1280x720"));
        assert_eq!(item.variants[0].bitrate_kbps, Some(641));
    }

    #[test]
    fn several_video_renditions_all_appear_as_variants() {
        let items = sources_to_media(
            &post(),
            &[
                source("https://cdn.example/media-video-avc1/", MediaSourceKind::Video),
                source("https://cdn.example/media-video-hevc/", MediaSourceKind::Video),
            ],
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].variants.len(), 2);
        // The item's primary URL is the first rendition the player listed.
        assert_eq!(items[0].source_url, items[0].variants[0].source_url);
    }

    #[test]
    fn an_undetermined_source_is_kept_as_another_rendition_not_dropped() {
        let items = sources_to_media(
            &post(),
            &[
                source("https://cdn.example/media-video-avc1/", MediaSourceKind::Video),
                source("https://cdn.example/unknown-track/", MediaSourceKind::Unknown),
            ],
        );
        assert_eq!(items.len(), 1, "it stays with the video rather than becoming its own item");
        assert_eq!(items[0].variants.len(), 2);
    }

    #[test]
    fn no_sources_yields_no_items() {
        assert!(sources_to_media(&post(), &[]).is_empty());
    }

    #[test]
    fn extensions_come_from_the_url_or_fall_back() {
        assert_eq!(extension_for("https://cdn/a/b.mp4?x=1", "bin"), "mp4");
        assert_eq!(extension_for("https://cdn/a/media-video-avc1/?x=1", "mp4"), "mp4");
        assert_eq!(extension_for("https://cdn/a/media-audio-und-mp4a/", "m4a"), "m4a");
    }

    #[test]
    fn the_ready_predicate_and_collector_are_syntactically_plausible() {
        // A cheap guard against a malformed literal reaching a browser.
        for script in [PLAYER_READY, COLLECT_PLAYER_MEDIA, TEXT_READY, GALLERY_READY, GALLERY_SETTLED] {
            assert!(script.starts_with('('), "scripts are wrapped expressions");
            assert_eq!(
                script.matches('{').count(),
                script.matches('}').count(),
                "braces must balance in {script}"
            );
            assert_eq!(
                script.matches('(').count(),
                script.matches(')').count(),
                "parentheses must balance in {script}"
            );
        }
    }

    #[test]
    fn the_collector_filters_non_fetchable_urls() {
        // The same rule the renderer enforces, asserted here so the contract is
        // visible from the platform side too.
        assert!(!crate::network::renderer::is_fetchable("blob:https://www.douyin.com/abc"));
    }
}
