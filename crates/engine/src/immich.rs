//! Immich (a self-hosted photo server) as a source of albums: its storage is a mounted folder,
//! so photos are read in place like a Local folder; the API only lists albums, the paths of their
//! photos, and the libraries to rescan after an import or export wrote into them.
//!
//! Config: `$XDG_CONFIG_HOME/lightcraft/immich.json` (else `~/.config/lightcraft/immich.json`);
//! no file, no Immich. `IMMICH_API_KEY` overrides `api_key_file`.
//!
//! ```json
//! { "url": "http://immich.example:2283",
//!   "api_key_file": "~/.config/immich/api-key",
//!   "paths": { "/mnt/media/Library": "~/mnt/photos/Library" } }
//! ```
//!
//! `paths` maps a folder as Immich sees it (inside its container) to the same folder here.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use lightcraft_fetch::http;
use serde_json::{Value, json};

/// Largest response body read (a page of 1,000 assets is a few MB).
const MAX_BODY: usize = 16 * 1024 * 1024;
/// Assets asked for per search page.
const PAGE_SIZE: u32 = 1000;
/// Smart previews fetched from the preview server at once.
const FETCH_THREADS: usize = 8;
/// Photos looked at to place an album in a [`Group`].
pub const GROUP_SAMPLE: u32 = 100;

/// Where Immich is and how its folders appear here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Base URL without a trailing `/`.
    pub url: String,
    pub api_key: String,
    /// (folder as Immich sees it, the same folder here), longest Immich folder first.
    pub paths: Vec<(String, String)>,
    /// A preview server (`lightcraft-cli preview-serve`) that builds smart previews of Immich photos (CRAFT-9).
    pub worker: Option<WorkerConfig>,
    /// The external libraries shown as "Lightroom" (by name); the others are "Phone & cloud".
    pub lightroom_libraries: Vec<String>,
}

/// Where the preview server is, and the token it wants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerConfig {
    pub addr: String,
    pub token: String,
}

/// An Immich album.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Album {
    pub id: String,
    pub name: String,
    pub count: u64,
    /// When its photos start (ISO 8601, Immich's `startDate`); `None` for an empty album.
    pub start: Option<String>,
    /// The asset shown as its cover ([`Client::album_cover`]); `None` until looked up or when it has none.
    pub cover: Option<String>,
}

/// How the album list is ordered (always under year headings).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlbumSort {
    /// Newest first.
    #[default]
    Date,
    /// By [`display_name`], ignoring case.
    Name,
}

/// An album's name as shown: without the `L_` that marks the albums brought over from Lightroom (one, and only when
/// something is left).
pub fn display_name(name: &str) -> &str {
    match name.strip_prefix("L_") {
        Some(rest) if !rest.is_empty() => rest,
        _ => name,
    }
}

/// The year an album starts in, when its date has one.
fn year_of(a: &Album) -> Option<&str> {
    let y = a.start.as_deref()?.get(..4)?;
    y.bytes().all(|b| b.is_ascii_digit()).then_some(y)
}

/// `albums` under year headings (newest year first, "Undated" last), ordered within a year by `sort`: indices into
/// `albums`.
pub fn year_groups(albums: &[(Album, Group)], sort: AlbumSort) -> Vec<(String, Vec<usize>)> {
    let mut ix: Vec<usize> = (0..albums.len()).collect();
    let key = |i: &usize| albums.get(*i).map(|(a, _)| a);
    ix.sort_by(|x, y| {
        let (Some(a), Some(b)) = (key(x), key(y)) else { return std::cmp::Ordering::Equal };
        // newest year first, undated last
        let by_year = match (year_of(a), year_of(b)) {
            (Some(p), Some(q)) => q.cmp(p),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        by_year.then_with(|| match sort {
            // (undated albums have no date to order by: by name)
            AlbumSort::Date if year_of(a).is_some() && year_of(b).is_some() => {
                b.start.cmp(&a.start).then_with(|| display_name(&a.name).to_lowercase().cmp(&display_name(&b.name).to_lowercase()))
            }
            AlbumSort::Date => display_name(&a.name).to_lowercase().cmp(&display_name(&b.name).to_lowercase()),
            AlbumSort::Name => display_name(&a.name).to_lowercase().cmp(&display_name(&b.name).to_lowercase()),
        })
    });
    let mut out: Vec<(String, Vec<usize>)> = Vec::new();
    for i in ix {
        let year = key(&i).and_then(year_of).map_or_else(|| "Undated".to_string(), str::to_string);
        match out.last_mut() {
            Some((y, v)) if *y == year => v.push(i),
            _ => out.push((year, vec![i])),
        }
    }
    out
}

/// An Immich external library and the folders it scans (as Immich sees them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Library {
    pub id: String,
    pub name: String,
    pub import_paths: Vec<String>,
}

/// Which part of My Photos an Immich folder or album shows under (CRAFT-9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Group {
    /// In a library [`Config::lightroom_libraries`] names.
    Lightroom,
    /// Every other library, and the phone uploads.
    Other,
}

/// The group of a path as Immich sees it.
pub fn group_of(cfg: &Config, libraries: &[Library], server_path: &str) -> Group {
    let lightroom = libraries
        .iter()
        .filter(|l| cfg.lightroom_libraries.iter().any(|n| n == &l.name))
        .any(|l| l.import_paths.iter().any(|p| under(server_path, p).is_some()));
    if lightroom { Group::Lightroom } else { Group::Other }
}

/// The group most of `paths` are in (`Other` on a tie or when there are none).
pub fn group_by_majority(cfg: &Config, libraries: &[Library], paths: &[String]) -> Group {
    let lr = paths.iter().filter(|p| group_of(cfg, libraries, p) == Group::Lightroom).count();
    if lr * 2 > paths.len() { Group::Lightroom } else { Group::Other }
}

fn trim_dir(s: &str) -> String {
    let t = s.trim_end_matches(['/', '\\']);
    if t.is_empty() { s.chars().take(1).collect() } else { t.to_string() }
}

fn expand_home(s: &str, home: Option<&Path>) -> Result<String, String> {
    match (s.strip_prefix('~'), home) {
        (Some(rest), Some(h)) if rest.is_empty() || rest.starts_with('/') => Ok(format!("{}{rest}", h.to_string_lossy().trim_end_matches('/'))),
        (Some(_), None) => Err(format!("{s}: no home folder to expand `~` with")),
        _ => Ok(s.to_string()),
    }
}

/// Whether `path` is folder `dir` or inside it (whole names only).
pub fn is_under(path: &str, dir: &str) -> bool {
    under(path, dir).is_some()
}

/// `s` for a URL query value: everything but unreserved characters percent-encoded.
fn query_value(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") }).collect()
}

/// `path` inside folder `dir`, or `dir` itself, by whole names: what follows `dir` in it.
fn under<'a>(path: &'a str, dir: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(dir)?;
    (rest.is_empty() || rest.starts_with(['/', '\\']) || dir.ends_with('/')).then_some(rest)
}

impl Config {
    /// The config file's place: `$XDG_CONFIG_HOME/lightcraft/immich.json`, else under `~/.config`.
    pub fn file() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::home_dir().map(|h| h.join(".config")))?;
        Some(base.join("lightcraft").join("immich.json"))
    }

    /// Read the config file: `Ok(None)` when there is none (Immich off).
    pub fn load() -> Result<Option<Config>, String> {
        let Some(file) = Config::file() else { return Ok(None) };
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", file.display())),
        };
        let env_key = std::env::var("IMMICH_API_KEY").ok().filter(|k| !k.trim().is_empty());
        Config::from_json(&text, env_key, std::env::home_dir().as_deref()).map(Some).map_err(|e| format!("{}: {e}", file.display()))
    }

    /// Parse the config's JSON; `env_key` (from `IMMICH_API_KEY`) wins over `api_key_file`, and `~`
    /// stands for `home`.
    pub fn from_json(text: &str, env_key: Option<String>, home: Option<&Path>) -> Result<Config, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
        let url = v.get("url").and_then(Value::as_str).map(|u| u.trim().trim_end_matches('/').to_string()).filter(|u| !u.is_empty());
        let url = url.ok_or("give the server's address as `url`")?;
        http::Url::parse(&url).map_err(|e| format!("url: {e}"))?;
        let api_key = match env_key {
            Some(k) => k.trim().to_string(),
            None => {
                let f = v.get("api_key_file").and_then(Value::as_str).ok_or("give `api_key_file` (or set IMMICH_API_KEY)")?;
                let f = expand_home(f, home)?;
                let k = std::fs::read_to_string(&f).map_err(|e| format!("API key file {f}: {e}"))?;
                k.trim().to_string()
            }
        };
        if api_key.is_empty() {
            return Err("the API key is empty".into());
        }
        let mut paths = Vec::new();
        match v.get("paths") {
            None | Some(Value::Null) => {}
            Some(Value::Object(m)) => {
                for (server, local) in m {
                    let local = local.as_str().ok_or_else(|| format!("paths: `{server}` must map to a folder name"))?;
                    paths.push((trim_dir(server), trim_dir(&expand_home(local, home)?)));
                }
            }
            Some(_) => return Err("paths: give an object of folder → folder".into()),
        }
        paths.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        let worker = match v.get("worker") {
            None | Some(Value::Null) => None,
            Some(w) => {
                let addr = w.get("addr").and_then(Value::as_str).filter(|a| !a.trim().is_empty()).ok_or("worker: give its `addr` (host:port)")?;
                let f = w.get("token_file").and_then(Value::as_str).ok_or("worker: give `token_file`")?;
                let f = expand_home(f, home)?;
                let token = std::fs::read_to_string(&f).map_err(|e| format!("worker token file {f}: {e}"))?.trim().to_string();
                if token.is_empty() {
                    return Err(format!("worker token file {f} is empty"));
                }
                Some(WorkerConfig { addr: addr.trim().to_string(), token })
            }
        };
        let lightroom_libraries = match v.get("lightroom_libraries").and_then(Value::as_array) {
            Some(a) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            None => vec!["Lightroom".to_string()],
        };
        Ok(Config { url, api_key, paths, worker, lightroom_libraries })
    }

    /// A path as Immich reports it, here; `None` when no mapped folder holds it.
    pub fn to_local(&self, server: &str) -> Option<String> {
        self.paths.iter().find_map(|(s, l)| under(server, s).map(|rest| format!("{l}{rest}")))
    }

    /// A path here as Immich sees it; `None` when no mapped folder holds it.
    pub fn to_server(&self, local: &str) -> Option<String> {
        let mut by_local: Vec<&(String, String)> = self.paths.iter().collect();
        by_local.sort_by_key(|(_, l)| std::cmp::Reverse(l.len()));
        by_local.into_iter().find_map(|(s, l)| under(local, l).map(|rest| format!("{s}{rest}")))
    }
}

fn str_field(v: &Value, k: &str) -> Result<String, String> {
    v.get(k).and_then(Value::as_str).map(str::to_string).ok_or_else(|| format!("Immich sent no `{k}`"))
}

/// `GET /api/albums`'s answer.
pub(crate) fn parse_albums(v: &Value) -> Result<Vec<Album>, String> {
    let list = v.as_array().ok_or("Immich sent no album list")?;
    list.iter()
        .map(|a| {
            Ok(Album {
                id: str_field(a, "id")?,
                name: str_field(a, "albumName")?,
                count: a.get("assetCount").and_then(Value::as_u64).unwrap_or(0),
                start: a.get("startDate").and_then(Value::as_str).map(str::to_string),
                cover: None,
            })
        })
        .collect()
}

/// A `POST /api/search/metadata` page: its assets' (`originalPath`, `id`) (the id empty when Immich sent none) and the
/// next page.
pub(crate) fn parse_page(v: &Value) -> Result<(Vec<(String, String)>, Option<u32>), String> {
    let assets = v.get("assets").ok_or("Immich sent no `assets`")?;
    let items = assets.get("items").and_then(Value::as_array).ok_or("Immich sent no `assets.items`")?;
    let paths = items
        .iter()
        .filter_map(|i| {
            let path = i.get("originalPath").and_then(Value::as_str)?;
            Some((path.to_string(), i.get("id").and_then(Value::as_str).unwrap_or_default().to_string()))
        })
        .collect();
    let next = match assets.get("nextPage") {
        Some(Value::String(s)) => s.parse::<u32>().ok(),
        Some(Value::Number(n)) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        _ => None,
    };
    Ok((paths, next))
}

/// `GET /api/libraries`'s answer.
pub(crate) fn parse_libraries(v: &Value) -> Result<Vec<Library>, String> {
    let list = v.as_array().ok_or("Immich sent no library list")?;
    list.iter()
        .map(|l| {
            let import_paths = l
                .get("importPaths")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(trim_dir).collect())
                .unwrap_or_default();
            Ok(Library { id: str_field(l, "id")?, name: l.get("name").and_then(Value::as_str).unwrap_or_default().to_string(), import_paths })
        })
        .collect()
}

/// An Immich id (a UUID) that is safe to put in a request path.
fn check_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 64 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(format!("bad Immich id {id:?}"));
    }
    Ok(())
}

/// Requests to the server of `cfg`, stopped by `cancel`.
pub struct Client<'a> {
    cfg: &'a Config,
    cancel: &'a AtomicBool,
}

impl<'a> Client<'a> {
    pub fn new(cfg: &'a Config, cancel: &'a AtomicBool) -> Self {
        Client { cfg, cancel }
    }

    fn limits(&self) -> http::Limits<'a> {
        http::Limits { connect: Duration::from_secs(5), stall: Duration::from_secs(30), cancel: self.cancel }
    }

    /// Send a request; the response once its status says success.
    fn send(&self, method: &str, path: &str, body: Option<&Value>) -> Result<http::Response, String> {
        let url = http::Url::parse(&format!("{}{path}", self.cfg.url)).map_err(|e| e.to_string())?;
        let limits = self.limits();
        let mut headers = vec![("x-api-key", self.cfg.api_key.clone()), ("Accept", "application/json".to_string())];
        let sent = match body {
            Some(b) => {
                headers.push(("Content-Type", "application/json".to_string()));
                http::post(&url, &headers, b.to_string().as_bytes(), &limits)
            }
            None if method == "POST" => http::post(&url, &headers, &[], &limits),
            None => http::get(&url, &headers, &limits),
        };
        let r = sent.map_err(|e| match e {
            http::HttpError::Connect(_) | http::HttpError::Stalled => format!("Immich unreachable at {}: {e}", self.cfg.url),
            e => format!("Immich: {e}"),
        })?;
        match r.status {
            401 | 403 => Err(format!("Immich refused the API key (status {})", r.status)),
            200..=299 => Ok(r),
            s => Err(format!("Immich answered {s} for {path}")),
        }
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Option<Value>, String> {
        let mut r = self.send(method, path, body)?;
        let limits = self.limits();
        let mut out = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = r.read(&mut buf, &limits).map_err(|e| format!("Immich: {e}"))?;
            if n == 0 {
                break;
            }
            if out.len().saturating_add(n) > MAX_BODY {
                return Err(format!("Immich's answer for {path} is too large (over {} MB)", MAX_BODY / (1024 * 1024)));
            }
            out.extend_from_slice(buf.get(..n).unwrap_or_default());
        }
        if out.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        serde_json::from_slice(&out).map(Some).map_err(|e| format!("Immich sent invalid JSON for {path}: {e}"))
    }

    fn json(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        self.call(method, path, body)?.ok_or_else(|| format!("Immich sent nothing for {path}"))
    }

    /// The albums the key's user can see.
    pub fn albums(&self) -> Result<Vec<Album>, String> {
        parse_albums(&self.json("GET", "/api/albums", None)?)
    }

    /// Page `page` (from 1) of album `id`'s photos: their paths as Immich sees them, and the next page.
    pub fn album_page(&self, id: &str, page: u32) -> Result<(Vec<String>, Option<u32>), String> {
        let (assets, next) = self.album_page_assets(id, page)?;
        Ok((assets.into_iter().map(|(p, _)| p).collect(), next))
    }

    /// [`Self::album_page`] with each photo's asset id.
    pub fn album_page_assets(&self, id: &str, page: u32) -> Result<(Vec<(String, String)>, Option<u32>), String> {
        let body = json!({"albumIds": [id], "type": "IMAGE", "size": PAGE_SIZE, "page": page, "withExif": false});
        parse_page(&self.json("POST", "/api/search/metadata", Some(&body))?)
    }

    /// The group most of album `id`'s first [`GROUP_SAMPLE`] photos are in.
    pub fn album_group(&self, id: &str, libraries: &[Library]) -> Result<Group, String> {
        let body = json!({"albumIds": [id], "type": "IMAGE", "size": GROUP_SAMPLE, "page": 1, "withExif": false});
        let (assets, _) = parse_page(&self.json("POST", "/api/search/metadata", Some(&body))?)?;
        let paths: Vec<String> = assets.into_iter().map(|(p, _)| p).collect();
        Ok(group_by_majority(self.cfg, libraries, &paths))
    }

    /// Every folder holding assets, as Immich sees it.
    pub fn folders(&self) -> Result<Vec<String>, String> {
        let v = self.json("GET", "/api/view/folder/unique-paths", None)?;
        let list = v.as_array().ok_or("Immich sent no folder list")?;
        Ok(list.iter().filter_map(Value::as_str).map(trim_dir).collect())
    }

    /// The assets directly in folder `path` (as Immich sees it): (`originalPath`, id).
    pub fn folder_assets(&self, path: &str) -> Result<Vec<(String, String)>, String> {
        let v = self.json("GET", &format!("/api/view/folder?path={}", query_value(path)), None)?;
        let list = v.as_array().ok_or("Immich sent no folder contents")?;
        Ok(list
            .iter()
            .filter(|a| a.get("type").and_then(Value::as_str).is_none_or(|t| t == "IMAGE"))
            .filter_map(|a| Some((a.get("originalPath")?.as_str()?.to_string(), a.get("id").and_then(Value::as_str).unwrap_or_default().to_string())))
            .collect())
    }

    /// The external libraries.
    pub fn libraries(&self) -> Result<Vec<Library>, String> {
        parse_libraries(&self.json("GET", "/api/libraries", None)?)
    }

    /// Asset `asset`'s original file name (its last path component only: never a folder to write into).
    pub fn asset_file_name(&self, asset: &str) -> Result<String, String> {
        check_id(asset)?;
        let v = self.json("GET", &format!("/api/assets/{asset}"), None)?;
        let name = v.get("originalFileName").and_then(Value::as_str).unwrap_or_default();
        let name = Path::new(name).file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if name.is_empty() || name.starts_with('.') {
            return Err(format!("Immich gave asset {asset} no usable file name"));
        }
        Ok(name.to_string())
    }

    /// Download asset `asset`'s original into file `to` (at most `max` bytes; a failed download leaves no file).
    pub fn download_original(&self, asset: &str, to: &Path, max: u64) -> Result<(), String> {
        check_id(asset)?;
        let path = format!("/api/assets/{asset}/original");
        let mut r = self.send("GET", &path, None)?;
        let limits = self.limits();
        let fail = |e: String| {
            let _ = std::fs::remove_file(to);
            e
        };
        let mut f = std::fs::File::create(to).map_err(|e| format!("{}: {e}", to.display()))?;
        let mut buf = vec![0u8; 256 * 1024];
        let mut total = 0u64;
        loop {
            let n = r.read(&mut buf, &limits).map_err(|e| fail(format!("Immich: {e}")))?;
            if n == 0 {
                break;
            }
            total = total.saturating_add(n as u64);
            if total > max {
                return Err(fail(format!("asset {asset}'s original is larger than {} MB", max / (1024 * 1024))));
            }
            std::io::Write::write_all(&mut f, buf.get(..n).unwrap_or_default()).map_err(|e| fail(format!("{}: {e}", to.display())))?;
        }
        Ok(())
    }

    /// Ask Immich to rescan library `id` (it queues the scan and answers at once).
    pub fn scan(&self, library_id: &str) -> Result<(), String> {
        check_id(library_id)?;
        self.call("POST", &format!("/api/libraries/{library_id}/scan"), None).map(|_| ())
    }
}

/// What [`crate::LibrarySource::Immich`] shows: an album's photos (Local or in the library), found
/// under `root`, the deepest folder holding them all (browsed photos only show in a folder view).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AlbumView {
    pub id: String,
    pub name: String,
    pub root: String,
    pub photos: Vec<lightcraft_catalog::PhotoId>,
}

/// An album's photos here, and what was left out of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AlbumFiles {
    /// Files on disk LightCraft can open, as local paths.
    pub files: Vec<String>,
    /// Photos in no mapped folder.
    pub unmapped: usize,
    /// Photos in a mapped folder but not on disk (the mount is offline, or the file is gone).
    pub missing: usize,
    /// Files LightCraft doesn't open.
    pub skipped: usize,
    /// (local path, Immich asset id) of [`Self::files`] that Immich gave an id.
    pub assets: Vec<(String, String)>,
}

/// Most pages read for one album (1,000 photos each).
const MAX_PAGES: u32 = 1000;

/// Immich's folders by group (as Immich sees them, sorted).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Folders {
    pub lightroom: Vec<String>,
    pub other: Vec<String>,
}

/// The albums, each with its [`Group`] (Other when it can't be told). Network: a worker thread's job.
pub fn albums_with_groups(cfg: &Config, cancel: &AtomicBool) -> Result<Vec<(Album, Group)>, String> {
    let client = Client::new(cfg, cancel);
    let albums = client.albums()?;
    let libraries = client.libraries().unwrap_or_default();
    Ok(albums
        .into_iter()
        .map(|a| {
            let g = client.album_group(&a.id, &libraries).unwrap_or(Group::Other);
            (a, g)
        })
        .collect())
}

/// Every folder of the server, split into [`Group`]s (network, so a worker thread's job).
pub fn folder_tree(cfg: &Config, cancel: &AtomicBool) -> Result<Folders, String> {
    let client = Client::new(cfg, cancel);
    let libraries = client.libraries()?;
    let mut out = Folders::default();
    for f in client.folders()? {
        match group_of(cfg, &libraries, &f) {
            Group::Lightroom => out.lightroom.push(f),
            Group::Other => out.other.push(f),
        }
    }
    out.lightroom.sort();
    out.other.sort();
    Ok(out)
}

/// Add Immich's (path, id)s to `out`, mapped here.
fn sort_assets(cfg: &Config, assets: Vec<(String, String)>, out: &mut AlbumFiles) {
    for (p, asset) in assets {
        match cfg.to_local(&p) {
            None => out.unmapped += 1,
            Some(l) if !Path::new(&l).is_file() => out.missing += 1,
            Some(l) if !crate::import::is_supported(Path::new(&l)) => out.skipped += 1,
            Some(l) => {
                if !asset.is_empty() {
                    out.assets.push((l.clone(), asset));
                }
                out.files.push(l)
            }
        }
    }
}

/// The photos directly in folder `path` (as Immich sees it), mapped here.
pub fn folder_files(cfg: &Config, path: &str, cancel: &AtomicBool) -> Result<AlbumFiles, String> {
    let mut out = AlbumFiles::default();
    sort_assets(cfg, Client::new(cfg, cancel).folder_assets(path)?, &mut out);
    Ok(out)
}

/// Every photo of album `id`, mapped to this computer (all pages; network and disk, so a worker thread's job).
pub fn album_files(cfg: &Config, id: &str, cancel: &AtomicBool) -> Result<AlbumFiles, String> {
    let client = Client::new(cfg, cancel);
    let mut out = AlbumFiles::default();
    let mut page = 1;
    for _ in 0..MAX_PAGES {
        let (assets, next) = client.album_page_assets(id, page)?;
        sort_assets(cfg, assets, &mut out);
        match next {
            Some(n) if n > page => page = n,
            _ => break,
        }
    }
    Ok(out)
}

/// The deepest folder holding every one of `files` (by whole names); empty for none.
pub fn common_root(files: &[String]) -> String {
    let mut iter = files.iter().map(|f| Path::new(f).parent().map(Path::to_path_buf).unwrap_or_default());
    let Some(mut root) = iter.next() else { return String::new() };
    for dir in iter {
        while !dir.starts_with(&root) {
            if !root.pop() {
                return String::new();
            }
        }
    }
    root.to_string_lossy().to_string()
}

impl crate::Session {
    /// Set Immich up (`None`: no Immich). With a `worker`, the file probe asks it about Immich photos
    /// ([`crate::immich_previews`]); without one the probe is the plain one again.
    pub fn set_immich(&mut self, cfg: Option<Config>) {
        if let Some((_, native)) = self.immich_previews.take() {
            self.media.file_probe = native;
        }
        if let Some(w) = cfg.as_ref().and_then(|c| c.worker.as_ref()) {
            let rp = std::sync::Arc::new(crate::immich_previews::RemotePreviews::new(&w.addr, &w.token));
            let native = self.media.file_probe.clone();
            if let Some(n) = native.clone() {
                self.media.file_probe = Some(crate::immich_previews::wrap_probe(n, rp.clone()));
            }
            self.immich_previews = Some((rp, native));
        }
        self.media.smart_first = cfg.as_ref().map(|c| c.paths.iter().map(|(_, local)| local.clone()).collect()).unwrap_or_default();
        self.immich = cfg;
    }

    /// Tell the preview server's client which photos are about to be imported.
    pub fn register_immich_assets(&self, assets: &[(String, String)]) {
        if let Some((rp, _)) = &self.immich_previews {
            rp.register(assets, self.media.smart_dir.clone());
        }
    }

    /// Fetch from the preview server, in the background, the smart previews Immich photos `assets` (local path, asset
    /// id) already in the catalog don't have (browsed before there was a server, or expired). Without a usable server
    /// they are queued to be built here.
    pub fn fetch_missing_immich_previews(&mut self, assets: &[(String, String)]) {
        let (Some((rp, _)), Some(dir)) = (self.immich_previews.clone(), self.media.smart_dir.clone()) else { return };
        let by_path: std::collections::HashMap<&str, &str> = assets.iter().map(|(p, a)| (p.as_str(), a.as_str())).collect();
        let jobs: Vec<(String, String, PathBuf)> = self
            .catalog
            .photos()
            .filter_map(|p| {
                let lightcraft_catalog::Source::File { path } = &p.source else { return None };
                let asset = by_path.get(path.as_str())?;
                let to = crate::smart::sharded_file_path(&dir, p);
                (!crate::smart::is_valid(&crate::smart::existing_file_path(&dir, to.clone()))).then(|| (path.clone(), (*asset).to_string(), to))
            })
            .collect();
        if jobs.is_empty() {
            return;
        }
        if !rp.ready() {
            rp.queue_paths(jobs.into_iter().map(|(p, _, _)| p));
            return;
        }
        let total = jobs.len() as u64;
        let guard = std::sync::Arc::new(self.activity.start("immich", "Fetching smart previews from the PC", crate::activity::Cancel::No));
        guard.progress(0, total);
        let queue = std::sync::Arc::new(std::sync::Mutex::new(jobs));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        // a few at once: the server decodes on many cores, and each answer is about a megabyte
        for _ in 0..FETCH_THREADS {
            let (rp, queue, done, guard) = (rp.clone(), queue.clone(), done.clone(), guard.clone());
            let spawned = std::thread::Builder::new().name("immich-previews".into()).spawn(move || {
                loop {
                    let Some((path, asset, to)) = queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pop() else { break };
                    // the server went away or can't do this one: built here instead (the app picks the queue up)
                    if let Err(e) = rp.fetch_into(&asset, &to) {
                        log::info!("smart preview of {path} from the PC: {e}");
                        rp.queue_paths([path]);
                    }
                    guard.progress(done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1, total);
                }
            });
            if let Err(e) = spawned {
                log::warn!("smart previews from the PC: {e}");
            }
        }
    }

    /// Build here, in the background, the smart previews of the Immich photos the preview server didn't do.
    pub fn build_queued_immich_previews(&mut self) {
        let Some((rp, _)) = &self.immich_previews else { return };
        // one build at a time: the queue waits for the running one
        if self.preview_build.as_ref().is_some_and(|b| b.running()) {
            return;
        }
        let queued: std::collections::HashSet<String> = rp.take_local_queue().into_iter().collect();
        if queued.is_empty() || self.media.smart_dir.is_none() {
            return;
        }
        let ids: Vec<u64> = self
            .catalog
            .photos()
            .filter(|p| matches!(&p.source, lightcraft_catalog::Source::File { path } if queued.contains(path)))
            .map(|p| p.id.0)
            .collect();
        if let Err(e) = self.execute("library.smartPreviews", &json!({"ids": ids, "background": true})) {
            log::warn!("smart previews of Immich photos: {e}");
        }
    }

    /// Switch the view to Immich album `id` (empty until [`crate::Session::add_to_immich_view`] adds its photos),
    /// its photos under `root`.
    pub fn show_immich_album(&mut self, id: &str, name: &str, root: String) {
        self.immich_view = Some(AlbumView { id: id.to_string(), name: name.to_string(), root, photos: Vec::new() });
        self.source = crate::LibrarySource::Immich;
        self.refresh_view();
        self.keep_active_in_view();
    }

    /// Add the photos of `files` (already in the catalog: browsed or imported) to the Immich album view.
    pub fn add_to_immich_view(&mut self, files: &[String]) {
        let wanted: std::collections::HashSet<&str> = files.iter().map(String::as_str).collect();
        let found: Vec<lightcraft_catalog::PhotoId> = self
            .catalog
            .photos()
            .filter(|p| !p.deleted && matches!(&p.source, lightcraft_catalog::Source::File { path } if wanted.contains(path.as_str())))
            .map(|p| p.id)
            .collect();
        if let Some(v) = &mut self.immich_view {
            let mut have: std::collections::HashSet<lightcraft_catalog::PhotoId> = v.photos.iter().copied().collect();
            v.photos.extend(found.into_iter().filter(|id| have.insert(*id)));
        }
        self.refresh_view();
        self.keep_active_in_view();
    }

    /// Narrow `f` to the Immich album view (nothing without one).
    pub(crate) fn immich_filter(&self, f: &mut lightcraft_catalog::Filter) {
        match &self.immich_view {
            Some(v) if !v.photos.is_empty() && !v.root.is_empty() => {
                f.folder = Some(v.root.clone());
                f.subfolders = true;
                f.only = v.photos.clone();
            }
            _ => f.only_none = true,
        }
    }
}

/// Asks Immich to rescan the libraries that hold newly written files, once no request has come for
/// `delay`, so the batches of one import (however long it runs) make one scan per library. Off the
/// caller's thread; a failure is logged, shown on the activity row and kept for
/// [`Rescans::take_error`], never returned. Cheap to clone.
#[derive(Clone)]
pub struct Rescans {
    inner: std::sync::Arc<RescanState>,
}

#[derive(Default)]
struct Queue {
    /// Paths as Immich sees them, waiting for a scan.
    paths: Vec<String>,
    /// A worker is waiting or scanning.
    running: bool,
    /// When the latest request came.
    last: Option<std::time::Instant>,
    /// The latest failure, until a front end takes it.
    error: Option<String>,
}

struct RescanState {
    queue: std::sync::Mutex<Queue>,
    idle: std::sync::Condvar,
    delay: Duration,
}

impl Default for Rescans {
    fn default() -> Self {
        Rescans::with_delay(Duration::from_secs(3))
    }
}

impl Rescans {
    /// Scan once no request has come for `delay` (gathering an import's batches).
    pub fn with_delay(delay: Duration) -> Self {
        Rescans { inner: std::sync::Arc::new(RescanState { queue: Default::default(), idle: std::sync::Condvar::new(), delay }) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.inner.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Rescan, soon, the libraries of `cfg` holding any of `paths` (local paths); paths in no mapped folder are left out.
    pub fn request(&self, cfg: &Config, paths: Vec<String>, activity: &crate::activity::Activity) {
        let server: Vec<String> = paths.iter().filter_map(|p| cfg.to_server(p)).collect();
        if server.is_empty() {
            return;
        }
        let mut q = self.lock();
        q.paths.extend(server);
        q.last = Some(std::time::Instant::now());
        if q.running {
            return;
        }
        q.running = true;
        drop(q);
        // the row shows from the request on, not from when the worker gets going
        let guard = activity.start("immich", "Telling Immich about new files", crate::activity::Cancel::No);
        let (me, cfg) = (self.clone(), cfg.clone());
        if let Err(e) = std::thread::Builder::new().name("immich-rescan".into()).spawn(move || me.work(&cfg, guard)) {
            log::warn!("Immich: could not start the rescan: {e}");
            let mut q = self.lock();
            q.running = false;
            q.error = Some(format!("could not start the rescan: {e}"));
            self.inner.idle.notify_all();
        }
    }

    fn work(&self, cfg: &Config, guard: crate::activity::TaskGuard) {
        loop {
            std::thread::sleep(self.inner.delay);
            let paths = {
                let mut q = self.lock();
                // requests keep coming (an import's batches): wait for the last one
                if q.last.is_some_and(|t| t.elapsed() < self.inner.delay) {
                    continue;
                }
                if q.paths.is_empty() {
                    // the row goes before anyone waiting hears the rescan is done
                    drop(guard);
                    q.running = false;
                    self.inner.idle.notify_all();
                    return;
                }
                std::mem::take(&mut q.paths)
            };
            if let Err(e) = scan_holding(cfg, &paths) {
                log::warn!("Immich: {e}");
                guard.detail(&e);
                self.lock().error = Some(e);
            }
        }
    }

    /// The latest rescan failure, once (for a front end to show).
    pub fn take_error(&self) -> Option<String> {
        self.lock().error.take()
    }

    /// Block until no rescan is waiting or running (tests; a one-shot CLI before it exits).
    pub fn wait_idle(&self) {
        let mut q = self.lock();
        while q.running {
            q = self.inner.idle.wait(q).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

/// Scan each library of `cfg` whose folders hold one of `paths` (as Immich sees them).
fn scan_holding(cfg: &Config, paths: &[String]) -> Result<(), String> {
    let cancel = AtomicBool::new(false);
    let client = Client::new(cfg, &cancel);
    for lib in client.libraries()? {
        if lib.import_paths.iter().any(|dir| paths.iter().any(|p| under(p, dir).is_some())) {
            client.scan(&lib.id)?;
        }
    }
    Ok(())
}
