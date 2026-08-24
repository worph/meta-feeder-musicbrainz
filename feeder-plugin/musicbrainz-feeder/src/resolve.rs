//! MusicBrainz responses → [`Card`]s.
//!
//! The card feeder's `resolve.rs` in the music domain. Everything here is
//! budget-gated through the clients; nothing here talks to the network
//! directly.

use std::sync::Arc;
use std::time::Duration;

use crate::card::{Card, CardKind, CardSource, Cover};
use crate::consts::MB_MAX_PAGES;
use crate::listenbrainz::{ListenBrainzClient, PopularArtist, PopularWork};
use crate::client::{
    primary_artist, render_credit, Artist, MusicBrainzClient, ReleaseGroup, Tag,
};

/// Minimum tag vote count for a tag to be treated as a genre.
///
/// MusicBrainz tags are open user input, so the long tail is full of one-vote
/// noise ("stuff i like", a misspelling, a mood). A floor of 1 net vote is
/// enough to remove the accidental and the vandalised while keeping genuinely
/// niche genres — the same role the card feeder's `DISCOVERY_MIN_VOTES` plays
/// for TMDB, at a far smaller scale because a tag vote is much cheaper to cast
/// than a TMDB rating.
const MIN_TAG_VOTES: i64 = 1;

/// Ceiling on genres carried by one card. A heavily-tagged release can have
/// forty; past a handful they stop being a facet and start being noise, and
/// each one is its own hash field on the record.
const MAX_GENRES: usize = 6;

/// Pick the genre names from a MusicBrainz tag/genre list.
///
/// `genres` (curated, from `inc=genres`) wins outright when present; `tags`
/// (open user input) is the fallback a search-result entry carries. They are
/// never merged — a curated list that says "Jazz" should not be diluted by a
/// tag list that also says "cool".
pub fn genre_names(genres: &[Tag], tags: &[Tag]) -> Vec<String> {
    let source = if genres.iter().any(|t| !t.name.trim().is_empty()) {
        genres
    } else {
        tags
    };
    let mut ranked: Vec<&Tag> = source
        .iter()
        .filter(|t| !t.name.trim().is_empty() && t.count >= MIN_TAG_VOTES)
        .collect();
    ranked.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    ranked
        .into_iter()
        .take(MAX_GENRES)
        .map(|t| title_case(t.name.trim()))
        .collect()
}

/// MusicBrainz tags are lowercase by convention (`modal jazz`), while
/// `METADATA_KEYS.md`'s `genres/{name}` key-set is written with the name
/// verbatim as the upstream publishes it — and every other writer publishes
/// display-cased names (`Animation`, `Sci-Fi`). Title-casing here keeps one
/// display convention across the whole key-set rather than splitting it by
/// which feeder happened to produce the record.
/// ⚠ Capitalise after **any** non-alphanumeric character, not just a space.
/// Splitting on spaces alone turned the tag `r&b` into `R&b`, which is visibly
/// wrong in a genre chip and — since the name is the key-set member itself —
/// would have made `genres/R&b` and a correctly-cased `genres/R&B` two
/// different members of the same set on the same record.
fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut at_boundary = true;
    for c in s.chars() {
        if at_boundary {
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
        at_boundary = !c.is_alphanumeric();
    }
    out
}

pub struct Resolver {
    pub mb: Arc<MusicBrainzClient>,
    pub lb: Arc<ListenBrainzClient>,
}

impl Resolver {
    pub fn new(mb: Arc<MusicBrainzClient>, lb: Arc<ListenBrainzClient>) -> Self {
        Self { mb, lb }
    }

    /// An album card from a release-group, with no further upstream calls.
    ///
    /// The cover is the release-group front endpoint, which may 404 — see
    /// [`Card::is_displayable`] for why that cannot be predicted here.
    pub fn album_card(&self, rg: &ReleaseGroup) -> Option<Card> {
        if rg.id.is_empty() || rg.title.trim().is_empty() {
            return None;
        }
        Some(Card {
            source: CardSource::MusicBrainz,
            kind: CardKind::Album,
            mbid: rg.id.clone(),
            title: rg.title.trim().to_string(),
            album_artist: Some(render_credit(&rg.artist_credit)).filter(|s| !s.is_empty()),
            artists: rg
                .artist_credit
                .iter()
                .filter_map(|c| c.artist.as_ref())
                .map(|a| a.name.clone())
                .filter(|n| !n.trim().is_empty())
                .collect(),
            artist_mbid: primary_artist(&rg.artist_credit).map(|a| a.id.clone()),
            cover: Some(Cover::ReleaseGroupFront {
                release_group_mbid: rg.id.clone(),
            }),
            disambiguation: rg.disambiguation.clone().filter(|s| !s.trim().is_empty()),
            genres: genre_names(&rg.genres, &rg.tags),
            release_date: rg.first_release_date.clone(),
            track_count: None,
            disc_count: None,
            label: None,
            catalog_number: None,
            primary_type: rg.primary_type.clone(),
            secondary_types: rg.secondary_types.clone(),
            tracks: Vec::new(),
            streaming_links: Vec::new(),
        })
    }

    /// An album card from a ListenBrainz browse entry, with no MusicBrainz
    /// call at all.
    ///
    /// This is what makes a browse row affordable: at 1 req/s, resolving
    /// twenty release-groups by MBID would take twenty seconds and every
    /// consumer-side idle cutoff would fire first. A ListenBrainz entry
    /// already carries the title, the artist credit, both MBIDs and often a
    /// *resolved* cover id — everything a renderable card needs.
    ///
    /// The price is a thinner card: no genres, no track count. Both are
    /// resolved on the detail path, which is where anything reads them.
    pub fn album_card_from_popular(&self, w: &PopularWork) -> Option<Card> {
        if w.release_group_mbid.is_empty() || w.title.trim().is_empty() {
            return None;
        }
        Some(Card {
            source: CardSource::MusicBrainz,
            kind: CardKind::Album,
            mbid: w.release_group_mbid.clone(),
            title: w.title.trim().to_string(),
            album_artist: Some(w.artist_name.clone()).filter(|s| !s.trim().is_empty()),
            artists: if w.artist_name.trim().is_empty() {
                Vec::new()
            } else {
                vec![w.artist_name.clone()]
            },
            artist_mbid: w.artist_mbid.clone(),
            cover: Some(match &w.caa {
                Some((release_mbid, caa_id)) => Cover::Resolved {
                    release_mbid: release_mbid.clone(),
                    caa_id: *caa_id,
                },
                None => Cover::ReleaseGroupFront {
                    release_group_mbid: w.release_group_mbid.clone(),
                },
            }),
            disambiguation: None,
            genres: Vec::new(),
            release_date: w.release_date.clone(),
            track_count: None,
            disc_count: None,
            label: None,
            catalog_number: None,
            primary_type: None,
            secondary_types: Vec::new(),
            tracks: Vec::new(),
            streaming_links: Vec::new(),
        })
    }

    /// An artist card.
    ///
    /// ⚠ **The image is borrowed, and that is a real compromise.** Neither
    /// MusicBrainz nor the Cover Art Archive holds artist photographs — the CAA
    /// is keyed by release and release-group only, and MusicBrainz does not
    /// host images at all for licensing reasons. The correct source is Wikidata
    /// (`P18`) reached through MusicBrainz `inc=url-rels`, which costs **two
    /// extra requests per artist** against a 1 req/s budget: a twenty-artist
    /// row would take forty seconds and every idle cutoff would fire first.
    ///
    /// So a browse-row artist card borrows a cover from one of that artist's
    /// releases, which is what the data actually supports at this cost. An
    /// artist for whom no cover is in hand gets no card rather than a blank
    /// tile. If artist photography becomes load-bearing, the Wikidata path
    /// belongs on the *detail* route, where one artist justifies two calls.
    pub fn artist_card(&self, a: &Artist, cover: Option<Cover>) -> Option<Card> {
        if a.id.is_empty() || a.name.trim().is_empty() {
            return None;
        }
        Some(Card {
            source: CardSource::MusicBrainz,
            kind: CardKind::Artist,
            mbid: a.id.clone(),
            title: a.name.trim().to_string(),
            album_artist: None,
            artists: vec![a.name.clone()],
            artist_mbid: None,
            cover,
            disambiguation: a.disambiguation.clone().filter(|s| !s.trim().is_empty()),
            genres: genre_names(&a.genres, &a.tags),
            release_date: None,
            track_count: None,
            disc_count: None,
            label: None,
            catalog_number: None,
            primary_type: a.artist_type.clone(),
            secondary_types: Vec::new(),
            tracks: Vec::new(),
            streaming_links: Vec::new(),
        })
    }

    /// Free-text album search.
    pub async fn album_cards_for_text(
        &self,
        text: &str,
        n: usize,
        deadline: Duration,
    ) -> Vec<Card> {
        self.mb
            .search_release_groups(text, n, deadline)
            .await
            .iter()
            .filter_map(|rg| self.album_card(rg))
            .collect()
    }

    /// Free-text artist search.
    ///
    /// Each hit needs a cover borrowed from one of its releases (see
    /// [`Resolver::artist_card`]), which is one browse call per artist against
    /// a 1 req/s budget — so this is deliberately capped far below the album
    /// search's breadth. A search box wants a handful of artists, not twenty.
    pub async fn artist_cards_for_text(
        &self,
        text: &str,
        n: usize,
        deadline: Duration,
    ) -> Vec<Card> {
        const MAX_ARTIST_COVER_LOOKUPS: usize = 3;
        let hits = self.mb.search_artists(text, n, deadline).await;
        let mut out = Vec::with_capacity(hits.len());
        for (i, a) in hits.iter().enumerate() {
            let cover = if i < MAX_ARTIST_COVER_LOOKUPS {
                self.cover_for_artist(&a.id, deadline).await
            } else {
                None
            };
            if let Some(card) = self.artist_card(a, cover) {
                out.push(card);
            }
        }
        out
    }

    /// Artist cards for a ListenBrainz popularity list.
    ///
    /// Same cover problem, same bounded answer: only the head of the row pays
    /// for a cover lookup. A short row of real tiles beats a long row of
    /// tiles the gate will hide anyway.
    pub async fn artist_cards_from_popular(
        &self,
        artists: &[PopularArtist],
        n: usize,
        deadline: Duration,
    ) -> Vec<Card> {
        let mut out = Vec::with_capacity(n);
        for pa in artists.iter().take(n) {
            let Some(cover) = self.cover_for_artist(&pa.artist_mbid, deadline).await else {
                continue;
            };
            let a = Artist {
                id: pa.artist_mbid.clone(),
                name: pa.name.clone(),
                ..Default::default()
            };
            if let Some(card) = self.artist_card(&a, Some(cover)) {
                out.push(card);
            }
        }
        out
    }

    /// Borrow a cover from one of an artist's release-groups. One call.
    async fn cover_for_artist(&self, artist_mbid: &str, deadline: Duration) -> Option<Cover> {
        let rgs = self.mb.release_groups_by_artist(artist_mbid, 5, deadline).await;
        // Prefer a full album over a single or a compilation: an album cover is
        // the image people associate with the artist.
        let pick = rgs
            .iter()
            .find(|rg| rg.primary_type.as_deref() == Some("Album"))
            .or_else(|| rgs.first())?;
        Some(Cover::ReleaseGroupFront {
            release_group_mbid: pick.id.clone(),
        })
    }

    /// Album cards for a genre tag — the genre row.
    pub async fn album_cards_for_tag(
        &self,
        tag: &str,
        n: usize,
        deadline: Duration,
    ) -> Vec<Card> {
        self.mb
            .release_groups_by_tag(tag, n, deadline)
            .await
            .iter()
            .filter_map(|rg| self.album_card(rg))
            .collect()
    }

    /// The direct "give me this work" lookup a deep link or a detail page uses.
    /// Resolves the full card, including the canonical track list.
    pub async fn album_card_by_mbid(&self, mbid: &str, deadline: Duration) -> Option<Card> {
        let rg = self.mb.release_group(mbid, deadline).await?;
        let card = self.album_card(&rg)?;
        match self.mb.tracklist(mbid, deadline).await {
            Some(release) => Some(card.with_release_detail(&release)),
            None => Some(card),
        }
    }

    /// The direct artist lookup — the **detail** path.
    ///
    /// Also collects the artist's official streaming links, which the browse
    /// paths deliberately do not: it costs one more MusicBrainz call against a
    /// 1 req/s budget, and a browse row renders no link affordances. Detail
    /// pages are opened one at a time, so the call is affordable exactly here.
    pub async fn artist_card_by_mbid(&self, mbid: &str, deadline: Duration) -> Option<Card> {
        let a = self.mb.artist(mbid, deadline).await?;
        let cover = self.cover_for_artist(mbid, deadline).await;
        let mut card = self.artist_card(&a, cover)?;
        card.streaming_links = self.streaming_links(mbid, deadline).await;
        Some(card)
    }

    /// Official "listen elsewhere" URLs from MusicBrainz's artist relations.
    ///
    /// Both flavours count: `free streaming` (Deezer, Spotify, Yandex) and
    /// `streaming` (Tidal, Apple, Qobuz, Amazon). YouTube relations are
    /// excluded — those are the *delegated playback* tier's channel keys, and a
    /// channel page is a browse target, not a rendition to open.
    async fn streaming_links(&self, mbid: &str, deadline: Duration) -> Vec<String> {
        let Some(rels) = self.mb.artist_url_relations(mbid, deadline).await else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (ty, url) in rels {
            let t = ty.to_ascii_lowercase();
            if t != "streaming" && t != "free streaming" {
                continue;
            }
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                continue;
            }
            if !out.contains(&url) {
                out.push(url);
            }
        }
        out
    }

    /// An artist's discography as album cards.
    pub async fn album_cards_for_artist(
        &self,
        artist_mbid: &str,
        n: usize,
        deadline: Duration,
    ) -> Vec<Card> {
        let cap = n.min(MB_MAX_PAGES as usize * 100);
        self.mb
            .release_groups_by_artist(artist_mbid, cap, deadline)
            .await
            .iter()
            .filter_map(|rg| self.album_card(rg))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(name: &str, count: i64) -> Tag {
        Tag {
            name: name.to_string(),
            count,
        }
    }

    /// Curated genres win outright — a curated "Jazz" must not be diluted by
    /// whatever the tag crowd also wrote.
    #[test]
    fn curated_genres_beat_open_tags() {
        let got = genre_names(&[tag("jazz", 3)], &[tag("cool", 9), tag("stuff i like", 9)]);
        assert_eq!(got, vec!["Jazz"]);
    }

    #[test]
    fn tags_are_the_fallback_ranked_by_votes() {
        let got = genre_names(
            &[],
            &[tag("rock", 2), tag("hip hop", 7), tag("noise", 0)],
        );
        // Sorted by votes desc; the zero-vote tag is filtered out entirely.
        assert_eq!(got, vec!["Hip Hop", "Rock"]);
    }

    /// Names join the same key-set other feeders write, so they carry the same
    /// display casing rather than MusicBrainz's lowercase convention.
    #[test]
    fn genre_names_are_title_cased() {
        assert_eq!(genre_names(&[tag("modal jazz", 4)], &[]), vec!["Modal Jazz"]);
    }

    /// ⚠ Regression guard, seen live: splitting on spaces alone rendered the
    /// real MusicBrainz tag `r&b` as `R&b`. Since the name *is* the key-set
    /// member, that would put `genres/R&b` and `genres/R&B` on the same record
    /// as two different members of one set.
    #[test]
    fn capitalisation_happens_after_punctuation_too() {
        assert_eq!(title_case("r&b"), "R&B");
        assert_eq!(title_case("hip-hop"), "Hip-Hop");
        assert_eq!(title_case("drum'n'bass"), "Drum'N'Bass");
        assert_eq!(title_case("trip hop"), "Trip Hop");
        assert_eq!(title_case(""), "");
    }

    #[test]
    fn genres_are_capped() {
        let many: Vec<Tag> = (0..20).map(|i| tag(&format!("g{i}"), 20 - i)).collect();
        assert_eq!(genre_names(&many, &[]).len(), MAX_GENRES);
    }

    /// A ListenBrainz entry that already names a CAA image must use it — that
    /// image is known to exist, whereas the release-group front endpoint 404s
    /// silently and the card is then hidden downstream.
    #[test]
    fn a_popular_entry_prefers_its_resolved_cover() {
        let r = Resolver::new(
            Arc::new(MusicBrainzClient::with_base(
                "http://unused".into(),
                meta_feeder_sdk::budget::RateBudget::new(1.0, 1.0),
                open_test_cache(),
            )),
            Arc::new(ListenBrainzClient::with_base(
                "http://unused".into(),
                meta_feeder_sdk::budget::RateBudget::new(1.0, 1.0),
            )),
        );
        let with_caa = PopularWork {
            release_group_mbid: "rg".into(),
            title: "T".into(),
            artist_name: "A".into(),
            artist_mbid: Some("am".into()),
            caa: Some(("rel".into(), 7)),
            release_date: None,
        };
        let card = r.album_card_from_popular(&with_caa).expect("card");
        assert!(matches!(card.cover, Some(Cover::Resolved { .. })));

        let without = PopularWork {
            caa: None,
            ..with_caa
        };
        let card = r.album_card_from_popular(&without).expect("card");
        assert!(matches!(card.cover, Some(Cover::ReleaseGroupFront { .. })));
    }

    fn open_test_cache() -> meta_feeder_sdk::cache::MidhashCache {
        let dir = tempfile::tempdir().expect("tempdir");
        let c = meta_feeder_sdk::cache::MidhashCache::open(dir.path()).expect("open");
        // Leak the tempdir so the redb file outlives this call in-test.
        std::mem::forget(dir);
        c
    }
}
