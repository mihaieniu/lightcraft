# Immich

*A fork addition: not in upstream LightCraft.*

LightCraft can edit the photos of a self-hosted [Immich](https://immich.app) server. Immich keeps
your albums and phone backups; LightCraft reads the same files from the server's storage and
edits them in place, the way it works on a Local folder.

- **Albums**: an *Immich* section at the bottom of the sidebar lists the server's albums. Click one
  to show its photos. They aren't added to All Photos, edits go to XMP sidecars next to the
  originals (Immich reads ratings and keywords from those on its next scan), and **Add to My
  Photos** adds them to the library as for any Local photo. Click the section's header to list
  the albums again.
- **New photos**: import (or export) into a folder an Immich external library scans, and
  LightCraft asks Immich to rescan that library a few seconds later, so the photos show up in
  Immich's apps within a minute. Browsing a folder never does.
- **Commands** (CLI, MCP, control channel): `immich.albums` and `immich.browseAlbum {id, name?}`.

## Setup

1. Mount Immich's storage on this computer so the files are readable (and writable for sidecars
   and imports): sshfs, NFS or SMB. For example, with sshfs as the user who owns the files:

   ```sh
   sshfs user@immich-host:/srv/immich ~/mnt/immich -o reconnect,ServerAliveInterval=15,idmap=user
   ```

2. Create an API key in Immich (Account Settings → API Keys) and save it in a file only you can
   read, e.g. `~/.config/immich/api-key` (mode 600). Or set `IMMICH_API_KEY`. Listing albums and
   their photos needs the album and asset read permissions; the rescan after an import needs the
   library permissions, which only an admin's key has. With a narrower key, rescans fail with a
   message and Immich picks the files up at its next scheduled scan.

3. Write `~/.config/lightcraft/immich.json` (or under `$XDG_CONFIG_HOME`):

   ```json
   { "url": "http://immich-host:2283",
     "api_key_file": "~/.config/immich/api-key",
     "paths": { "/mnt/media/Library": "~/mnt/immich/Library",
                "/usr/src/app/upload": "~/mnt/immich/upload" } }
   ```

   `paths` maps each folder **as Immich sees it** (the paths inside its container: an external
   library's import path, and the upload folder) to the same folder on this computer. Photos in no
   mapped folder are counted and left out.

4. Restart LightCraft. Without the file the section stays hidden.

## Smart previews from another computer

Opening an Immich album or folder used to read every original over the network just to show it. With a
**worker** on a computer next to the NAS (a desktop on the home network), LightCraft asks the worker instead:
it downloads each original from Immich, builds its smart preview and reads its details, and sends those back.
The laptop shows and edits the photo from the smart preview and reads the original only when the photo is
opened at full size (loupe, 1:1, export). When the worker can't be reached, the laptop builds the smart
previews itself from the mount, slower.

On the worker computer (Docker, any Linux or WSL2):

```sh
git clone <your fork> lightcraft && cd lightcraft      # the same revision as the laptop's LightCraft
cp docker/worker/.env.example docker/worker/.env       # fill in LISTEN_IP, IMMICH_URL, IMMICH_API_KEY, token
docker compose -f docker/worker/compose.yml up -d --build
```

It restarts by itself (`restart: unless-stopped`) and keeps no photos: originals go to a memory-backed folder
and are deleted once their preview is built. Moving it to another computer is the same three commands.

On the laptop, add to `immich.json` and save the same token in a file only you can read:

```json
"worker": { "addr": "100.100.5.5:7991", "token_file": "~/.config/lightcraft/worker-token" }
```

**The protocol is not encrypted** (as AI Denoise's): publish the worker on a VPN (Tailscale) or LAN address
only. Both sides must be built from the same revision; a worker that builds smart previews differently is
refused at its greeting and the laptop builds them itself.

## Importing into Immich

In the import dialog choose **Copy**, the mapped folder of the external library new shoots belong
in as the destination, and organize **By day** (`YYYY/YYYY-MM-DD/`); save it as an import preset.
For edited versions, export into a folder of that library (e.g. an `Edits` subfolder).

## Limits

- The section is drawn below the other sections and can't be hidden or moved yet.
- Photos uploaded from phones keep Immich's file names (`<uuid>.jpg`).
- Nothing is written back through the API (albums, favourites); ratings and keywords travel in the
  sidecars.
