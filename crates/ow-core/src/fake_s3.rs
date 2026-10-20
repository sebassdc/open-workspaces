//! A loopback S3 for tests: conditional writes, SHA-256 checksums, multipart
//! uploads, ranged reads and listings, with request counts and fault injection.
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Clone)]
pub struct Stored {
    pub bytes: Vec<u8>,
    pub checksum: String,
    pub etag: String,
}

/// Uploaded parts of one multipart upload: bytes and checksum by part number.
type Parts = BTreeMap<u16, (Vec<u8>, String)>;

#[derive(Default)]
pub struct State {
    pub objects: Mutex<BTreeMap<String, Stored>>,
    uploads: Mutex<HashMap<String, Parts>>,
    /// Requests by kind: "put", "head", "get", "range", "list", "create", "part", "complete", "abort".
    pub calls: Mutex<BTreeMap<&'static str, usize>>,
    active: AtomicUsize,
    /// Most requests of a kind served at once.
    pub peak: Mutex<BTreeMap<&'static str, usize>>,
    /// The next this many object or part PUTs answer 403.
    pub refuse_puts: AtomicUsize,
    /// The next this many completions take effect, then answer 500.
    pub lose_completions: AtomicUsize,
    uploads_made: AtomicUsize,
}

pub struct FakeS3 {
    pub endpoint: String,
    pub state: Arc<State>,
}

impl FakeS3 {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(State::default());
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = shared.clone();
                std::thread::spawn(move || {
                    let _ = serve(stream, &state);
                });
            }
        });
        Self { endpoint, state }
    }

    pub fn calls(&self, kind: &str) -> usize {
        self.state
            .calls
            .lock()
            .unwrap()
            .get(kind)
            .copied()
            .unwrap_or(0)
    }

    pub fn peak(&self, kind: &str) -> usize {
        self.state
            .peak
            .lock()
            .unwrap()
            .get(kind)
            .copied()
            .unwrap_or(0)
    }
}

fn digest(bytes: &[u8]) -> String {
    base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        Sha256::digest(bytes),
    )
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn reply(status: u16, body: impl Into<Vec<u8>>) -> Reply {
    Reply {
        status,
        headers: vec![],
        body: body.into(),
    }
}

fn serve(stream: TcpStream, state: &State) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let mut parts = line.split_whitespace();
        let (method, target) = (
            parts.next().unwrap_or("").to_owned(),
            parts.next().unwrap_or("").to_owned(),
        );
        let mut headers = HashMap::new();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header)?;
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
            }
        }
        let length: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let query: HashMap<String, String> = query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (decode(k), decode(v))
            })
            .collect();
        // Path style: /bucket/key.
        let key = decode(
            path.trim_start_matches('/')
                .split_once('/')
                .map_or("", |(_, k)| k),
        );
        let kind = match (method.as_str(), query.contains_key("uploadId")) {
            ("PUT", true) => "part",
            ("PUT", false) => "put",
            ("POST", true) => "complete",
            ("POST", false) => "create",
            ("DELETE", _) => "abort",
            ("HEAD", _) => "head",
            ("GET", _) if query.contains_key("list-type") => "list",
            ("GET", _) if headers.contains_key("range") => "range",
            _ => "get",
        };
        *state.calls.lock().unwrap().entry(kind).or_default() += 1;
        let now = state.active.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut peak = state.peak.lock().unwrap();
            let entry = peak.entry(kind).or_default();
            *entry = (*entry).max(now);
        }
        if matches!(kind, "part" | "range" | "put") {
            // Long enough for parallel requests to overlap.
            std::thread::sleep(Duration::from_millis(15));
        }
        let response = handle(state, kind, &key, &query, &headers, body);
        state.active.fetch_sub(1, Ordering::SeqCst);
        let mut head = format!(
            "HTTP/1.1 {} X\r\ncontent-length: {}\r\n",
            response.status,
            if method == "HEAD" {
                response
                    .headers
                    .iter()
                    .find(|(n, _)| n == "content-length")
                    .map_or(0, |(_, v)| v.parse().unwrap())
            } else {
                response.body.len()
            }
        );
        for (name, value) in &response.headers {
            if name != "content-length" {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
        }
        head.push_str("\r\n");
        writer.write_all(head.as_bytes())?;
        if method != "HEAD" {
            writer.write_all(&response.body)?;
        }
    }
}

fn handle(
    state: &State,
    kind: &str,
    key: &str,
    query: &HashMap<String, String>,
    headers: &HashMap<String, String>,
    body: Vec<u8>,
) -> Reply {
    let mut objects = state.objects.lock().unwrap();
    let checked = |body: &[u8]| {
        headers
            .get("x-amz-checksum-sha256")
            .is_none_or(|c| *c == digest(body))
    };
    match kind {
        "put" | "part" if state.refuse_puts.load(Ordering::SeqCst) > 0 => {
            state.refuse_puts.fetch_sub(1, Ordering::SeqCst);
            reply(403, "<Error><Code>AccessDenied</Code></Error>")
        }
        "put" => {
            if !checked(&body) {
                return reply(400, "<Error><Code>BadDigest</Code></Error>");
            }
            if headers.get("if-none-match").map(String::as_str) == Some("*")
                && objects.contains_key(key)
            {
                return reply(412, "<Error><Code>PreconditionFailed</Code></Error>");
            }
            let checksum = digest(&body);
            objects.insert(
                key.into(),
                Stored {
                    etag: format!("\"{}\"", &checksum[..8]),
                    checksum,
                    bytes: body,
                },
            );
            reply(200, "")
        }
        "head" => match objects.get(key) {
            None => reply(404, ""),
            Some(object) => {
                let mut r = reply(200, "");
                r.headers = vec![
                    ("content-length".into(), object.bytes.len().to_string()),
                    ("etag".into(), object.etag.clone()),
                ];
                if headers.get("x-amz-checksum-mode").map(String::as_str) == Some("ENABLED") {
                    r.headers
                        .push(("x-amz-checksum-sha256".into(), object.checksum.clone()));
                }
                r
            }
        },
        "get" => match objects.get(key) {
            None => reply(404, "<Error><Code>NoSuchKey</Code></Error>"),
            Some(object) => reply(200, object.bytes.clone()),
        },
        "range" => {
            let Some(object) = objects.get(key) else {
                return reply(404, "<Error><Code>NoSuchKey</Code></Error>");
            };
            if headers.get("if-match") != Some(&object.etag) {
                return reply(412, "<Error><Code>PreconditionFailed</Code></Error>");
            }
            let (start, end) = headers["range"]
                .strip_prefix("bytes=")
                .and_then(|r| r.split_once('-'))
                .map(|(a, b)| (a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap()))
                .unwrap();
            reply(
                206,
                object.bytes[start..=end.min(object.bytes.len() - 1)].to_vec(),
            )
        }
        "list" => {
            let prefix = query.get("prefix").cloned().unwrap_or_default();
            let mut xml = String::from(
                r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#,
            );
            for (name, object) in objects.range(prefix.clone()..) {
                if !name.starts_with(&prefix) {
                    break;
                }
                xml.push_str(&format!(
                    "<Contents><Key>{name}</Key><LastModified>2026-10-19T00:00:00.000Z</LastModified><ETag>{}</ETag><Size>{}</Size></Contents>",
                    object.etag.replace('"', "&quot;"),
                    object.bytes.len()
                ));
            }
            xml.push_str("</ListBucketResult>");
            reply(200, xml)
        }
        "create" => {
            let id = format!(
                "upload-{}",
                state.uploads_made.fetch_add(1, Ordering::SeqCst)
            );
            state
                .uploads
                .lock()
                .unwrap()
                .insert(id.clone(), BTreeMap::new());
            reply(
                200,
                format!(
                    r#"<InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>b</Bucket><Key>{key}</Key><UploadId>{id}</UploadId></InitiateMultipartUploadResult>"#
                ),
            )
        }
        "part" => {
            let mut uploads = state.uploads.lock().unwrap();
            let Some(upload) = uploads.get_mut(&query["uploadId"]) else {
                return reply(404, "<Error><Code>NoSuchUpload</Code></Error>");
            };
            if !checked(&body) {
                return reply(400, "<Error><Code>BadDigest</Code></Error>");
            }
            let checksum = digest(&body);
            upload.insert(
                query["partNumber"].parse().unwrap(),
                (body, checksum.clone()),
            );
            let mut r = reply(200, "");
            r.headers = vec![
                ("etag".into(), format!("\"part-{}\"", query["partNumber"])),
                ("x-amz-checksum-sha256".into(), checksum),
            ];
            r
        }
        "complete" => {
            let text = String::from_utf8(body).unwrap();
            let Some(upload) = state.uploads.lock().unwrap().remove(&query["uploadId"]) else {
                return reply(404, "<Error><Code>NoSuchUpload</Code></Error>");
            };
            if headers.get("if-none-match").map(String::as_str) == Some("*")
                && objects.contains_key(key)
            {
                return reply(412, "<Error><Code>PreconditionFailed</Code></Error>");
            }
            let mut bytes = Vec::new();
            let mut composite = Vec::new();
            for (i, part) in text.split("<Part>").skip(1).enumerate() {
                let field = |name: &str| {
                    part.split(&format!("<{name}>"))
                        .nth(1)
                        .and_then(|r| r.split(&format!("</{name}>")).next())
                        .unwrap_or("")
                        .to_owned()
                };
                let number: u16 = field("PartNumber").parse().unwrap();
                let Some((data, checksum)) = upload.get(&number) else {
                    return reply(400, "<Error><Code>InvalidPart</Code></Error>");
                };
                if number as usize != i + 1
                    || field("ETag") != format!("\"part-{number}\"")
                    || field("ChecksumSHA256") != *checksum
                {
                    return reply(400, "<Error><Code>InvalidPart</Code></Error>");
                }
                bytes.extend_from_slice(data);
                composite.extend(
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, checksum)
                        .unwrap(),
                );
            }
            let count = text.matches("<Part>").count();
            objects.insert(
                key.into(),
                Stored {
                    bytes,
                    checksum: format!("{}-{count}", digest(&composite)),
                    etag: format!("\"multi-{count}\""),
                },
            );
            if state.lose_completions.load(Ordering::SeqCst) > 0 {
                state.lose_completions.fetch_sub(1, Ordering::SeqCst);
                return reply(500, "<Error><Code>InternalError</Code></Error>");
            }
            reply(
                200,
                r#"<CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"></CompleteMultipartUploadResult>"#,
            )
        }
        "abort" => {
            state.uploads.lock().unwrap().remove(&query["uploadId"]);
            reply(204, "")
        }
        _ => reply(400, ""),
    }
}
