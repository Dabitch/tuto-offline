use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use qrcode::render::svg;
use qrcode::QrCode;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

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
    acceptors: Vec<JoinHandle<()>>,
    idle: Option<JoinHandle<()>>,
}

struct Shared {
    stop: AtomicBool,
    last_activity: Mutex<Instant>,
    wake: Condvar,
    connected: AtomicBool,
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
    let mut guard = state
        .phone
        .inner
        .lock()
        .map_err(|error| error.to_string())?;
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
    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
        last_activity: Mutex::new(Instant::now()),
        wake: Condvar::new(),
        connected: AtomicBool::new(false),
    });
    let ctx = Arc::new(Handler {
        token,
        max_file_bytes: config.max_file_bytes,
        on_file: config.on_file,
        on_seen: config.on_seen,
        shared: Arc::clone(&shared),
        nonce: AtomicU64::new(1),
    });
    let mut acceptors = Vec::new();
    for listener in servers {
        listener
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;
        let ctx = Arc::clone(&ctx);
        let shared = Arc::clone(&shared);
        acceptors.push(
            thread::Builder::new()
                .name("tuto-phone".into())
                .spawn(move || accept_loop(listener, ctx, shared))
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
            acceptors,
            idle: Some(idle),
        },
        info,
    })
}

fn accept_loop(listener: TcpListener, ctx: Arc<Handler>, shared: Arc<Shared>) {
    while !shared.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((sock, _)) => {
                let ctx = Arc::clone(&ctx);
                thread::spawn(move || handle_socket(sock, &ctx));
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(40));
            }
            Err(_) => break,
        }
    }
}

fn handle_socket(mut sock: TcpStream, ctx: &Handler) {
    let _ = sock.set_nodelay(true);
    let _ = sock.set_read_timeout(Some(Duration::from_secs(60)));
    let _ = sock.set_write_timeout(Some(Duration::from_secs(60)));
    let incoming = match read_incoming(
        &mut sock,
        ctx.max_file_bytes.saturating_add(MULTIPART_SLACK),
    ) {
        Ok(incoming) => incoming,
        Err(SocketRead::Tls) => {
            reject_tls(&mut sock);
            return;
        }
        Err(_) => return,
    };
    note_activity(&ctx.shared);
    let reply = dispatch(incoming, ctx);
    let _ = write_reply(&mut sock, &reply);
    let _ = sock.shutdown(Shutdown::Write);
    note_activity(&ctx.shared);
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
    }

    fn shutdown(self) {
        self.request_stop();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.request_stop();
        let here = thread::current().id();
        for acceptor in self.acceptors.drain(..) {
            if acceptor.thread().id() != here {
                let _ = acceptor.join();
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

enum SocketRead {
    Tls,
    Failed,
}

struct Incoming {
    method: String,
    path: String,
    content_type: String,
    body: Vec<u8>,
    too_large: bool,
}

struct Reply {
    status: u16,
    content_type: &'static str,
    extra: &'static [(&'static str, &'static str)],
    body: Vec<u8>,
}

fn read_incoming(sock: &mut TcpStream, limit: usize) -> Result<Incoming, SocketRead> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        let n = sock.read(&mut tmp).map_err(|_| SocketRead::Failed)?;
        if n == 0 {
            return Err(SocketRead::Failed);
        }
        if buf.is_empty() && tmp[0] == 0x16 {
            return Err(SocketRead::Tls);
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 64 * 1024 {
            return Err(SocketRead::Failed);
        }
        if let Some(end) = find_crlf_crlf(&buf) {
            break end;
        }
    };
    let header_bytes = &buf[..header_end];
    let mut rest = buf[header_end + 4..].to_vec();
    let header_text = String::from_utf8_lossy(header_bytes);
    let mut lines = header_text.split("\r\n");
    let Some(request_line) = lines.next() else {
        return Err(SocketRead::Failed);
    };
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let mut headers = Vec::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    let content_type = header_lookup(&headers, "content-type").unwrap_or_default();
    if let Some(length) =
        header_lookup(&headers, "content-length").and_then(|value| value.parse::<usize>().ok())
    {
        if length > limit {
            return Ok(Incoming {
                method,
                path,
                content_type,
                body: Vec::new(),
                too_large: true,
            });
        }
    }
    let expect = header_lookup(&headers, "expect").unwrap_or_default();
    if expect.to_ascii_lowercase().contains("100-continue") {
        sock.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .map_err(|_| SocketRead::Failed)?;
        sock.flush().map_err(|_| SocketRead::Failed)?;
    }
    if let Some(length) =
        header_lookup(&headers, "content-length").and_then(|value| value.parse::<usize>().ok())
    {
        let body = read_exact(sock, &mut rest, length).map_err(|_| SocketRead::Failed)?;
        return Ok(Incoming {
            method,
            path,
            content_type,
            body,
            too_large: false,
        });
    }
    if header_lookup(&headers, "transfer-encoding")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("chunked")
    {
        return match read_chunked(sock, &mut rest, limit) {
            Ok(body) => Ok(Incoming {
                method,
                path,
                content_type,
                body,
                too_large: false,
            }),
            Err(error) if error.kind() == ErrorKind::FileTooLarge => Ok(Incoming {
                method,
                path,
                content_type,
                body: Vec::new(),
                too_large: true,
            }),
            Err(_) => Err(SocketRead::Failed),
        };
    }
    Ok(Incoming {
        method,
        path,
        content_type,
        body: Vec::new(),
        too_large: false,
    })
}

#[derive(Debug)]
struct HttpError {
    status: u16,
    message: String,
}

fn dispatch(incoming: Incoming, ctx: &Handler) -> Reply {
    if incoming.too_large {
        return json_reply(
            413,
            r#"{"ok":false,"error":"This file is larger than 25 MB."}"#,
        );
    }
    let Some(token) = route_token(&incoming.path) else {
        return text_reply(404, "Not found.");
    };
    if !tokens_equal(token, &ctx.token) {
        return text_reply(404, "Not found.");
    }
    if !ctx.shared.connected.swap(true, Ordering::SeqCst) {
        (ctx.on_seen)();
    }
    match incoming.method.as_str() {
        "GET" | "HEAD" => html_reply(PAGE),
        "POST" => accept_post(incoming, ctx),
        _ => text_reply(404, "Not found."),
    }
}

fn accept_post(incoming: Incoming, ctx: &Handler) -> Reply {
    let files = match parse_uploads(&incoming.content_type, &incoming.body, ctx.max_file_bytes) {
        Ok(files) => files,
        Err(error) => return json_reply(error.status, &json_error(&error.message)),
    };
    if files.is_empty() {
        return json_reply(400, r#"{"ok":false,"error":"Choose a photo or a PDF."}"#);
    }
    let mut saved = Vec::new();
    let mut papers = Vec::new();
    for file in files {
        if sniff_kind(&file.bytes).is_none() {
            let message = if is_heic(&file.bytes) {
                "This photo is HEIC. On the phone, set the camera to JPEG and take it again."
            } else {
                "Tuto reads JPEG, PNG, WebP, GIF, TIFF, and PDF."
            };
            return json_reply(415, &json_error(message));
        }
        let kind = sniff_kind(&file.bytes).unwrap_or("jpg");
        let filename = safe_filename(&file.filename, kind);
        let nonce = ctx.nonce.fetch_add(1, Ordering::Relaxed);
        papers.push(UploadedPaper {
            filename: filename.clone(),
            bytes: file.bytes,
            nonce,
        });
        saved.push(filename);
    }
    let on_file = Arc::clone(&ctx.on_file);
    thread::spawn(move || {
        for paper in papers {
            let _ = on_file(paper);
        }
    });
    let name = saved.join(", ");
    json_reply(
        200,
        &format!(r#"{{"ok":true,"name":{}}}"#, json_string(&name)),
    )
}

fn json_error(message: &str) -> String {
    format!(r#"{{"ok":false,"error":{}}}"#, json_string(message))
}

fn html_reply(body: &str) -> Reply {
    Reply {
        status: 200,
        content_type: "text/html; charset=utf-8",
        extra: &[
            ("Cache-Control", "no-store"),
            ("X-Content-Type-Options", "nosniff"),
            ("Referrer-Policy", "no-referrer"),
            (
                "Content-Security-Policy",
                "default-src 'none'; connect-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'self'",
            ),
        ],
        body: body.as_bytes().to_vec(),
    }
}

fn text_reply(status: u16, body: &str) -> Reply {
    Reply {
        status,
        content_type: "text/plain; charset=utf-8",
        extra: &[("Cache-Control", "no-store")],
        body: body.as_bytes().to_vec(),
    }
}

fn json_reply(status: u16, body: &str) -> Reply {
    Reply {
        status,
        content_type: "application/json; charset=utf-8",
        extra: &[("Cache-Control", "no-store")],
        body: body.as_bytes().to_vec(),
    }
}

fn write_reply(sock: &mut TcpStream, reply: &Reply) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reason(reply.status),
        reply.content_type,
        reply.body.len()
    );
    for (name, value) in reply.extra {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    sock.write_all(head.as_bytes())?;
    sock.write_all(&reply.body)?;
    sock.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

fn header_lookup(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
}

fn find_crlf_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

fn read_exact(
    sock: &mut TcpStream,
    already: &mut Vec<u8>,
    length: usize,
) -> std::io::Result<Vec<u8>> {
    if already.len() >= length {
        let body = already[..length].to_vec();
        already.drain(..length);
        return Ok(body);
    }
    let mut body = std::mem::take(already);
    let start = body.len();
    body.resize(length, 0);
    let mut filled = start;
    while filled < length {
        match sock.read(&mut body[filled..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "upload ended early",
                ))
            }
            Ok(n) => filled += n,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(body)
}

fn read_chunked(
    sock: &mut TcpStream,
    already: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    let mut pending = std::mem::take(already);
    let mut body = Vec::new();
    loop {
        let line = read_line(sock, &mut pending)?;
        let size_hex = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16).unwrap_or(usize::MAX);
        if size == usize::MAX {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "bad chunk"));
        }
        if size == 0 {
            let _ = read_line(sock, &mut pending);
            break;
        }
        if body.len().saturating_add(size) > limit {
            return Err(std::io::Error::new(
                ErrorKind::FileTooLarge,
                "upload too large",
            ));
        }
        let chunk = read_exact(sock, &mut pending, size)?;
        body.extend_from_slice(&chunk);
        let _ = read_line(sock, &mut pending);
    }
    Ok(body)
}

fn read_line(sock: &mut TcpStream, pending: &mut Vec<u8>) -> std::io::Result<String> {
    loop {
        if let Some(end) = pending.windows(2).position(|window| window == b"\r\n") {
            let line = String::from_utf8_lossy(&pending[..end]).into_owned();
            pending.drain(..end + 2);
            return Ok(line);
        }
        if pending.len() > 8192 {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "line too long"));
        }
        let mut tmp = [0u8; 1024];
        match sock.read(&mut tmp) {
            Ok(0) => return Err(std::io::Error::new(ErrorKind::UnexpectedEof, "closed")),
            Ok(n) => pending.extend_from_slice(&tmp[..n]),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn reject_tls(sock: &mut TcpStream) {
    // The ClientHello is already partly read. Drain the rest so closing the
    // socket is a FIN. Closing with unread bytes makes Linux send a RST, and
    // current Android Chrome retries a reset until it shows ERR_TOO_MANY_RETRIES
    // instead of falling back to HTTP.
    let _ = sock.set_read_timeout(Some(Duration::from_millis(50)));
    let mut ignore = [0u8; 2048];
    loop {
        match sock.read(&mut ignore) {
            Ok(0) | Err(_) => break,
            Ok(_) => continue,
        }
    }
    // Fatal handshake_failure. Chrome records this as
    // ERR_SSL_VERSION_OR_CIPHER_MISMATCH and does not retry it.
    let alert = [0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28];
    let _ = sock.write_all(&alert);
    let _ = sock.flush();
    let _ = sock.shutdown(Shutdown::Write);
}

impl HttpError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
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
        "docker",
        "veth",
        "br-",
        "virbr",
        "tun",
        "tap",
        "wg",
        "tailscale",
        "zt",
        "flannel",
        "cni",
        "vboxnet",
        "vmnet",
        "lo",
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

fn bind_listeners(
    addrs: &[LanAddr],
    port_start: u16,
    attempts: u16,
) -> Result<(u16, Vec<TcpListener>), String> {
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
        let listener = TcpListener::bind(SocketAddr::from((addrs[0].ip, 0)))
            .map_err(|error| format!("Phone drop could not listen. {error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        return Ok((port, vec![listener]));
    }
    let mut last_error = String::from("the port is in use");
    let attempts = attempts.max(1);
    for offset in 0..attempts {
        let Some(port) = port_start.checked_add(offset) else {
            break;
        };
        let mut listeners = Vec::new();
        let mut failed = false;
        for addr in addrs {
            match TcpListener::bind(SocketAddr::from((addr.ip, port))) {
                Ok(listener) => listeners.push(listener),
                Err(error) => {
                    last_error = error.to_string();
                    failed = true;
                    break;
                }
            }
        }
        if !failed {
            return Ok((port, listeners));
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

fn parse_uploads(
    content_type: &str,
    body: &[u8],
    max_file_bytes: usize,
) -> Result<Vec<RawFile>, HttpError> {
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
        if value.is_empty()
            || value.len() > 70
            || value
                .as_bytes()
                .iter()
                .any(|byte| byte.is_ascii_whitespace())
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
        let encoded = star
            .split_once("''")
            .map(|(_, rest)| rest)
            .unwrap_or(star.as_str());
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
        && matches!(
            &bytes[8..12],
            b"heic" | b"heix" | b"heif" | b"mif1" | b"hevc"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;
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
        assert_eq!(
            ips,
            vec!["192.168.1.20".to_string(), "172.30.0.2".to_string()]
        );
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
        assert_eq!(
            route_token(&format!("/drop/{token}/")),
            Some(token.as_str())
        );
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
        assert!(PAGE.contains("Trying again"));
        assert!(PAGE.contains("HTTP "));
        assert!(PAGE.contains(".heic"));
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
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x18, 0xDD, 0x8D,
            0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
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
            files_cb.lock().unwrap().push((paper.filename, paper.bytes));
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
        let headers = curl(&["-sSI", &server.info.primary_url]);
        let headers = String::from_utf8_lossy(&headers.stdout);
        assert!(
            headers.to_ascii_lowercase().contains("connect-src 'self'"),
            "{headers}"
        );

        let wrong = curl(&[
            "-sS",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            &format!(
                "http://127.0.0.1:{}/drop/{}",
                server.info.port,
                "ab".repeat(32)
            ),
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
        wait_until(|| server.files.lock().unwrap().len() == 1);
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
        assert!(
            !probed.status.success(),
            "idle server still accepted traffic"
        );
        assert!(server.slot.lock().unwrap().is_none());
    }

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            if ready() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for the upload to be recorded");
    }

    fn jpeg_bytes(size: usize) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
        bytes.resize(size, 0x11);
        bytes
    }

    fn post_multipart(url: &str, filename: &str, bytes: &[u8], extra: &[&str]) -> String {
        let dir = std::env::temp_dir().join(format!(
            "tuto-phone-{}-{}",
            std::process::id(),
            filename.replace('.', "-")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(filename);
        std::fs::write(&path, bytes).unwrap();
        let mut args = vec![
            "-sS".to_string(),
            "--max-time".to_string(),
            "20".to_string(),
            "-w".to_string(),
            "\n%{http_code}".to_string(),
            "-F".to_string(),
            format!("file=@{};filename={}", path.display(), filename),
        ];
        for flag in extra {
            args.push((*flag).to_string());
        }
        args.push(url.to_string());
        let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        let output = curl(&arg_refs);
        let _ = std::fs::remove_dir_all(&dir);
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    #[test]
    fn tls_client_hello_gets_a_fatal_alert() {
        let server = serve_loopback(1024, Duration::from_secs(30));
        let mut sock = TcpStream::connect(("127.0.0.1", server.info.port)).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        sock.set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let started = Instant::now();
        sock.write_all(&[0x16, 0x03, 0x01, 0x00, 0x2a, 0x01, 0x00, 0x00, 0x26])
            .unwrap();
        let mut buf = [0u8; 16];
        let n = sock.read(&mut buf).expect("tls alert");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "tls probe hung for {:?}",
            started.elapsed()
        );
        assert!(
            buf[..n].starts_with(&[0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28]),
            "expected a fatal handshake_failure alert, got {:02x?}",
            &buf[..n]
        );
    }

    #[test]
    fn large_uploads_return_before_ocr_and_accept_continue_and_chunked() {
        let files = Arc::new(StdMutex::new(Vec::new()));
        let slot: Arc<StdMutex<Option<Session>>> = Arc::new(StdMutex::new(None));
        let files_cb = Arc::clone(&files);
        let on_file: FileHandler = Arc::new(move |paper| {
            thread::sleep(Duration::from_millis(2000));
            files_cb
                .lock()
                .unwrap()
                .push((paper.filename.clone(), paper.bytes.len()));
            Ok(())
        });
        let slot_idle = Arc::clone(&slot);
        let started = open_server(OpenConfig {
            bind: vec![LanAddr {
                name: "lo".into(),
                ip: Ipv4Addr::LOCALHOST,
            }],
            port_start: 0,
            port_attempts: 1,
            idle: Duration::from_secs(60),
            poll: Duration::from_millis(40),
            max_file_bytes: 25 * 1024 * 1024,
            on_file,
            on_seen: Arc::new(|| {}),
            on_idle: Arc::new(move || {
                if let Some(session) = slot_idle.lock().unwrap().take() {
                    session.shutdown();
                }
            }),
            generation: 2,
        })
        .unwrap();
        let info = started.info.clone();
        *slot.lock().unwrap() = Some(started.session);
        let url = info.primary_url.clone();

        let slow = jpeg_bytes(6 * 1024 * 1024);
        let started_at = Instant::now();
        let response = post_multipart(&url, "slow.jpg", &slow, &[]);
        assert!(
            started_at.elapsed() < Duration::from_millis(1500),
            "response waited on the import callback: {:?} {response}",
            started_at.elapsed()
        );
        assert!(response.contains("\"ok\":true"), "{response}");
        assert!(response.trim().ends_with("200"), "{response}");

        let photo = jpeg_bytes(5 * 1024 * 1024);
        let url_a = url.clone();
        let url_b = url.clone();
        let url_c = url.clone();
        let photo_a = photo.clone();
        let photo_b = photo.clone();
        let photo_c = photo;
        let first = thread::spawn(move || post_multipart(&url_a, "a.jpg", &photo_a, &[]));
        let second = thread::spawn(move || post_multipart(&url_b, "b.jpg", &photo_b, &[]));
        let third = thread::spawn(move || post_multipart(&url_c, "c.jpg", &photo_c, &[]));
        for (name, handle) in [("a", first), ("b", second), ("c", third)] {
            let body = handle.join().unwrap();
            assert!(body.contains("\"ok\":true"), "{name}: {body}");
            assert!(body.trim().ends_with("200"), "{name}: {body}");
        }

        let continued = jpeg_bytes(1024 * 1024);
        let continued_body = post_multipart(
            &url,
            "continued.jpg",
            &continued,
            &["-H", "Expect: 100-continue"],
        );
        assert!(continued_body.contains("\"ok\":true"), "{continued_body}");

        let chunk = jpeg_bytes(256 * 1024);
        let boundary = "tutoBoundary";
        let mut multipart = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"chunked.jpg\"\r\nContent-Type: image/jpeg\r\n\r\n"
        )
        .into_bytes();
        multipart.extend_from_slice(&chunk);
        multipart.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let mut sock = TcpStream::connect(("127.0.0.1", info.port)).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let head = format!(
            "POST /drop/{} HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nConnection: close\r\n\r\n{:X}\r\n",
            info.token,
            multipart.len()
        );
        sock.write_all(head.as_bytes()).unwrap();
        sock.write_all(&multipart).unwrap();
        sock.write_all(b"\r\n0\r\n\r\n").unwrap();
        let mut chunked_reply = String::new();
        sock.read_to_string(&mut chunked_reply).unwrap();
        assert!(chunked_reply.contains("200"), "{chunked_reply}");
        assert!(chunked_reply.contains("\"ok\":true"), "{chunked_reply}");

        let mut huge = TcpStream::connect(("127.0.0.1", info.port)).unwrap();
        huge.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let request = format!(
            "POST /drop/{} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            info.token,
            40 * 1024 * 1024
        );
        huge.write_all(request.as_bytes()).unwrap();
        let mut huge_reply = String::new();
        let _ = huge.read_to_string(&mut huge_reply);
        assert!(huge_reply.contains("413"), "{huge_reply}");

        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(8) {
            if files.lock().unwrap().len() >= 6 {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let saved = files.lock().unwrap();
        assert!(
            saved.len() >= 6,
            "expected the slow import to record every accepted file, got {saved:?}"
        );
        if let Some(session) = slot.lock().unwrap().take() {
            session.shutdown();
        }
    }

    #[test]
    fn https_first_on_a_lan_address_is_an_ssl_mismatch() {
        let Some(bind) = lan_interfaces()
            .ok()
            .and_then(|list| list.into_iter().next())
        else {
            return;
        };
        let slot: Arc<StdMutex<Option<Session>>> = Arc::new(StdMutex::new(None));
        let slot_idle = Arc::clone(&slot);
        let started = open_server(OpenConfig {
            bind: vec![bind],
            port_start: 0,
            port_attempts: 1,
            idle: Duration::from_secs(60),
            poll: Duration::from_millis(40),
            max_file_bytes: 1024,
            on_file: Arc::new(|_| Ok(())),
            on_seen: Arc::new(|| {}),
            on_idle: Arc::new(move || {
                if let Some(session) = slot_idle.lock().unwrap().take() {
                    session.shutdown();
                }
            }),
            generation: 4,
        })
        .unwrap();
        let info = started.info.clone();
        *slot.lock().unwrap() = Some(started.session);
        let dir = std::env::temp_dir().join(format!("tuto-hfm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Default")).unwrap();
        std::fs::write(
            dir.join("Default/Preferences"),
            r#"{"https_only_mode_enabled":true}"#,
        )
        .unwrap();
        let netlog = dir.join("netlog.json");
        let debug = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let mut chrome = Command::new("google-chrome")
            .args([
                "--headless=new",
                "--no-sandbox",
                "--disable-gpu",
                "--disable-dev-shm-usage",
                "--no-first-run",
                "--enable-features=HttpsUpgrades,HttpsFirstModeV2",
                &format!("--remote-debugging-port={debug}"),
                "--remote-allow-origins=*",
                &format!("--user-data-dir={}", dir.display()),
                &format!("--log-net-log={}", netlog.display()),
                "about:blank",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("google-chrome");
        let script = r#"
const [debug, url] = process.argv.slice(2);
let version = null;
for (let attempt = 0; attempt < 40; attempt += 1) {
  try {
    version = await (await fetch(`http://127.0.0.1:${debug}/json/version`)).json();
    break;
  } catch (error) {
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
}
if (!version) throw new Error('devtools did not open');
const browser = new WebSocket(version.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  browser.addEventListener('open', resolve);
  browser.addEventListener('error', () => reject(new Error('ws')));
});
let id = 0;
const pending = new Map();
browser.addEventListener('message', (event) => {
  const message = JSON.parse(event.data);
  if (message.id && pending.has(message.id)) pending.get(message.id)(message);
});
const call = (method, params, sessionId) => new Promise((resolve, reject) => {
  const thisId = ++id;
  const timer = setTimeout(() => reject(new Error('timeout ' + method)), 12000);
  pending.set(thisId, (message) => { clearTimeout(timer); resolve(message.result || {}); });
  browser.send(JSON.stringify({ id: thisId, method, params, sessionId }));
});
const created = await call('Target.createTarget', { url: 'about:blank' });
const attached = await call('Target.attachToTarget', { targetId: created.targetId, flatten: true });
await call('Page.enable', {}, attached.sessionId);
await call('Page.navigate', { url }, attached.sessionId);
await new Promise((resolve) => setTimeout(resolve, 4000));
const info = await call('Runtime.evaluate', {
  expression: 'location.href + " " + (document.body ? document.body.innerText : "")',
  returnByValue: true,
}, attached.sessionId);
console.log(info.result && info.result.value || '');
process.exit(0);
"#;
        let script_path = dir.join("nav.mjs");
        std::fs::write(&script_path, script).unwrap();
        let script_arg = script_path.to_string_lossy().into_owned();
        let debug_arg = debug.to_string();
        let output = Command::new("node")
            .args([&script_arg, &debug_arg, &info.primary_url])
            .output();
        let mut log = String::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            log = std::fs::read_to_string(&netlog).unwrap_or_default();
            if log.contains("-113") {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let _ = chrome.kill();
        let _ = chrome.wait();
        if let Some(session) = slot.lock().unwrap().take() {
            session.shutdown();
        }
        let output = output.expect("node");
        let page = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            page.contains("Send papers") || page.contains("chrome-error://"),
            "https-first navigation did not finish\n{page}"
        );
        let saw_mismatch =
            log.contains("\"net_error\": -113") || log.contains("\"net_error\":-113");
        let saw_reset = log.contains("\"net_error\": -101") || log.contains("\"net_error\":-101");
        assert!(
            saw_mismatch || page.contains("Send papers"),
            "Chrome did not record the TLS alert as an SSL mismatch (log {} bytes).\n{page}",
            log.len()
        );
        assert!(
            !saw_reset,
            "Chrome still saw a TCP reset on the HTTPS probe"
        );
    }

    #[test]
    fn headless_chrome_uploads_large_jpegs() {
        let bind = lan_interfaces()
            .ok()
            .and_then(|list| list.into_iter().next())
            .unwrap_or(LanAddr {
                name: "lo".into(),
                ip: Ipv4Addr::LOCALHOST,
            });
        let files = Arc::new(StdMutex::new(Vec::new()));
        let slot: Arc<StdMutex<Option<Session>>> = Arc::new(StdMutex::new(None));
        let files_cb = Arc::clone(&files);
        let on_file: FileHandler = Arc::new(move |paper| {
            files_cb.lock().unwrap().push(paper.filename);
            Ok(())
        });
        let slot_idle = Arc::clone(&slot);
        let started = open_server(OpenConfig {
            bind: vec![bind.clone()],
            port_start: 0,
            port_attempts: 1,
            idle: Duration::from_secs(120),
            poll: Duration::from_millis(40),
            max_file_bytes: 25 * 1024 * 1024,
            on_file,
            on_seen: Arc::new(|| {}),
            on_idle: Arc::new(move || {
                if let Some(session) = slot_idle.lock().unwrap().take() {
                    session.shutdown();
                }
            }),
            generation: 3,
        })
        .unwrap();
        let info = started.info.clone();
        *slot.lock().unwrap() = Some(started.session);

        let dir = std::env::temp_dir().join(format!("tuto-chrome-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut paths = Vec::new();
        for (name, size) in [("cam-a.jpg", 6), ("cam-b.jpg", 7), ("cam-c.jpg", 8)] {
            let path = dir.join(name);
            std::fs::write(&path, jpeg_bytes(size * 1024 * 1024)).unwrap();
            paths.push(path);
        }
        let debug_port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let profile = dir.join("chrome-profile");
        let chrome_log = std::fs::File::create(dir.join("chrome.log")).unwrap();
        let chrome = Command::new("google-chrome")
            .args([
                "--headless=new",
                "--no-sandbox",
                "--disable-gpu",
                "--disable-dev-shm-usage",
                "--no-first-run",
                "--enable-features=HttpsUpgrades",
                &format!("--remote-debugging-port={debug_port}"),
                "--remote-debugging-address=127.0.0.1",
                "--remote-allow-origins=*",
                &format!("--user-data-dir={}", profile.display()),
                "about:blank",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(chrome_log))
            .spawn()
            .expect("google-chrome is installed");
        struct KillChild(std::process::Child);
        impl Drop for KillChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let chrome = KillChild(chrome);
        let script = dir.join("upload.mjs");
        std::fs::write(&script, super::CHROME_UPLOAD_SCRIPT).unwrap();
        let mut args = vec![
            script.to_string_lossy().into_owned(),
            debug_port.to_string(),
            info.primary_url.clone(),
        ];
        for path in &paths {
            args.push(path.to_string_lossy().into_owned());
        }
        let output = Command::new("node")
            .args(&args)
            .output()
            .expect("node is installed");
        let report = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !output.status.success() {
            let err = std::fs::read_to_string(dir.join("chrome.log")).unwrap_or_default();
            panic!("chrome upload failed\n{report}\nchrome: {err}");
        }
        assert!(
            report.contains("parallel-ok"),
            "browser fetch did not accept the large photos\n{report}"
        );
        assert!(
            report.contains("page-ok"),
            "the phone page did not save the files one at a time\n{report}"
        );
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if files.lock().unwrap().len() >= 6 {
                break;
            }
            thread::sleep(Duration::from_millis(30));
        }
        let saved = files.lock().unwrap().clone();
        assert!(saved.len() >= 6, "server recorded {saved:?}\n{report}");
        drop(chrome);
        if let Some(session) = slot.lock().unwrap().take() {
            session.shutdown();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
const CHROME_UPLOAD_SCRIPT: &str = r#"
import { request } from 'node:http';

const [port, pageUrl, ...files] = process.argv.slice(2);

function get(url) {
  return new Promise((resolve, reject) => {
    const req = request(url, (res) => {
      let body = '';
      res.on('data', (chunk) => { body += chunk; });
      res.on('end', () => resolve(body));
    });
    req.on('error', reject);
    req.end();
  });
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function version() {
  for (let attempt = 0; attempt < 50; attempt += 1) {
    try {
      return JSON.parse(await get(`http://127.0.0.1:${port}/json/version`));
    } catch (error) {
      await sleep(200);
    }
  }
  throw new Error('chrome devtools did not open');
}

function rpc(ws) {
  let next = 0;
  const pending = new Map();
  ws.addEventListener('message', (event) => {
    const message = JSON.parse(event.data);
    if (message.id && pending.has(message.id)) {
      pending.get(message.id)(message);
      pending.delete(message.id);
    }
  });
  return (method, params) => new Promise((resolve, reject) => {
    const id = ++next;
    const timer = setTimeout(() => reject(new Error(`timeout ${method}`)), 30000);
    pending.set(id, (message) => {
      clearTimeout(timer);
      if (message.error) {
        reject(new Error(JSON.stringify(message.error)));
        return;
      }
      resolve(message.result ?? {});
    });
    ws.send(JSON.stringify({ id, method, params }));
  });
}

const browserInfo = await version();
const browser = new WebSocket(browserInfo.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  browser.addEventListener('open', resolve);
  browser.addEventListener('error', () => reject(new Error('devtools websocket failed')));
});
const browserCall = rpc(browser);
const created = await browserCall('Target.createTarget', { url: pageUrl });
let page = null;
for (let attempt = 0; attempt < 30; attempt += 1) {
  const list = JSON.parse(await get(`http://127.0.0.1:${port}/json/list`));
  page = list.find((item) => item.id === created.targetId);
  if (page && page.webSocketDebuggerUrl) break;
  await sleep(200);
}
if (!page) throw new Error('chrome did not open the drop page');
const pageWs = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  pageWs.addEventListener('open', resolve);
  pageWs.addEventListener('error', () => reject(new Error('page websocket failed')));
});
const call = rpc(pageWs);
await call('Runtime.enable');
await call('Page.enable');
await call('DOM.enable');
let text = '';
for (let attempt = 0; attempt < 40; attempt += 1) {
  const evaluated = await call('Runtime.evaluate', {
    expression: 'document.body ? document.body.innerText : ""',
    returnByValue: true,
  });
  text = evaluated.result?.value || '';
  if (text.includes('Send papers')) break;
  await sleep(250);
}
if (!text.includes('Send papers')) {
  const where = await call('Runtime.evaluate', { expression: 'location.href', returnByValue: true });
  throw new Error('page did not load over http: ' + text + ' url=' + (where.result?.value || ''));
}
const parallel = await call('Runtime.evaluate', {
  expression: `(async () => {
    function jpeg(size) {
      const bytes = new Uint8Array(size);
      bytes[0] = 0xff; bytes[1] = 0xd8; bytes[2] = 0xff; bytes[3] = 0xe0;
      return new Blob([bytes], { type: 'image/jpeg' });
    }
    const jobs = [
      ['p-6.jpg', 6 * 1024 * 1024],
      ['p-7.jpg', 7 * 1024 * 1024],
      ['p-8.jpg', 8 * 1024 * 1024],
    ];
    const results = await Promise.all(jobs.map(async ([name, size]) => {
      const body = new FormData();
      body.append('file', jpeg(size), name);
      try {
        const response = await fetch(location.pathname, { method: 'POST', body });
        const payload = await response.text();
        return response.status + ' ' + payload;
      } catch (error) {
        return 'ERR ' + (error && error.message ? error.message : 'failed');
      }
    }));
    return results.join(' | ');
  })()`,
  awaitPromise: true,
  returnByValue: true,
});
const parallelText = parallel.result?.value || JSON.stringify(parallel);
console.log(parallelText);
if (!parallelText.includes('200') || parallelText.includes('ERR') || parallelText.split('200').length < 4) {
  throw new Error('parallel browser upload failed');
}
console.log('parallel-ok');
const documentNode = await call('DOM.getDocument', { depth: 1 });
const input = await call('DOM.querySelector', {
  nodeId: documentNode.root.nodeId,
  selector: '#library',
});
await call('DOM.setFileInputFiles', { nodeId: input.nodeId, files });
let pageText = '';
for (let attempt = 0; attempt < 40; attempt += 1) {
  const evaluated = await call('Runtime.evaluate', {
    expression: 'document.getElementById("list").innerText',
    returnByValue: true,
  });
  pageText = evaluated.result?.value || '';
  const saved = pageText.split('Saved on the laptop').length - 1;
  if (saved >= 3) break;
  if (pageText.includes('HTTP ') || pageText.includes('network error') || pageText.includes('timed out')) {
    throw new Error(pageText);
  }
  await sleep(250);
}
console.log(pageText);
if (pageText.split('Saved on the laptop').length - 1 < 3) {
  throw new Error('page did not finish one-at-a-time uploads: ' + pageText);
}
console.log('page-ok');
process.exit(0);
"#;
