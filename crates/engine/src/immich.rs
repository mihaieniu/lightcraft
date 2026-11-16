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

/// Where Immich is and how its folders appear here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Base URL without a trailing `/`.
    pub url: String,
    pub api_key: String,
    /// (folder as Immich sees it, the same folder here), longest Immich folder first.
    pub paths: Vec<(String, String)>,
}

/// An Immich album.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Album {
    pub id: String,
    pub name: String,
    pub count: u64,
}

/// An Immich external library and the folders it scans (as Immich sees them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Library {
    pub id: String,
    pub import_paths: Vec<String>,
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
        Ok(Config { url, api_key, paths })
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
            Ok(Album { id: str_field(a, "id")?, name: str_field(a, "albumName")?, count: a.get("assetCount").and_then(Value::as_u64).unwrap_or(0) })
        })
        .collect()
}

/// A `POST /api/search/metadata` page: its assets' `originalPath`s and the next page.
pub(crate) fn parse_page(v: &Value) -> Result<(Vec<String>, Option<u32>), String> {
    let assets = v.get("assets").ok_or("Immich sent no `assets`")?;
    let items = assets.get("items").and_then(Value::as_array).ok_or("Immich sent no `assets.items`")?;
    let paths = items.iter().filter_map(|i| i.get("originalPath").and_then(Value::as_str)).map(str::to_string).collect();
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
            Ok(Library { id: str_field(l, "id")?, import_paths })
        })
        .collect()
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

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Option<Value>, String> {
        let url = http::Url::parse(&format!("{}{path}", self.cfg.url)).map_err(|e| e.to_string())?;
        let limits = http::Limits { connect: Duration::from_secs(5), stall: Duration::from_secs(30), cancel: self.cancel };
        let mut headers = vec![("x-api-key", self.cfg.api_key.clone()), ("Accept", "application/json".to_string())];
        let sent = match body {
            Some(b) => {
                headers.push(("Content-Type", "application/json".to_string()));
                http::post(&url, &headers, b.to_string().as_bytes(), &limits)
            }
            None if method == "POST" => http::post(&url, &headers, &[], &limits),
            None => http::get(&url, &headers, &limits),
        };
        let mut r = sent.map_err(|e| match e {
            http::HttpError::Connect(_) | http::HttpError::Stalled => format!("Immich unreachable at {}: {e}", self.cfg.url),
            e => format!("Immich: {e}"),
        })?;
        match r.status {
            401 | 403 => return Err(format!("Immich refused the API key (status {})", r.status)),
            200..=299 => {}
            s => return Err(format!("Immich answered {s} for {path}")),
        }
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
        let body = json!({"albumIds": [id], "type": "IMAGE", "size": PAGE_SIZE, "page": page, "withExif": false});
        parse_page(&self.json("POST", "/api/search/metadata", Some(&body))?)
    }

    /// The external libraries.
    pub fn libraries(&self) -> Result<Vec<Library>, String> {
        parse_libraries(&self.json("GET", "/api/libraries", None)?)
    }

    /// Ask Immich to rescan library `id` (it queues the scan and answers at once).
    pub fn scan(&self, library_id: &str) -> Result<(), String> {
        if library_id.is_empty() || !library_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(format!("bad library id {library_id:?}"));
        }
        self.call("POST", &format!("/api/libraries/{library_id}/scan"), None).map(|_| ())
    }
}
