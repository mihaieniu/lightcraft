# Immich album list polish and camera import into Immich: design

*Fork addition (CRAFT-11). Builds on `2026-11-18-immich-smart-previews-design.md`.*

## Intent

The Immich section is now Mihai's photo library. Two things make it work like one:

1. **Albums read like a Lightroom collection list.**
   - Names don't carry the `L_` prefix.
   - Each album has a small cover thumbnail.
   - Albums are sorted by date, newest first, under year headings.
2. **Shooting goes straight into the library.** Plug in the Sony a6700 (USB Mass Storage), pick an existing or new
   Lightroom album, and the raw files are:
   - copied to the laptop as a backup;
   - pushed to the NAS's Lightroom library;
   - scanned by Immich;
   - added to the album.

The user also decided on two leftovers:
- Album pins follow photos added to the album after it was pinned.
- Lightroom-folder previews expire after 30 days unless pinned. This is the current behaviour, so nothing changes.

## Goals

### Album list

- **Display name** drops a leading `L_` in the UI only. The album in Immich keeps its name.
- **Cover:** the first favourite photo, else the first 5★ photo, else the first photo. It is shown as a small
  thumbnail in place of the album icon.
  - Covers use Immich's own small thumbnail (`/api/assets/{id}/thumbnail?size=thumbnail`, WebP).
  - They are cached on the laptop under the LightCraft cache folder.
- **Sort:**
  - by date (default): album `startDate`, newest first;
  - by name: alphabetical, ignoring the `L_` prefix and case.
  - In both modes, albums sit under **year headings** (the year of `startDate`, newest year first). Albums with
    no date go under "Undated", last.
  - The choice is in the Immich header's context menu and is remembered.
- **Album pins follow the album:** whenever a pinned album is loaded, its pin's photo list is replaced with the
  album's current photos.

### Camera import

- The import review (File ▸ Import from Device, or any import with Immich set up) gains a destination:
  **Immich album**, either an existing Lightroom album or a new one by name.
- With an Immich destination the import is a **Copy**:
  - into `~/Pictures/LightCraft Imports/` organised `YYYY/YYYY-MM-DD`;
  - with the existing verified copy, presets, keywords and renames;
  - into the LightCraft library, as today.
  - The laptop copies are kept as a backup.
- **Then, in the background, an upload job:**
  1. Copy each imported file to the NAS, into the mapped folder of the first library in `lightroom_libraries`,
     keeping the same relative path (`YYYY/YYYY-MM-DD/<name>`). Write to a temp name and rename, skip a file
     already there with the same size, and never overwrite a different file.
  2. Ask Immich to scan that library.
  3. Wait for each file to show up as an asset, found by `originalPath`. Poll every 10 s for up to 30 minutes.
  4. For a new album, create it as `L_<name>` with description `From Lightroom`.
  5. Add the assets to the album, then refresh the sidebar's album list.
- The job shows in the activity stack: "Sending N photos to Immich". Its state is kept in
  `~/.local/share/lightcraft/immich-uploads.json`, so a job cut short by quitting resumes at the next start.
  Failures are toasted with the reason, and the job keeps what it has done.

## Non-goals

- PTP/MTP camera access. The a6700 in Mass Storage mode mounts through udiskie, and LightCraft already lists it.
- Uploading through the Immich upload API. Files must land in the Lightroom folder layout.
- Deleting the laptop backup copies.
- Videos: the existing import may copy them, but only photos are sent to the album.

## Constraints

- Everything is a command:
  - `immich.uploadStatus {}`
  - `immich.setAlbumSort {sort: "date" | "name"}`
  - the import's new `immichAlbum` / `newImmichAlbum` params on the UI side.
- Network and disk work happen on worker threads. The UI thread only polls channels.
- The Immich API key never appears in logs or the UI.

## Approach

One sensible way: extend the existing device import. Upload via the API was rejected because it puts files
outside the Lightroom layout. Routing through the PC was rejected: an extra hop, and no faster.

## Left to the builder

- Cover sizes and placement within the row.
- How the cover cache is bounded (small files; keep at most about 2 000).
- Exact JSON shape of the upload job file.
