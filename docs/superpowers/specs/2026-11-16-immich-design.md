# Immich integration (fork feature)

## Intent

Use LightCraft as the editor for a self-hosted [Immich](https://immich.app) photo server: see
Immich's **albums** inside LightCraft and edit their photos, keep browsing the server's folders, and
**import new shoots** through LightCraft into a folder Immich watches, so they show up in Immich's
phone app. Editing works as for any other photo.

## Assumptions about the setup

- Immich (2.x) storage is reachable as a local folder (an sshfs/NFS/SMB mount), writable by the user.
- Photos are in Immich **external libraries** and/or its upload folder; Immich reports each asset's
  `originalPath` as seen inside its container, so a prefix map turns it into a local path.
- The user has an Immich API key.

LightCraft already provides what the rest needs: Local browsing (photos shown without joining the
library, edits to XMP sidecars — `cmd/browse.rs`, `import_with(local: true)`), import Copy with
`Organize::ByDay` (`YYYY/YYYY-MM-DD/`), XMP auto-write (Immich reads ratings/keywords from
sidecars on scan), Smart Previews for editing while the mount is offline, and an HTTP client in
`lightcraft-fetch` (http and https).

## Goals

- An **Immich** section in the left sidebar listing albums (name, count), refreshable.
- Clicking an album shows its photos the way a Local folder does: not added to All Photos, editable,
  edits to sidecars, "Add to My Photos" works as for Local.
- After an **import or export** writes files under a mapped folder, ask Immich to rescan the library
  that owns it, so the files appear in Immich within a minute.
- Everything is an engine command (UI, CLI, control channel and MCP get it alike).

## Non-goals (v1)

- Downloading originals or uploading through the API (the mount is the file path).
- Writing album membership, ratings or favourites back through the API (sidecars carry ratings and
  keywords).
- Immich timeline, people or search views. Folders are reachable through Local roots.
- Showing `originalFileName` for UUID-named uploads (they show as `<uuid>.jpg`).

## Anti-goals

- Never delete, move or rewrite files on the server; only XMP sidecars, as Local browsing already does.
- Don't reshape upstream code: new code in new files, upstream files get hooks of a few lines, so
  rebasing the fork on upstream stays easy.
- No API key, server address or personal path in the repository, catalog or `prefs.json`.
- A missing config, unreachable server, bad JSON or unmapped path is an error message, never a panic
  (house rules: never crash; responses are hostile input, sizes capped).

## Approaches considered

- **A. Mount + API for albums (chosen).** Files come through the mount; the API only lists albums,
  their assets' `originalPath`, and triggers library scans. Small; reuses Local browsing end to end.
- **B. API only.** Download originals into a cache, upload imports. Works without a mount, but sidecars
  can't sit next to originals and it touches the file layer. About 3× the code.
- **C. Mount only.** No code, but no albums, and uploads appear as UUID folders.

## Design

**Config**: `$XDG_CONFIG_HOME/lightcraft/immich.json`; a missing file turns the feature off and hides
the section. `IMMICH_API_KEY` in the environment overrides `api_key_file`.

```json
{ "url": "http://immich.example:2283",
  "api_key_file": "~/.config/immich/api-key",
  "paths": { "/mnt/media/Library": "~/mnt/photos/Library",
             "/usr/src/app/upload": "~/mnt/photos/uploads" } }
```

**Engine**: `crates/engine/src/immich.rs` (config, client, path map) and `crates/engine/src/cmd/immich.rs`:

- `immich.albums` → `GET /api/albums` → `[{id, name, count}]`.
- `immich.browseAlbum {id}` → `GET /api/albums/{id}`, map each asset's `originalPath` by longest
  matching prefix, keep supported files that exist, then the same steps as `library.browse`
  (`import_with` local, stamp browsed) and set `LibrarySource::ImmichAlbum(id)`, whose filter is the
  album's mapped path set. Reports `photos`, `unmapped`, `missing` and `skipped` (videos, unsupported).
- `immich.rescan {paths}` → `GET /api/libraries`, map each library's `importPaths` to local paths,
  `POST /api/libraries/{id}/scan` for each library holding one of `paths`. The import and export jobs
  call it after writing (a one-line hook each); a failure is logged and shown as a toast and never
  fails the import or export.
- `lightcraft-fetch` only has `get`; add a bodiless `post` beside it (same limits and TLS path) for
  the scan call. This is the one change to an upstream crate's API.
- Album loading and rescans run off the UI thread and hold a `TaskGuard` in the activity stack.

**UI**: an "Immich" section in the `ui-egui` left panel (same `section` frame as Albums and Local)
listing albums, with refresh and a status line (off / unreachable / N albums). Clicking an album runs
`immich.browseAlbum` on the worker thread like Local browsing; big albums fill in batch by batch.

**Import workflow** (only the rescan hook is new): an import preset with Copy, destination = the
mapped folder of the library new shoots belong to, organize By Day. An export preset writing into a
mapped folder sends edited JPEGs to Immich the same way.

## Testing

- Unit: prefix mapping (longest prefix wins, trailing slash, `~` expansion, unmapped paths), album and
  library JSON parsing against small synthetic fixtures (malformed and oversized ones too),
  library-for-path lookup, config errors.
- Integration (skipped unless `IMMICH_TEST_URL` and `IMMICH_API_KEY` are set): list albums, browse an
  album, rescan.
- Manual: import a few raws → they appear in Immich within a minute; edit one and export → the
  edited JPEG appears in Immich.

## Left to the builder

The section's place in the sidebar, the album view's title, caching the album list between restarts,
toast wording.
