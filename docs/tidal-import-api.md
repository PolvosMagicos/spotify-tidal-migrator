# TIDAL playlist import API discovery

Discovery date: 2026-07-16

Last revalidated: 2026-07-28

Status: **blocked for ordinary third-party applications**. No playlist mutation
code is implemented or invoked in this repository.

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
The API is versioned independently and must be checked again before unblocking
mutations.

The currently published Android and iOS SDK module indexes expose authentication,
player, and event-related modules, but no playlist mutation client. The Web API
OpenAPI document is therefore the authoritative operation/schema source for
this discovery.

## Verified operations and schemas

All API operations use the base URL `https://openapi.tidal.com/v2`, bearer
authentication, and the JSON:API media type `application/vnd.api+json` for
request and response documents.

### Create a playlist

- Method and endpoint: `POST /playlists`
- Access tier shown on operation: `THIRD_PARTY`
- OAuth security requirement: `playlists.write` **and** `w_usr`
- Query parameter: optional `countryCode`
- Optional request header: `Idempotency-Key` (maximum 128 characters)
- Success: HTTP 201, `Playlists_Single_Resource_Data_Document`
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
`UNLISTED`; there is no documented `PRIVATE` value. The creation schema does
not publish maximum name or description lengths.

### Add playlist items

- Method and endpoint: `POST /playlists/{id}/relationships/items`
- Access tier shown on operation: `THIRD_PARTY`
- OAuth security requirement: `playlists.write` **and** `w_usr`
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

The payload can contain tracks and videos. Per-item `meta.addedAt` is optional.
Top-level `meta.positionBefore` is optional as a whole, but
`positionBefore` is required when `meta` is supplied. The reference does not
state the insertion position when top-level `meta` is omitted, nor does it
explicitly document duplicate-track behavior. Therefore append ordering and
duplicate preservation have not been assumed.

### Read and verify playlist items

- Method and endpoint: `GET /playlists/{id}/relationships/items`
- Supports client credentials or authorization-code PKCE without an additional
  operation scope in the published security object.
- Success: HTTP 200, a resource-identifier array with optional `itemCursor` and
  `itemId` metadata.
- Pagination uses the opaque `links.next` cursor URL. There is no documented
  page-size parameter on this operation.

The endpoint makes post-import verification structurally possible, but order
semantics and duplicate guarantees remain undocumented.

### Read the authenticated user and owned playlists

- `GET /users/me` is officially documented through `GET /users/{id}` with
  `id=me`, but requires both `user.read` and `r_usr`.
- `GET /playlists?filter[owners.id]=me` is documented, but requires
  `playlists.read` and `r_usr` for authorization-code tokens.

## OAuth and dashboard blocker

The same official OpenAPI security scheme assigns these access tiers:

| Scope | Published required tier |
| --- | --- |
| `playlists.read` | `THIRD_PARTY` |
| `playlists.write` | `THIRD_PARTY` |
| `user.read` | `THIRD_PARTY` |
| `r_usr` | `INTERNAL` |
| `w_usr` | `INTERNAL` |

OAuth security arrays require all listed scopes. Consequently, playlist
creation and item insertion both require the internal-only `w_usr` in addition
to the public `playlists.write`. Identity and owned-playlist reads require the
internal-only `r_usr` in addition to their public scopes.

The dashboard guide says scopes must be enabled per app and recommends least
privilege, but it does not document a way for a normal third-party app to
enable an `INTERNAL` scope. Until TIDAL removes the internal scope requirement,
publishes that it is implicitly granted, or enables it for this app in the
dashboard, the required authorization is not fully available and mutation must
remain blocked.

The local application configuration checked during the 2026-07-28
revalidation requests the public `playlists.write` scope but does not request
the internal `w_usr` scope. OAuth cannot grant an unrequested scope, and the
official dashboard does not expose `w_usr` as an ordinary third-party scope.
Consequently, both official mutation requests would fail authorization for the
currently configured app.

`TIDAL_SCOPES` is intentionally user-configured. No unverified scope is
hardcoded. The user authorization implementation rejects an empty setting and
checks that every requested scope was actually returned by the token endpoint.

## Other documented HTTP behavior

- Mutation requests accept `Idempotency-Key`. Reusing the same key and payload
  within one hour replays the response after completion; an in-progress request
  returns 409, and reuse with a different payload returns 422.
- The operations document 429, 500, and 503 responses.
- The OpenAPI response definition for 429 does not declare a `Retry-After`
  response header, so its presence cannot be assumed even though a future
  importer should honor it whenever supplied.
- TIDAL states that a successful write is immediately visible to the same
  client, while other clients may observe it later.

## Conditions required to unblock import

Before implementing `import-tidal`, all of the following need primary-source
confirmation:

1. A normal dashboard application can obtain every scope required by both
   playlist mutation operations, especially `w_usr`, or the OpenAPI security
   requirements are revised.
2. Default insertion position and ordering for batched relationship additions.
3. Duplicate-track behavior.
4. Maximum playlist-name and description lengths, or an official statement
   that no smaller service limit applies.

Private endpoints, unofficial libraries, browser storage, intercepted traffic,
scraping, and browser automation are not acceptable substitutes.
