//! Constants for the music feeder.
//!
//! The music-domain counterpart of the card feeder's `consts.rs`. Where a knob
//! has the same job as one over there, it keeps the same name and the same
//! reasoning so the two feeders stay readable side by side.

/// User-Agent for outbound HTTP.
///
/// ⚠ **Not cosmetic for MusicBrainz.** Their API rejects requests with a
/// generic or absent User-Agent, and the rule is that it must identify the
/// application and carry contact information. A 403 here looks exactly like a
/// rate-limit and is easy to misdiagnose, so keep this descriptive.
pub const USER_AGENT: &str = concat!(
    "meta-music/",
    env!("CARGO_PKG_VERSION"),
    " ( https://github.com/worph/meta-feeder-music )"
);

// ---------------------------------------------------------------------------
// MusicBrainz
// ---------------------------------------------------------------------------

/// MusicBrainz web-service v2 base.
pub const MB_API_BASE: &str = "https://musicbrainz.org/ws/2";

/// Sustained MusicBrainz request rate.
///
/// **One per second is the service's published ceiling, not a suggestion** —
/// exceeding it earns a block, and the block is applied per source IP, so one
/// misbehaving feeder takes out every peer behind the same NAT. Unlike TMDB's
/// (removed) limit there is no headroom to trade for latency here.
pub const MB_RATE_PER_SEC: f64 = 1.0;

/// MusicBrainz burst ceiling. Deliberately tiny: the rate above is a hard
/// ceiling, so a burst only borrows against the next second.
pub const MB_BURST: f64 = 2.0;

/// Request timeout for a MusicBrainz call (seconds).
pub const MB_TIMEOUT_SECS: u64 = 20;

/// Default `Retry-After` applied when MusicBrainz returns 503 with no (or an
/// unparseable) header. Their throttle answers 503, not 429.
pub const MB_RATE_LIMIT_DEFAULT_RETRY_SECS: u64 = 2;

/// Budget-permit deadline for a query somebody is waiting on.
///
/// Far shorter than the card feeder's 3000 s TMDB equivalent, because the
/// budget here is 1/s rather than 20/s: a deep queue means the answer is
/// minutes away, and no search box should hold a connection open that long.
pub const MB_WAIT_DEADLINE_SECS: u64 = 30;

/// Budget-permit deadline for a **browse row**, deliberately far shorter than
/// [`MB_WAIT_DEADLINE_SECS`].
///
/// Same reasoning as the card feeder's `TMDB_DISCOVERY_WAIT_DEADLINE_SECS`:
/// nobody asked for *this* row in particular, it refreshes on a timer, and the
/// consumer keeps the previously-filled row on an empty pass (meta-listen's
/// sticky per-row merge). Waiting out a queue to eventually produce what a
/// fast bail produces now just pins a search stream open.
pub const MB_DISCOVERY_WAIT_DEADLINE_SECS: u64 = 5;

/// Results MusicBrainz returns per search page (its own default and maximum
/// useful page size for our purposes).
pub const MB_PAGE_SIZE: usize = 25;

/// Hard ceiling on MusicBrainz pages walked for one row, whatever the
/// requested card count. Bounds a misconfigured `card_discovery_n` to a
/// predictable per-row cost against a 1/s budget.
pub const MB_MAX_PAGES: u32 = 3;

// ---------------------------------------------------------------------------
// Cover Art Archive
// ---------------------------------------------------------------------------

/// Cover Art Archive base. Keyed by MusicBrainz entity id.
pub const CAA_BASE: &str = "https://coverartarchive.org";

/// Cover size segment. `front-500` is the grid sweet spot — roughly 60 KB,
/// against `front`'s frequently multi-MB original scan.
pub const CAA_FRONT_SIZE: &str = "front-500";

// ---------------------------------------------------------------------------
// ListenBrainz
// ---------------------------------------------------------------------------

/// ListenBrainz API base. No credential needed for the sitewide/browse
/// endpoints this feeder reads.
pub const LB_API_BASE: &str = "https://api.listenbrainz.org/1";

/// Sustained ListenBrainz request rate. Their limit is generous and
/// header-advertised; this is a courtesy ceiling, not a published one.
pub const LB_RATE_PER_SEC: f64 = 4.0;

/// ListenBrainz burst ceiling.
pub const LB_BURST: f64 = 8.0;

/// Request timeout for a ListenBrainz call (seconds).
pub const LB_TIMEOUT_SECS: u64 = 20;

/// Statistics range passed to the sitewide stats endpoints.
///
/// ⚠ The parameter is `range`. It was `stats_range` until a documented
/// breaking change; a request using the old name is answered with a 400 that
/// reads like a bad endpoint.
pub const LB_STATS_RANGE: &str = "month";

// ---------------------------------------------------------------------------
// Result sizing
// ---------------------------------------------------------------------------

/// Default number of cards a free-text search returns.
///
/// Matches the card feeder's `DEFAULT_CARD_TOP_N` and costs the same: at most
/// one cached MusicBrainz search. The retrieval budget is spent later, on the
/// single card the user clicks.
pub const DEFAULT_CARD_TOP_N: usize = 10;

/// Default number of cards a keyword-less browse row returns.
pub const DEFAULT_CARD_DISCOVERY_N: usize = 20;

/// Over-fetch factor applied when walking MusicBrainz pages for a browse row.
///
/// **Why a row asks for more works than it needs.** The quality gate hides any
/// album card with no cover art, and — unlike the card feeder, where TMDB's
/// own response says whether a cover exists — nothing in a MusicBrainz
/// response reveals whether the Cover Art Archive holds a front image for a
/// release-group. The only way to know is to fetch it, which the **gateway
/// core** does (it upgrades the `cover` locator to a seeded cid), long after
/// this feeder has returned.
///
/// So the drop is genuinely unpredictable here and the row would simply come
/// up short. Over-fetching trades a bounded number of extra rows-worth of
/// candidates for a full row. It is a mitigation, not a fix: a row can still
/// be short, and the consumer's sticky per-row merge is what makes that
/// acceptable.
pub const DISCOVERY_OVERFETCH: usize = 2;

/// Defensive ceiling on a release's reported track count (guards a corrupt or
/// absurd upstream value from sizing an allocation).
pub const MAX_TRACK_COUNT: u32 = 500;

// ---------------------------------------------------------------------------
// youtube — the delegated-playback tier
// ---------------------------------------------------------------------------
