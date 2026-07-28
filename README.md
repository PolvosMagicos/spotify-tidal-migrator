# spotify-tidal-migrator

A Rust CLI for migrating Spotify playlists and Liked Songs to TIDAL using only
the official Spotify and TIDAL APIs.

The migrator exports Spotify tracks, searches the public TIDAL catalog, scores
and classifies candidate matches, lets you resolve uncertain tracks, and can
create a new TIDAL playlist with the original Spotify playlist name. It never
downloads audio.

## Safety and current behavior

- No TIDAL playlist is created or modified unless `--apply` is present.
- A dry run is the default for the full `migrate` command.
- Existing TIDAL playlists are never overwritten or deleted.
- Exact matches are included by default.
- Probable matches require `--include-probable`.
- Review matches require `--include-review` and a confirmed selection.
- Missing matches can only be resolved by entering and confirming a TIDAL track
  ID during Review.
- Local Spotify tracks, failed searches, and unresolved version conflicts are
  skipped.
- Import batches are persisted and can be resumed safely.
- Tokens, client secrets, and Authorization headers are never printed.
- Tokens, caches, exports, and reports are stored under the Git-ignored `data/`
  directory.

The implementation uses `https://openapi.tidal.com/v2`; it does not use private
TIDAL endpoints, browser cookies, scraping, or unofficial TIDAL libraries.
See [the TIDAL import API notes](docs/tidal-import-api.md) for the documented
operations, schemas, scopes, batch limits, and verified API behavior.

## Requirements

- A Rust toolchain supporting Rust edition 2024
- A Spotify developer application
- A TIDAL third-party developer application
- A browser for the OAuth authorization steps

## Developer application setup

### Spotify

Add this redirect URI to the Spotify application:

```text
http://127.0.0.1:8989/callback/spotify
```

The CLI requests these Spotify scopes:

- `playlist-read-private`
- `playlist-read-collaborative`
- `user-library-read`

Spotify authentication uses Authorization Code Flow with PKCE and does not
require a Spotify client secret.

### TIDAL

Add this redirect URI to the TIDAL application:

```text
http://127.0.0.1:8989/callback/tidal
```

Enable `playlists.write` in the TIDAL developer dashboard to create playlists
and add tracks. Configure additional user scopes only when you need them; the
CLI verifies that every requested scope was actually granted.

TIDAL catalog matching uses a client-credentials token. Playlist import uses a
separate user token obtained through Authorization Code Flow with PKCE.

## Configuration

Create `.env` in the repository root:

```dotenv
SPOTIFY_CLIENT_ID=your_spotify_client_id
SPOTIFY_REDIRECT_URI=http://127.0.0.1:8989/callback/spotify

TIDAL_CLIENT_ID=your_tidal_client_id
TIDAL_CLIENT_SECRET=your_tidal_client_secret
TIDAL_REDIRECT_URI=http://127.0.0.1:8989/callback/tidal
TIDAL_COUNTRY_CODE=PE
TIDAL_SCOPES=playlists.write
```

`TIDAL_SCOPES` must be a non-empty, space-separated list using the exact scope
names enabled in the TIDAL dashboard. Do not add undocumented or internal scope
names.

Optional search settings:

```dotenv
# Sustained TIDAL searches per second. A CLI --rate-limit value takes priority.
TIDAL_SEARCH_RATE_LIMIT=4

# Used only when no rate limit is configured.
TIDAL_SEARCH_DELAY_MS=150

# Successful and empty-result cache lifetimes.
TIDAL_CACHE_TTL_SECS=2592000
TIDAL_NEGATIVE_CACHE_TTL_SECS=86400
```

## Authenticate

Authenticate Spotify:

```bash
cargo run -- auth spotify
```

Authenticate the TIDAL user:

```bash
cargo run -- auth tidal
```

Test TIDAL client-credentials authentication and catalog access:

```bash
cargo run -- tidal-test
```

The tokens are saved to:

- `data/spotify-token.json`
- `data/tidal-user-token.json`

## Recommended full workflow

Start with a dry run. This opens a multi-select menu containing Liked Songs and
the Spotify playlists available to the authenticated user:

```bash
cargo run -- migrate \
  --concurrency 12 \
  --rate-limit 4 \
  --fallback-searches \
  --include-review \
  --dry-run
```

For each selected source, the command:

1. Exports its Spotify tracks.
2. Searches and scores TIDAL candidates.
3. Reuses cached searches from earlier playlists and runs.
4. Opens Review for uncertain or missing matches when `--include-review` is
   present.
5. Writes an import plan without modifying TIDAL.
6. Prints all skipped songs and their reasons.

After reviewing the generated reports, run the same selection with explicit
mutation:

```bash
cargo run -- migrate \
  --concurrency 12 \
  --rate-limit 4 \
  --fallback-searches \
  --include-review \
  --apply
```

Add `--include-probable` only when you want matches classified as Probable to be
imported automatically:

```bash
cargo run -- migrate \
  --concurrency 12 \
  --rate-limit 4 \
  --fallback-searches \
  --include-probable \
  --include-review \
  --apply
```

Each applied source creates one new TIDAL playlist using the Spotify playlist
name. Running another non-resume apply can create another playlist; playlist
names are not used as unique identifiers.

## Staged workflow

The same process can be run one stage at a time.

### 1. Export Spotify

Export a playlist by URL, URI, or ID:

```bash
cargo run -- export-spotify \
  "https://open.spotify.com/playlist/<playlist-id>" \
  --concurrency 4
```

Export Liked Songs:

```bash
cargo run -- export-spotify-liked --concurrency 4
```

Or select multiple sources interactively and match them without importing:

```bash
cargo run -- select-spotify \
  --concurrency 12 \
  --rate-limit 4 \
  --fallback-searches
```

### 2. Match TIDAL

```bash
cargo run -- match-tidal \
  data/<spotify-export>.json \
  --concurrency 12 \
  --rate-limit 4 \
  --fallback-searches
```

Use `--limit 10` for a short test. Use `--refresh-cache` to ignore stored search
results and replace them with fresh catalog responses.

Matches are classified as:

- `Exact`: exact ISRC or a very strong metadata match without a version conflict
- `Probable`: strong title, artist, duration, and metadata agreement
- `Review`: ambiguous or lower-confidence candidates needing a decision
- `Missing`: no acceptable candidate

Live, acoustic, remix, demo, instrumental, karaoke, radio edit, remaster, and
clean/explicit differences are treated as meaningful version indicators.

### 3. Review uncertain and missing tracks

Discover review reports under `data/` and select which playlists to review:

```bash
cargo run -- review
```

You can also provide one or more review reports:

```bash
cargo run -- review data/<playlist>-tidal-review.json
```

For Review matches, choose a suggested candidate, enter a TIDAL track ID, or
skip the song. A Missing match offers only manual track ID, Skip, and Finish.
Manual IDs are resolved through the official TIDAL catalog and shown for
confirmation before they are accepted.

Press `Esc` to return to the previous song. The final confirmation must be
accepted before new decisions and shared cache choices are saved.

Confirmed choices are reused across playlists when the country code and Spotify
track ID match:

```text
data/tidal-review-choice-cache.json
```

### 4. Validate the import

```bash
cargo run -- import-tidal \
  data/<playlist>-tidal-matches.json \
  --include-review \
  --dry-run
```

The preflight report shows selected and skipped counts, the destination name,
the first and last selected tracks, and the calculated batches. It performs no
playlist mutation.

### 5. Apply the import

```bash
cargo run -- import-tidal \
  data/<playlist>-tidal-matches.json \
  --include-review \
  --apply
```

To include Probable matches:

```bash
cargo run -- import-tidal \
  data/<playlist>-tidal-matches.json \
  --include-probable \
  --include-review \
  --apply
```

To resume a compatible partial import, use the same selection flags:

```bash
cargo run -- import-tidal \
  data/<playlist>-tidal-matches.json \
  --include-probable \
  --include-review \
  --apply \
  --resume
```

Resume refuses a state file whose source or selected-track fingerprint differs.

## Caching

TIDAL catalog results are persisted in:

```text
data/tidal-search-cache.jsonl
```

The key is the country code plus the exact search query, not the playlist ID.
Therefore, a song searched for one playlist can show `[cache]` when encountered
in another playlist or a later process. Successful searches are cached for 30
days by default; empty results are cached for 24 hours.

The catalog cache and Review choice cache serve different purposes:

| Cache | Key | Purpose |
| --- | --- | --- |
| `tidal-search-cache.jsonl` | country + query | Reuse TIDAL catalog responses |
| `tidal-review-choice-cache.json` | country + Spotify track ID | Reuse confirmed human choices |

TIDAL rate limiting still applies to uncached searches. `--rate-limit 4` is a
conservative starting point; the client also honors HTTP 429 `Retry-After`,
backs off temporary failures, and adapts request spacing.

## Generated files

File names are derived from the Spotify source and stored under `data/`:

| File | Purpose |
| --- | --- |
| `<playlist>.json` | Spotify export |
| `<playlist>-tidal-matches.json` | Detailed matching report |
| `<playlist>-tidal-review.json` | Tracks available for Review |
| `<playlist>-tidal-review-decisions.json` | Confirmed per-run decisions |
| `<playlist>-tidal-import-state.json` | Incremental and resumable import state |
| `<playlist>-tidal-import-report.json` | Dry-run plan or final import report |
| `tidal-search-cache.jsonl` | Persistent catalog search cache |
| `tidal-review-choice-cache.json` | Cross-playlist confirmed choices |

These files may contain playlist metadata and should not be committed or shared
without review. Tokens are never written into match, review, state, or import
reports.

## Validation

Run the project checks:

```bash
cargo fmt --check
cargo check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Automated tests use synthetic fixtures and local mock servers. They do not
perform real TIDAL playlist mutations.

## Source layout

```text
src/
├── main.rs                 CLI, Spotify integration, and workflow orchestration
├── cache.rs                Persistent TIDAL search cache
├── model.rs                Export, match, review, and report models
├── tidal.rs                TIDAL catalog client and rate limiting
├── tidal_user.rs           TIDAL PKCE user client and playlist API operations
├── tidal_import.rs         Import planning, state, batching, and verification
└── matching/
    ├── mod.rs              Candidate scoring and classification
    └── normalize.rs        Title and artist normalization
```

Use `cargo run -- --help` or `cargo run -- <command> --help` for the complete
current CLI reference.
