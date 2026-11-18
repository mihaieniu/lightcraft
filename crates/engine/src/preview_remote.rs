//! Smart previews built on another computer (CRAFT-9, a fork addition): a preview server reads a photo's original
//! (from Immich, by asset id), probes it and builds its smart preview with the same code as the app, and sends back
//! the probe and the proxy's bytes. The laptop then imports the photo without reading its original.
//!
//! Meant for a LAN or a VPN such as Tailscale: the connection itself is not encrypted. Requests carry a shared token
//! (compared in constant time). Modelled on the denoise server (`lightcraft_denoise::remote`).
//!
//! Protocol (version 1). Each message is one JSON line (`\n`-terminated, at most [`MAX_LINE`] bytes), optionally
//! followed by a payload of `bytes` bytes (at most [`MAX_PAYLOAD`]):
//!
//! ```text
//! → {"v":1,"token":"…","op":"hello"}
//! ← {"ok":true,"v":1,"encode":<smart::ENCODE_VERSION>,"look":<camera_preview::LOOK_VERSION>,"threads":N}
//! → {"v":1,"token":"…","op":"build","asset":"<immich asset id>"}
//! ← {"ok":true,"asset":"…","probe":{ProbeInfo…},"bytes":N}  + N bytes (.lcsp)
//! ← {"ok":false,"asset":"…","error":"…"}
//! ```
//!
//! A connection serves any number of requests, one after another; a client wanting more at once opens more.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::media::ProbeInfo;

mod build;
pub use build::immich_builder;

pub const PROTOCOL: u64 = 1;
pub const DEFAULT_PORT: u16 = 7991;
/// Longest message line (a probe carries the file's XMP packet).
pub const MAX_LINE: usize = 64 * 1024;
/// Largest proxy accepted (they are about 1 MB).
pub const MAX_PAYLOAD: u64 = 64 * 1024 * 1024;
/// Shortest token a server accepts.
pub const MIN_TOKEN: usize = 16;
/// Consecutive failures after which a client stops asking.
pub const MAX_FAILURES: u32 = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// One photo, described and proxied by the server.
#[derive(Clone, Debug)]
pub struct Built {
    pub probe: ProbeInfo,
    pub smart: Vec<u8>,
}

/// What a server builds an asset with (an asset id in, its probe and smart preview out).
pub type Builder = Arc<dyn Fn(&str) -> Result<Built, String> + Send + Sync>;

/// What a server said in its greeting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub v: u64,
    pub encode: u64,
    pub look: u64,
    pub threads: u64,
}

fn read_line(r: &mut impl BufRead) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    let n = r.by_ref().take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "message line too long or truncated"));
    }
    String::from_utf8(buf).map(Some).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "message is not UTF-8"))
}

/// Constant-time comparison (the token must not leak through timing).
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut d = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        d |= (a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0)) as usize;
    }
    d == 0
}

fn threads() -> u64 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u64)
}

/// Serve `builder` on `listener` until it fails or `stop` is set (checked between connections). At most
/// `max_connections` at once; more are refused with "server busy".
pub fn serve(listener: TcpListener, token: String, builder: Builder, max_connections: usize, stop: Arc<AtomicBool>) -> std::io::Result<()> {
    if token.trim().chars().count() < MIN_TOKEN {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("the token must be at least {MIN_TOKEN} characters")));
    }
    let active = Arc::new(AtomicUsize::new(0));
    let token = Arc::new(token);
    for conn in listener.incoming() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(mut stream) = conn else { continue };
        let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "?".into());
        if active.load(Ordering::SeqCst) >= max_connections.max(1) {
            let _ = writeln!(stream, "{}", json!({"ok": false, "error": "server busy"}));
            log::info!("preview server: {peer} refused (busy)");
            continue;
        }
        active.fetch_add(1, Ordering::SeqCst);
        let (b, t, n) = (builder.clone(), token.clone(), active.clone());
        let spawned = std::thread::Builder::new().name(format!("preview {peer}")).spawn(move || {
            if let Err(e) = handle(stream, &t, &*b) {
                log::info!("preview server: {peer}: {e}");
            }
            n.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            active.fetch_sub(1, Ordering::SeqCst);
            log::warn!("preview server: could not start a connection thread");
        }
    }
    Ok(())
}

fn handle(stream: TcpStream, token: &str, builder: &(dyn Fn(&str) -> Result<Built, String> + Send + Sync)) -> std::io::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(READ_TIMEOUT))?;
    stream.set_nodelay(true)?;
    let mut r = BufReader::new(stream.try_clone()?);
    let mut w = stream;
    let fail = |w: &mut TcpStream, asset: Option<&str>, msg: &str| writeln!(w, "{}", json!({"ok": false, "asset": asset, "error": msg}));
    while let Some(line) = read_line(&mut r)? {
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            fail(&mut w, None, "malformed request")?;
            break;
        };
        if req.get("v").and_then(Value::as_u64) != Some(PROTOCOL) {
            fail(&mut w, None, &format!("protocol version {PROTOCOL} only"))?;
            break;
        }
        if !same(req.get("token").and_then(Value::as_str).unwrap_or(""), token) {
            fail(&mut w, None, "bad token")?;
            break;
        }
        match req.get("op").and_then(Value::as_str) {
            Some("hello") => writeln!(
                w,
                "{}",
                json!({"ok": true, "v": PROTOCOL, "encode": crate::smart::ENCODE_VERSION, "look": crate::camera_preview::LOOK_VERSION, "threads": threads()})
            )?,
            Some("build") => {
                let Some(asset) = req.get("asset").and_then(Value::as_str).filter(|a| !a.is_empty() && a.len() <= 128) else {
                    fail(&mut w, None, "missing asset id")?;
                    break;
                };
                match builder(asset) {
                    Ok(b) if b.smart.len() as u64 <= MAX_PAYLOAD => {
                        writeln!(w, "{}", json!({"ok": true, "asset": asset, "probe": b.probe, "bytes": b.smart.len()}))?;
                        w.write_all(&b.smart)?;
                    }
                    Ok(_) => fail(&mut w, Some(asset), "the smart preview is too large")?,
                    Err(e) => fail(&mut w, Some(asset), &e)?,
                }
            }
            _ => {
                fail(&mut w, None, "unknown op")?;
                break;
            }
        }
        w.flush()?;
    }
    Ok(())
}

/// A preview server's client. Every request opens its own connection, so one client can be shared by threads.
#[derive(Debug)]
pub struct Client {
    addr: String,
    token: String,
    failures: AtomicU32,
}

impl Client {
    pub fn new(addr: &str, token: &str) -> Self {
        Client { addr: addr.to_string(), token: token.to_string(), failures: AtomicU32::new(0) }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Whether asking is still worth it: false after [`MAX_FAILURES`] failures in a row (a success resets them).
    pub fn usable(&self) -> bool {
        self.failures.load(Ordering::Relaxed) < MAX_FAILURES
    }

    fn counted<T>(&self, r: Result<T, String>) -> Result<T, String> {
        match &r {
            Ok(_) => self.failures.store(0, Ordering::Relaxed),
            Err(_) => {
                self.failures.fetch_add(1, Ordering::Relaxed);
            }
        }
        r
    }

    fn request(&self, op: &str, asset: Option<&str>) -> Result<(Value, BufReader<TcpStream>), String> {
        let addr =
            self.addr.to_socket_addrs().map_err(|e| format!("{}: {e}", self.addr))?.next().ok_or_else(|| format!("{}: no address", self.addr))?;
        let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| format!("{}: {e}", self.addr))?;
        stream.set_read_timeout(Some(READ_TIMEOUT)).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(READ_TIMEOUT)).map_err(|e| e.to_string())?;
        let mut w = stream.try_clone().map_err(|e| e.to_string())?;
        let mut req = json!({"v": PROTOCOL, "token": self.token, "op": op});
        if let Some(a) = asset {
            req["asset"] = json!(a);
        }
        writeln!(w, "{req}").and_then(|()| w.flush()).map_err(|e| format!("{}: {e}", self.addr))?;
        let mut r = BufReader::new(stream);
        let line = read_line(&mut r)
            .map_err(|e| format!("{}: {e}", self.addr))?
            .ok_or_else(|| format!("{}: the server closed the connection", self.addr))?;
        let v: Value = serde_json::from_str(&line).map_err(|e| format!("{}: malformed answer: {e}", self.addr))?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            let e = v.get("error").and_then(Value::as_str).unwrap_or("refused");
            return Err(format!("{}: {e}", self.addr));
        }
        Ok((v, r))
    }

    /// Greet the server and check that it builds proxies exactly as this build does.
    pub fn hello(&self) -> Result<Hello, String> {
        let r = self.request("hello", None).and_then(|(v, _)| {
            let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
            let h = Hello { v: n("v"), encode: n("encode"), look: n("look"), threads: n("threads").max(1) };
            let ours = (PROTOCOL, crate::smart::ENCODE_VERSION, u64::from(crate::camera_preview::LOOK_VERSION));
            if (h.v, h.encode, h.look) != ours {
                return Err(format!(
                    "{}: the server builds smart previews of another version (protocol {}, encoding {}, look {}; this build {}, {}, {}): update both from the same revision",
                    self.addr, h.v, h.encode, h.look, ours.0, ours.1, ours.2
                ));
            }
            Ok(h)
        });
        self.counted(r)
    }

    /// Asset `asset`'s probe and smart preview.
    pub fn build(&self, asset: &str) -> Result<Built, String> {
        let r = self.request("build", Some(asset)).and_then(|(v, mut r)| {
            let bytes = v.get("bytes").and_then(Value::as_u64).ok_or_else(|| format!("{}: answer without a payload size", self.addr))?;
            if bytes > MAX_PAYLOAD {
                return Err(format!("{}: a smart preview of {bytes} bytes is too large", self.addr));
            }
            let probe: ProbeInfo =
                serde_json::from_value(v.get("probe").cloned().unwrap_or(Value::Null)).map_err(|e| format!("{}: malformed probe: {e}", self.addr))?;
            let mut smart = Vec::new();
            r.by_ref().take(bytes).read_to_end(&mut smart).map_err(|e| format!("{}: {e}", self.addr))?;
            if smart.len() as u64 != bytes {
                return Err(format!("{}: the smart preview was cut short", self.addr));
            }
            Ok(Built { probe, smart })
        });
        self.counted(r)
    }
}

#[cfg(test)]
mod tests;
