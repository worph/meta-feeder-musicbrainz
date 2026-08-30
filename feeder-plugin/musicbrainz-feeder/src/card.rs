//! The music card model and its projection to a `DiscoveryRecord`.
//!
//! A **card** identifies a *work* — an album, an artist — not a file. It
//! carries the display pair the quality gate needs (title + cover) plus an
//! **id bag** of every cross-source identifier the bridge resolved. Nothing
//! here is a byte locator; the card's own CID is a `0x1007` card-locator
//! derived from `(source, id)` and nothing is ever fetchable by it.
//!
//! The direct counterpart of `meta-feeder-card`'s `card.rs`, and deliberately
//! shaped the same so the two read side by side.
//!
//! ## Two kinds, one address space
//!
//! ⚠ **A music card locator takes no kind prefix**, unlike TMDB's
//! `tv:95479` / `movie:95479`. TMDB needs one because it reuses numeric ids
//! across media types; a MusicBrainz Identifier is a UUID and is unique across
//! every entity type, so `compute_card_cid("musicbrainz", "<mbid>")` is minted
//! from the bare id. This is the IMDb convention (`METADATA_KEYS.md` §4), and
//! it is why `record_id` is `"musicbrainz:<mbid>"` with nothing in between —
//! the record id *is* the CID preimage, so inserting a kind would silently
//! re-mint every card address.

use std::collections::BTreeMap;

use meta_feeder_sdk::hash::compute_card_cid;
use meta_feeder_sdk::types::DiscoveryRecord;

use crate::consts::{CAA_BASE, CAA_FRONT_SIZE, MAX_TRACK_COUNT};
use meta_feeder_sdk::hash::artwork_locator;
use crate::client::{normalise_date, Release};

/// The metadata source that published a card. The string form is the CID's
/// source namespace, so **changing it re-mints every card CID from that
/// source** — treat it as a wire constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardSource {
    MusicBrainz,
}

impl CardSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CardSource::MusicBrainz => "musicbrainz",
        }
    }
}

/// Which work a card describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKind {
    Album,
    Artist,
}

impl CardKind {
    /// The `contentKind` facet. Orthogonal to `fileType=card`.
    pub fn content_kind(self) -> &'static str {
        match self {
            CardKind::Album => "album",
            CardKind::Artist => "artist",
        }
    }

    /// Which artwork field this kind writes. An album has a **sleeve**, an
    /// artist has a **portrait**, and the registry keeps them apart rather than
    /// calling both a `poster` the way the video tier does.
    ///
    /// The value is a **`url` locator cid**, never a raw URL — see
    /// [`artwork_locator`]. See METADATA_KEYS.md §6.
    pub fn artwork_field(self) -> &'static str {
        match self {
            CardKind::Album => "cover",
            CardKind::Artist => "photo",
        }
    }
}

/// Where a card's cover image comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cover {
    /// A Cover Art Archive front image for a release-group. The endpoint
    /// 307-redirects to the archive and **404s when nothing is illustrated**,
    /// which is how a coverless work ends up gated out — see [`Card::to_record`].
    ReleaseGroupFront { release_group_mbid: String },
    /// A specific CAA image somebody already resolved for us (ListenBrainz
    /// returns these). Strictly better than the front endpoint: it names an
    /// image **known to exist**, so it cannot 404 into a hidden card.
    Resolved { release_mbid: String, caa_id: i64 },
}

impl Cover {
    pub fn url(&self) -> String {
        match self {
            Cover::ReleaseGroupFront { release_group_mbid } => {
                format!("{CAA_BASE}/release-group/{release_group_mbid}/{CAA_FRONT_SIZE}")
            }
            Cover::Resolved {
                release_mbid,
                caa_id,
            } => format!("{CAA_BASE}/release/{release_mbid}/{caa_id}-500.jpg"),
        }
    }
}

/// A resolved work.
///
/// Immutable by contract, exactly as a TMDB card is: no listen counts, no
/// popularity, no rating — nothing that drifts without the work changing.
/// `METADATA_KEYS.md` rule #4 forbids those on the shared hash anyway, and a
/// card is the one record type several peers derive independently at the same
/// CID, so a drifting value would make two peers disagree forever.
#[derive(Debug, Clone)]
pub struct Card {
    pub source: CardSource,
    pub kind: CardKind,
    /// The bare MBID. Half of the card CID's preimage — see [`Card::record_id`].
    pub mbid: String,

    // -- display ------------------------------------------------------------
    pub title: String,
    /// Credited album artist (albums only). The *display* string, join phrases
    /// included.
    pub album_artist: Option<String>,
    /// Every credited artist name, for the `artists/{name}` key-set.
    pub artists: Vec<String>,
    /// The primary artist's own MBID, when the credit resolved one.
    pub artist_mbid: Option<String>,
    pub cover: Option<Cover>,
    /// MusicBrainz has no synopsis field. Its `disambiguation` ("live album",
    /// "1997 remaster") is the nearest thing and is what fills
    /// `description/eng` when present. The album gate deliberately does **not**
    /// require it — see `meta-listen/ARCHITECTURE.md` §6.
    pub disambiguation: Option<String>,
    pub genres: Vec<String>,
    /// Variable-precision MusicBrainz date, normalised on projection.
    pub release_date: Option<String>,

    // -- structure (albums, detail path only) --------------------------------
    pub track_count: Option<u32>,
    pub disc_count: Option<u32>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    /// `Album` / `Single` / `EP` / … Kept for the artist page's discography
    /// grouping; not a `contentKind` (the work is an `album` either way).
    pub primary_type: Option<String>,
    pub secondary_types: Vec<String>,
    /// The **canonical track list**, resolved on the detail path only.
    ///
    /// This is what lets a consumer say "12 tracks, 9 playable" truthfully and
    /// name the three it cannot get. Without it an album page can only render
    /// the releases it happened to find, which silently redefines the album as
    /// whatever is available — the failure mode the whole card tier exists to
    /// avoid.
    pub tracks: Vec<CanonicalTrack>,
    /// Official places to *listen elsewhere* — MusicBrainz's `streaming` and
    /// `free streaming` artist relations (Spotify, Tidal, Apple, Deezer, …),
    /// measured present in the delegated-playback study §4.2.
    ///
    /// Projected as `ext-play` (`0x1009`) locators in [`Card::to_record`], which
    /// is where the reasoning for keeping them out of `cids/*` lives.
    pub streaming_links: Vec<String>,
}

/// One entry of an album card's canonical track list.
///
/// Carries no CID and no availability: it says *what the album contains*, not
/// what anyone can fetch. Joining it to real releases is the consumer's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalTrack {
    /// 1-based position within its disc.
    pub position: u32,
    /// 1-based disc index.
    pub disc: u32,
    pub title: String,
    /// Milliseconds, when anyone has timed it.
    pub length_ms: Option<u64>,
    pub mb_recording_id: Option<String>,
    pub isrc: Option<String>,
}

impl Card {
    /// The feeder-scoped record id, `"musicbrainz:<mbid>"` — also the exact
    /// preimage of the card CID, so `compute_outcomes` round-trips a record id
    /// straight back to a CID with no lookup.
    pub fn record_id(&self) -> String {
        format!("{}:{}", self.source.as_str(), self.mbid)
    }

    /// This card's `0x1007` locator CID. Deterministic in `(source, mbid)`, so
    /// any peer holding the MBID derives the same address offline.
    pub fn cid(&self) -> Option<String> {
        compute_card_cid(self.source.as_str(), &self.mbid)
    }

    /// The release type a discography groups by, folded from MusicBrainz's
    /// primary + secondary types into one token.
    ///
    /// **Secondary types win over the primary.** MusicBrainz says a live album
    /// is `primary=Album, secondary=[Live]`, and grouping it under "Albums"
    /// buries an artist's studio records under decades of concert recordings.
    /// The same reasoning applies to compilations and soundtracks: what the
    /// release *is* to a listener is the secondary type whenever there is one.
    pub fn release_type(&self) -> Option<&'static str> {
        for s in &self.secondary_types {
            match s.trim().to_ascii_lowercase().as_str() {
                "compilation" => return Some("Compilation"),
                "live" => return Some("Live"),
                "soundtrack" => return Some("Soundtrack"),
                "remix" => return Some("Remix"),
                "dj-mix" | "dj mix" => return Some("DJMix"),
                _ => {}
            }
        }
        match self.primary_type.as_deref()?.trim().to_ascii_lowercase().as_str() {
            "album" => Some("Album"),
            "single" => Some("Single"),
            "ep" => Some("EP"),
            "broadcast" => Some("Broadcast"),
            "other" => Some("Other"),
            _ => None,
        }
    }

    /// Would this card actually render? Exactly [`Card::to_record`]'s emit
    /// condition, hoisted so a caller can predict the drop *before* projecting.
    ///
    /// ⚠ **This cannot predict the cover.** Unlike TMDB — whose response says
    /// outright whether a cover exists — nothing in a MusicBrainz response
    /// reveals whether the Cover Art Archive holds an image, and the fetch that
    /// finds out happens in the **gateway core**, long after this feeder has
    /// returned. So a card that passes here can still be hidden downstream. See
    /// `consts::DISCOVERY_OVERFETCH` for the mitigation and why it is only a
    /// mitigation.
    pub fn is_displayable(&self) -> bool {
        !self.title.trim().is_empty() && self.cover.is_some()
    }

    /// Project to the wire record.
    ///
    /// Returns `None` when the card has no title or no cover source — it would
    /// fail the consumer's quality gate and render as a hole in the grid, so
    /// the feeder declines to emit it at all rather than shipping an
    /// unrenderable card.
    ///
    /// `query_filters` echoes back every structured filter the query carried,
    /// which is **load-bearing**: both the gateway dispatcher and meta-search
    /// re-apply `record_matches` to each record on the way out, and a *missing*
    /// field fails the match — so an un-echoed `popular` means every card in
    /// the row is dropped at one of the two tiers, and the row renders empty
    /// with no error anywhere.
    pub fn to_record(&self, query_filters: &BTreeMap<String, Vec<String>>) -> Option<DiscoveryRecord> {
        let title = self.title.trim();
        if title.is_empty() {
            return None;
        }
        let cover = self.cover.as_ref()?;

        let mut fields: BTreeMap<String, String> = BTreeMap::new();

        // Type axes. `card` on the fileType axis and the work kind on the
        // contentKind axis stay orthogonal, so `fileType:card contentKind:album`
        // is a well-formed query and routing needs no special case.
        fields.insert("fileType".to_string(), "card".to_string());
        fields.insert("contentKind".to_string(), self.kind.content_kind().to_string());
        // Third axis (METADATA_KEYS.md §1): MusicBrainz is the identity graph
        // that *defines* the music domain, so both card kinds land there.
        // Literal rather than via `meta_feeder_sdk::domain` — this feeder pins
        // the SDK by git tag.
        fields.insert("domain".to_string(), "music".to_string());
        fields.insert("title".to_string(), title.to_string());

        // The id bag. Which field carries the card's own MBID depends on what
        // the card *is* — an album is addressed by its release-group, an artist
        // by its artist id.
        match self.kind {
            CardKind::Album => {
                fields.insert("mbReleaseGroupId".to_string(), self.mbid.clone());
            }
            CardKind::Artist => {
                fields.insert("mbArtistId".to_string(), self.mbid.clone());
            }
        }
        if let Some(a) = self.artist_mbid.as_deref().filter(|s| !s.is_empty()) {
            fields.insert("mbArtistId".to_string(), a.to_string());
        }

        if let Some(a) = self.album_artist.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            fields.insert("albumArtist".to_string(), a.to_string());
        }
        for a in &self.artists {
            let a = a.trim();
            if !a.is_empty() {
                fields.insert(format!("artists/{a}"), "true".to_string());
            }
        }

        if let Some(d) = self.release_date.as_deref().and_then(normalise_date) {
            fields.insert("releasedate".to_string(), d);
        }
        if let Some(n) = self.track_count.filter(|n| *n > 0 && *n <= MAX_TRACK_COUNT) {
            fields.insert("trackCount".to_string(), n.to_string());
        }
        if let Some(n) = self.disc_count.filter(|n| *n > 0) {
            fields.insert("discCount".to_string(), n.to_string());
        }

        // The release *type*, which is what a discography groups by. Written on
        // album cards only.
        //
        // ⚠ Distinct from `contentKind`, which stays `album` for every one of
        // these. A single and a live album are both albums as far as the type
        // axis is concerned — folding "is it a single?" into `contentKind`
        // would make it a routing decision, and meta-search would then have to
        // know the music vocabulary to fan a query out.
        if self.kind == CardKind::Album {
            if let Some(t) = self.release_type() {
                fields.insert("releaseType".to_string(), t.to_string());
            }
        }
        if let Some(l) = self.label.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            fields.insert("label".to_string(), l.to_string());
        }
        if let Some(c) = self
            .catalog_number
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            fields.insert("catalogNumber".to_string(), c.to_string());
        }

        // MusicBrainz has no synopsis; `disambiguation` is the nearest thing.
        // Written under the namespaced key meta-search indexes, not a flat
        // `description` (METADATA_KEYS §14.6).
        if let Some(d) = self
            .disambiguation
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            fields.insert("description/eng".to_string(), d.to_string());
        }

        // Genres as a KEY-SET (`genres/<Name> = "true"`), never the legacy
        // comma-joined value. METADATA_KEYS §14.12 forbids new csv-set writers,
        // and the reason bites hardest here: a card is the one record type
        // multiple peers derive independently at the same CID, so two peers
        // resolving different genre sets must union by key-merge rather than
        // diverge into two strings that need diffing to reconcile.
        for g in &self.genres {
            let g = g.trim();
            if !g.is_empty() {
                fields.insert(format!("genres/{g}"), "true".to_string());
            }
        }

        // The canonical track list, as an INDEXED-LIST (`tracks/{n}/<field>` —
        // `METADATA_KEYS.md` "Value formats"). The integer index carries
        // position, which is what a set encoding could not express: a track
        // list is ordered, and two tracks on the same album can share a title
        // (a reprise, a hidden track), so key-set members would collide.
        //
        // Present only on a card resolved through the DETAIL path. A browse
        // card carries none, because filling one would cost an extra
        // MusicBrainz request per row entry against a 1 req/s budget — twenty
        // seconds for a twenty-card row, past every consumer-side idle cutoff.
        // Growth is append-only: the detail resolution adds fields to the same
        // CID, it never edits one.
        for (i, t) in self.tracks.iter().enumerate() {
            fields.insert(format!("tracks/{i}/title"), t.title.clone());
            fields.insert(format!("tracks/{i}/position"), t.position.to_string());
            fields.insert(format!("tracks/{i}/disc"), t.disc.to_string());
            if let Some(ms) = t.length_ms {
                fields.insert(format!("tracks/{i}/length"), ms.to_string());
            }
            if let Some(id) = &t.mb_recording_id {
                fields.insert(format!("tracks/{i}/mbRecordingId"), id.clone());
            }
            if let Some(isrc) = &t.isrc {
                fields.insert(format!("tracks/{i}/isrc"), isrc.clone());
            }
        }

        // The artwork, as a `url` LOCATOR cid — never a raw `*_url` field.
        //
        // METADATA_KEYS rule #5: *"it does not add a URL field — it emits a
        // `url` locator CID"*. The feeder invariant against fetching bytes is
        // no obstacle, because the locator is a pure function of the URL
        // string: no request, no hash, no blockstore. The gateway core upgrades
        // it to a real content cid when it seeds the bytes, and meta-share
        // resolves it lazily if the gateway never got there.
        //
        // **Music vocabulary, not video's.** An album card writes `cover`, an
        // artist card `photo`. The images come from the Cover Art Archive and a
        // portrait is not a sleeve, so neither is a `poster`.
        //
        // ⚠ A 404 stays silent from here: the gateway deletes the field, and
        // the consumer's gate then hides the card. That is the correct outcome
        // (a coverless album card is a hole in a grid), but it means "row came
        // back short" and "artwork missing" look identical downstream.
        if let Some(locator) = artwork_locator(&cover.url()) {
            fields.insert(self.kind.artwork_field().to_string(), locator);
        }

        // The bare-CID key-set member.
        //
        // **Load-bearing, and not merely cosmetic.** The gateway's search path
        // persists a hit to meta-core only if it can find a CID on it — no key
        // means the record is silently unpersisted. A card is the one record
        // type whose CID is *derived* rather than discovered, and
        // `compute_outcomes` (the other place it is derived) is not on the
        // search path. Without this line a card is never persisted at all,
        // which interacts badly with the gateway's search-coverage gate: it
        // marks `(upstream, query)` covered whenever the feeder produced
        // records and thereafter serves meta-core for an hour — coverage
        // marked plus nothing persisted means **an identical search returns
        // empty for the rest of the window**.
        if let Some(cid) = self.cid() {
            fields.insert(format!("cids/{cid}"), "true".to_string());
        }

        // Default provenance. The SDK copy this crate vendors (meta-gateway's)
        // has no `stamp_default_source` helper — that lives only in the indexer
        // feeder's copy — so it is stamped explicitly here rather than
        // diverging the vendored SDK. See `cache.rs` for why the copy is kept
        // byte-identical.
        fields.insert(
            format!("source/gateway:{}", self.source.as_str()),
            "true".to_string(),
        );

        // ⚠ **`externalPlay/`, deliberately NOT `cids/`.**
        //
        // An `ext-play` locator ranks tier 5 — the same tier as the `card`
        // locator that *is* this record's address. Putting one in the cid
        // key-set would enter it into the canonical election as a peer of the
        // card locator, and a tie there resolves on the lexicographically
        // smaller cid: the work's own address could flip to a Spotify link,
        // silently breaking every `work_key` route pointing at it.
        //
        // A separate key-set keeps them cid-shaped (rule #5 — never a raw
        // `*_url`, which only *looks* transient) without letting them address
        // the work. They are link affordances, never versions, and never
        // queueable: an external page cannot report `ended`, so a queued one
        // would stop playback silently.
        for url in &self.streaming_links {
            if let Some(cid) = meta_feeder_sdk::hash::compute_ext_play_cid(url) {
                fields.insert(format!("externalPlay/{cid}"), "true".to_string());
            }
        }

        // The filter echo (see the fn doc). `genres` and `languages` are
        // excluded for the same reason the card feeder excludes them: both are
        // key-sets, so the flat field this would write is not the shape a
        // reader looks for, and the filter *value* is a slug
        // (`hip-hop`) rather than a genre name — echoing it would persist a
        // slug masquerading as a genre.
        for (key, allowed) in query_filters {
            if key == "languages" || key == "genres" || fields.contains_key(key) || allowed.is_empty()
            {
                continue;
            }
            fields.insert(key.clone(), allowed.join(","));
        }

        Some(DiscoveryRecord {
            upstream_id: self.source.as_str().to_string(),
            record_id: self.record_id(),
            fields,
        })
    }

    /// Fold a resolved canonical release into an album card: track/disc
    /// counts, label and catalogue number. The detail-path enrichment a browse
    /// row deliberately skips.
    pub fn with_release_detail(mut self, release: &Release) -> Self {
        let tracks: u32 = release
            .media
            .iter()
            .map(|m| {
                if m.track_count > 0 {
                    m.track_count
                } else {
                    m.tracks.len() as u32
                }
            })
            .sum();
        if tracks > 0 {
            self.track_count = Some(tracks.min(MAX_TRACK_COUNT));
        }
        if !release.media.is_empty() {
            self.disc_count = Some(release.media.len() as u32);
        }
        if let Some(li) = release.label_info.first() {
            self.label = li.label.as_ref().map(|l| l.name.clone()).filter(|s| !s.is_empty());
            self.catalog_number = li.catalog_number.clone().filter(|s| !s.is_empty());
        }
        self.tracks = release
            .media
            .iter()
            .flat_map(|m| {
                // A medium's `position` is 1-based, but MusicBrainz omits it on
                // single-disc releases — default to disc 1 rather than 0, or
                // every track on a one-disc album reports disc 0 and a consumer
                // sorting by (disc, track) puts them before disc 1.
                let disc = if m.position == 0 { 1 } else { m.position };
                m.tracks.iter().map(move |t| {
                    let rec = t.recording.as_ref();
                    CanonicalTrack {
                        // Same reasoning: `position` is the reliable ordinal
                        // (`number` is a printed string — vinyl uses `A1`).
                        position: t.position,
                        disc,
                        title: if t.title.trim().is_empty() {
                            rec.map(|r| r.title.clone()).unwrap_or_default()
                        } else {
                            t.title.clone()
                        },
                        length_ms: t.length.or_else(|| rec.and_then(|r| r.length)),
                        mb_recording_id: rec.map(|r| r.id.clone()).filter(|s| !s.is_empty()),
                        isrc: rec.and_then(|r| r.isrcs.first().cloned()),
                    }
                })
            })
            .filter(|t| !t.title.trim().is_empty())
            .collect();
        self
    }
}

/// Split a card `record_id` (`"musicbrainz:<mbid>"`) back into
/// `(source, mbid)` — the two halves of the CID preimage.
///
/// ⚠ `split_once`, not `rsplit_once`: an MBID contains hyphens but never a
/// colon, so the *first* colon is the source boundary.
pub fn split_record_id(record_id: &str) -> Option<(&str, &str)> {
    record_id
        .split_once(':')
        .filter(|(s, id)| !s.is_empty() && !id.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RG: &str = "f5093c06-23e3-404f-aeaa-40f72885ee3a";
    const AR: &str = "561d854a-6a28-4aa7-8c99-323e6ce46c2a";

    fn album() -> Card {
        Card {
            source: CardSource::MusicBrainz,
            kind: CardKind::Album,
            mbid: RG.to_string(),
            title: "Kind of Blue".to_string(),
            album_artist: Some("Miles Davis".to_string()),
            artists: vec!["Miles Davis".to_string()],
            artist_mbid: Some(AR.to_string()),
            cover: Some(Cover::ReleaseGroupFront {
                release_group_mbid: RG.to_string(),
            }),
            disambiguation: None,
            genres: vec!["Jazz".to_string(), "Modal Jazz".to_string()],
            release_date: Some("1959-08-17".to_string()),
            track_count: None,
            disc_count: None,
            label: None,
            catalog_number: None,
            primary_type: Some("Album".to_string()),
            secondary_types: vec![],
            tracks: Vec::new(),
            streaming_links: Vec::new(),
        }
    }

    #[test]
    fn album_projects_the_expected_field_set() {
        let r = album().to_record(&BTreeMap::new()).expect("record");
        assert_eq!(r.upstream_id, "musicbrainz");
        assert_eq!(r.record_id, format!("musicbrainz:{RG}"));
        assert_eq!(r.fields.get("fileType").map(String::as_str), Some("card"));
        assert_eq!(r.fields.get("contentKind").map(String::as_str), Some("album"));
        assert_eq!(r.fields.get("title").map(String::as_str), Some("Kind of Blue"));
        assert_eq!(r.fields.get("mbReleaseGroupId").map(String::as_str), Some(RG));
        assert_eq!(r.fields.get("mbArtistId").map(String::as_str), Some(AR));
        assert_eq!(
            r.fields.get("albumArtist").map(String::as_str),
            Some("Miles Davis")
        );
        assert_eq!(
            r.fields.get("releasedate").map(String::as_str),
            Some("1959-08-17")
        );
        // Genres are a key-set, never a comma-joined value.
        assert_eq!(r.fields.get("genres/Jazz").map(String::as_str), Some("true"));
        assert!(r.fields.get("genres").is_none());
        // Provenance is always stamped.
        assert_eq!(
            r.fields.get("source/gateway:musicbrainz").map(String::as_str),
            Some("true")
        );
    }

    /// The `cids/<locator>` member is what makes the card persistable at all —
    /// without it the gateway silently drops it and the search-coverage gate
    /// then serves an empty result for an hour.
    #[test]
    fn album_stamps_its_own_locator_as_a_cid_member() {
        let c = album();
        let cid = c.cid().expect("locator");
        let r = c.to_record(&BTreeMap::new()).expect("record");
        assert_eq!(r.fields.get(&format!("cids/{cid}")).map(String::as_str), Some("true"));
    }

    /// ⚠ The no-kind-prefix rule. If this ever fails, every music card address
    /// on the network has been re-minted and nothing resolves to what it used
    /// to.
    #[test]
    fn the_locator_preimage_is_the_bare_mbid() {
        let c = album();
        assert_eq!(c.record_id(), format!("musicbrainz:{RG}"));
        assert_eq!(
            c.cid(),
            compute_card_cid("musicbrainz", RG),
            "the card CID must be minted from the BARE mbid — no album:/artist: prefix"
        );
        // And an artist card with the same id space does not collide, because
        // MBIDs are globally unique across entity types.
        assert_ne!(compute_card_cid("musicbrainz", RG), compute_card_cid("musicbrainz", AR));
    }

    /// The filter echo — an un-echoed filter means the record is dropped by
    /// `record_matches` at the gateway or at meta-search, and the row renders
    /// empty with no error.
    #[test]
    fn structured_filters_are_echoed_back_except_the_key_sets() {
        let mut filters = BTreeMap::new();
        filters.insert("popular".to_string(), vec!["true".to_string()]);
        filters.insert("genres".to_string(), vec!["jazz".to_string()]);
        filters.insert("languages".to_string(), vec!["eng".to_string()]);
        let r = album().to_record(&filters).expect("record");
        assert_eq!(r.fields.get("popular").map(String::as_str), Some("true"));
        // The slug must NOT be persisted as if it were a genre name.
        assert_eq!(r.fields.get("genres").map(String::as_str), None);
        assert_eq!(r.fields.get("languages").map(String::as_str), None);
        assert_eq!(r.fields.get("genres/Jazz").map(String::as_str), Some("true"));
    }

    /// The echo must never overwrite a field the card itself resolved.
    #[test]
    fn the_echo_does_not_clobber_a_resolved_field() {
        let mut filters = BTreeMap::new();
        filters.insert("contentKind".to_string(), vec!["album".to_string()]);
        filters.insert("title".to_string(), vec!["something else".to_string()]);
        let r = album().to_record(&filters).expect("record");
        assert_eq!(r.fields.get("title").map(String::as_str), Some("Kind of Blue"));
        assert_eq!(r.fields.get("contentKind").map(String::as_str), Some("album"));
    }

    #[test]
    fn a_coverless_or_titleless_card_is_not_emitted() {
        let mut c = album();
        c.cover = None;
        assert!(c.to_record(&BTreeMap::new()).is_none());

        let mut c = album();
        c.title = "   ".to_string();
        assert!(c.to_record(&BTreeMap::new()).is_none());
    }

    #[test]
    fn artist_cards_are_addressed_by_their_artist_mbid() {
        let c = Card {
            kind: CardKind::Artist,
            mbid: AR.to_string(),
            title: "Miles Davis".to_string(),
            album_artist: None,
            artist_mbid: None,
            ..album()
        };
        let r = c.to_record(&BTreeMap::new()).expect("record");
        assert_eq!(r.fields.get("contentKind").map(String::as_str), Some("artist"));
        assert_eq!(r.fields.get("mbArtistId").map(String::as_str), Some(AR));
        assert!(r.fields.get("mbReleaseGroupId").is_none());
        assert!(r.fields.get("albumArtist").is_none());
    }

    #[test]
    fn a_resolved_cover_beats_the_front_endpoint() {
        let front = Cover::ReleaseGroupFront {
            release_group_mbid: RG.to_string(),
        };
        assert!(front.url().ends_with(&format!("/release-group/{RG}/front-500")));

        let resolved = Cover::Resolved {
            release_mbid: "rel-1".to_string(),
            caa_id: 42,
        };
        assert!(resolved.url().ends_with("/release/rel-1/42-500.jpg"));
    }

    #[test]
    fn release_detail_folds_in_counts_label_and_catalogue() {
        use crate::client::{Label, LabelInfo, Medium, Release, Track};
        let release = Release {
            id: "rel".into(),
            title: "Kind of Blue".into(),
            media: vec![
                Medium {
                    position: 1,
                    track_count: 5,
                    tracks: vec![Track::default(); 5],
                    ..Default::default()
                },
                Medium {
                    position: 2,
                    track_count: 3,
                    tracks: vec![Track::default(); 3],
                    ..Default::default()
                },
            ],
            label_info: vec![LabelInfo {
                catalog_number: Some("CL 1355".into()),
                label: Some(Label {
                    name: "Columbia".into(),
                }),
            }],
            ..Default::default()
        };
        let c = album().with_release_detail(&release);
        assert_eq!(c.track_count, Some(8));
        assert_eq!(c.disc_count, Some(2));
        assert_eq!(c.label.as_deref(), Some("Columbia"));
        assert_eq!(c.catalog_number.as_deref(), Some("CL 1355"));
    }

    /// The medium's **declared** `track-count` wins over how many tracks were
    /// inlined. The album genuinely has that many; a truncated response should
    /// not shorten it. This is what lets a consumer say "12 tracks, 9 playable"
    /// truthfully rather than quietly claiming the album is 9 tracks long.
    #[test]
    fn a_declared_track_count_beats_the_inlined_track_list() {
        use crate::client::{Medium, Release, Track};
        let release = Release {
            media: vec![Medium {
                position: 1,
                track_count: 12,
                tracks: vec![Track::default(); 2],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(album().with_release_detail(&release).track_count, Some(12));
    }

    /// The canonical track list is what lets a consumer say "12 tracks, 9
    /// playable" and *name* the three it cannot get. It is an indexed-list
    /// because a track list is ordered and two tracks on one album can share a
    /// title (a reprise, a hidden track) — key-set members would collide.
    #[test]
    fn the_canonical_track_list_rides_out_as_an_indexed_list() {
        use crate::client::{Medium, Recording, Release, Track};
        let release = Release {
            media: vec![
                Medium {
                    position: 1,
                    track_count: 2,
                    tracks: vec![
                        Track {
                            position: 1,
                            title: "So What".into(),
                            length: Some(545_000),
                            recording: Some(Recording {
                                id: "rec-1".into(),
                                isrcs: vec!["USSM15900001".into()],
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        Track {
                            position: 2,
                            title: "Freddie Freeloader".into(),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
                Medium {
                    position: 2,
                    track_count: 1,
                    tracks: vec![Track {
                        position: 1,
                        title: "Blue in Green".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let c = album().with_release_detail(&release);
        assert_eq!(c.tracks.len(), 3);
        let r = c.to_record(&BTreeMap::new()).expect("record");
        assert_eq!(r.fields["tracks/0/title"], "So What");
        assert_eq!(r.fields["tracks/0/position"], "1");
        assert_eq!(r.fields["tracks/0/disc"], "1");
        assert_eq!(r.fields["tracks/0/length"], "545000");
        assert_eq!(r.fields["tracks/0/isrc"], "USSM15900001");
        assert_eq!(r.fields["tracks/0/mbRecordingId"], "rec-1");
        // A track with no timing simply omits the field rather than claiming 0.
        assert!(!r.fields.contains_key("tracks/1/length"));
        // Disc 2's track keeps its own disc index, and its position restarts —
        // which is why (disc, position) is the sort key, not position alone.
        assert_eq!(r.fields["tracks/2/disc"], "2");
        assert_eq!(r.fields["tracks/2/position"], "1");
    }

    /// ⚠ A single-disc release omits the medium position, and defaulting it to
    /// 0 puts every track *before* disc 1 in any (disc, position) sort.
    #[test]
    fn a_medium_with_no_position_is_disc_one() {
        use crate::client::{Medium, Release, Track};
        let release = Release {
            media: vec![Medium {
                position: 0,
                track_count: 1,
                tracks: vec![Track {
                    position: 1,
                    title: "Only Track".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let c = album().with_release_detail(&release);
        assert_eq!(c.tracks[0].disc, 1);
    }

    /// ⚠ A live album is `primary=Album, secondary=[Live]`. Grouping it under
    /// "Albums" buries an artist's studio records under decades of concert
    /// recordings, so the secondary type wins.
    #[test]
    fn a_secondary_release_type_outranks_the_primary() {
        let live = Card {
            primary_type: Some("Album".into()),
            secondary_types: vec!["Live".into()],
            ..album()
        };
        assert_eq!(live.release_type(), Some("Live"));

        let comp = Card {
            primary_type: Some("Album".into()),
            secondary_types: vec!["Compilation".into()],
            ..album()
        };
        assert_eq!(comp.release_type(), Some("Compilation"));

        // A plain studio album keeps its primary type.
        assert_eq!(album().release_type(), Some("Album"));

        let single = Card {
            primary_type: Some("Single".into()),
            secondary_types: vec![],
            ..album()
        };
        assert_eq!(single.release_type(), Some("Single"));
    }

    /// `releaseType` must not leak onto an artist card, and must never be
    /// confused with `contentKind` — which stays `album` for every one of them.
    #[test]
    fn release_type_is_an_album_only_field_beside_content_kind() {
        let r = Card {
            primary_type: Some("Single".into()),
            ..album()
        }
        .to_record(&BTreeMap::new())
        .expect("record");
        assert_eq!(r.fields["releaseType"], "Single");
        assert_eq!(r.fields["contentKind"], "album", "the routing axis is unchanged");

        let artist = Card {
            kind: CardKind::Artist,
            mbid: AR.to_string(),
            primary_type: Some("Group".into()),
            ..album()
        }
        .to_record(&BTreeMap::new())
        .expect("record");
        assert!(!artist.fields.contains_key("releaseType"));
    }

    /// A browse card carries no track list — filling one would cost an extra
    /// MusicBrainz request per row entry against a 1 req/s budget.
    #[test]
    fn a_browse_card_emits_no_track_fields() {
        let r = album().to_record(&BTreeMap::new()).expect("record");
        assert!(
            !r.fields.keys().any(|k| k.starts_with("tracks/")),
            "a browse card must stay cheap"
        );
    }

    /// …but a medium that declares nothing falls back to what it did inline,
    /// rather than reporting zero tracks.
    #[test]
    fn an_undeclared_track_count_falls_back_to_the_inlined_list() {
        use crate::client::{Medium, Release, Track};
        let release = Release {
            media: vec![Medium {
                position: 1,
                track_count: 0,
                tracks: vec![Track::default(); 3],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(album().with_release_detail(&release).track_count, Some(3));
    }

    /// An MBID is full of hyphens but never a colon — the source boundary is
    /// the FIRST colon, and splitting from the right would shred the id.
    #[test]
    fn record_ids_split_on_the_first_colon() {
        assert_eq!(
            split_record_id(&format!("musicbrainz:{RG}")),
            Some(("musicbrainz", RG))
        );
        assert!(split_record_id("nocolon").is_none());
        assert!(split_record_id(":empty-source").is_none());
        assert!(split_record_id("empty-id:").is_none());
    }
}

#[cfg(test)]
mod ext_play_tests {
    use super::*;

    fn artist_card_with_links(links: &[&str]) -> Card {
        Card {
            source: CardSource::MusicBrainz,
            kind: CardKind::Artist,
            mbid: "056e4f3e-d505-4dad-8ec1-d04f521cbb56".into(),
            title: "Daft Punk".into(),
            album_artist: None,
            artists: vec!["Daft Punk".into()],
            artist_mbid: None,
            cover: Some(Cover::ReleaseGroupFront { release_group_mbid: "rg".into() }),
            disambiguation: None,
            genres: vec![],
            release_date: None,
            track_count: None,
            disc_count: None,
            label: None,
            catalog_number: None,
            primary_type: None,
            secondary_types: vec![],
            tracks: Vec::new(),
            streaming_links: links.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// **The hazard this key-set exists to avoid.**
    ///
    /// An `ext-play` locator ranks tier 5, the same tier as the `card` locator
    /// that addresses the work. In `cids/*` it would be a peer in the canonical
    /// election, and a tie resolves on the lexicographically smaller cid — so
    /// the artist's own address could flip to a Spotify link and break every
    /// route pointing at it.
    #[test]
    fn streaming_links_never_enter_the_cid_keyset() {
        let rec = artist_card_with_links(&["https://open.spotify.com/artist/4tZwfgrHOc3mvqYlEYSvVi"])
            .to_record(&Default::default())
            .expect("record");
        let ext = meta_feeder_sdk::hash::compute_ext_play_cid(
            "https://open.spotify.com/artist/4tZwfgrHOc3mvqYlEYSvVi",
        )
        .expect("mints");
        assert!(
            rec.fields.contains_key(&format!("externalPlay/{ext}")),
            "the link is emitted"
        );
        // The record legitimately carries its OWN card locator under `cids/` —
        // that is its address. What must never appear there is the ext-play
        // locator, which would make a Spotify link a candidate for the
        // canonical election alongside it.
        assert!(
            !rec.fields.contains_key(&format!("cids/{ext}")),
            "an ext-play locator must never be a cid the work can be addressed by"
        );
        assert_eq!(
            rec.fields.keys().filter(|k| k.starts_with("cids/")).count(),
            1,
            "exactly one cid: the card locator"
        );
    }

    #[test]
    fn a_card_with_no_links_emits_no_key_set() {
        let rec = artist_card_with_links(&[]).to_record(&Default::default()).expect("record");
        assert!(!rec.fields.keys().any(|k| k.starts_with("externalPlay/")));
    }

    /// Non-http payloads must never mint a locator: the client hands this
    /// string to an anchor or a new tab.
    #[test]
    fn a_non_http_link_is_dropped() {
        let rec = artist_card_with_links(&["javascript:alert(1)"])
            .to_record(&Default::default())
            .expect("record");
        assert!(!rec.fields.keys().any(|k| k.starts_with("externalPlay/")));
    }
}
