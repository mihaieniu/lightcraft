# Immich album polish and camera import: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task by task.

**Goal:**
- Immich albums show without `L_`, with a cover, sorted by date or by name under year headings.
- Album pins follow the album's photos.
- An import can target an Immich Lightroom album: laptop backup, NAS copy, Immich scan, then add to the album.

**Architecture:**
- Engine side: `immich.rs` gains album dates, display names, sorting and cover choice, and a new module
  `immich_upload.rs` runs the resumable upload job.
- UI side: the Immich section draws covers and year headings, and the import dialog gains an Immich destination
  whose finish starts the upload job on a worker thread.

**Spec:** `docs/superpowers/specs/2026-11-18-immich-albums-and-camera-import-design.md`

## Global constraints

- House rules apply: no panics, no `unsafe`, capped input sizes, long work in the activity stack, everything is a
  command.
- Laptop builds and tests run under `systemd-run --user --scope -q -p MemoryMax=3500M -p MemorySwapMax=0`, with `-j 2`.
- UI tests run with `WGPU_BACKEND=gl` and two threads.
- Commit messages start with `CRAFT-11:`.

## Review focus

1. **NAS unmounted mid-upload.** The job stops with the reason, keeps what it did, and resumes at the next start.
   Covered by Task 4's `upload_resumes_after_a_failure`.
2. **A file already on the NAS with different bytes.** It is never overwritten: that one file errors and the
   rest go on. Covered by Task 4's `different_file_on_the_nas_is_left_alone`.
3. **Immich never finds a file after the scan.** The job times out with "not found by Immich" for those files,
   and the others are still added. Covered by Task 4's `assets_not_found_are_reported`.
4. **Album names and covers.** An album with no photos, no date, or a name that is just `L_` neither crashes nor
   shows empty. Covered by Task 1's `display_name_and_years_edge_cases`.
5. **The cover cache** holds hostile or huge thumbnails. Covers are capped at 512 KB and fail quietly. Covered by
   Task 2's `cover_bytes_are_capped`.

---

### Task 1: album dates, display names, sort and year groups (engine)

- `Album` gains `start: Option<String>`, the ISO `startDate`.
- `pub fn display_name(name: &str) -> &str` strips one leading `L_` and returns the original when nothing is
  left.
- `pub enum AlbumSort { Date, Name }`.
- `pub fn year_groups(albums: &[(Album, Group)], sort: AlbumSort) -> Vec<(String /*year or "Undated"*/, Vec<usize>)>`:
  years newest first, "Undated" last. Within a year, date sorts newest first and name sorts by display name,
  case-insensitively.
- Tests:
  - `display_name_and_years_edge_cases`
  - `albums_sort_by_date_within_years`
  - `albums_sort_by_name_within_years`
  - `parse_albums_reads_start_date`

### Task 2: album covers (engine)

- `Client::album_cover(id) -> Result<Option<String>, String>` makes up to three `POST /api/search/metadata`
  queries with `size: 1`: `{albumIds, isFavorite: true}`, then `{albumIds, rating: 5}`, then `{albumIds}`. It
  returns the first asset id found.
- `Client::thumbnail(asset) -> Result<Vec<u8>, String>` calls `GET /api/assets/{id}/thumbnail?size=thumbnail`,
  capped at `MAX_COVER = 512 * 1024` bytes.
- `pub fn cover_file(asset) -> Option<PathBuf>` returns `<cache dir>/immich-covers/<asset>.webp`.
- `pub fn cached_cover(cfg, asset, cancel) -> Result<PathBuf, String>` reads the cache, else fetches and writes
  the file.
- `albums_with_groups` also returns each album's cover id; a failure gives `None`.
- Tests:
  - `cover_prefers_favourite_then_five_stars_then_first`, against a fake server whose routes match on body text;
  - `cover_bytes_are_capped`.

### Task 3: sidebar albums with covers, names, sort and years; pins follow albums (UI)

- The Immich state keeps `covers: HashMap<asset, TextureHandle>`. A worker thread decodes cached covers with
  `lightcraft_codecs::decode` and sends back RGBA at most 64 px on the long edge.
- `Row` gains `.image(Option<egui::TextureId>)`, drawn in place of the icon in a rounded square.
- The album rows of each group are drawn under year headings from `year_groups`.
- The sort comes from the Immich header's context menu ("Sort by Date" / "Sort by Name"), through the new command
  `immich.setAlbumSort`. The sort is kept in `ui.json` (`immichAlbumSort`).
- When a pinned album finishes loading, its pin's paths are replaced with the album's current files.
- UI test: `albums_show_covers_names_and_years`. It checks that `L_Trip` is painted as "Trip", that the year
  heading sits above it, that the order follows the sort, and that the cover texture is used.

### Task 4: the upload job (engine, `immich_upload.rs`)

- `pub enum Target { Album(String /*id*/), New(String /*name without L_*/) }`
- `pub struct Job { pub files: Vec<String> /*laptop copies*/, pub staging: String, pub target: Target, pub done_copy: Vec<bool>, pub assets: Vec<Option<String>>, pub album: Option<String> }`.
  It is persisted with `save_jobs`/`load_jobs` in `<data dir>/immich-uploads.json`.
- `pub fn run(cfg, job: &mut Job, progress: &dyn Fn(u64, u64, &str), cancel: &AtomicBool, poll: Duration, timeout: Duration) -> Result<Report, String>`
  works in these steps:
  1. **Copy to the NAS.** The destination is `cfg.to_local(<first Lightroom import path>)/<path relative to staging>`.
     It is written as `.part` and then renamed. A file with the same size already there counts as done. A
     different size is an error for that file and is never overwritten.
  2. **Scan** the Lightroom library.
  3. **Find the assets.** Poll `POST /api/search/metadata {originalPath}` for each file not yet found, until all
     are found or the timeout passes.
  4. **Create the album** if the target is `New`: `POST /api/albums {albumName: "L_"+name, description: "From Lightroom"}`.
  5. **Add the assets:** `PUT /api/albums/{id}/assets {ids}`.
  6. **Report** `{copied, found, added, missing: [paths], failed: [(path, why)]}`.
  7. The job is saved after each step.
- New `Client` methods: `find_asset(original_path)`, `create_album(name, description)`,
  `add_to_album(id, ids)`. These need routes in the fake server for PUT and query-free paths.
- Tests, against the fake Immich and a temp "NAS" folder:
  - `upload_copies_scans_finds_and_adds`
  - `new_album_is_created_with_prefix_and_description`
  - `different_file_on_the_nas_is_left_alone`
  - `assets_not_found_are_reported`
  - `upload_resumes_after_a_failure`

### Task 5: the import dialog's Immich destination and the background job (UI)

- `ImportDialog` gains `immich_album: Option<String>` and `new_immich_album: String`. In the review, a
  "Send to Immich" block offers a combo of the Lightroom albums by display name, or a new album name.
- With an Immich target, `start` forces a copy into `~/Pictures/LightCraft Imports`, organised by date.
  `ImportTask` keeps the target. On `finish`, the imported photos' paths start an upload job.
- `immich_upload_ui`:
  - runs the job on a thread with an activity row "Sending N photos to Immich" and progress;
  - toasts the report;
  - refreshes the album list.
  - At startup it resumes the jobs found in `immich-uploads.json`.
- Command `immich.uploadStatus {}` returns the running and pending jobs.
- UI test: `import_to_a_new_immich_album_sends_the_photos`, which uses a fake Immich and a fake NAS folder.

### Task 6: deploy and check end to end

- Push, then do a release build with `craft.sh` or the capped cargo command.
- Import a few real photos, copied into a fake device folder with a `DCIM` layout, into a new album
  "LightCraft import test".
- Check that the photos are in Immich, that the album exists, and that the sidebar shows the album with its cover.
- Leave the test album and files for the user to keep or delete.
