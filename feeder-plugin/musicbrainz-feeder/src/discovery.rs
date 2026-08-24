//! Keyword-less browse: ListenBrainz popularity and fresh-release feeds →
//! [`Card`]s.
//!
//! The card tier's *browse* surface as opposed to its search surface. A client
//! with a home board cannot ask "what should I show?" through a text query —
//! there is no text. This branch answers a query whose intent is carried
//! entirely by structured filters.
//!
//! ## Query dialect
//!
//! Deliberately identical to the card feeder's, so a client switches domains
//! by swapping `contentKind:series` for `contentKind:album` and nothing else:
//!
//! - **mode** (one of) — `popular:true` → ListenBrainz sitewide top;
//!   `trending:true` → the same feed over a shorter window (ListenBrainz has no
//!   distinct trending endpoint, so trending approximates to recent
//!   popularity, exactly as the card feeder's anime path approximates trending
//!   with popularity); `fresh:true` → the fresh-releases feed.
//! - **kind** (required) — `contentKind:album` → albums, `contentKind:artist`
//!   → artists. Also what routes the query here at all, alongside
//!   `fileType:card`.
//!
//! ⚠ There is deliberately **no `top_rated` mode**. ListenBrainz measures
//! listens, not ratings, and MusicBrainz holds no rating at all — a
//! `top_rated:true` row would have to be silently answered with popularity,
//! which is exactly the kind of quiet substitution that makes a browse row
//! impossible to reason about. An unsupported mode returns nothing and says so
//! in the log.
//!
//! ## Why nothing here is cached
//!
//! A popularity list is **mutable by nature** — "popular this month" is the one
//! thing that must not be frozen — and this crate's cache is a permanent-hit
//! store sized for immutable MBID-keyed data. Persisting a chart there would
//! pin the board to whatever it looked like on first boot. The layer above
//! already absorbs the repeat cost: the gateway persists each emitted card to
//! meta-core and its search-coverage gate then skips a repeated
//! `(upstream, query)` for an hour, serving meta-core instead. Every call is
//! budget-gated, so a burst cannot run away.

use std::time::Duration;

use meta_feeder_sdk::plugin::GatewayQuery;
use tracing::{debug, warn};

use crate::card::{Card, CardKind};
use crate::consts::DISCOVERY_OVERFETCH;
use crate::resolve::Resolver;

/// Days back the `fresh:true` row reaches.
const FRESH_WINDOW_DAYS: u32 = 30;

/// The browse flavour a query asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BrowseMode {
    Popular,
    Trending,
    Fresh,
}

impl BrowseMode {
    /// The query-filter key that selects this mode.
    ///
    /// It is also echoed onto every emitted record — not here, but by
    /// [`Card::to_record`]'s trailing filter echo, which stamps back every
    /// filter the query carried that the card did not already set. That echo
    /// is load-bearing: both the gateway dispatcher and meta-search re-apply
    /// `record_matches` to each record, and a *missing* field fails the match,
    /// so an un-echoed `popular` would see every card in the row dropped at
    /// one of the two tiers.
    fn marker(self) -> &'static str {
        match self {
            BrowseMode::Popular => "popular",
            BrowseMode::Trending => "trending",
            BrowseMode::Fresh => "fresh",
        }
    }
}

/// True when this is a keyword-less browse query this branch should answer.
///
/// Three conditions, each excluding a query that belongs to a *different*
/// branch of `handle_query`: no free text (that is the principal search), no
/// MBID filter (that is the direct by-id lookup a deep link uses), and a
/// truthy mode marker. `contentKind` alone deliberately does **not** select
/// this branch — a text query carries it too.
pub fn is_browse_query(query: &GatewayQuery) -> bool {
    query.free_text.trim().is_empty()
        && !query.filters.contains_key("mbReleaseGroupId")
        && !query.filters.contains_key("mbArtistId")
        && browse_mode(query).is_some()
}

/// First truthy mode marker on the query, in precedence order.
fn browse_mode(query: &GatewayQuery) -> Option<BrowseMode> {
    for mode in [BrowseMode::Fresh, BrowseMode::Trending, BrowseMode::Popular] {
        if filter_is_true(query, mode.marker()) {
            return Some(mode);
        }
    }
    None
}

/// True iff `query.filters[key]` carries a truthy value.
fn filter_is_true(query: &GatewayQuery, key: &str) -> bool {
    query
        .filters
        .get(key)
        .is_some_and(|v| v.iter().any(|s| s.eq_ignore_ascii_case("true")))
}

/// Map the query's `contentKind` filter to a card kind.
pub fn browse_kind(query: &GatewayQuery) -> Option<CardKind> {
    let values = query.filters.get("contentKind")?;
    for v in values {
        match v.trim().to_ascii_lowercase().as_str() {
            "album" => return Some(CardKind::Album),
            "artist" => return Some(CardKind::Artist),
            _ => {}
        }
    }
    None
}

/// The genre slug a row is filtered by, if any.
///
/// ⚠ A genre row is answered by a **MusicBrainz tag search**, not by filtering
/// a ListenBrainz feed — see [`MusicBrainzClient::release_groups_by_tag`] for
/// why the filtering approach cannot work at all.
fn genre_slug(query: &GatewayQuery) -> Option<String> {
    query
        .filters
        .get("genres")
        .and_then(|v| v.first())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
}

/// A genre slug back to the tag MusicBrainz indexes it under. The query DSL
/// cannot spell a name with spaces (space is its token separator), so the wire
/// carries `hip-hop` and MusicBrainz wants `hip hop`.
fn slug_to_tag(slug: &str) -> String {
    slug.replace('-', " ")
}

/// Answer a browse query.
pub async fn cards_for_browse(
    resolver: &Resolver,
    query: &GatewayQuery,
    n: usize,
    deadline: Duration,
) -> Vec<Card> {
    let Some(mode) = browse_mode(query) else {
        return Vec::new();
    };
    let Some(kind) = browse_kind(query) else {
        warn!(
            target: "meta-music",
            content_kind = ?query.filters.get("contentKind"),
            "browse row has no album/artist contentKind; a card is a WORK — \
             use contentKind:album or contentKind:artist"
        );
        return Vec::new();
    };

    // Over-fetch, because the cover gate's drop rate is not predictable here —
    // see `consts::DISCOVERY_OVERFETCH`.
    let want = n.saturating_mul(DISCOVERY_OVERFETCH).clamp(1, 100);

    // A genre row is its own path: MusicBrainz tag search, not a filtered feed.
    if let (CardKind::Album, Some(slug)) = (kind, genre_slug(query)) {
        let tag = slug_to_tag(&slug);
        let mut cards = resolver.album_cards_for_tag(&tag, want, deadline).await;
        if cards.is_empty() {
            debug!(target: "meta-music", %tag, "genre row returned nothing");
        }
        cards.truncate(n);
        return cards;
    }

    let mut cards: Vec<Card> = match (kind, mode) {
        (CardKind::Album, BrowseMode::Fresh) => resolver
            .lb
            .fresh_releases(FRESH_WINDOW_DAYS, deadline)
            .await
            .iter()
            .filter_map(|w| resolver.album_card_from_popular(w))
            .collect(),
        (CardKind::Album, _) => resolver
            .lb
            .popular_release_groups(want, deadline)
            .await
            .iter()
            .filter_map(|w| resolver.album_card_from_popular(w))
            .collect(),
        (CardKind::Artist, BrowseMode::Fresh) => {
            // "Fresh artists" is not a thing ListenBrainz reports, and inventing
            // it from the fresh-release feed would silently answer a different
            // question. Say so and return nothing.
            debug!(target: "meta-music", "fresh:true is not defined for contentKind:artist");
            Vec::new()
        }
        (CardKind::Artist, _) => {
            let popular = resolver.lb.popular_artists(want, deadline).await;
            resolver
                .artist_cards_from_popular(&popular, n, deadline)
                .await
        }
    };

    cards.truncate(n);
    cards
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{CardSource, Cover};

    fn q(raw: &str, filters: &[(&str, &str)]) -> GatewayQuery {
        let mut q = GatewayQuery::from_free_text(raw);
        for (k, v) in filters {
            q.filters
                .entry(k.to_string())
                .or_default()
                .push(v.to_string());
        }
        q
    }

    #[test]
    fn a_keywordless_mode_query_is_a_browse_query() {
        assert!(is_browse_query(&q(
            "",
            &[("popular", "true"), ("contentKind", "album")]
        )));
        assert!(is_browse_query(&q(
            "",
            &[("fresh", "true"), ("contentKind", "album")]
        )));
    }

    /// Free text means the principal search owns the query, mode marker or not.
    #[test]
    fn free_text_excludes_the_browse_branch() {
        assert!(!is_browse_query(&q(
            "miles davis",
            &[("popular", "true"), ("contentKind", "album")]
        )));
    }

    /// An explicit id is the direct lookup, which is a third branch.
    #[test]
    fn an_id_filter_excludes_the_browse_branch() {
        assert!(!is_browse_query(&q(
            "",
            &[
                ("popular", "true"),
                ("contentKind", "album"),
                ("mbReleaseGroupId", "rg-1")
            ]
        )));
        assert!(!is_browse_query(&q(
            "",
            &[("popular", "true"), ("mbArtistId", "a-1")]
        )));
    }

    /// `contentKind` alone must not select the browse branch — a text query
    /// carries it too.
    #[test]
    fn a_content_kind_alone_is_not_a_browse_query() {
        assert!(!is_browse_query(&q("", &[("contentKind", "album")])));
    }

    /// The card feeder's vocabulary must NOT resolve here. A row written for
    /// video (`contentKind:series`) reaching the music feeder is a
    /// misconfiguration, and answering it with albums would hide that.
    #[test]
    fn video_content_kinds_do_not_resolve_to_a_music_kind() {
        assert_eq!(browse_kind(&q("", &[("contentKind", "series")])), None);
        assert_eq!(browse_kind(&q("", &[("contentKind", "movie")])), None);
        assert_eq!(
            browse_kind(&q("", &[("contentKind", "album")])),
            Some(CardKind::Album)
        );
        assert_eq!(
            browse_kind(&q("", &[("contentKind", "artist")])),
            Some(CardKind::Artist)
        );
    }

    /// `top_rated` is deliberately unsupported — nothing upstream measures
    /// ratings, and quietly answering with popularity would make the row a lie.
    #[test]
    fn top_rated_is_not_a_mode() {
        assert!(!is_browse_query(&q(
            "",
            &[("top_rated", "true"), ("contentKind", "album")]
        )));
    }

    #[test]
    fn mode_precedence_is_fresh_then_trending_then_popular() {
        let all = q(
            "",
            &[("popular", "true"), ("trending", "true"), ("fresh", "true")],
        );
        assert_eq!(browse_mode(&all), Some(BrowseMode::Fresh));
        let two = q("", &[("popular", "true"), ("trending", "true")]);
        assert_eq!(browse_mode(&two), Some(BrowseMode::Trending));
    }

    fn card_with_genres(genres: &[&str]) -> Card {
        Card {
            source: CardSource::MusicBrainz,
            kind: CardKind::Album,
            mbid: "rg".into(),
            title: "T".into(),
            album_artist: None,
            artists: vec![],
            artist_mbid: None,
            cover: Some(Cover::ReleaseGroupFront {
                release_group_mbid: "rg".into(),
            }),
            disambiguation: None,
            genres: genres.iter().map(|s| s.to_string()).collect(),
            release_date: None,
            track_count: None,
            disc_count: None,
            label: None,
            catalog_number: None,
            primary_type: None,
            secondary_types: vec![],
            tracks: Vec::new(),
            streaming_links: Vec::new(),
        }
    }

    /// The query language cannot spell "Hip Hop" (space is its token
    /// separator), so the wire carries a slug and MusicBrainz wants the tag.
    #[test]
    fn a_genre_slug_maps_back_to_the_musicbrainz_tag() {
        assert_eq!(slug_to_tag("hip-hop"), "hip hop");
        assert_eq!(slug_to_tag("jazz"), "jazz");
        assert_eq!(slug_to_tag("drum-and-bass"), "drum and bass");
    }

    #[test]
    fn genre_slug_is_read_from_the_filter() {
        assert_eq!(
            genre_slug(&q("", &[("genres", "Hip-Hop")])).as_deref(),
            Some("hip-hop")
        );
        assert_eq!(genre_slug(&q("", &[])), None);
    }
}
