//! The `musicbrainz` upstream — a **card** feeder plugin.
//!
//! Answers the discovery half of gateway search for the music domain: free
//! text or a browse marker in, `fileType=card` records out. It never calls an
//! indexer and never serves bytes — a card query costs at most one cached
//! MusicBrainz search, so the expensive retrieval budget is spent later,
//! entirely on the single card the user clicks.
//!
//! The Cover Art Archive and ListenBrainz are **clients of this plugin**, not
//! separate plugins: neither is independently queryable (a cover is addressed
//! by a MusicBrainz id, and a popularity chart is a ranking *of* MusicBrainz
//! works), so registering them as routable upstreams would advertise
//! capabilities nothing can ask for.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use meta_feeder_sdk::config::ConfigSchema;
use meta_feeder_sdk::plugin::{ConfigError, FeederPlugin, GatewayQuery, HashKind, HashOutcome};
use meta_feeder_sdk::types::{DiscoveryRecord, GatewayError, Hash, PluginHealth};
use tracing::warn;

use meta_feeder_sdk::cache::MidhashCache;
use crate::card::{split_record_id, Card, CardKind, CardSource};
use crate::consts::{
    DEFAULT_CARD_DISCOVERY_N, DEFAULT_CARD_TOP_N, LB_BURST, LB_RATE_PER_SEC,
    MB_BURST, MB_DISCOVERY_WAIT_DEADLINE_SECS, MB_RATE_PER_SEC, MB_WAIT_DEADLINE_SECS,
};
use crate::listenbrainz::ListenBrainzClient;
use crate::client::MusicBrainzClient;
use meta_feeder_sdk::budget::RateBudget;
use crate::resolve::Resolver;

/// Operator config, supplied via the feeder's own web form.
///
/// **No API key appears here, and that is the point.** MusicBrainz, the Cover
/// Art Archive and ListenBrainz all serve this feeder's traffic without a
/// credential, so unlike the TMDB card feeder there is no soft-skip path and
/// no "configure me first" state: a freshly deployed music feeder answers
/// queries on its first boot. That is what makes a cold meta-listen client
/// show a populated board with nothing configured.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct MusicBrainzConfig {
    #[serde(default)]
    pub card_top_n: Option<usize>,
    #[serde(default)]
    pub card_discovery_n: Option<usize>,
}

impl MusicBrainzConfig {
    /// Env seed, used on first boot before any `config.json` exists.
    pub fn from_env() -> Self {
        Self {
            card_top_n: std::env::var("CARD_TOP_N").ok().and_then(|v| v.parse().ok()),
            card_discovery_n: std::env::var("CARD_DISCOVERY_N")
                .ok()
                .and_then(|v| v.parse().ok()),
        }
    }
}

pub struct MusicBrainzCardPlugin {
    config: MusicBrainzConfig,
    resolver: Option<Resolver>,
    top_n: usize,
    discovery_n: usize,
    /// Test hook: `(musicbrainz_base, listenbrainz_base)` overriding the real
    /// endpoints so the contract test can point at a wiremock server.
    api_bases: Option<(String, String)>,
}

impl Default for MusicBrainzCardPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl MusicBrainzCardPlugin {
    pub fn new() -> Self {
        Self {
            config: MusicBrainzConfig::from_env(),
            resolver: None,
            top_n: DEFAULT_CARD_TOP_N,
            discovery_n: DEFAULT_CARD_DISCOVERY_N,
            api_bases: None,
        }
    }

    /// Test constructor: endpoint overrides, so the contract test can drive the
    /// real router against a wiremock upstream.
    pub fn with_api_bases(mb_base: String, lb_base: String) -> Self {
        let mut p = Self::new();
        p.api_bases = Some((mb_base, lb_base));
        p
    }

    /// File wins over env, exactly like every other feeder. No hot reload —
    /// bounce the feeder.
    fn load_config(&mut self, cache_dir: &Path) {
        let path = cache_dir.join("config.json");
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(file_cfg) = serde_json::from_slice::<MusicBrainzConfig>(&bytes) {
                if file_cfg.card_top_n.is_some() {
                    self.config.card_top_n = file_cfg.card_top_n;
                }
                if file_cfg.card_discovery_n.is_some() {
                    self.config.card_discovery_n = file_cfg.card_discovery_n;
                }
            }
        }
    }

    fn resolver(&self) -> Option<&Resolver> {
        self.resolver.as_ref()
    }
}

/// The deadline a query somebody is waiting on may spend on the budget.
fn search_deadline() -> Duration {
    Duration::from_secs(MB_WAIT_DEADLINE_SECS)
}

/// The (much shorter) deadline a browse row may spend. See
/// `consts::MB_DISCOVERY_WAIT_DEADLINE_SECS`.
fn browse_deadline() -> Duration {
    Duration::from_secs(MB_DISCOVERY_WAIT_DEADLINE_SECS)
}

#[async_trait]
impl FeederPlugin for MusicBrainzCardPlugin {
    fn upstream_id(&self) -> &'static str {
        "musicbrainz"
    }

    /// **The routing declaration.** `card` on the fileType axis is what makes
    /// meta-search fan a `fileType:card` query out to this gateway;
    /// `album`/`artist` on the contentKind axis is what keeps a *video* card
    /// query (`contentKind:series`) from reaching it. Both are matched
    /// dynamically against the heartbeat by
    /// `meta-search`'s `upstreams_for_query_from_raw`, so adding this tier
    /// needs no meta-search change.
    fn served_file_types(&self) -> &'static [&'static str] {
        &["card"]
    }

    fn served_content_kinds(&self) -> &'static [&'static str] {
        &["album", "artist"]
    }

    fn configure(&mut self, cache_dir: &Path) -> Result<(), ConfigError> {
        self.load_config(cache_dir);
        self.top_n = self
            .config
            .card_top_n
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_CARD_TOP_N);
        self.discovery_n = self
            .config
            .card_discovery_n
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_CARD_DISCOVERY_N);

        let cache = MidhashCache::open(cache_dir).map_err(|e| ConfigError::Other {
            plugin: "musicbrainz",
            source: anyhow::anyhow!("open music cache: {e}"),
        })?;

        // Two independent budgets. A shared one would either throttle
        // ListenBrainz to MusicBrainz's 1/s, or let MusicBrainz burst past a
        // ceiling whose breach blocks the whole source IP.
        let mb_budget = RateBudget::new(MB_RATE_PER_SEC, MB_BURST);
        let lb_budget = RateBudget::new(LB_RATE_PER_SEC, LB_BURST);

        let (mb, lb) = match &self.api_bases {
            Some((mb_base, lb_base)) => (
                MusicBrainzClient::with_base(mb_base.clone(), mb_budget, cache),
                ListenBrainzClient::with_base(lb_base.clone(), lb_budget),
            ),
            None => (
                MusicBrainzClient::new(mb_budget, cache),
                ListenBrainzClient::new(lb_budget),
            ),
        };
        self.resolver = Some(Resolver::new(Arc::new(mb), Arc::new(lb)));
        Ok(())
    }

    fn health(&self) -> PluginHealth {
        match self.resolver {
            Some(_) => PluginHealth::Ok,
            // Only reachable before `configure` ran, or if the cache failed to
            // open. Unlike the TMDB card feeder there is no credential to be
            // missing, so this is a real fault rather than an opt-in soft-skip.
            None => PluginHealth::Degraded {
                reason: "configure() not yet called (or the local cache failed to open)".to_string(),
            },
        }
    }

    async fn handle_query(
        &self,
        query: &GatewayQuery,
        max_results: usize,
    ) -> Result<Vec<DiscoveryRecord>, GatewayError> {
        // Layer A early-return: this plugin only ever serves music cards.
        if !meta_feeder_sdk::query_eval::query_accepts_plugin(
            query,
            self.served_file_types(),
            self.served_content_kinds(),
        ) {
            // One misconfiguration lands here often enough to deserve a name: a
            // browse row copied from the *video* card tier says
            // `contentKind:series`, which is not in `served_content_kinds`, so
            // the row is rejected here before any upstream is touched and the
            // operator sees an empty wall with no error anywhere.
            if crate::discovery::is_browse_query(query) {
                warn!(
                    target: "meta-music",
                    content_kind = ?query.filters.get("contentKind"),
                    "browse row rejected by the served-kinds gate: this feeder \
                     serves contentKind:album / contentKind:artist"
                );
            }
            return Ok(Vec::new());
        }
        let Some(resolver) = self.resolver() else {
            return Ok(Vec::new());
        };

        // Three shapes, in precedence order:
        //   1. an explicit MBID filter — the direct "give me this work" lookup
        //      a deep link (or the album/artist page) uses;
        //   2. a keyword-less browse query (`popular:true contentKind:album`);
        //   3. free text — the principal search.
        let cards: Vec<Card> = if let Some(mbid) = first_filter(query, "mbReleaseGroupId") {
            resolver
                .album_card_by_mbid(&mbid, search_deadline())
                .await
                .into_iter()
                .collect()
        } else if let Some(mbid) = first_filter(query, "mbArtistId") {
            // An artist id means one of two questions: "who is this artist?"
            // (an artist card) or "what did they release?" (their albums). The
            // requested `contentKind` decides, and album is the default — a
            // detail page asks for the discography far more often than it asks
            // for the artist tile it already has.
            if wants_kind(query, CardKind::Artist) && !wants_kind(query, CardKind::Album) {
                resolver
                    .artist_card_by_mbid(&mbid, search_deadline())
                    .await
                    .into_iter()
                    .collect()
            } else {
                resolver
                    .album_cards_for_artist(&mbid, max_results.max(1), search_deadline())
                    .await
            }
        } else if crate::discovery::is_browse_query(query) {
            let n = self.discovery_n.min(max_results.max(1));
            crate::discovery::cards_for_browse(resolver, query, n, browse_deadline()).await
        } else {
            let free_text = query.free_text.trim();
            if free_text.is_empty() {
                return Ok(Vec::new());
            }
            let n = self.top_n.min(max_results.max(1));
            let mut cards = Vec::new();
            if wants_kind(query, CardKind::Album) {
                cards.extend(resolver.album_cards_for_text(free_text, n, search_deadline()).await);
            }
            if wants_kind(query, CardKind::Artist) {
                cards.extend(resolver.artist_cards_for_text(free_text, n, search_deadline()).await);
            }
            cards
        };

        Ok(cards
            .iter()
            .filter_map(|c| c.to_record(&query.filters))
            .take(max_results)
            .collect())
    }

    /// Resolve a card `record_id` back into its locator CID.
    ///
    /// Not a stub for a byte-less plugin — this *is* the card path. The outcome
    /// carries `bytes: None`, so the gateway core's three-branch auto-store
    /// routes it to metadata-only: the record lands in meta-core, nothing is
    /// written to WebDAV, and nothing is seeded to bitswap (seeding is gated on
    /// `Sha2_256`). The record IS the payload.
    ///
    /// The CID needs no network call — it is a pure function of the record id.
    /// The card is re-resolved anyway so the stored record is complete, but a
    /// resolution failure still yields the CID, because the identity is
    /// derivable regardless.
    async fn compute_outcomes(&self, record_id: &str) -> Result<Vec<HashOutcome>, GatewayError> {
        let (source, mbid) = split_record_id(record_id).ok_or_else(|| {
            GatewayError::Permanent(format!("malformed card record_id '{record_id}'"))
        })?;
        if source != CardSource::MusicBrainz.as_str() {
            return Err(GatewayError::NotFound);
        }
        let hash = meta_feeder_sdk::hash::compute_card_cid(source, mbid).ok_or_else(|| {
            GatewayError::Permanent(format!("card id too long to encode: '{record_id}'"))
        })?;

        // ⚠ The record id carries no kind (see `card.rs` — the id IS the CID
        // preimage, so a `album:`/`artist:` prefix would re-mint every
        // address), so which entity this MBID names has to be discovered.
        // Release-group first: albums outnumber artists in every board and
        // every search, so the common case costs one call rather than two.
        let record = match self.resolver() {
            Some(r) => match r.album_card_by_mbid(mbid, search_deadline()).await {
                Some(c) => c.to_record(&Default::default()),
                None => r
                    .artist_card_by_mbid(mbid, search_deadline())
                    .await
                    .and_then(|c| c.to_record(&Default::default())),
            },
            None => None,
        };

        Ok(vec![HashOutcome {
            hash: Hash(hash),
            hash_kind: HashKind::CardLocator,
            bytes: None,
            record,
            file_extension: None,
        }])
    }

    fn config_schema(&self) -> ConfigSchema {
        use meta_feeder_sdk::config::{ConfigField as F, ConfigSchema};
        ConfigSchema {
            fields: vec![
                F::number("card_top_n", "Cards per search").with_help(
                    "How many works a free-text search returns. Blank keeps the \
                     built-in default (10). Costs only MusicBrainz calls — no \
                     indexer budget is spent until a card is clicked.",
                ),
                F::number("card_discovery_n", "Cards per browse row").with_help(
                    "How many cards a keyword-less browse row (popular / trending / \
                     fresh) returns. Blank keeps the built-in default (20). A row \
                     costs one ListenBrainz call; the feeder over-fetches because \
                     it cannot tell in advance which works have cover art.",
                ),
            ],
        }
    }

    fn config_values(&self) -> serde_json::Value {
        serde_json::json!({
            "card_top_n": self.config.card_top_n,
            "card_discovery_n": self.config.card_discovery_n,
        })
    }
}

/// The first value of a structured filter, trimmed and non-empty.
fn first_filter(query: &GatewayQuery, key: &str) -> Option<String> {
    query
        .filters
        .get(key)
        .and_then(|v| v.first())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Does the query ask for this card kind? A query with no `contentKind` filter
/// wants everything this plugin serves.
fn wants_kind(query: &GatewayQuery, kind: CardKind) -> bool {
    match query.filters.get("contentKind") {
        None => true,
        Some(values) if values.is_empty() => true,
        Some(values) => values
            .iter()
            .any(|v| v.trim().eq_ignore_ascii_case(kind.content_kind())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RG: &str = "f5093c06-23e3-404f-aeaa-40f72885ee3a";

    /// The routing declaration is load-bearing: `card` here is the only reason
    /// meta-search fans a `fileType:card` query out to this gateway, and the
    /// content kinds are what keep a video card row from landing here.
    #[test]
    fn declares_the_card_file_type_and_the_music_work_kinds() {
        let p = MusicBrainzCardPlugin::new();
        assert_eq!(p.upstream_id(), "musicbrainz");
        assert_eq!(p.served_file_types(), &["card"]);
        assert_eq!(p.served_content_kinds(), &["album", "artist"]);
    }

    /// A card CID is derivable with no network and no resolver, so
    /// `compute_outcomes` must still produce one on an unconfigured plugin —
    /// only the record is missing.
    #[tokio::test]
    async fn compute_outcomes_yields_a_byteless_locator_without_configuration() {
        let p = MusicBrainzCardPlugin::new();
        let outcomes = p
            .compute_outcomes(&format!("musicbrainz:{RG}"))
            .await
            .expect("outcomes");
        assert_eq!(outcomes.len(), 1);
        let o = &outcomes[0];
        assert_eq!(o.hash_kind, HashKind::CardLocator);
        assert!(o.bytes.is_none(), "a card has no bytes, ever");
        assert!(o.file_extension.is_none());
        assert_eq!(
            o.hash.0,
            meta_feeder_sdk::hash::compute_card_cid("musicbrainz", RG).unwrap()
        );
    }

    #[tokio::test]
    async fn compute_outcomes_rejects_a_foreign_or_malformed_id() {
        let p = MusicBrainzCardPlugin::new();
        assert!(p.compute_outcomes("tmdb:tv:95479").await.is_err());
        assert!(p.compute_outcomes("nosource").await.is_err());
    }

    /// Layer-A routing: a query for something this plugin cannot serve returns
    /// immediately, without touching any upstream.
    #[tokio::test]
    async fn rejects_a_query_for_a_type_it_does_not_serve() {
        let p = MusicBrainzCardPlugin::new();
        let mut q = GatewayQuery::from_free_text("miles davis");
        q.filters
            .insert("fileType".to_string(), vec!["video".to_string()]);
        assert!(p.handle_query(&q, 10).await.unwrap().is_empty());
    }

    /// An unconfigured plugin answers nothing rather than panicking — but it
    /// reports itself degraded, because unlike the TMDB feeder there is no
    /// credential that could legitimately be absent.
    #[tokio::test]
    async fn an_unconfigured_plugin_is_degraded_and_answers_nothing() {
        let p = MusicBrainzCardPlugin::new();
        let q = GatewayQuery::from_free_text("miles davis");
        assert!(p.handle_query(&q, 10).await.unwrap().is_empty());
        assert!(matches!(p.health(), PluginHealth::Degraded { .. }));
    }

    #[test]
    fn a_query_with_no_content_kind_wants_every_served_kind() {
        let q = GatewayQuery::from_free_text("x");
        assert!(wants_kind(&q, CardKind::Album));
        assert!(wants_kind(&q, CardKind::Artist));
    }

    #[test]
    fn a_content_kind_filter_selects_one_kind() {
        let mut q = GatewayQuery::from_free_text("x");
        q.filters
            .insert("contentKind".to_string(), vec!["artist".to_string()]);
        assert!(!wants_kind(&q, CardKind::Album));
        assert!(wants_kind(&q, CardKind::Artist));
    }

    #[test]
    fn first_filter_trims_and_rejects_empties() {
        let mut q = GatewayQuery::from_free_text("x");
        q.filters
            .insert("mbReleaseGroupId".to_string(), vec!["  rg-1 ".to_string()]);
        q.filters
            .insert("mbArtistId".to_string(), vec!["   ".to_string()]);
        assert_eq!(first_filter(&q, "mbReleaseGroupId").as_deref(), Some("rg-1"));
        assert_eq!(first_filter(&q, "mbArtistId"), None);
        assert_eq!(first_filter(&q, "absent"), None);
    }
}
