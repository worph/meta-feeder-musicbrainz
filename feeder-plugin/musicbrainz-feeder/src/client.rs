//! MusicBrainz web-service v2 client.
//!
//! The music domain's TMDB. Every call is gated by a shared 1 req/s
//! [`RateBudget`] and every MBID-keyed response is cached permanently — an
//! MBID names an immutable entity, so there is nothing to revalidate.
//!
//! ## One call, complete cards
//!
//! The same principle the card feeder documents: a MusicBrainz **search or
//! browse response already carries everything a renderable card needs** —
//! `id`, `title`, `first-release-date`, and the `artist-credit` (with the
//! artist's own MBID). So a browse row costs *one* request, not one plus
//! twenty by-id lookups.
//!
//! That matters far more here than it does for TMDB. At 1 req/s a
//! per-item detail fetch would make a 20-card row take twenty seconds, which
//! is past every consumer-side idle cutoff — the row would come back empty,
//! not slow. The price is a thinner card (no track count, no label), and those
//! are exactly the fields nobody reads until the card is clicked, at which
//! point [`MusicBrainzClient::tracklist`] fetches them.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::consts::{
    MB_API_BASE, MB_PAGE_SIZE, MB_RATE_LIMIT_DEFAULT_RETRY_SECS, MB_TIMEOUT_SECS, USER_AGENT,
};
use meta_feeder_sdk::budget::{Lease, RateBudget};

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

/// An artist credit entry. MusicBrainz models a credit as an ordered list of
/// `{artist, joinphrase}` so `"Miles Davis feat. John Coltrane"` round-trips;
/// the flattened display string is rebuilt by [`ArtistCredit::render`].
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ArtistCredit {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub joinphrase: String,
    #[serde(default)]
    pub artist: Option<CreditedArtist>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct CreditedArtist {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
}

/// Flatten a credit list into its display string, honouring join phrases.
pub fn render_credit(credits: &[ArtistCredit]) -> String {
    let mut out = String::new();
    for c in credits {
        out.push_str(&c.name);
        out.push_str(&c.joinphrase);
    }
    out.trim().to_string()
}

/// The **primary** artist of a credit list — the first entry with a resolved
/// artist MBID. This is what an album card is filed under; the full rendered
/// credit is what gets displayed.
pub fn primary_artist(credits: &[ArtistCredit]) -> Option<&CreditedArtist> {
    credits
        .iter()
        .filter_map(|c| c.artist.as_ref())
        .find(|a| !a.id.is_empty())
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Tag {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub count: i64,
}

/// A release-group: the **work**, an album across all its pressings.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ReleaseGroup {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    /// `Album`, `Single`, `EP`, `Broadcast`, `Other`. Absent on sparse entries.
    #[serde(rename = "primary-type", default)]
    pub primary_type: Option<String>,
    /// `Compilation`, `Live`, `Soundtrack`, `Remix`, … Multiple allowed.
    #[serde(rename = "secondary-types", default)]
    pub secondary_types: Vec<String>,
    /// `YYYY`, `YYYY-MM` or `YYYY-MM-DD` — MusicBrainz dates are
    /// variable-precision and a consumer must not assume full ISO.
    #[serde(rename = "first-release-date", default)]
    pub first_release_date: Option<String>,
    #[serde(rename = "artist-credit", default)]
    pub artist_credit: Vec<ArtistCredit>,
    #[serde(default)]
    pub tags: Vec<Tag>,
    #[serde(default)]
    pub genres: Vec<Tag>,
    #[serde(default)]
    pub disambiguation: Option<String>,
    /// Lucene relevance, 0–100, present only on *search* responses.
    ///
    /// ⚠ **Saturated, and therefore nearly useless alone.** Measured on
    /// `query=kind of blue`: six release-groups scored exactly 100 — TQ, Swiss
    /// Blues Authority, 大山百合香, Hellberg, Various Artists, and Miles Davis
    /// *sixth*. An exact title match scores 100 whether the album is famous or
    /// was uploaded once. See [`rank_release_groups`].
    #[serde(default)]
    pub score: i32,
    /// Number of releases (pressings, editions, reissues) in this group.
    ///
    /// **This is the popularity signal MusicBrainz does not otherwise have.**
    /// A work that has been pressed 135 times is the one people mean; a work
    /// with one release is somebody's upload. Measured on the same query:
    /// Miles Davis 135, every other score-100 hit exactly 1.
    #[serde(default)]
    pub count: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Artist {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// `Person`, `Group`, `Orchestra`, `Choir`, `Character`, `Other`.
    #[serde(rename = "type", default)]
    pub artist_type: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub disambiguation: Option<String>,
    #[serde(default)]
    pub tags: Vec<Tag>,
    #[serde(default)]
    pub genres: Vec<Tag>,
}

/// One track on a medium.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Track {
    #[serde(default)]
    pub id: String,
    /// The printed track number — **a string, not an integer**: vinyl uses
    /// `A1`/`B2` and classical releases use `1.i`. `position` is the reliable
    /// ordinal.
    #[serde(default)]
    pub number: Option<String>,
    #[serde(default)]
    pub position: u32,
    #[serde(default)]
    pub title: String,
    /// Milliseconds. Absent on tracks nobody has timed.
    #[serde(default)]
    pub length: Option<u64>,
    #[serde(default)]
    pub recording: Option<Recording>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Recording {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub length: Option<u64>,
    #[serde(default)]
    pub isrcs: Vec<String>,
}

/// One physical/logical medium (a disc) within a release.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Medium {
    #[serde(default)]
    pub position: u32,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(rename = "track-count", default)]
    pub track_count: u32,
    #[serde(default)]
    pub tracks: Vec<Track>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct LabelInfo {
    #[serde(rename = "catalog-number", default)]
    pub catalog_number: Option<String>,
    #[serde(default)]
    pub label: Option<Label>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Label {
    #[serde(default)]
    pub name: String,
}

/// A release: one specific pressing/edition of a release-group.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Release {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub media: Vec<Medium>,
    #[serde(rename = "label-info", default)]
    pub label_info: Vec<LabelInfo>,
}

#[derive(Deserialize, Default)]
struct ReleaseGroupSearch {
    #[serde(rename = "release-groups", default)]
    release_groups: Vec<ReleaseGroup>,
}

#[derive(Deserialize, Default)]
struct ArtistSearch {
    #[serde(default)]
    artists: Vec<Artist>,
}

#[derive(Deserialize, Default)]
struct ReleaseBrowse {
    #[serde(default)]
    releases: Vec<Release>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct MusicBrainzClient {
    http: reqwest::Client,
    base: String,
    budget: Arc<RateBudget>,
    cache: meta_feeder_sdk::cache::MidhashCache,
}

impl MusicBrainzClient {
    pub fn new(budget: Arc<RateBudget>, cache: meta_feeder_sdk::cache::MidhashCache) -> Self {
        Self::with_base(MB_API_BASE.to_string(), budget, cache)
    }

    /// Test constructor: point the client at a wiremock server.
    pub fn with_base(
        base: String,
        budget: Arc<RateBudget>,
        cache: meta_feeder_sdk::cache::MidhashCache,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .timeout(Duration::from_secs(MB_TIMEOUT_SECS))
                .build()
                .expect("build musicbrainz http client"),
            base: base.trim_end_matches('/').to_string(),
            budget,
            cache,
        }
    }

    /// One budget-gated GET returning the raw body.
    ///
    /// Returns `None` on **every** failure path — deadline, transport, non-2xx.
    /// A feeder degrades to fewer results; it never propagates an upstream
    /// hiccup as a hard error, because a hard error empties the whole row
    /// rather than shortening it.
    async fn get_text(&self, path_and_query: &str, deadline: Duration) -> Option<String> {
        if self.budget.acquire(deadline).await == Lease::DeadlineExceeded {
            debug!(target: "meta-music", path = %path_and_query, "musicbrainz budget deadline; degrading");
            return None;
        }
        let url = format!("{}/{}", self.base, path_and_query.trim_start_matches('/'));
        let resp = match self.http.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(target: "meta-music", %url, error = %e, "musicbrainz request failed");
                return None;
            }
        };
        // MusicBrainz answers its throttle with 503 + Retry-After, not 429.
        // Reading this as a generic server error is how a feeder walks straight
        // into a block: it would keep hammering at full rate.
        if resp.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE
            || resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            let retry = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(MB_RATE_LIMIT_DEFAULT_RETRY_SECS)
                // ⚠ **Floor it.** `unwrap_or` only covers a *missing* or
                // unparseable header — and MusicBrainz sends a literal
                // `Retry-After: 0` under load, which parses fine and pauses the
                // budget for zero seconds. The client then retries immediately,
                // which is precisely how a throttled client stays throttled:
                // observed live as an unbroken run of "musicbrainz throttled;
                // pausing budget retry_after_s=0" while a plain curl to the
                // same URL answered 200.
                .max(MB_RATE_LIMIT_DEFAULT_RETRY_SECS);
            warn!(target: "meta-music", %url, retry_after_s = retry, "musicbrainz throttled; pausing budget");
            self.budget.note_throttled(Duration::from_secs(retry));
            return None;
        }
        if !resp.status().is_success() {
            warn!(target: "meta-music", %url, status = %resp.status(), "musicbrainz non-2xx");
            return None;
        }
        resp.text().await.ok()
    }

    /// Search release-groups by free text. Cached permanently under the
    /// normalised query — a search result set is a ranking over immutable
    /// entities, and a stale ranking is a far cheaper error than an extra call
    /// against a 1/s budget.
    pub async fn search_release_groups(
        &self,
        text: &str,
        limit: usize,
        deadline: Duration,
    ) -> Vec<ReleaseGroup> {
        let key = meta_feeder_sdk::cache::search_key("release-group", text);
        if let Ok(Some(hit)) = self.cache.get_mb_search(&key) {
            if let Ok(v) = serde_json::from_str::<Vec<ReleaseGroup>>(&hit) {
                return v.into_iter().take(limit).collect();
            }
        }
        // ⚠ Ask MusicBrainz for MORE than the caller wants.
        //
        // The re-ranking below is what surfaces the famous work, and it can
        // only rank what came back. On the measured `kind of blue` query the
        // Miles Davis release-group was **sixth** in MusicBrainz's own order —
        // so requesting the caller's `limit` of, say, 5 would have MusicBrainz
        // truncate him away before the ranking ever sees him, and no amount of
        // local sorting recovers that. One page is one request either way.
        let fetch = (limit * 4).clamp(MB_PAGE_SIZE, 100);
        let q = format!(
            "release-group?query={}&fmt=json&limit={}",
            urlencode(text),
            fetch
        );
        let Some(body) = self.get_text(&q, deadline).await else {
            return Vec::new();
        };
        let parsed: ReleaseGroupSearch = serde_json::from_str(&body).unwrap_or_default();
        // Rank BEFORE caching and before truncating: the cache stores the
        // ranked list, and truncating an unranked list to `limit` would throw
        // away the very hit the ranking exists to surface (Miles Davis was
        // sixth on the measured query — a `limit` of 5 would have dropped him).
        let out = rank_release_groups(parsed.release_groups);
        if !out.is_empty() {
            if let Ok(json) = serde_json::to_string(&out.iter().collect::<Vec<_>>()) {
                let _ = self.cache.put_mb_search(&key, &json);
            }
        }
        out.into_iter().take(limit).collect()
    }

    /// Browse release-groups by **genre tag**.
    ///
    /// ⚠ **This is why a genre row is a MusicBrainz search and not a
    /// ListenBrainz feed.** ListenBrainz has no genre parameter, so a first cut
    /// answered `genres:jazz` with the plain popularity feed and filtered the
    /// cards afterwards. That cannot work: a browse card carries no genres (they
    /// are resolved on the detail path), so the filter matched nothing — and
    /// falling back to the *unfiltered* feed was worse, because both the gateway
    /// and meta-search re-apply `record_matches` with `genres:jazz` and drop
    /// every card that does not carry it. Every genre row was empty either way.
    ///
    /// MusicBrainz's Lucene index does have `tag:`, and a tag search returns
    /// cards whose `tags` then survive the re-filter. One call, real genres.
    pub async fn release_groups_by_tag(
        &self,
        tag: &str,
        limit: usize,
        deadline: Duration,
    ) -> Vec<ReleaseGroup> {
        let key = meta_feeder_sdk::cache::search_key("rg-by-tag", tag);
        if let Ok(Some(hit)) = self.cache.get_mb_search(&key) {
            if let Ok(v) = serde_json::from_str::<Vec<ReleaseGroup>>(&hit) {
                return v.into_iter().take(limit).collect();
            }
        }
        // `primarytype:Album` keeps a genre row to full-length records; a page
        // of singles is not what "browse Jazz" means.
        let lucene = format!("tag:{tag} AND primarytype:Album");
        let q = format!(
            "release-group?query={}&fmt=json&limit={}",
            urlencode(&lucene),
            (limit * 3).clamp(MB_PAGE_SIZE, 100)
        );
        let Some(body) = self.get_text(&q, deadline).await else {
            return Vec::new();
        };
        let parsed: ReleaseGroupSearch = serde_json::from_str(&body).unwrap_or_default();
        let out = rank_release_groups(parsed.release_groups);
        if !out.is_empty() {
            if let Ok(json) = serde_json::to_string(&out.iter().collect::<Vec<_>>()) {
                let _ = self.cache.put_mb_search(&key, &json);
            }
        }
        out.into_iter().take(limit).collect()
    }

    /// Search artists by free text.
    pub async fn search_artists(
        &self,
        text: &str,
        limit: usize,
        deadline: Duration,
    ) -> Vec<Artist> {
        let key = meta_feeder_sdk::cache::search_key("artist", text);
        if let Ok(Some(hit)) = self.cache.get_mb_search(&key) {
            if let Ok(v) = serde_json::from_str::<Vec<Artist>>(&hit) {
                return v.into_iter().take(limit).collect();
            }
        }
        let q = format!(
            "artist?query={}&fmt=json&limit={}",
            urlencode(text),
            limit.clamp(1, MB_PAGE_SIZE * 4)
        );
        let Some(body) = self.get_text(&q, deadline).await else {
            return Vec::new();
        };
        let parsed: ArtistSearch = serde_json::from_str(&body).unwrap_or_default();
        let out = parsed.artists;
        if !out.is_empty() {
            if let Ok(json) = serde_json::to_string(&out.iter().collect::<Vec<_>>()) {
                let _ = self.cache.put_mb_search(&key, &json);
            }
        }
        out.into_iter().take(limit).collect()
    }

    /// One release-group by MBID, with genres and artist credits.
    pub async fn release_group(&self, mbid: &str, deadline: Duration) -> Option<ReleaseGroup> {
        if let Ok(Some(hit)) = self.cache.get_mb_release_group(mbid) {
            if let Ok(v) = serde_json::from_str::<ReleaseGroup>(&hit) {
                return Some(v);
            }
        }
        let q = format!("release-group/{}?fmt=json&inc=artist-credits+genres", urlencode(mbid));
        let body = self.get_text(&q, deadline).await?;
        let rg: ReleaseGroup = serde_json::from_str(&body).ok()?;
        if rg.id.is_empty() {
            return None;
        }
        let _ = self.cache.put_mb_release_group(mbid, &body);
        Some(rg)
    }

    /// One artist by MBID, with genres.
    pub async fn artist(&self, mbid: &str, deadline: Duration) -> Option<Artist> {
        if let Ok(Some(hit)) = self.cache.get_mb_artist(mbid) {
            if let Ok(v) = serde_json::from_str::<Artist>(&hit) {
                return Some(v);
            }
        }
        let q = format!("artist/{}?fmt=json&inc=genres", urlencode(mbid));
        let body = self.get_text(&q, deadline).await?;
        let a: Artist = serde_json::from_str(&body).ok()?;
        if a.id.is_empty() {
            return None;
        }
        let _ = self.cache.put_mb_artist(mbid, &body);
        Some(a)
    }

    /// An artist's **YouTube channel relations**, in the two flavours
    /// MusicBrainz distinguishes: `("youtube music", url)` and `("youtube", url)`.
    ///
    /// # Why this is an artist call and not a recording one
    ///
    /// Measured, not assumed (study §4.1): two "Get Lucky" recordings and the
    /// *Random Access Memories* release-group returned **zero** YouTube
    /// relations between them. There is no per-track and no per-album shortcut
    /// — the links live at artist level, and only there. One lookup per artist,
    /// permanent, free, no key.
    ///
    /// Returns the raw relation list; the caller ranks it. `None` is
    /// **"the lookup did not answer"**, which the caller must not confuse with
    /// "this artist has no channel" — see [`Self::get_text`], which reports a
    /// MusicBrainz `503` the same way it reports an empty result.
    /// Every name this artist might be searched under: the primary `name`, the
    /// `sort-name`, and all aliases.
    ///
    /// # Why the credited string is not enough
    ///
    /// MusicBrainz credits works in the artist's own script, and the name match
    /// against YouTube's artist index needs the one people actually type. The
    /// study's measured case: *Final Fantasy VII* is credited to `植松伸夫`,
    /// which finds no exact-name channel, even though `Nobuo Uematsu` resolves
    /// fine (§7.2.1). Without the aliases the channel key is simply unavailable
    /// for that artist, and every track then fails to reach EXACT.
    pub async fn artist_names(&self, mbid: &str, deadline: Duration) -> Vec<String> {
        let q = format!("artist/{}?fmt=json&inc=aliases", urlencode(mbid));
        let Some(body) = self.get_text(&q, deadline).await else {
            return Vec::new();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut push = |s: Option<&str>| {
            if let Some(s) = s.map(str::trim).filter(|s| !s.is_empty()) {
                if !out.iter().any(|e: &String| e.eq_ignore_ascii_case(s)) {
                    out.push(s.to_string());
                }
            }
        };
        push(v.get("name").and_then(|n| n.as_str()));
        push(v.get("sort-name").and_then(|n| n.as_str()));
        if let Some(aliases) = v.get("aliases").and_then(|a| a.as_array()) {
            for a in aliases {
                push(a.get("name").and_then(|n| n.as_str()));
            }
        }
        out
    }

    pub async fn artist_url_relations(
        &self,
        mbid: &str,
        deadline: Duration,
    ) -> Option<Vec<(String, String)>> {
        let q = format!("artist/{}?fmt=json&inc=url-rels", urlencode(mbid));
        let body = self.get_text(&q, deadline).await?;
        let v: serde_json::Value = serde_json::from_str(&body).ok()?;
        let rels = v.get("relations")?.as_array()?;
        Some(
            rels.iter()
                .filter_map(|r| {
                    let ty = r.get("type")?.as_str()?.to_string();
                    let url = r.get("url")?.get("resource")?.as_str()?.to_string();
                    Some((ty, url))
                })
                .collect(),
        )
    }

    /// The canonical track list for a release-group.
    ///
    /// **Why this is one call and a season walk is not.** MusicBrainz returns
    /// every medium and every track of a release in a single response, so an
    /// album detail page needs exactly one request here plus one retrieval
    /// query for availability. There is no per-disc fan-out, no walk, no TTL
    /// that could hide a track — the shape of the data removes the problem
    /// that `meta-watch`'s `season_walk.rs` exists to manage.
    ///
    /// Which pressing counts as canonical: the **first release returned that
    /// actually carries tracks**. MusicBrainz orders a release-group's
    /// releases by date, so this is the earliest complete one — the edition
    /// whose track list people mean by "the album". Later remasters add bonus
    /// tracks, which is exactly what should not define the canonical list.
    pub async fn tracklist(&self, release_group_mbid: &str, deadline: Duration) -> Option<Release> {
        if let Ok(Some(hit)) = self.cache.get_mb_tracklist(release_group_mbid) {
            if let Ok(v) = serde_json::from_str::<Release>(&hit) {
                return Some(v);
            }
        }
        let q = format!(
            "release?release-group={}&fmt=json&inc=recordings+artist-credits+isrcs+labels&limit=5",
            urlencode(release_group_mbid)
        );
        let body = self.get_text(&q, deadline).await?;
        let parsed: ReleaseBrowse = serde_json::from_str(&body).unwrap_or_default();
        let chosen = parsed
            .releases
            .into_iter()
            .find(|r| r.media.iter().any(|m| !m.tracks.is_empty()))?;
        if let Ok(json) = serde_json::to_string(&chosen) {
            let _ = self.cache.put_mb_tracklist(release_group_mbid, &json);
        }
        Some(chosen)
    }

    /// A browse of an artist's release-groups — their discography.
    pub async fn release_groups_by_artist(
        &self,
        artist_mbid: &str,
        limit: usize,
        deadline: Duration,
    ) -> Vec<ReleaseGroup> {
        let key = meta_feeder_sdk::cache::search_key("rg-by-artist", artist_mbid);
        if let Ok(Some(hit)) = self.cache.get_mb_search(&key) {
            if let Ok(v) = serde_json::from_str::<Vec<ReleaseGroup>>(&hit) {
                return v.into_iter().take(limit).collect();
            }
        }
        let q = format!(
            "release-group?artist={}&fmt=json&limit={}",
            urlencode(artist_mbid),
            limit.clamp(1, 100)
        );
        let Some(body) = self.get_text(&q, deadline).await else {
            return Vec::new();
        };
        let parsed: ReleaseGroupSearch = serde_json::from_str(&body).unwrap_or_default();
        let out = parsed.release_groups;
        if !out.is_empty() {
            if let Ok(json) = serde_json::to_string(&out.iter().collect::<Vec<_>>()) {
                let _ = self.cache.put_mb_search(&key, &json);
            }
        }
        out.into_iter().take(limit).collect()
    }
}

/// Percent-encode a query-string value. Deliberately conservative — the Lucene
/// syntax MusicBrainz accepts uses `:`/`(`/`)`/`"` meaningfully, so anything
/// outside the unreserved set is escaped rather than passed through.
pub(crate) fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Re-rank release-group search hits: relevance band first, then how many
/// times the work has actually been released.
///
/// **Why this is not MusicBrainz's own order.** Their `score` is a Lucene
/// relevance figure that saturates at 100 for any exact title match, so a
/// search for a famous album returns it *behind* every obscure record that
/// happens to share its name. Measured live on `query=kind of blue`:
///
/// | # | score | releases | artist |
/// |---|-------|----------|--------|
/// | 0 | 100 | 1 | Swiss Blues Authority |
/// | 1 | 100 | 1 | TQ |
/// | 2 | 100 | 1 | 大山百合香 |
/// | … | … | … | … |
/// | 5 | 100 | **135** | **Miles Davis** |
///
/// TMDB solves this with a popularity field; MusicBrainz has none. The release
/// count is the closest thing it does carry, it comes free in the same
/// response, and it is decisive here.
///
/// **Score stays the primary key on purpose.** Sorting by release count alone
/// would let a prolific artist's loosely-matching album outrank an exact hit.
/// The count only breaks ties *within* a relevance band — which is exactly the
/// case the ranking fails at, since MusicBrainz's bands are coarse (100, 89,
/// 80, 76, …) and an exact-match band is where the pile-up happens.
pub fn rank_release_groups(mut hits: Vec<ReleaseGroup>) -> Vec<ReleaseGroup> {
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.count.cmp(&a.count))
            // Stable, deterministic final tiebreak so a cached ranking and a
            // fresh one cannot disagree.
            .then_with(|| a.id.cmp(&b.id))
    });
    hits
}

/// Normalise a MusicBrainz variable-precision date to what `releasedate`
/// expects.
///
/// MusicBrainz emits `YYYY`, `YYYY-MM` or `YYYY-MM-DD` depending on what is
/// known. `METADATA_KEYS.md`'s `iso-date` format is `YYYY-MM-DD`, so a partial
/// date is completed with `-01` rather than dropped — losing the year entirely
/// because the day is unknown would be the worse error, and every consumer
/// that renders this only shows the year.
pub fn normalise_date(raw: &str) -> Option<String> {
    // ⚠ Validate the shape, do not infer it from the length. A first cut
    // matched on `len()` alone, which turned the seven-character string
    // "garbage" into the `releasedate` "garbage-01" — a malformed value
    // written to the shared hash, where nothing downstream would ever question
    // it. Every segment is checked for digits and width.
    let digits = |s: &str, n: usize| s.len() == n && s.chars().all(|c| c.is_ascii_digit());
    let parts: Vec<&str> = raw.trim().split('-').collect();
    match parts.as_slice() {
        [y] if digits(y, 4) => Some(format!("{y}-01-01")),
        [y, m] if digits(y, 4) && digits(m, 2) => Some(format!("{y}-{m}-01")),
        [y, m, d] if digits(y, 4) && digits(m, 2) && digits(d, 2) => Some(format!("{y}-{m}-{d}")),
        _ => None,
    }
}

/// The year from a MusicBrainz variable-precision date.
pub fn year_of(raw: &str) -> Option<u16> {
    raw.trim().get(0..4)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_rendering_honours_join_phrases() {
        let credits = vec![
            ArtistCredit {
                name: "Miles Davis".into(),
                joinphrase: " feat. ".into(),
                artist: Some(CreditedArtist {
                    id: "561d854a-6a28-4aa7-8c99-323e6ce46c2a".into(),
                    name: "Miles Davis".into(),
                }),
            },
            ArtistCredit {
                name: "John Coltrane".into(),
                joinphrase: String::new(),
                artist: Some(CreditedArtist {
                    id: "b625448e-bf4a-41c3-a421-72ad46cdb831".into(),
                    name: "John Coltrane".into(),
                }),
            },
        ];
        assert_eq!(render_credit(&credits), "Miles Davis feat. John Coltrane");
        assert_eq!(
            primary_artist(&credits).map(|a| a.name.as_str()),
            Some("Miles Davis")
        );
    }

    /// A credit entry with no resolved artist must not become the primary —
    /// the card would be filed under an artist MBID that does not exist.
    #[test]
    fn primary_artist_skips_credits_with_no_mbid() {
        let credits = vec![
            ArtistCredit {
                name: "Various Artists".into(),
                joinphrase: String::new(),
                artist: None,
            },
            ArtistCredit {
                name: "Real Artist".into(),
                joinphrase: String::new(),
                artist: Some(CreditedArtist {
                    id: "abc".into(),
                    name: "Real Artist".into(),
                }),
            },
        ];
        assert_eq!(primary_artist(&credits).map(|a| a.id.as_str()), Some("abc"));
    }

    /// Variable-precision dates are the norm, not the exception — a
    /// year-only release date must still produce a usable `releasedate`.
    #[test]
    fn dates_are_completed_not_dropped() {
        assert_eq!(normalise_date("1959").as_deref(), Some("1959-01-01"));
        assert_eq!(normalise_date("1959-08").as_deref(), Some("1959-08-01"));
        assert_eq!(normalise_date("1959-08-17").as_deref(), Some("1959-08-17"));
        assert_eq!(normalise_date(""), None);
        // ⚠ Regression guard: "garbage" is seven characters, exactly the width
        // of `YYYY-MM`. A length-only check wrote "garbage-01" into
        // `releasedate` on the shared hash.
        assert_eq!(normalise_date("garbage"), None);
        assert_eq!(normalise_date("19xx"), None);
        assert_eq!(normalise_date("1959-ab"), None);
        assert_eq!(normalise_date("1959-08-1x"), None);
        assert_eq!(normalise_date("1959-8-17"), None, "segments are fixed-width");
        assert_eq!(year_of("1959-08-17"), Some(1959));
        assert_eq!(year_of("19"), None);
    }

    fn rg(id: &str, score: i32, count: u32) -> ReleaseGroup {
        ReleaseGroup {
            id: id.to_string(),
            title: "Kind of Blue".to_string(),
            score,
            count,
            ..Default::default()
        }
    }

    /// ⚠ The measured failure this exists for: on `query=kind of blue`,
    /// MusicBrainz returned six release-groups all scoring exactly 100, with
    /// **Miles Davis sixth** behind five one-release records that happen to
    /// share the title. Release count is the only discriminator in the payload.
    #[test]
    fn release_count_breaks_a_saturated_score_tie() {
        let hits = vec![
            rg("swiss", 100, 1),
            rg("tq", 100, 1),
            rg("oyama", 100, 1),
            rg("hellberg", 100, 1),
            rg("various", 100, 1),
            rg("miles", 100, 135),
        ];
        let ranked = rank_release_groups(hits);
        assert_eq!(ranked[0].id, "miles", "the 135-release work must lead");
    }

    /// …but relevance still wins over popularity across bands, or a prolific
    /// artist's loosely-matching album would outrank an exact hit.
    #[test]
    fn score_outranks_release_count_across_bands() {
        let ranked = rank_release_groups(vec![rg("loose", 89, 500), rg("exact", 100, 2)]);
        assert_eq!(ranked[0].id, "exact");
    }

    /// A cached ranking and a fresh one must not disagree, so ties are broken
    /// deterministically rather than left to sort stability.
    #[test]
    fn ranking_is_deterministic_for_identical_hits() {
        let a = rank_release_groups(vec![rg("bbb", 100, 1), rg("aaa", 100, 1)]);
        let b = rank_release_groups(vec![rg("aaa", 100, 1), rg("bbb", 100, 1)]);
        assert_eq!(
            a.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            b.iter().map(|r| r.id.as_str()).collect::<Vec<_>>()
        );
    }

    /// The Lucene metacharacters MusicBrainz treats as syntax must be escaped,
    /// or a punctuated title (`Re:ZERO`, `!!!`) is read as a field query.
    #[test]
    fn urlencode_escapes_lucene_metacharacters() {
        assert_eq!(urlencode("Re:ZERO"), "Re%3AZERO");
        assert_eq!(urlencode("kind of blue"), "kind%20of%20blue");
        assert_eq!(urlencode("!!!"), "%21%21%21");
        assert_eq!(urlencode("safe-Chars_1.0~"), "safe-Chars_1.0~");
    }
}
