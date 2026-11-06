use std::io::{Cursor, Read};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use qrcode::QrCode;
use qrcode::render::svg;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tiny_http::{Header, ListenAddr, Method, Request, Response, Server, StatusCode};

use crate::import::{self, AppState};

const PREFERRED_PORT: u16 = 8787;
const PORT_ATTEMPTS: u16 = 10;
pub const IDLE_MINUTES: u64 = 15;
const MAX_FILE_BYTES: usize = 25 * 1024 * 1024;
const MULTIPART_SLACK: usize = 64 * 1024;
const PAGE: &str = include_str!("phone_page.html");

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhoneAddress {
    pub name: String,
    pub ip: String,
    pub url: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhoneDropInfo {
    pub port: u16,
    pub token: String,
    pub addresses: Vec<PhoneAddress>,
    pub primary_url: String,
    pub qr_svg: String,
    pub idle_minutes: u64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PhoneNotice {
    name: String,
}

#[derive(Clone, Serialize)]
struct PhoneClosed {
    reason: String,
}

struct Session {
    generation: u64,
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
    idle: Option<JoinHandle<()>>,
}

struct Shared {
    stop: AtomicBool,
    last_activity: Mutex<Instant>,
    wake: Condvar,
    connected: AtomicBool,
    servers: Mutex<Vec<Arc<Server>>>,
}

pub struct PhoneDrop {
    inner: Mutex<Option<Session>>,
}

impl PhoneDrop {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }
}

impl Default for PhoneDrop {
    fn default() -> Self {
        Self::new()
    }
}

pub fn start(app: &AppHandle) -> Result<PhoneDropInfo, String> {
    let interfaces = lan_interfaces().map_err(|error| error.to_string())?;
    if interfaces.is_empty() {
        return Err(
            "Tuto could not find a local network address. Join the same Wi-Fi as the phone, then try again."
                .into(),
        );
    }
    spawn_drop(
        app,
        interfaces,
        PREFERRED_PORT,
        PORT_ATTEMPTS,
        Duration::from_secs(IDLE_MINUTES * 60),
        Duration::from_secs(5),
        MAX_FILE_BYTES,
    )
}

pub fn stop(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Ok(mut guard) = state.phone.inner.lock() else {
        return;
    };
    if let Some(session) = guard.take() {
        drop(guard);
        session.shutdown();
    }
}

fn close_generation(app: &AppHandle, generation: u64) -> bool {
    let state = app.state::<AppState>();
    let Ok(mut guard) = state.phone.inner.lock() else {
        return false;
    };
    if guard.as_ref().map(|session| session.generation) != Some(generation) {
        return false;
    }
    let Some(session) = guard.take() else {
        return false;
    };
    drop(guard);
    session.shutdown();
    true
}

fn spawn_drop(
    app: &AppHandle,
    interfaces: Vec<LanAddr>,
    port_start: u16,
    port_attempts: u16,
    idle: Duration,
    poll: Duration,
    max_file_bytes: usize,
) -> Result<PhoneDropInfo, String> {
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    let app_for_files = app.clone();
    let on_file: FileHandler = Arc::new(move |paper| accept_upload(&app_for_files, paper));
    let app_for_seen = app.clone();
    let on_seen: Notify = Arc::new(move || {
        let _ = app_for_seen.emit("phone-drop-seen", ());
    });
    let app_for_idle = app.clone();
    let on_idle: Notify = Arc::new(move || {
        if close_generation(&app_for_idle, generation) {
            let _ = app_for_idle.emit(
                "phone-drop-closed",
                PhoneClosed {
                    reason: "idle".into(),
                },
            );
        }
    });
    let started = open_server(OpenConfig {
        bind: interfaces,
        port_start,
        port_attempts,
        idle,
        poll,
        max_file_bytes,
        on_file,
        on_seen,
        on_idle,
        generation,
    })?;
    replace_session(app, started.session)?;
    Ok(started.info)
}

fn replace_session(app: &AppHandle, session: Session) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut guard = state.phone.inner.lock().map_err(|error| error.to_string())?;
    let previous = guard.replace(session);
    drop(guard);
    if let Some(previous) = previous {
        previous.shutdown();
    }
    Ok(())
}

fn accept_upload(app: &AppHandle, paper: UploadedPaper) -> Result<(), String> {
    let state = app.state::<AppState>();
    let dir = state
        .data_dir
        .join("phone-inbox")
        .join(format!("{:x}", paper.nonce));
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let path = dir.join(&paper.filename);
    std::fs::write(&path, &paper.bytes).map_err(|error| error.to_string())?;
    let imported = import::import_one(app, &state, &path);
    let _ = std::fs::remove_dir_all(&dir);
    let summary = imported?;
    let _ = app.emit(
        "phone-drop",
        PhoneNotice {
            name: summary.original_name,
        },
    );
    Ok(())
}

struct OpenConfig {
    bind: Vec<LanAddr>,
    port_start: u16,
    port_attempts: u16,
    idle: Duration,
    poll: Duration,
    max_file_bytes: usize,
    on_file: FileHandler,
    on_seen: Notify,
    on_idle: Notify,
    generation: u64,
}

type FileHandler = Arc<dyn Fn(UploadedPaper) -> Result<(), String> + Send + Sync>;
type Notify = Arc<dyn Fn() + Send + Sync>;

struct UploadedPaper {
    filename: String,
    bytes: Vec<u8>,
    nonce: u64,
}

struct Started {
    session: Session,
    info: PhoneDropInfo,
}

fn open_server(config: OpenConfig) -> Result<Started, String> {
    let token = new_token()?;
    let (port, servers) = bind_listeners(&config.bind, config.port_start, config.port_attempts)?;
    let addresses = config
        .bind
        .iter()
        .map(|addr| PhoneAddress {
            name: addr.name.clone(),
            ip: addr.ip.to_string(),
            url: drop_url(addr.ip, port, &token),
        })
        .collect::<Vec<_>>();
    let primary_url = addresses
        .first()
        .map(|addr| addr.url.clone())
        .ok_or_else(|| "Tuto could not find a local network address.".to_string())?;
    let info = PhoneDropInfo {
        port,
        token: token.clone(),
        addresses,
        primary_url: primary_url.clone(),
        qr_svg: qr_svg(&primary_url).unwrap_or_default(),
        idle_minutes: IDLE_MINUTES,
    };
    let servers = servers.into_iter().map(Arc::new).collect::<Vec<_>>();
    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
        last_activity: Mutex::new(Instant::now()),
        wake: Condvar::new(),
        connected: AtomicBool::new(false),
        servers: Mutex::new(servers.clone()),
    });
    let ctx = Arc::new(Handler {
        token,
        max_file_bytes: config.max_file_bytes,
        on_file: config.on_file,
        on_seen: config.on_seen,
        shared: Arc::clone(&shared),
        nonce: AtomicU64::new(1),
    });
    let mut workers = Vec::new();
    for server in servers {
        let ctx = Arc::clone(&ctx);
        workers.push(
            thread::Builder::new()
                .name("tuto-phone".into())
                .spawn(move || worker_loop(server, ctx))
                .map_err(|error| error.to_string())?,
        );
    }
    let idle_shared = Arc::clone(&shared);
    let on_idle = config.on_idle;
    let idle_for = config.idle;
    let poll = config.poll;
    let idle = thread::Builder::new()
        .name("tuto-phone-idle".into())
        .spawn(move || idle_loop(idle_shared, idle_for, poll, on_idle))
        .map_err(|error| error.to_string())?;
    Ok(Started {
        session: Session {
            generation: config.generation,
            shared,
            workers,
            idle: Some(idle),
        },
        info,
    })
}

fn worker_loop(server: Arc<Server>, ctx: Arc<Handler>) {
    loop {
        if ctx.shared.stop.load(Ordering::SeqCst) {
            break;
        }
        let request = match server.recv() {
            Ok(request) => request,
            Err(_) => break,
        };
        if ctx.shared.stop.load(Ordering::SeqCst) {
            break;
        }
        note_activity(&ctx.shared);
        handle_request(request, &ctx);
        note_activity(&ctx.shared);
    }
}

fn idle_loop(shared: Arc<Shared>, idle_for: Duration, poll: Duration, on_idle: Notify) {
    loop {
        let guard = shared
            .last_activity
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let elapsed = guard.elapsed();
        if elapsed >= idle_for {
            drop(guard);
            if !shared.stop.swap(true, Ordering::SeqCst) {
                unblock(&shared);
                on_idle();
            }
            return;
        }
        let wait = poll.min(idle_for.saturating_sub(elapsed));
        let (guard, _) = shared
            .wake
            .wait_timeout(guard, wait)
            .unwrap_or_else(|poison| poison.into_inner());
        drop(guard);
    }
}

fn note_activity(shared: &Shared) {
    if let Ok(mut instant) = shared.last_activity.lock() {
        *instant = Instant::now();
    }
}

fn unblock(shared: &Shared) {
    if let Ok(servers) = shared.servers.lock() {
        for server in servers.iter() {
            server.unblock();
        }
    }
}

impl Session {
    fn request_stop(&self) {
        if let Ok(guard) = self.shared.last_activity.lock() {
            self.shared.stop.store(true, Ordering::SeqCst);
            self.shared.wake.notify_all();
            drop(guard);
        } else {
            self.shared.stop.store(true, Ordering::SeqCst);
            self.shared.wake.notify_all();
        }
        unblock(&self.shared);
    }

    fn shutdown(self) {
        self.request_stop();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.request_stop();
        let here = thread::current().id();
        for worker in self.workers.drain(..) {
            if worker.thread().id() != here {
                let _ = worker.join();
            }
        }
        if let Some(idle) = self.idle.take() {
            if idle.thread().id() != here {
                let _ = idle.join();
            }
        }
    }
}

struct Handler {
    token: String,
    max_file_bytes: usize,
    on_file: FileHandler,
    on_seen: Notify,
    shared: Arc<Shared>,
    nonce: AtomicU64,
}

fn handle_request(mut request: Request, ctx: &Handler) {
    let response = match respond(&mut request, ctx) {
        Ok(response) => response,
        Err(error) => json_response(
            error.status,
            &format!(r#"{{"ok":false,"error":{}}}"#, json_string(&error.message)),
        ),
    };
    let _ = request.respond(response);
}

#[derive(Debug)]
struct HttpError {
    status: u16,
    message: String,
}

enum Verb {
    Get,
    Post,
    Other,
}

fn respond(request: &mut Request, ctx: &Handler) -> Result<Response<Cursor<Vec<u8>>>, HttpError> {
    let path = request.url().to_string();
    let verb = match request.method() {
        Method::Get => Verb::Get,
        Method::Post => Verb::Post,
        _ => Verb::Other,
    };
    let content_type = header_value(request, "Content-Type");
    let mut body = Vec::new();
    let limit = ctx.max_file_bytes.saturating_add(MULTIPART_SLACK);
    request
        .as_reader()
        .take((limit + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|_| HttpError::new(400, "Tuto could not read that upload."))?;
    if body.len() > limit {
        return Err(HttpError::new(413, "This file is larger than 25 MB."));
    }
    let Some(token) = route_token(&path) else {
        return Ok(text_response(404, "Not found."));
    };
    if !tokens_equal(token, &ctx.token) {
        return Ok(text_response(404, "Not found."));
    }
    if !ctx.shared.connected.swap(true, Ordering::SeqCst) {
        (ctx.on_seen)();
    }
    match verb {
        Verb::Get => Ok(html_response(PAGE)),
        Verb::Post => {
            let files = parse_uploads(&content_type.unwrap_or_default(), &body, ctx.max_file_bytes)?;
            if files.is_empty() {
                return Err(HttpError::new(400, "Choose a photo or a PDF."));
            }
            let mut saved = Vec::new();
            for file in files {
                if sniff_kind(&file.bytes).is_none() {
                    let message = if is_heic(&file.bytes) {
                        "This photo is HEIC. Set the camera to JPEG, or choose a JPEG, PNG, or PDF."
                    } else {
                        "Tuto reads JPEG, PNG, WebP, GIF, TIFF, and PDF."
                    };
                    return Err(HttpError::new(415, message));
                }
                let kind = sniff_kind(&file.bytes).unwrap_or("jpg");
                let filename = safe_filename(&file.filename, kind);
                let nonce = ctx.nonce.fetch_add(1, Ordering::Relaxed);
                (ctx.on_file)(UploadedPaper {
                    filename: filename.clone(),
                    bytes: file.bytes,
                    nonce,
                })
                .map_err(|error| HttpError::new(500, error))?;
                saved.push(filename);
            }
            let name = saved.join(", ");
            Ok(json_response(
                200,
                &format!(r#"{{"ok":true,"name":{}}}"#, json_string(&name)),
            ))
        }
        Verb::Other => Ok(text_response(404, "Not found.")),
    }
}

impl HttpError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

fn html_response(body: &str) -> Response<Cursor<Vec<u8>>> {
    with_headers(
        Response::from_data(body.as_bytes().to_vec()),
        200,
        &[
            ("Content-Type", "text/html; charset=utf-8"),
            ("Cache-Control", "no-store"),
            ("X-Content-Type-Options", "nosniff"),
            ("Referrer-Policy", "no-referrer"),
            (
                "Content-Security-Policy",
                "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'self'",
            ),
        ],
    )
}

fn text_response(status: u16, body: &str) -> Response<Cursor<Vec<u8>>> {
    with_headers(
        Response::from_data(body.as_bytes().to_vec()),
        status,
        &[
            ("Content-Type", "text/plain; charset=utf-8"),
            ("Cache-Control", "no-store"),
        ],
    )
}

fn json_response(status: u16, body: &str) -> Response<Cursor<Vec<u8>>> {
    with_headers(
        Response::from_data(body.as_bytes().to_vec()),
        status,
        &[
            ("Content-Type", "application/json; charset=utf-8"),
            ("Cache-Control", "no-store"),
        ],
    )
}

fn with_headers(
    response: Response<Cursor<Vec<u8>>>,
    status: u16,
    headers: &[(&str, &str)],
) -> Response<Cursor<Vec<u8>>> {
    let mut response = response.with_status_code(StatusCode(status));
    for (name, value) in headers {
        if let Ok(header) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            response = response.with_header(header);
        }
    }
    response
}

fn header_value(request: &Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str().trim().to_string())
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LanAddr {
    name: String,
    ip: Ipv4Addr,
}

fn lan_interfaces() -> std::io::Result<Vec<LanAddr>> {
    let mut raw = Vec::new();
    for iface in if_addrs::get_if_addrs()? {
        let if_addrs::IfAddr::V4(addr) = iface.addr else {
            continue;
        };
        raw.push((iface.name, addr.ip));
    }
    Ok(candidate_addresses(&raw))
}

fn candidate_addresses(ifaces: &[(String, Ipv4Addr)]) -> Vec<LanAddr> {
    let mut found = Vec::new();
    for (name, ip) in ifaces {
        if !is_private_lan(*ip) || skip_interface(name) {
            continue;
        }
        if found.iter().any(|existing: &LanAddr| existing.ip == *ip) {
            continue;
        }
        found.push(LanAddr {
            name: name.clone(),
            ip: *ip,
        });
    }
    found.sort_by(|left, right| {
        interface_rank(&left.name)
            .cmp(&interface_rank(&right.name))
            .then(left.ip.octets().cmp(&right.ip.octets()))
    });
    found
}

fn is_private_lan(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    match a {
        10 => true,
        172 => (16..=31).contains(&b),
        192 => b == 168,
        _ => false,
    }
}

fn skip_interface(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "docker", "veth", "br-", "virbr", "tun", "tap", "wg", "tailscale", "zt", "flannel", "cni",
        "vboxnet", "vmnet", "lo",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

fn interface_rank(name: &str) -> u8 {
    let name = name.to_ascii_lowercase();
    if name.starts_with("wl") || name.starts_with("wifi") {
        0
    } else if name.starts_with("en") || name.starts_with("eth") {
        1
    } else {
        2
    }
}

fn bind_listeners(addrs: &[LanAddr], port_start: u16, attempts: u16) -> Result<(u16, Vec<Server>), String> {
    if addrs.is_empty() {
        return Err("Tuto could not find a local network address.".into());
    }
    for addr in addrs {
        if !is_private_lan(addr.ip) && !addr.ip.is_loopback() {
            return Err("Phone drop only listens on local network addresses.".into());
        }
    }
    if port_start == 0 {
        if addrs.len() != 1 {
            return Err("Phone drop could not listen.".into());
        }
        let server = Server::http(SocketAddr::from((addrs[0].ip, 0)))
            .map_err(|error| format!("Phone drop could not listen. {error}"))?;
        let port = match server.server_addr() {
            ListenAddr::IP(addr) => addr.port(),
            #[cfg(unix)]
            ListenAddr::Unix(_) => {
                return Err("Phone drop could not listen.".into());
            }
        };
        return Ok((port, vec![server]));
    }
    let mut last_error = String::from("the port is in use");
    let attempts = attempts.max(1);
    for offset in 0..attempts {
        let Some(port) = port_start.checked_add(offset) else {
            break;
        };
        let mut servers = Vec::new();
        let mut failed = false;
        for addr in addrs {
            match Server::http(SocketAddr::from((addr.ip, port))) {
                Ok(server) => servers.push(server),
                Err(error) => {
                    last_error = error.to_string();
                    failed = true;
                    break;
                }
            }
        }
        if !failed {
            return Ok((port, servers));
        }
    }
    Err(format!(
        "Phone drop could not open a local port (tried {port_start}–{}). {last_error}",
        port_start.saturating_add(attempts - 1)
    ))
}

fn drop_url(ip: Ipv4Addr, port: u16, token: &str) -> String {
    format!("http://{ip}:{port}/drop/{token}")
}

fn qr_svg(url: &str) -> Result<String, String> {
    let code = QrCode::new(url.as_bytes()).map_err(|error| error.to_string())?;
    let image = code
        .render::<svg::Color>()
        .min_dimensions(232, 232)
        .dark_color(svg::Color("#1c1915"))
        .light_color(svg::Color("#faf7f1"))
        .build();
    let start = image.find("<svg").unwrap_or(0);
    let svg = image[start..].to_string();
    if !svg.starts_with("<svg") || svg.to_ascii_lowercase().contains("<script") {
        return Err("Could not draw a QR code.".into());
    }
    Ok(svg)
}

fn new_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|error| error.to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn tokens_equal(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.bytes().zip(right.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn route_token(path: &str) -> Option<&str> {
    let path = path.split('?').next().unwrap_or(path);
    let path = path.trim_end_matches('/');
    let token = path.strip_prefix("/drop/")?;
    if token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(token)
    } else {
        None
    }
}

#[derive(Debug)]
struct RawFile {
    filename: String,
    bytes: Vec<u8>,
}

fn parse_uploads(content_type: &str, body: &[u8], max_file_bytes: usize) -> Result<Vec<RawFile>, HttpError> {
    let boundary = multipart_boundary(content_type)
        .ok_or_else(|| HttpError::new(400, "Choose a photo or a PDF."))?;
    let mut files = Vec::new();
    for part in split_parts(body, &boundary) {
        let Some((headers, content)) = split_part(part) else {
            continue;
        };
        if content.len() > max_file_bytes {
            return Err(HttpError::new(413, "This file is larger than 25 MB."));
        }
        let disposition = header_from_block(headers, "content-disposition").unwrap_or_default();
        let Some(filename) = filename_from_disposition(&disposition) else {
            continue;
        };
        if content.is_empty() {
            return Err(HttpError::new(400, "That file was empty."));
        }
        files.push(RawFile {
            filename,
            bytes: content.to_vec(),
        });
    }
    Ok(files)
}

fn multipart_boundary(content_type: &str) -> Option<String> {
    for piece in content_type.split(';') {
        let Some(value) = piece.trim().strip_prefix("boundary=") else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        if value.is_empty() || value.len() > 70 || value.as_bytes().iter().any(|byte| byte.is_ascii_whitespace())
        {
            return None;
        }
        return Some(value.to_string());
    }
    None
}

fn split_parts<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let marker = format!("--{boundary}");
    let marker = marker.as_bytes();
    let mut starts = Vec::new();
    let mut index = 0;
    while index + marker.len() <= body.len() {
        if &body[index..index + marker.len()] == marker {
            starts.push(index);
            index += marker.len();
        } else {
            index += 1;
        }
    }
    let mut parts = Vec::new();
    for (offset, start) in starts.iter().enumerate() {
        let after = start + marker.len();
        if body.get(after..after + 2) == Some(b"--") {
            break;
        }
        let header_at = if body.get(after..after + 2) == Some(b"\r\n") {
            after + 2
        } else {
            after
        };
        let end = starts.get(offset + 1).copied().unwrap_or(body.len());
        if header_at < end {
            parts.push(&body[header_at..end]);
        }
    }
    parts
}

fn split_part(part: &[u8]) -> Option<(&[u8], &[u8])> {
    let split = part.windows(4).position(|window| window == b"\r\n\r\n")?;
    let headers = &part[..split];
    let mut content = &part[split + 4..];
    if let Some(stripped) = content.strip_suffix(b"\r\n") {
        content = stripped;
    }
    Some((headers, content))
}

fn header_from_block(headers: &[u8], name: &str) -> Option<String> {
    let text = String::from_utf8_lossy(headers);
    for line in text.split("\r\n") {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case(name) {
            return Some(value.trim().to_string());
        }
    }
    None
}

fn filename_from_disposition(value: &str) -> Option<String> {
    if let Some(star) = value.split(';').find_map(|piece| {
        piece
            .trim()
            .strip_prefix("filename*=")
            .map(str::trim)
            .map(str::to_string)
    }) {
        let encoded = star.split_once("''").map(|(_, rest)| rest).unwrap_or(star.as_str());
        let decoded = percent_decode(encoded.trim_matches('"'));
        if !decoded.is_empty() {
            return Some(decoded);
        }
    }
    for piece in value.split(';') {
        let Some(raw) = piece.trim().strip_prefix("filename=") else {
            continue;
        };
        let name = if let Some(quoted) = raw.trim().strip_prefix('"') {
            unescape_quoted(quoted)
        } else {
            raw.trim().to_string()
        };
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

fn unescape_quoted(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '"' {
            break;
        }
        if ch == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or(""),
                16,
            ) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn safe_filename(raw: &str, ext: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("").trim();
    let mut cleaned = String::new();
    for ch in base.chars() {
        if cleaned.len() >= 80 {
            break;
        }
        if ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
            continue;
        }
        cleaned.push(ch);
    }
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        return format!("photo.{ext}");
    }
    let stem = cleaned
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(cleaned.as_str());
    let stem = if stem.is_empty() { "photo" } else { stem };
    format!("{stem}.{ext}")
}

fn sniff_kind(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"%PDF") {
        return Some("pdf");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpg");
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n']) {
        return Some("png");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    if bytes.starts_with(&[b'I', b'I', b'*', 0]) || bytes.starts_with(&[b'M', b'M', 0, b'*']) {
        return Some("tiff");
    }
    None
}

fn is_heic(bytes: &[u8]) -> bool {
    bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && matches!(&bytes[8..12], b"heic" | b"heix" | b"heif" | b"mif1" | b"hevc")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::Mutex as StdMutex;

    fn addr(name: &str, ip: &str) -> (String, Ipv4Addr) {
        (name.into(), ip.parse().unwrap())
    }

    #[test]
    fn lan_candidates_skip_public_loopback_and_docker() {
        let found = candidate_addresses(&[
            addr("lo", "127.0.0.1"),
            addr("eth0", "8.8.8.8"),
            addr("docker0", "172.17.0.1"),
            addr("enp0s3", "172.30.0.2"),
            addr("wlan0", "192.168.1.20"),
            addr("wlan0", "192.168.1.20"),
            addr("veth123", "10.1.1.1"),
            addr("tailscale0", "100.64.0.2"),
            addr("en0", "169.254.1.1"),
        ]);
        let ips: Vec<_> = found.iter().map(|item| item.ip.to_string()).collect();
        assert_eq!(ips, vec!["192.168.1.20".to_string(), "172.30.0.2".to_string()]);
        assert!(found[0].name == "wlan0");
    }

    #[test]
    fn private_ranges_are_the_only_bind_targets() {
        assert!(is_private_lan("10.0.0.1".parse().unwrap()));
        assert!(is_private_lan("172.16.0.1".parse().unwrap()));
        assert!(is_private_lan("172.31.255.255".parse().unwrap()));
        assert!(!is_private_lan("172.15.0.1".parse().unwrap()));
        assert!(!is_private_lan("172.32.0.1".parse().unwrap()));
        assert!(is_private_lan("192.168.0.1".parse().unwrap()));
        assert!(!is_private_lan("192.169.0.1".parse().unwrap()));
        assert!(!is_private_lan("127.0.0.1".parse().unwrap()));
        assert!(!is_private_lan("0.0.0.0".parse().unwrap()));
        assert!(!is_private_lan("169.254.1.1".parse().unwrap()));
        let public = vec![LanAddr {
            name: "eth0".into(),
            ip: "1.2.3.4".parse().unwrap(),
        }];
        assert!(bind_listeners(&public, 8787, 1).is_err());
    }

    #[test]
    fn route_requires_the_session_token() {
        let token = "ab".repeat(32);
        assert_eq!(route_token(&format!("/drop/{token}")), Some(token.as_str()));
        assert_eq!(route_token(&format!("/drop/{token}/")), Some(token.as_str()));
        assert!(route_token("/drop/short").is_none());
        assert!(route_token("/").is_none());
        assert!(route_token(&format!("/drop/{token}/extra")).is_none());
        assert!(tokens_equal(&token, &token));
        assert!(!tokens_equal(&token, &"cd".repeat(32)));
        assert!(!tokens_equal("abc", "ab"));
    }

    #[test]
    fn magic_bytes_and_names() {
        assert_eq!(sniff_kind(b"%PDF-1.4"), Some("pdf"));
        assert_eq!(sniff_kind(&[0xFF, 0xD8, 0xFF, 0xD9]), Some("jpg"));
        assert_eq!(sniff_kind(b"GIF89a"), Some("gif"));
        assert_eq!(sniff_kind(b"MZ executable"), None);
        assert!(is_heic(b"\0\0\0\x18ftypheic"));
        assert_eq!(safe_filename("../../etc/passwd.jpg", "jpg"), "passwd.jpg");
        assert_eq!(safe_filename("shot.HEIC", "jpg"), "shot.jpg");
        assert_eq!(safe_filename("", "png"), "photo.png");
        assert_eq!(safe_filename("..\\papers\\note.pdf", "pdf"), "note.pdf");
    }

    #[test]
    fn multipart_extracts_one_file_and_rejects_a_fat_one() {
        let body = b"\
--bound\r\n\
Content-Disposition: form-data; name=\"file\"; filename=\"receipt.jpg\"\r\n\
Content-Type: image/jpeg\r\n\
\r\n\
JPEGDATA\r\n\
--bound--\r\n";
        let files = parse_uploads("multipart/form-data; boundary=bound", body, 100).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "receipt.jpg");
        assert_eq!(files[0].bytes, b"JPEGDATA");
        let err = parse_uploads("multipart/form-data; boundary=bound", body, 4).unwrap_err();
        assert_eq!(err.status, 413);
        let star = b"\
--bound\r\n\
Content-Disposition: form-data; name=\"file\"; filename*=UTF-8''caf%C3%A9.png\r\n\
\r\n\
PNG\r\n\
--bound--\r\n";
        let files = parse_uploads("multipart/form-data; boundary=bound", star, 100).unwrap();
        assert_eq!(files[0].filename, "café.png");
    }

    #[test]
    fn phone_page_stays_on_the_device() {
        assert!(PAGE.contains("capture=\"environment\""));
        assert!(PAGE.contains("Take photo"));
        assert!(PAGE.contains("Choose files"));
        assert!(PAGE.contains("25 * 1024 * 1024"));
        let lower = PAGE.to_ascii_lowercase();
        assert!(!lower.contains("http://"));
        assert!(!lower.contains("https://"));
        assert!(!lower.contains("src=\"//"));
    }

    #[test]
    fn qr_is_local_svg() {
        let svg = qr_svg("http://192.168.1.20:8787/drop/ab").unwrap();
        assert!(svg.starts_with("<svg"));
        assert!(!svg.to_ascii_lowercase().contains("<script"));
        assert!(!svg.to_ascii_lowercase().contains("onload"));
    }

    fn tiny_png() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
            0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
            0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
            0xCF, 0xC0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x18, 0xDD, 0x8D, 0xB4, 0x00, 0x00, 0x00,
            0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    struct TestServer {
        info: PhoneDropInfo,
        slot: Arc<StdMutex<Option<Session>>>,
        files: Arc<StdMutex<Vec<(String, Vec<u8>)>>>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            if let Some(session) = self.slot.lock().unwrap().take() {
                session.shutdown();
            }
        }
    }

    fn serve_loopback(max_file_bytes: usize, idle: Duration) -> TestServer {
        let files = Arc::new(StdMutex::new(Vec::new()));
        let slot: Arc<StdMutex<Option<Session>>> = Arc::new(StdMutex::new(None));
        let files_cb = Arc::clone(&files);
        let on_file: FileHandler = Arc::new(move |paper| {
            files_cb
                .lock()
                .unwrap()
                .push((paper.filename, paper.bytes));
            Ok(())
        });
        let slot_idle = Arc::clone(&slot);
        let on_idle: Notify = Arc::new(move || {
            if let Some(session) = slot_idle.lock().unwrap().take() {
                session.shutdown();
            }
        });
        let started = open_server(OpenConfig {
            bind: vec![LanAddr {
                name: "lo".into(),
                ip: Ipv4Addr::LOCALHOST,
            }],
            port_start: 0,
            port_attempts: 1,
            idle,
            poll: Duration::from_millis(40),
            max_file_bytes,
            on_file,
            on_seen: Arc::new(|| {}),
            on_idle,
            generation: 1,
        })
        .unwrap();
        let info = started.info.clone();
        *slot.lock().unwrap() = Some(started.session);
        TestServer { info, slot, files }
    }

    fn curl(args: &[&str]) -> std::process::Output {
        Command::new("curl")
            .args(args)
            .output()
            .expect("curl is installed")
    }

    #[test]
    fn curl_upload_accepts_a_png_and_rejects_the_rest() {
        let server = serve_loopback(64 * 1024, Duration::from_secs(30));
        let dir = std::env::temp_dir().join(format!("tuto-phone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png_path = dir.join("receipt.png");
        let junk_path = dir.join("notes.jpg");
        let fat_path = dir.join("fat.png");
        std::fs::write(&png_path, tiny_png()).unwrap();
        std::fs::write(&junk_path, b"MZ not a photo").unwrap();
        let mut fat = tiny_png();
        fat.extend(std::iter::repeat(0).take(80 * 1024));
        std::fs::write(&fat_path, &fat).unwrap();

        let page = curl(&[
            "-sS",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            &server.info.primary_url,
        ]);
        assert_eq!(String::from_utf8_lossy(&page.stdout).trim(), "200");

        let wrong = curl(&[
            "-sS",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            &format!("http://127.0.0.1:{}/drop/{}", server.info.port, "ab".repeat(32)),
        ]);
        assert_eq!(String::from_utf8_lossy(&wrong.stdout).trim(), "404");

        let other = curl(&[
            "-sS",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            &format!("http://127.0.0.1:{}/", server.info.port),
        ]);
        assert_eq!(String::from_utf8_lossy(&other.stdout).trim(), "404");

        let uploaded = curl(&[
            "-sS",
            "-w",
            "\n%{http_code}",
            "-F",
            &format!("file=@{}", png_path.display()),
            &server.info.primary_url,
        ]);
        let uploaded = String::from_utf8_lossy(&uploaded.stdout);
        assert!(uploaded.contains("\"ok\":true"), "{uploaded}");
        assert!(uploaded.trim().ends_with("200"), "{uploaded}");
        let saved = server.files.lock().unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].0, "receipt.png");
        assert_eq!(saved[0].1, tiny_png());
        drop(saved);

        let rejected = curl(&[
            "-sS",
            "-w",
            "\n%{http_code}",
            "-F",
            &format!("file=@{}", junk_path.display()),
            &server.info.primary_url,
        ]);
        let rejected = String::from_utf8_lossy(&rejected.stdout);
        assert!(rejected.contains("\"ok\":false"), "{rejected}");
        assert!(rejected.trim().ends_with("415"), "{rejected}");

        let too_big = curl(&[
            "-sS",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-F",
            &format!("file=@{}", fat_path.display()),
            &server.info.primary_url,
        ]);
        assert_eq!(String::from_utf8_lossy(&too_big.stdout).trim(), "413");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn idle_server_stops_listening() {
        let server = serve_loopback(1024, Duration::from_millis(120));
        let port = server.info.port;
        thread::sleep(Duration::from_millis(500));
        let probed = curl(&[
            "-sS",
            "--max-time",
            "2",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            &format!("http://127.0.0.1:{port}/drop/{}", "ab".repeat(32)),
        ]);
        assert!(!probed.status.success(), "idle server still accepted traffic");
        assert!(server.slot.lock().unwrap().is_none());
    }
}
