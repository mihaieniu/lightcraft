# Immich smart previews from the PC: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task by task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Immich becomes the head of My Photos, split into Lightroom and Phone & cloud. Its photos get smart
previews on demand: built on the PC by a Docker container, kept for good when pinned, otherwise expired after 30
days. Originals are read only when a photo is opened, and the laptop builds previews itself when the PC is
unavailable.

**Architecture:**
- A new engine module `preview_remote` holds a token-protected JSON-line TCP protocol, modelled on
  `crates/denoise/src/remote.rs`:
  - the server streams an Immich original by asset id into a temp file;
  - it probes the file with the native `FileProbe`, which yields the content hash and details;
  - it builds the smart-preview bytes with the same code as the app;
  - it returns both.
- On the laptop, a wrapping `FileProbe` answers for Immich files from the server and writes the `.lcsp` into the
  smart previews folder. The import then never reads the original.
- Immich photos render previews from their smart preview even when the original is online.

**Tech stack:** Rust (engine, CLI, egui UI), `lightcraft-fetch` for HTTP, Docker plus compose on the PC
(WSL2 Ubuntu, Docker 29.8).

**Spec:** `docs/superpowers/specs/2026-11-18-immich-smart-previews-design.md`

## Global constraints

- House rules (AGENTS.md):
  - No `unwrap`/`expect`/`panic`/`unsafe` in non-test code, and new crate roots carry the deny attribute.
  - Input-derived sizes are capped.
  - Long work holds an activity `TaskGuard`.
  - Everything is a command.
- Retention default: **30 days** after the photo was last shown, configurable, with 0 meaning never expire.
- Lightroom group: libraries named in `immich.json` `"lightroom_libraries"`, default `["Lightroom"]`.
- Server: listens on a configured address, never `0.0.0.0` by default. Token at least 16 characters. A greeting
  checks protocol version 1 and `smart::ENCODE_VERSION`. At most 8 connections. The originals' temp dir is under
  `/dev/shm` or `$TMPDIR`, and each file is deleted after its asset is done.
- Laptop fallback: after **3** consecutive server failures, stop asking for the session and build locally.
- Laptop builds and tests are wrapped in `systemd-run --user --scope -q -p MemoryMax=3500M -p MemorySwapMax=0`
  with `-j 2`. Heavy whole-suite runs may instead run on the PC: fetch the branch from the `gitlab` remote there.
- One task id per commit: `CRAFT-9: …`.

## Review focus

1. **The PC dies mid-folder** (container restarted or Wi-Fi drops after 10 of 200 photos). Expected: the photos
   already done keep their previews, the rest fall back to local without a hang. Covered in Task 4,
   `server_dropping_mid_batch_falls_back`.
2. **An Immich file whose original fails to decode** (a corrupt or unsupported raw). Expected: an error result for
   that asset only, the batch goes on, and locally it becomes an ordinary import failure. Covered in Task 2,
   `undecodable_asset_reports_error_and_batch_continues`.
3. **A server built from another revision** (different `ENCODE_VERSION`). Expected: refused at hello, and the
   laptop uses the local path. Covered in Task 2, `hello_refuses_other_encode_version`.
4. **Expiry against a pinned subfolder and an album pin covering the same photo.** Expected: a photo kept by any pin
   is never expired. Covered in Task 6, `photo_in_any_pin_survives_expiry`.
5. **Huge or hostile answers** (a payload length over the cap, or a probe JSON line over 64 KiB). Expected: refused
   with an error, nothing written. Covered in Task 2, `oversized_payload_is_refused`.

---

### Task 1: Engine building blocks (smart bytes for a photo, serializable probe, key-based file name)

**Files:**
- Modify: `crates/engine/src/smart.rs`: add `file_name_for_key`, and make `file_name` use it.
- Modify: `crates/engine/src/media/source.rs:371`: `ProbeInfo` derives `Serialize, Deserialize` (`#[serde(default)]`).
  `MediaKind` already does; add the derive to `lightcraft_geom::Orientation` if it is missing.
- Modify: `crates/engine/src/cmd/previews.rs:650-700`: extract the build body into `pub(crate) fn smart_bytes`.
- Test: `crates/engine/src/tests/smart_remote.rs` (new, registered in `tests.rs`).

**Interfaces (produces):**
- `pub fn smart::file_name_for_key(content_key: &str) -> String`, which gives `"{:032x}.lcsp"` of
  `Hasher128::new().str(key)`.
- `pub fn smart::sharded_path_for_key(dir: &Path, content_key: &str) -> PathBuf`.
- `pub(crate) fn cmd::previews::smart_bytes(s: &Session, p: &Photo) -> Result<Vec<u8>, String>`: the decoded
  preview-size source of `p` (`media.origin_ref(&p.source, SourceLevel::Preview.max_edge())` with
  `preview_fitted_look`), encoded with `smart::encode`. The build loop calls it.
- `ProbeInfo: Serialize + Deserialize`.

- [ ] Test `file_name_for_key_matches_photo_name`: for a `Photo` with `content_hash = Some("ab…")`,
  `file_name(&p) == file_name_for_key("ab…")`.
- [ ] Test `probe_info_round_trips_json`: a `ProbeInfo` with meta, orientation, xmp and content hash survives
  `serde_json` to and from.
- [ ] Test `smart_bytes_equals_built_file`: import a `lightcraft-scenes` JPEG, run `previews.smart` (build), and
  assert the written `.lcsp` equals `smart_bytes(&s, &photo)`.
- [ ] Run (fail): `systemd-run … cargo test -p lightcraft-engine -j 2 smart_remote`.
- [ ] Implement. The existing build loop keeps its behaviour, so the previews tests stay green.
- [ ] Run `smart_remote` plus `cargo test -p lightcraft-engine -j 2 previews` (pass).
- [ ] Commit `CRAFT-9: smart preview bytes for one photo, serializable probe`.

### Task 2: `preview_remote` protocol, server and client (with an injected builder)

**Files:**
- Create: `crates/engine/src/preview_remote.rs`. Native only (`#[cfg(not(target_arch = "wasm32"))]`), declared in
  `lib.rs`.
- Test: `crates/engine/src/preview_remote/tests.rs`.

**Protocol (version 1):** one JSON line of at most 64 KiB, optionally followed by `bytes` payload bytes, with the
payload capped at 64 MiB.

```text
→ {"v":1,"token":"…","op":"hello"}
← {"ok":true,"v":1,"encode":<smart::ENCODE_VERSION>,"look":<camera_preview::LOOK_VERSION>,"threads":N}
→ {"v":1,"token":"…","op":"build","asset":"<immich asset id>"}
← {"ok":true,"asset":"…","probe":{ProbeInfo…},"bytes":N}  + N bytes (.lcsp)
← {"ok":false,"asset":"…","error":"…"}
```

**Interfaces (produces):**
- `pub struct Built { pub probe: ProbeInfo, pub smart: Vec<u8> }`
- `pub type Builder = Arc<dyn Fn(&str /*asset id*/) -> Result<Built, String> + Send + Sync>`
- `pub fn serve(listener: TcpListener, token: String, builder: Builder, max_connections: usize, stop: Arc<AtomicBool>) -> io::Result<()>`.
  Each connection runs on its own thread, and requests run serially on it. The client opens several connections
  for parallelism.
- `pub struct Client { addr: String, token: String, failures: AtomicU32 }` with:
  - `pub fn new(addr: &str, token: &str) -> Self`
  - `pub fn hello(&self) -> Result<Hello, String>`
  - `pub fn build(&self, asset: &str) -> Result<Built, String>`
  - `pub fn usable(&self) -> bool`, which is false after 3 consecutive failures; a success resets the count.
- Connect timeout 3 s, read timeout 120 s.

- [ ] Tests, against a server on `127.0.0.1:0` with a fake builder:
  - `hello_reports_versions`;
  - `wrong_token_is_refused`;
  - `hello_refuses_other_encode_version`: the client checks the `encode` it receives against its own
    `ENCODE_VERSION` and returns an error;
  - `build_returns_probe_and_bytes`;
  - `undecodable_asset_reports_error_and_batch_continues`: the builder errs for "bad"; "bad" then "good" on one
    connection gives an error, then success;
  - `oversized_payload_is_refused`: a hand-written server line with `bytes` above 64 MiB makes the client return an
    error;
  - `three_failures_make_client_unusable`.
- [ ] Run (fail), implement, run (pass): `cargo test -p lightcraft-engine -j 2 preview_remote`.
- [ ] Commit `CRAFT-9: preview_remote protocol`.

### Task 3: The real builder, the `preview-serve` CLI and the Docker image

**Files:**
- Modify: `crates/engine/src/immich.rs`: add `Client::download_original(&self, asset: &str, to: &Path, max: u64) -> Result<(), String>`
  (`GET /api/assets/{id}/original`, streamed to `to`, capped at `read_file::MAX_MEDIA_BYTES`). The original's file
  name comes from `GET /api/assets/{id}` (`originalFileName`), so the extension, which decides the decoder, is kept.
- Create: `crates/engine/src/preview_remote/build.rs`, which adds
  `pub fn immich_builder(cfg: immich::Config, tmp: PathBuf) -> Builder`. Per asset it:
  - downloads to `tmp/<asset>/<originalFileName>`;
  - creates a headless `Session::new().with_fs()` (or reuses a per-thread one) and imports the file in browse mode;
  - calls `smart_bytes`, and takes the probe from `media.file_probe`;
  - deletes the temp folder;
  - wraps the work in `crate::memory::work_gate()` like `fs_load`.
- Modify: `apps/lightcraft-cli/src/main.rs:185`: add `Some("preview-serve") => preview_serve(&args[1..])`, with
  `--listen ADDR` (required), `--max-connections N` (default 8) and `--tmp DIR` (default `/dev/shm/lightcraft`).
  It reads the token from `LIGHTCRAFT_PREVIEW_TOKEN` and the Immich settings from `IMMICH_URL` plus
  `IMMICH_API_KEY`. No `paths` are needed.
- Create: `docker/worker/Dockerfile`: a `rust:1-bookworm` build stage with
  `cargo build --release -p lightcraft-cli --locked`, then `debian:bookworm-slim` with only the binary, running as
  a non-root user and `ENTRYPOINT ["lightcraft-cli","preview-serve"]`.
- Create: `docker/worker/compose.yml`: service `lightcraft-worker` with `build: ../..` and
  `dockerfile: docker/worker/Dockerfile`, `env_file: .env`, `restart: unless-stopped`, a ports mapping of
  `"${LISTEN_IP}:7991:7991"`, `mem_limit: 12g`, and `tmpfs: /dev/shm/lightcraft:size=4g`. The command is
  `--listen 0.0.0.0:7991`, which is fine inside the container because the port is published on `LISTEN_IP` only.
- Create: `docker/worker/.env.example` with `LISTEN_IP=`, `IMMICH_URL=`, `IMMICH_API_KEY=` and
  `LIGHTCRAFT_PREVIEW_TOKEN=`. `.env` is gitignored.
- Modify: `docs/immich.md`: add a "Smart previews from another computer" section.

- [ ] Test `immich_builder_matches_local_build` in `preview_remote/tests.rs`:
  - a fake Immich HTTP server on `127.0.0.1:0` serves a scenes JPEG for `/api/assets/a/original` and
    `{"originalFileName":"a.jpg"}`;
  - the builder's `Built` must equal a local import of the same file: `probe.content_hash` equal, and `smart` equal
    to `smart_bytes`.
- [ ] Test `builder_cleans_temp_dir`: after a build, `tmp` is empty.
- [ ] Run (fail), implement, run (pass).
- [ ] Run `cargo build -p lightcraft-cli -j 2`, then `lightcraft-cli preview-serve --help` exits 0.
- [ ] Commit `CRAFT-9: preview-serve builds smart previews from Immich; worker container`.

### Task 4: The laptop asks the PC on import (remote-aware probe), with fallback

**Files:**
- Modify: `crates/engine/src/immich.rs`:
  - `Config` gains `worker: Option<WorkerConfig { addr: String, token: String }>`, read from `"worker": {"addr", "token_file"}`
    in `immich.json`, with `~` expanded and the token read from the file;
  - `Config` gains `lightroom_libraries: Vec<String>`, defaulting to `["Lightroom"]`;
  - `album_files` and the new folder listing (Task 5) also return `assets: Vec<(String /*local path*/, String /*asset id*/)>`.
- Create: `crates/engine/src/immich_previews.rs`, with:
  - `pub struct RemotePreviews { client: preview_remote::Client, ids: Mutex<HashMap<String, String>>, smart_dir: PathBuf }`;
  - `pub fn wrap_probe(native: FileProbe, rp: Arc<RemotePreviews>) -> FileProbe`. For a path in `ids` while
    `client.usable()`, it asks `build`, writes `smart` atomically (`safe_file::write_atomic`) at
    `smart::sharded_path_for_key(smart_dir, &probe.content_hash)`, marks it in `availability`, and returns the
    probe. Otherwise, or when `build` errs, it calls `native(path)` and records the path in `rp.local_queue`.
- Modify: `crates/engine/src/cmd/immich.rs::browse_album`. Before `import_with`, when `cfg.worker` is set and a
  library is on disk, it installs the wrapped probe for that import (swaps `s.media.file_probe` and restores it
  after), fills `ids`, and afterwards starts a background local smart build (`previews.smart`, background) for
  `rp.local_queue`.
- Test: `crates/engine/src/tests/immich.rs`.

- [ ] Tests:
  - `browse_with_worker_reads_no_original`: a fake worker server returns a probe and lcsp for asset "a"; the album
    file path does not exist on disk (`missing` is allowed because the probe answered). The photo is in the catalog,
    `has_smart_preview` is true, and no file read happened (the native probe is a closure that panics in the test).
  - `server_dropping_mid_batch_falls_back`: the fake server answers 1 of 3, then closes; all 3 are in the catalog;
    2 came through the native probe and are queued for a local build.
  - `no_worker_config_keeps_todays_path`.
- [ ] Album listing currently skips paths that are not files (`missing`). With a worker, keep them, because the
  photo can be described remotely. This is a test assertion in the first test.
- [ ] Run (fail), implement, run (pass): `cargo test -p lightcraft-engine -j 2 immich`.
- [ ] Commit `CRAFT-9: Immich imports ask the PC for probe and smart preview, local fallback`.

### Task 5: Immich photos preview from their smart preview; folders and Lightroom grouping in the engine

**Files:**
- Modify: `crates/engine/src/media/cache.rs:376`. When `p`'s path is under an Immich local root
  (`self.smart_first: Vec<String>`, set from `immich.paths` when the config loads), `level != SourceLevel::Full`,
  and the smart preview exists, return `SourceRef::Smart` even when the original is online.
- Modify: `crates/engine/src/immich.rs`:
  - `Client::folders() -> Result<Vec<String>, String>` (`GET /api/view/folder/unique-paths`);
  - `Client::folder_assets(path) -> Result<Vec<(String /*originalPath*/, String /*id*/)>, String>` (`GET /api/view/folder?path=`);
  - `pub fn group_of(cfg, libraries: &[Library], server_path: &str) -> Group`, with `enum Group { Lightroom, Other }`,
    where Lightroom means under an import path of a library named in `lightroom_libraries`.
- Modify: `crates/engine/src/cmd/immich.rs`:
  - `immich.folders {}` returns `{lightroom: [paths], other: [paths]}`, with uploads in other;
  - `immich.browseFolder {path, name?}` works like `browseAlbum`, sharing a `fn show_files(s, id, name, files, assets)`;
  - `immich.albums` adds `group: "lightroom"|"other"` per album, by majority of its first page's paths, cached in
    `Session::immich_groups`.
- Tests:
  - `immich_photos_thumb_from_smart_preview_when_online`;
  - `folders_split_by_lightroom_library`;
  - `album_group_by_majority`;
  - `browse_folder_shows_its_photos`.
- [ ] Run (fail), implement, run (pass). Commit `CRAFT-9: Immich folders, Lightroom grouping, smart-first previews`.

### Task 6: Pins and 30-day expiry

**Files:**
- Modify: `crates/engine/src/library.rs` (prefs at about line 315): add
  `immich_pins: Vec<ImmichPin>` (`enum ImmichPin { Folder(String), Album(String) }`, serde
  `{"folder": …}`/`{"album": …}`) and `immich_keep_days: Option<u32>` (`None` means 30, `0` means never).
- Create: `crates/engine/src/immich_retention.rs`:
  - `pub fn touch(s: &mut Session, ids: &[PhotoId])` records the last-shown time in a per-library JSON
    `immich-shown.json`, and is called when grid thumbnails are drawn and on open;
  - `pub fn expire(s: &mut Session, now: DateTime<Utc>) -> Expired { removed: usize, bytes: u64 }` deletes the
    smart previews of Immich photos (under an Immich root) that no pin covers and that have not been shown for the
    keep-days. A pinned album's photo set is resolved through the API and cached in `immich-pins.json` when pinned
    or refreshed.
- Commands in `cmd/immich.rs`:
  - `immich.pin {folder?|album?}`: adds the pin and starts a background fetch of the whole folder (recursive) or
    album through the Task 4 path, with an activity row "Keeping … offline";
  - `immich.unpin {…}`;
  - `immich.cacheStats {}` returns `{kept: {count, bytes}, temporary: {count, bytes}}`;
  - `immich.clearTemporary {}`.
- `expire` runs at library open (background) and every 24 h from the app tick.
- Tests:
  - `photo_in_any_pin_survives_expiry`;
  - `unshown_31_days_is_expired_30_is_kept`;
  - `keep_days_zero_never_expires`;
  - `clear_temporary_keeps_pins`;
  - `pin_fetches_whole_folder`, against a fake worker.
- [ ] Run (fail), implement, run (pass). Commit `CRAFT-9: Immich keep-offline pins and 30-day expiry`.

### Task 7: UI (Immich at the top with groups, folders and pins; Preferences)

**Files:**
- Modify: `crates/ui-egui/src/panels/left.rs:~150` so `immich::section` is called before the movable sections.
  Keep the trailing drop zone intact.
- Modify: `crates/ui-egui/src/panels/left/immich.rs`:
  - the header reads "Immich";
  - a sub-header for each group: "Lightroom" and "Phone & cloud";
  - under each group, collapsible **Folders** (a tree from `immich.folders`, children folded by default, a click
    runs `immich.browseFolder`) and **Albums** (filtered by `group`);
  - each row has a pin toggle (`Icon::Pin`, or a drawn pin when the set has none; record it in
    `assets/ATTRIBUTION.md` if new) that calls `immich.pin` or `immich.unpin`; pinned rows show the icon filled.
- Modify: `crates/ui-egui/src/immich_load.rs`: load folders alongside albums (`refresh_albums` also lists
  folders), and add `start_folder(app, path, name)`.
- Preferences (find the panel holding "Smart Previews" settings): add a "Immich offline cache" block showing kept
  and temporary sizes, a "Keep for N days" field and a **Clear temporary** button.
- While the Task 4 fallback is building locally, the Immich section's header status reads "Building previews on this
  computer (PC unavailable)". It comes from a `RemotePreviews::fallback_active()` exposed in `immich.status {}`.
- Grid: call `immich_retention::touch` through a command `immich.touch {ids}`, batched per frame from the grid's
  drawn thumbnails, only when the view is Immich.
- Test in `crates/ui-egui/src/tests/` (masking tests are the pattern):
  `immich_section_is_first_with_groups_and_pins`, using a fake config and folders, that asserts the widget order
  (`button:immichRefresh` above the Albums section header) and that `immich:pin:<path>` dispatches `immich.pin`.
  Run it with `WGPU_BACKEND=gl`, 2 threads.
- [ ] Run (fail), implement, run (pass), then look at a headless screenshot. Commit
  `CRAFT-9: Immich at the top of My Photos, Lightroom / Phone & cloud, keep-offline pins`.

### Task 8: Deploy and check end to end

- [ ] Push `mihai` to `origin` and `gitlab`.
- [ ] On the PC (`ssh mihai@100.100.5.5`):
  - clone the fork into `~/Craft/lightcraft` from the gitlab remote and check out `mihai`;
  - copy `docker/worker/.env.example` to `.env` and fill it in: `LISTEN_IP=100.100.5.5`, `IMMICH_URL` from the
    laptop's `immich.json`, a new Immich API key with asset read permission (asked from the user or created in
    Immich by them), and a random 32-character token;
  - run `docker compose -f docker/worker/compose.yml up -d --build`.
  - Check C: free space first: `df -h /mnt/c`. If the build needs more than the space free, list the Steam games
    with their sizes and ask the user which to delete. Never delete without that answer.
- [ ] On the laptop, add `"worker": {"addr": "100.100.5.5:7991", "token_file": "~/.config/lightcraft/worker-token"}`
  (mode 600) to `immich.json`, then run `craft.sh install lightcraft`.
- [ ] End to end:
  - open the Cununie album and record the time to the first thumbnail;
  - confirm no original is read (`strace -f -e trace=openat -p <pid>` shows no `~/PhotoNAS/...jpg` opens while
    the grid loads);
  - open a photo in Develop and confirm the original is opened then;
  - `docker stop lightcraft-worker`, open another folder, and check that the fallback status shows and the
    previews are built locally;
  - start the container again.
- [ ] Update CRAFT-9, `docs/immich.md` and the README fork bullet. Commit and push.
