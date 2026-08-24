# meta-feeder-musicbrainz

MusicBrainz identity cards (album / artist), plus the shared MusicBrainz client used by sibling music feeders.

**Upstream:** MusicBrainz /ws/2 + Cover Art Archive + ListenBrainz
**Role:** identity

A MetaMesh gateway feeder — one upstream, one job. Implements `FeederPlugin`
from [`meta-feeder-sdk`](https://github.com/worph/meta-feeder-sdk).

```bash
cargo build --release --bin musicbrainz-feeder
cargo test
```

Scope and naming conventions: `docs/project-architecture/feeder-architecture.md`
in the meta-root.
