# TIDAL playlist import API

Discovery date: 2026-07-16

Last revalidated: 2026-07-28

Implementation status: the official create, add-items, and read-items operations
are implemented behind an explicit `import-tidal --apply` safety gate. Dry-run
is the default. A live third-party token successfully created a playlist. The
test playlist was then manually deleted in the TIDAL app, which explains the
subsequent 404 observed during read and add-items probes. The importer still
never reports completion unless ordered read-back verification succeeds.

## Primary official sources

- TIDAL Web API reference: <https://tidal-music.github.io/tidal-api-reference/>
- Published OpenAPI document:
  <https://tidal-music.github.io/tidal-api-reference/tidal-api-oas.json>
- TIDAL authorization guide:
  <https://developer.tidal.com/documentation/api-sdk/api-sdk-authorization>
- TIDAL dashboard/app settings guide:
  <https://developer.tidal.com/documentation/api-sdk/api-sdk-manage-apps>
- Official Android SDK generated documentation:
  <https://tidal-music.github.io/tidal-sdk-android/>
- Official iOS SDK generated documentation:
  <https://tidal-music.github.io/tidal-sdk-ios/>
- TIDAL's statement that only `openapi.tidal.com` is authorized for third-party
  use: <https://github.com/orgs/tidal-music/discussions/38>

The OpenAPI file retrieved during the latest revalidation reported version
`1.10.74` and SHA-256
`d738d1aa2949b28a873a9e239567d4aae238daed6f01e281754aab1aa0d43a83`.
The API is versioned independently and must be rechecked when its version
changes.

The published Android and iOS SDK module indexes expose authentication, player,
and event-related modules, but no playlist mutation client. The Web API OpenAPI
document is therefore the operation and schema source used here.

## Official operations and schemas

All operations use `https://openapi.tidal.com/v2`, bearer authentication, and
the JSON:API media type `application/vnd.api+json`.

### Create a playlist

- Method and endpoint: `POST /playlists`
- Access tier shown on operation: `THIRD_PARTY`
- Published OAuth security array: `playlists.write` and `w_usr`
- Query parameter: optional `countryCode`
- Optional request header: `Idempotency-Key` (maximum 128 characters)
- Success: HTTP 201, `Playlists_Single_Resource_Data_Document`
- Implemented access type: `UNLISTED`
- Request:

```json
{
  "data": {
    "type": "playlists",
    "attributes": {
      "name": "Playlist name",
      "description": "Optional description",
      "accessType": "UNLISTED"
    }
  }
}
```

Only `name` is required. The documented access types are `PUBLIC` and
`UNLISTED`; there is no documented `PRIVATE` value. `createdAt` appears in the
schema but is server-owned and is not sent. The schema publishes no maximum
name or description length, so the importer does not invent one.

### Add playlist items

- Method and endpoint: `POST /playlists/{id}/relationships/items`
- Access tier shown on operation: `THIRD_PARTY`
- Published OAuth security array: `playlists.write` and `w_usr`
- Query parameter: optional `countryCode`
- Optional request header: `Idempotency-Key`
- Success: HTTP 200, `Playlists_Items_Multi_Relationship_Data_Document`
- Batch size: 1 to 50 resource identifiers
- Track request:

```json
{
  "data": [
    { "type": "tracks", "id": "opaque-tidal-track-id" }
  ]
}
```

The payload can contain tracks and videos; this importer sends only tracks.
Per-item `meta.addedAt` is optional. Top-level `meta.positionBefore` is
optional, but `positionBefore` is required whenever `meta` is supplied. The
importer omits `meta`, submits batches sequentially, and verifies the final
`itemIndex` order. A mismatch is a failed import, never a success.

### Read and verify playlist items

- Method and endpoint: `GET /playlists/{id}/relationships/items`
- Supports client credentials or authorization-code PKCE without an additional
  operation scope in the published security object
- Success: HTTP 200, a resource-identifier array
- Pagination: follow the opaque `links.next` URL until absent
- Ordering used by the importer: `sort=itemIndex`

Completion requires the exact expected item count, track-only resource types,
ordered TIDAL identifiers, and duplicate positions. This makes unsupported
append or duplicate behavior visible instead of silently corrupting order.

### Read the authenticated user and owned playlists

- `GET /users/me` is documented through `GET /users/{id}` with `id=me`, but
  publishes `user.read` plus internal `r_usr`.
- `GET /playlists?filter[owners.id]=me` publishes `playlists.read` plus internal
  `r_usr`.

Neither operation is required to create a new playlist because the create
response returns its destination identifier.

## OAuth behavior

The OpenAPI security scheme publishes:

| Scope | Published required tier |
| --- | --- |
| `playlists.read` | `THIRD_PARTY` |
| `playlists.write` | `THIRD_PARTY` |
| `user.read` | `THIRD_PARTY` |
| `r_usr` | `INTERNAL` |
| `w_usr` | `INTERNAL` |

The dashboard exposes `playlists.write` to the current third-party app but not
`w_usr`. Despite the published security array, a live token whose returned
scope list contained `playlists.write` and not `w_usr` received HTTP 201 from
`POST /playlists` on 2026-07-28. This proves creation was accepted for that
request, but it does not explain the OpenAPI discrepancy.

The test playlist ID later returned `NOT_FOUND` from:

- `GET /playlists/{id}`
- `POST /playlists/{id}/relationships/items`

The user confirmed that the playlist had been deleted manually in the TIDAL app
before those probes. The 404 therefore does not indicate an ownership or scope
failure. These observations are sanitized and intentionally do not include
tokens, request Authorization headers, response dumps, or user playlist data.

`TIDAL_SCOPES` remains user-configured. The importer requires the granted token
to contain `playlists.write`; it does not hardcode or request internal `w_usr`.

### Refresh-token discrepancy

TIDAL's authorization guide shows a refresh request containing only
`grant_type=refresh_token` and `refresh_token`. The live token endpoint returned
HTTP 400 with `Missing parameters: client_id` for that request. Supplying the
existing public `client_id` succeeded. The Rust client therefore includes
`client_id` in refresh requests. It never sends the client secret in a browser
URL or refresh request.

## Reliability and safety

- No mutation occurs unless `--apply` is present.
- Exact matches are selected by default.
- Probable matches require `--include-probable`.
- Review matches require both `--include-review` and a persisted manual
  `Selected` review decision.
- Missing, local, failed-search, and unresolved conflict results are skipped.
- Creation and every batch use stable idempotency keys.
- Import state is atomically replaced before and after every batch.
- A pending batch is reconciled against the remote ordered prefix on resume.
- HTTP 429 honors `Retry-After` when present and adds jitter.
- Temporary network failures, 500, and 503 use bounded exponential backoff.
- Permanent 4xx responses are not retried, except one refresh and retry on 401.
- Response bodies are bounded and API errors include only sanitized JSON:API
  error code/detail fields.
- Existing playlists are never selected, overwritten, or deleted.

The API documents one-hour idempotency replay. A process interrupted after a
server-side write but before local state persistence should be resumed promptly;
the importer also uses read-back reconciliation to avoid re-adding a confirmed
pending batch.

## CLI and generated files

Single-invocation interactive flow:

```bash
cargo run -- migrate \
  --concurrency 12 \
  --rate-limit 4 \
  --fallback-searches \
  --apply \
  --include-probable
```

This selects Spotify playlists or Liked Songs, exports them, matches them,
optionally runs interactive Review when `--include-review` is supplied, and
imports each successful source using its original Spotify name. Without
`--apply`, the entire command stops at import-plan reports and makes no TIDAL
mutation.

Dry run:

```bash
cargo run -- import-tidal data/<playlist>-tidal-matches.json --dry-run
```

Apply exact matches:

```bash
cargo run -- import-tidal data/<playlist>-tidal-matches.json --apply
```

Apply exact and probable matches:

```bash
cargo run -- import-tidal data/<playlist>-tidal-matches.json \
  --apply \
  --include-probable
```

Resume:

```bash
cargo run -- import-tidal data/<playlist>-tidal-matches.json \
  --apply \
  --include-probable \
  --resume
```

Selection flags must be identical on resume because they contribute to the
fingerprint.

Default generated files:

- `data/<playlist>-tidal-import-state.json`
- `data/<playlist>-tidal-import-report.json`

Private endpoints, unofficial libraries, browser storage, intercepted traffic,
scraping, and browser automation are not used.
