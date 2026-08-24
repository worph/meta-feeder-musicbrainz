//! `musicbrainz-feeder` — the MusicBrainz identity feeder, and the shared MusicBrainz
//! client its sibling feeders depend on.
//!
//! This crate is **lib + bin**. The binary serves the `musicbrainz` plugin
//! (`fileType:card`, `contentKind:album|artist`); the library exports
//! [`client`] and [`consts`] so `meta-feeder-youtube` can anchor a track on a
//! MusicBrainz recording without vendoring a second client.
//!
//! Everything upstream-agnostic — the redb cache, the rate budget, the
//! `0x1006` url locator, `percent_encode`, `normalise_date`, `licence_from_url`
//! — lives in `meta-feeder-sdk`, not here.

pub mod card;
pub mod client;
pub mod consts;
pub mod discovery;
pub mod listenbrainz;
pub mod musicbrainz;
pub mod resolve;
