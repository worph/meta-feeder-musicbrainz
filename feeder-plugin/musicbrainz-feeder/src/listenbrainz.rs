//! ListenBrainz client — the **browse** signal.
//!
//! MusicBrainz knows what exists; it has no opinion about what anyone is
//! listening to. ListenBrainz supplies that, from real listen counts, with no
//! credential needed for the sitewide and fresh-release feeds. It is the
//! `popular:true` / `trending:true` / new-releases answer, exactly as TMDB's
//! `/discover` and `/trending` are for the card feeder.
//!
//! ## Why the parsing here is deliberately lenient
//!
//! Every other upstream in this crate is parsed into typed structs. This one
//! is walked as `serde_json::Value`, on purpose:
//!
//! - The statistics API had a documented breaking change (`stats_range` →
//!   `range`) and the payload key differs per endpoint (`artists`,
//!   `release_groups`, `releases`).
//! - The failure mode of a rigid struct here is a **whole empty browse row
//!   with no error** — the consumer shows a blank wall and there is nothing in
//!   any log to explain it. That exact class of silent-empty-row failure has
//!   cost real debugging time elsewhere in this stack.
//!
//! So each field is read by name with fallbacks, an entry that yields no MBID
//! is skipped individually rather than failing the batch, and a response whose
//! shape we do not recognise logs loudly at `warn` with the keys it *did*
//! carry.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tracing::{debug, warn};

use crate::consts::{LB_API_BASE, LB_STATS_RANGE, LB_TIMEOUT_SECS, USER_AGENT};
use meta_feeder_sdk::budget::{Lease, RateBudget};

/// A work ListenBrainz reports as popular or newly released.
///
/// Everything needed to build an album card *without a MusicBrainz round
/// trip* — which is the whole point: at 1 req/s, resolving twenty
/// release-groups by MBID would take twenty seconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopularWork {
    pub release_group_mbid: String,
    pub title: String,
    pub artist_name: String,
    pub artist_mbid: Option<String>,
    /// `(release_mbid, caa_id)` when ListenBrainz already resolved a Cover Art
    /// Archive image. Saves the consumer a redirect and, more importantly,
    /// names a cover that is **known to exist** — the release-group front
    /// endpoint 404s silently for anything unillustrated.
    pub caa: Option<(String, i64)>,
    pub release_date: Option<String>,
}

/// An artist ListenBrainz reports as popular.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopularArtist {
    pub artist_mbid: String,
    pub name: String,
}

pub struct ListenBrainzClient {
    http: reqwest::Client,
    base: String,
    budget: Arc<RateBudget>,
}

impl ListenBrainzClient {
    pub fn new(budget: Arc<RateBudget>) -> Self {
        Self::with_base(LB_API_BASE.to_string(), budget)
    }

    /// Test constructor: point the client at a wiremock server.
    pub fn with_base(base: String, budget: Arc<RateBudget>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .timeout(Duration::from_secs(LB_TIMEOUT_SECS))
                .build()
                .expect("build listenbrainz http client"),
            base: base.trim_end_matches('/').to_string(),
            budget,
        }
    }

    async fn get_json(&self, path_and_query: &str, deadline: Duration) -> Option<Value> {
        if self.budget.acquire(deadline).await == Lease::DeadlineExceeded {
            debug!(target: "meta-music", path = %path_and_query, "listenbrainz budget deadline; degrading");
            return None;
        }
        let url = format!("{}/{}", self.base, path_and_query.trim_start_matches('/'));
        let resp = match self.http.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(target: "meta-music", %url, error = %e, "listenbrainz request failed");
                return None;
            }
        };
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(2);
            warn!(target: "meta-music", %url, retry_after_s = retry, "listenbrainz throttled; pausing budget");
            self.budget.note_throttled(Duration::from_secs(retry));
            return None;
        }
        if !resp.status().is_success() {
            warn!(target: "meta-music", %url, status = %resp.status(), "listenbrainz non-2xx");
            return None;
        }
        resp.json::<Value>().await.ok()
    }

    /// Sitewide top release-groups over [`LB_STATS_RANGE`].
    pub async fn popular_release_groups(&self, count: usize, deadline: Duration) -> Vec<PopularWork> {
        let q = format!(
            "stats/sitewide/release-groups?range={LB_STATS_RANGE}&count={}",
            count.clamp(1, 100)
        );
        let Some(v) = self.get_json(&q, deadline).await else {
            return Vec::new();
        };
        parse_works(&v, "release_groups")
    }

    /// Newly released works. `days` bounds how far back "new" reaches.
    pub async fn fresh_releases(&self, days: u32, deadline: Duration) -> Vec<PopularWork> {
        let q = format!(
            "explore/fresh-releases?days={}&sort=release_date&past=true&future=false",
            days.clamp(1, 90)
        );
        let Some(v) = self.get_json(&q, deadline).await else {
            return Vec::new();
        };
        parse_works(&v, "releases")
    }

    /// Sitewide top artists over [`LB_STATS_RANGE`].
    pub async fn popular_artists(&self, count: usize, deadline: Duration) -> Vec<PopularArtist> {
        let q = format!(
            "stats/sitewide/artists?range={LB_STATS_RANGE}&count={}",
            count.clamp(1, 100)
        );
        let Some(v) = self.get_json(&q, deadline).await else {
            return Vec::new();
        };
        parse_artists(&v)
    }
}

/// Read the first present string field from a set of candidate names.
fn pick_str<'a>(obj: &'a Value, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|n| obj.get(*n).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// The payload array under `payload.<key>`, tolerating a bare top-level array.
fn payload_array<'a>(v: &'a Value, key: &str) -> Option<&'a Vec<Value>> {
    v.get("payload")
        .and_then(|p| p.get(key))
        .and_then(Value::as_array)
        .or_else(|| v.get(key).and_then(Value::as_array))
        .or_else(|| v.as_array())
}

/// Parse a works payload (`release_groups` for stats, `releases` for fresh).
pub fn parse_works(v: &Value, key: &str) -> Vec<PopularWork> {
    let Some(items) = payload_array(v, key) else {
        warn!(
            target: "meta-music",
            expected = key,
            got = ?v.get("payload").map(|p| p.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>())),
            "listenbrainz payload has no recognised works array; browse row will be empty"
        );
        return Vec::new();
    };
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        // The release-group MBID is the identity an album card is addressed
        // by. An entry without one cannot become a card at all, so skip it
        // individually rather than failing the batch — a partial row beats no
        // row.
        let Some(rg) = pick_str(it, &["release_group_mbid", "release_group_id"]) else {
            continue;
        };
        let Some(title) = pick_str(
            it,
            &["release_group_name", "release_name", "title", "name"],
        ) else {
            continue;
        };
        let artist_name = pick_str(
            it,
            &["artist_credit_name", "artist_name", "artist"],
        )
        .unwrap_or_default()
        .to_string();
        // `artist_mbids` is a list on the stats endpoints and absent on some
        // fresh-release entries; take the first.
        let artist_mbid = pick_str(it, &["artist_mbid"])
            .map(str::to_string)
            .or_else(|| {
                it.get("artist_mbids")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|s| !s.is_empty());
        let caa = match (
            pick_str(it, &["caa_release_mbid"]),
            it.get("caa_id").and_then(Value::as_i64),
        ) {
            (Some(rel), Some(id)) => Some((rel.to_string(), id)),
            _ => None,
        };
        out.push(PopularWork {
            release_group_mbid: rg.to_string(),
            title: title.to_string(),
            artist_name,
            artist_mbid,
            caa,
            release_date: pick_str(it, &["release_date", "date", "first_release_date"])
                .map(str::to_string),
        });
    }
    // The same work can appear twice across a fresh-release window (one entry
    // per pressing). A card is per *work*, so collapse on the release-group.
    let mut seen = std::collections::HashSet::new();
    out.retain(|w| seen.insert(w.release_group_mbid.clone()));
    out
}

/// Parse a sitewide-artists payload.
pub fn parse_artists(v: &Value) -> Vec<PopularArtist> {
    let Some(items) = payload_array(v, "artists") else {
        warn!(
            target: "meta-music",
            "listenbrainz payload has no recognised artists array; browse row will be empty"
        );
        return Vec::new();
    };
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        let Some(mbid) = pick_str(it, &["artist_mbid"]).or_else(|| {
            it.get("artist_mbids")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
        }) else {
            continue;
        };
        let Some(name) = pick_str(it, &["artist_name", "name"]) else {
            continue;
        };
        out.push(PopularArtist {
            artist_mbid: mbid.to_string(),
            name: name.to_string(),
        });
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|a| seen.insert(a.artist_mbid.clone()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_the_sitewide_release_groups_shape() {
        let v = json!({"payload": {"release_groups": [{
            "release_group_mbid": "rg-1",
            "release_group_name": "Kind of Blue",
            "artist_name": "Miles Davis",
            "artist_mbids": ["artist-1"],
            "caa_id": 12345,
            "caa_release_mbid": "rel-1",
            "listen_count": 900
        }]}});
        let got = parse_works(&v, "release_groups");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].release_group_mbid, "rg-1");
        assert_eq!(got[0].title, "Kind of Blue");
        assert_eq!(got[0].artist_mbid.as_deref(), Some("artist-1"));
        assert_eq!(got[0].caa, Some(("rel-1".to_string(), 12345)));
    }

    #[test]
    fn parses_the_fresh_releases_shape() {
        let v = json!({"payload": {"releases": [{
            "release_group_mbid": "rg-2",
            "release_name": "New Thing",
            "artist_credit_name": "Somebody",
            "release_date": "2026-08-01"
        }]}});
        let got = parse_works(&v, "releases");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].artist_name, "Somebody");
        assert_eq!(got[0].release_date.as_deref(), Some("2026-08-01"));
        assert!(got[0].caa.is_none(), "no caa fields ⇒ no resolved cover");
    }

    /// One malformed entry must not empty the row — a partial browse row is
    /// strictly better than a blank wall with no error.
    #[test]
    fn a_bad_entry_is_skipped_not_fatal() {
        let v = json!({"payload": {"release_groups": [
            {"nothing": "useful"},
            {"release_group_mbid": "rg-ok", "release_group_name": "Fine", "artist_name": "A"}
        ]}});
        let got = parse_works(&v, "release_groups");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].release_group_mbid, "rg-ok");
    }

    /// A work with several pressings in the window is one card, not three.
    #[test]
    fn works_are_deduplicated_by_release_group() {
        let v = json!({"payload": {"releases": [
            {"release_group_mbid": "rg-3", "release_name": "Album", "artist_credit_name": "A"},
            {"release_group_mbid": "rg-3", "release_name": "Album (Deluxe)", "artist_credit_name": "A"}
        ]}});
        assert_eq!(parse_works(&v, "releases").len(), 1);
    }

    /// Shape tolerance: a bare top-level array, or an unwrapped key, still
    /// parses. This is the guard against the next breaking change.
    #[test]
    fn tolerates_an_unwrapped_payload() {
        let unwrapped = json!({"release_groups": [
            {"release_group_mbid": "rg-4", "release_group_name": "T", "artist_name": "A"}
        ]});
        assert_eq!(parse_works(&unwrapped, "release_groups").len(), 1);

        let bare = json!([{"release_group_mbid": "rg-5", "release_group_name": "T", "artist_name": "A"}]);
        assert_eq!(parse_works(&bare, "release_groups").len(), 1);
    }

    #[test]
    fn parses_artists_and_skips_mbid_less_entries() {
        let v = json!({"payload": {"artists": [
            {"artist_name": "No MBID"},
            {"artist_name": "Real", "artist_mbid": "a-1"}
        ]}});
        let got = parse_artists(&v);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "Real");
    }

    /// An unrecognised payload yields an empty row rather than a panic — and
    /// the caller logs it.
    #[test]
    fn an_unrecognised_payload_is_empty_not_fatal() {
        let v = json!({"payload": {"something_else": 1}});
        assert!(parse_works(&v, "release_groups").is_empty());
        assert!(parse_artists(&v).is_empty());
    }
}
