# Immich as My Photos, with smart previews from the PC — design

*Fork addition (CRAFT-9). Builds on the Immich integration (`docs/immich.md`) and LightCraft's smart previews
(`crates/engine/src/smart.rs`).*

## Intent

Immich is where all of Mihai's photos live, about 100k photos and 11k videos. In LightCraft it should be the
library ("My Photos"), and browsing it on the laptop should feel local:

- an Immich folder or album loads once;
- its photos stay on the laptop as smart previews, so they can be browsed and edited without reading originals;
- a photo's full original crosses the network only when that photo is opened.

The heavy work runs on the home PC, not the laptop: reading originals, fingerprinting them, and building smart
previews. The laptop has little RAM and is on Wi-Fi or away from home, while the PC sits on the home network next
to the NAS. The PC side must survive an OS reinstall or a move to another computer, so it is one Docker container,
like Immich's machine-learning container that already runs there.

## Goals

1. **Immich at the top.** The Immich section moves to the top of the left panel and becomes the main part of My
   Photos. It shows two clearly separate groups:
   - **Lightroom**: the Immich external library named `Lightroom` (Synced LightroomLibrary, 21.9k mostly raw photos,
     700 GB).
   - **Phone & cloud**: every other library (GooglePhotos, iCloudPhotos) and the phone uploads.

   Each group has its **folders** (Immich's folder tree, `GET /api/view/folder/unique-paths`) and its **albums**.
2. **Smart previews on demand.**
   - Opening an Immich folder or album gets smart previews for its photos that don't have one yet.
   - Thumbnails and Develop work from the smart preview.
   - The original is read only when a photo is opened in the loupe or Develop at full size, or for export, 1:1
     zoom, Lens Blur, denoise or merge, as for any offline-capable photo.
3. **Keep or expire.**
   - The user can mark a folder or album **Keep offline**. Its smart previews are fetched for the whole folder
     straight away and never expire.
   - Smart previews of other photos are deleted 30 days after the photo was last shown. They come back on demand.
4. **Built on the PC, laptop as fallback.**
   - The PC's container fingerprints the original, reads its details, and builds the smart preview.
   - When the PC can't be reached, the laptop does the same itself from the `~/PhotoNAS` mount, as it would today,
     slower.
5. **Portable PC side.** One Docker image and one compose file, from this repo, with `restart: unless-stopped`.
   Reinstalling or moving means installing Docker, copying the compose file and its `.env` (Immich URL, API key,
   token), and running `docker compose up -d`.

## Non-goals

- Videos: listed as today; no smart previews.
- Writing back to Immich through the API (albums, favourites). Edits still go to XMP sidecars.
- AI masks and denoise on the PC's graphics card. That is CRAFT-8, but the image is laid out so those servers can
  be added to the same container later.
- Syncing a library between two LightCraft installs.

## Anti-goals (a failure even if it works)

- The laptop reading every original of a folder over Wi-Fi just to show it. This is what happens today: the import
  probes and hashes each file.
- Smart previews silently filling the laptop disk. Retention must actually delete, and the cache size is shown.
- The app hanging or erroring when the PC is off. It falls back to the laptop, quietly and without losing work.
- The PC disk filling up. The container stores almost nothing: originals are streamed, not kept.

## Constraints and facts

- **PC** (`mihai@100.100.5.5`, "mihaiPRIME"):
  - Windows with WSL2 Ubuntu 24.04: 24 cores, 30 GB RAM, RTX 4060 8 GB visible through CUDA.
  - Docker 29.8 runs inside WSL; `immich-machine-learning:v2.2.3-cuda` already runs there.
  - It is on the home LAN at 10.5.5.5, and Immich answers it in about 3 ms.
  - The WSL disk lives on C:, which has **36 GB free (97% full)**. The user allows deleting Steam games to make
    room, but only with the list confirmed first.
- **Laptop:**
  - 405 GB free disk, tight RAM: builds and tests are capped at 3–4 GB.
  - Originals are reachable at `~/PhotoNAS` (sshfs). The Immich API is reachable through `~/.config/lightcraft/immich.json`.
- **Smart previews** (`smart.rs`):
  - About 1 MB each: the decoded preview-size source as a JPEG with a header and a tail.
  - Named by the photo's **content key** (`media::content_key` → the content hash), stored sharded under the
    library's smart previews folder.
  - They are content-addressed and not tied to a library, so another machine can build them, provided it computes
    the same content hash (`lightcraft_preview::hash_bytes` of the whole file, as the native probe does) and uses
    the same smart-preview format version.
- **Import probe:** `FileProbe` (`media/source.rs`) is a `path → ProbeInfo` hook, the natural place to answer from
  the PC instead of reading the file.
- **Existing remote servers to copy:**
  - `crates/denoise/src/remote.rs`: JSON-line TCP with a shared token; checks that both ends match; falls back to
    the local runner after failures.
  - `lightcraft-sam3-worker`: loopback port reached through an SSH tunnel.

## Approaches considered

1. **A preview service on the PC (chosen).** The container streams originals from the Immich API, and the laptop
   asks it per photo for the content hash, the details and the smart preview.
   - Opening a folder is fast, and no original crosses Wi-Fi.
   - The same container can later host the GPU servers.
   - Cost: a new service and protocol.
2. **The PC writes previews into a folder on the NAS, and the laptop copies them.** Simpler, but on demand it means
   ask, wait for the write, then copy. Pinned folders need a separate sync. Rejected.
3. **A second, headless LightCraft on the PC with its own library.** Keeping two libraries in step is far more work
   for no gain. Rejected.

## Design

### PC: `lightcraft-preview-serve` in the `lightcraft-worker` container

- **Binary:** a new `lightcraft-cli preview-serve` subcommand, or a small bin in the engine crate (builder's call).
  It uses the same decoder and smart-preview writer as the app, so its output matches byte for byte with what the
  laptop would build.
- **Originals:** fetched from Immich with `GET /api/assets/{id}/original`, streamed into memory and never written
  to disk. No NAS mount is needed in the container, so moving it needs only the Immich URL and an API key.
- **Per asset it returns:**
  - the content hash, computed on the whole original;
  - the `ProbeInfo` as JSON (size, format, kind, capture date, metadata, as-shot WB, embedded lens, XMP,
    orientation);
  - the smart-preview bytes (`.lcsp`).
  - It does not send thumbnails: the laptop renders those from the smart preview.
- **Protocol:** modelled on denoise remote.
  - JSON lines plus binary payloads, a shared token, and `hello` with protocol version, LightCraft revision and
    smart-preview format version.
  - The laptop refuses a server whose format differs.
  - Request batches of asset ids; the server answers each one as it is done, so visible photos can go first.
  - It caps concurrent decodes and concurrent connections.
  - It listens on the PC's Tailscale/LAN address, never the internet. The token lives in `.env`, not in git.
  - The connection is unencrypted, like denoise. Tailscale encrypts it in transit.
- **Resources:**
  - Decodes in parallel, about one per core minus headroom.
  - The compose file caps the container's memory at about 12 GB.
  - No disk use beyond the image, so the 36 GB free on C: is enough.
- **Packaging:**
  - `docker/worker/Dockerfile`: a multi-stage Rust build into a slim runtime image.
  - `docker/worker/compose.yml` and `.env.example`, with `restart: unless-stopped`.
  - The image is built on the PC from the fork (GitLab or GitHub) at the same revision as the laptop.
  - `craft.sh` gains `worker` to rebuild and restart it over SSH (builder's call on the exact form).

### Laptop: sidebar

- The Immich section is drawn first, above the movable sections, as the head of My Photos, with groups
  **Lightroom** and **Phone & cloud**.
- Which libraries count as Lightroom is set in `immich.json` (`"lightroom_libraries": ["Lightroom"]`) and defaults
  to the library named `Lightroom`.
- **Folders:**
  - a tree built from `unique-paths`, split by library import path; uploads go under Phone & cloud;
  - children load from Immich, not by listing the mount;
  - each folder row shows its count when known;
  - each folder and album row has a **Keep offline** toggle (pin icon), shown with what the pin keeps.
- **Albums:** an album goes in the group where most of its photos live. This is worked out from the album's assets
  the first time it is listed, cached with the album list, and refreshed when the album is opened.
- The other sections (Albums, Local, Dates, Folders, Keywords) stay below for library photos and local folders.

### Laptop: opening a folder or album

1. List its assets through the Immich API: ids, `originalPath`, checksum, type. Map them to local paths with the
   existing `paths`.
2. A photo already in the catalog with a valid smart preview shows at once from that preview.
3. The rest are requested from the PC, visible ones first. Each answer:
   - writes the `.lcsp` into the smart previews folder;
   - creates or updates the catalog entry from the returned `ProbeInfo` and hash, through the import path, browse
     mode (as `immich.browseAlbum` does now) but with the probe answered remotely, so the mount is not read;
   - triggers a thumbnail render.
4. Photos from Immich render their **thumbnails from the smart preview even when the original is online**. Today an
   online original is preferred. Loupe and Develop at full quality read the original, as now. The original is not
   read for thumbnails or the grid.
5. **Fallback:** if the PC is unreachable, or fails three times in a row as denoise does, the same photos go through
   today's local path:
   - native probe and hash over the mount;
   - then a local smart-preview build, in the background at low priority.
   - A status line says "Building previews on this computer (PC unavailable)".

### Laptop: retention

- **Pins:** stored in LightCraft's prefs as Immich folder paths and album ids. Pinning fetches the whole folder,
  including subfolders, or album in the background.
- **Last shown:** each Immich photo's last-shown time is recorded when its thumbnail is drawn in a grid or it is
  opened.
- **Expiry job:** runs at start and then daily. It deletes smart previews of Immich photos that are not pinned and
  were not shown for 30 days (configurable in Preferences, default 30). Their catalog entries follow the existing
  "browsed photos are forgotten" rules. Edited photos keep their entry, since edits live in the XMP.
- **Preferences** shows the cache's size and count, split into kept and temporary, with a **Clear temporary**
  button.

## Testing

- **Protocol unit tests:** hello and version mismatch, a wrong token, oversized payloads, and the fallback after
  failures, as in denoise remote.
- **Equality test:** a smart preview, hash and probe built by the server path equal the local build for the same
  file, on test raws and JPEGs. This is the key invariant.
- **Engine tests:**
  - opening an Immich folder with a fake server creates catalog entries without reading the file;
  - pins survive expiry;
  - unpinned previews older than 30 days are deleted;
  - thumbnails of Immich photos come from the smart preview.
- **UI test:** Immich is drawn first, with the Lightroom and Phone & cloud groups and the pin toggle.
- **Manual end to end:**
  - the container on the PC, an album opened on the laptop: time to first thumbnail, and no reads of originals on
    `~/PhotoNAS` (watch with `strace` or `iotop`);
  - stop the container, open another folder, and check that the fallback runs.

## Left to the builder

- The exact command and binary names, the protocol message shapes, and the batch sizes.
- How visible-first ordering is fed from the grid.
- The sidebar look within the existing panel style.
- Whether the Immich thumbnail is shown as a placeholder while a smart preview is on its way (nice to have).

## Calls made without the user (open to correction)

- Lightroom vs the rest is decided **by library** (where the files live), not by the "From Lightroom" albums.
- Originals reach the container through the **Immich API**, not a NAS mount, so the container is portable.
- The transport is **token TCP on the Tailscale address** (as denoise does), not an SSH tunnel. It works at home
  and away without a tunnel service on the laptop.
- The 30-day clock is **per photo, from when it was last shown**, not per folder.
- No smart previews for videos.
