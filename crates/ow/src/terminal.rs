//! Access-authenticated WebSocket to a dedicated PTY inside the selected guest.
use crate::{runtime, wire};
use axum::{
    extract::{
        FromRequestParts, Request,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::{
    path::Path,
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{OwnedSemaphorePermit, Semaphore},
};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum Control {
    Resize { cols: u16, rows: u16 },
    Ack { bytes: usize },
}

pub fn local(root: &Path, id: &str) -> anyhow::Result<i32> {
    use std::{
        io::{Read, Write},
        os::fd::AsRawFd,
    };
    anyhow::ensure!(
        unsafe { libc::isatty(0) } == 1,
        "ow shell requires an interactive terminal; use ow exec for scripts"
    );
    let (mut stream, _) = wire::connect(root, &serde_json::json!({"op":"terminal","id":id}))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    struct Raw(libc::termios);
    impl Drop for Raw {
        fn drop(&mut self) {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &self.0);
            }
        }
    }
    let mut attrs = std::mem::MaybeUninit::uninit();
    anyhow::ensure!(
        unsafe { libc::tcgetattr(0, attrs.as_mut_ptr()) } == 0,
        "terminal attributes unavailable"
    );
    let attrs = unsafe { attrs.assume_init() };
    let _raw = Raw(attrs);
    let mut raw = attrs;
    unsafe {
        libc::cfmakeraw(&mut raw);
    }
    anyhow::ensure!(
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } == 0,
        "could not enter raw terminal mode"
    );
    fn frame(
        stream: &mut std::os::unix::net::UnixStream,
        kind: u8,
        bytes: &[u8],
    ) -> std::io::Result<()> {
        stream.write_all(&[kind])?;
        stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
        stream.write_all(bytes)
    }
    let mut previous = (0, 0);
    loop {
        let mut size = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut size) } == 0 {
            let next = (size.ws_col.clamp(1, 500), size.ws_row.clamp(1, 300));
            if next != previous {
                frame(
                    &mut stream,
                    1,
                    &[next.0.to_be_bytes(), next.1.to_be_bytes()].concat(),
                )?;
                previous = next;
            }
        }
        let mut fds = [
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, 100) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(std::io::Error::last_os_error().into());
        }
        if fds[0].revents & libc::POLLIN != 0 {
            let mut bytes = [0; 4096];
            let n = std::io::stdin().read(&mut bytes)?;
            if n == 0 {
                return Ok(0);
            }
            frame(&mut stream, 0, &bytes[..n])?;
        }
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let mut header = [0; 5];
            if stream.read_exact(&mut header).is_err() {
                return Ok(0);
            }
            let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
            anyhow::ensure!(len <= 4096, "invalid terminal output");
            let mut bytes = vec![0; len];
            stream.read_exact(&mut bytes)?;
            match header[0] {
                0 => {
                    std::io::stdout().write_all(&bytes)?;
                    std::io::stdout().flush()?;
                }
                2 if len == 4 => return Ok(i32::from_be_bytes(bytes.try_into().unwrap())),
                _ => anyhow::bail!("invalid terminal output"),
            }
        }
    }
}

pub async fn upgrade(root: &Path, request: Request, expires: u64, id: String) -> Response {
    if request.uri().query().is_some()
        || request.uri().scheme().is_some()
        || request.uri().authority().is_some()
    {
        return (StatusCode::BAD_REQUEST, "Invalid terminal target").into_response();
    }
    if runtime::identifier(&id).is_err() {
        return (StatusCode::BAD_REQUEST, "Invalid machine name").into_response();
    }
    let (mut parts, _) = request.into_parts();
    let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(ws) => ws,
        Err(e) => return e.into_response(),
    };
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let Ok(permit) = SLOTS
        .get_or_init(|| Arc::new(Semaphore::new(16)))
        .clone()
        .try_acquire_owned()
    else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Terminal capacity reached").into_response();
    };
    let root = root.to_path_buf();
    let stream = match tokio::task::spawn_blocking(move || {
        wire::connect(&root, &serde_json::json!({"op":"terminal","id":id}))
    })
    .await
    {
        Ok(Ok((stream, _))) => stream,
        error => {
            eprintln!("Terminal connection failed: {error:?}");
            return (
                StatusCode::CONFLICT,
                "Could not open terminal. Check the machine is running.",
            )
                .into_response();
        }
    };
    if stream.set_nonblocking(true).is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let stream = match tokio::net::UnixStream::from_std(stream) {
        Ok(stream) => stream,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    ws.max_message_size(4096)
        .max_frame_size(4096)
        .write_buffer_size(0)
        .max_write_buffer_size(65536)
        .on_upgrade(move |socket| relay(socket, stream, expires, permit))
}

async fn send_frame(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    kind: u8,
    bytes: &[u8],
) -> std::io::Result<()> {
    writer.write_u8(kind).await?;
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(bytes).await
}
async fn relay(
    mut socket: WebSocket,
    stream: tokio::net::UnixStream,
    expires: u64,
    _permit: OwnedSemaphorePermit,
) {
    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    // read_exact is isolated from select cancellation; otherwise partial frame headers can be lost.
    struct Reader(tokio::task::JoinHandle<()>);
    impl Drop for Reader {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _reader = Reader(tokio::spawn(async move {
        loop {
            let mut header = [0; 5];
            if reader.read_exact(&mut header).await.is_err() {
                break;
            }
            let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
            if !matches!(header[0], 0 | 2) || len > 4096 || (header[0] == 2 && len != 4) {
                break;
            }
            let mut bytes = vec![0; len];
            if reader.read_exact(&mut bytes).await.is_err() {
                break;
            }
            if tx.send((header[0], bytes)).await.is_err() {
                break;
            }
        }
    }));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let deadline = tokio::time::sleep(Duration::from_secs(expires.saturating_sub(now).min(3600)));
    tokio::pin!(deadline);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
    let mut seen = tokio::time::Instant::now();
    let mut unacked = 0usize;
    loop {
        tokio::select! {
            _=&mut deadline=>break,
            _=heartbeat.tick()=>{
                if seen.elapsed()>Duration::from_secs(90) {break;}
                if !matches!(tokio::time::timeout(Duration::from_secs(5),socket.send(Message::Ping(Vec::new().into()))).await,Ok(Ok(()))) {break;}
            }
            message=socket.recv()=>{
                seen=tokio::time::Instant::now();
                let Some(Ok(message))=message else {break;};
                let result=match message {
                    Message::Binary(bytes)=>tokio::time::timeout(Duration::from_secs(5),send_frame(&mut writer,0,&bytes)).await,
                    Message::Text(text)=>match serde_json::from_str::<Control>(&text) {
                        Ok(Control::Resize{cols,rows}) if (1..=500).contains(&cols) && (1..=300).contains(&rows)=>{
                            let bytes=[cols.to_be_bytes(),rows.to_be_bytes()].concat();
                            tokio::time::timeout(Duration::from_secs(5),send_frame(&mut writer,1,&bytes)).await
                        }
                        Ok(Control::Ack{bytes}) if bytes>0 && bytes<=unacked=>{unacked-=bytes;continue;}
                        _=>break,
                    },
                    Message::Close(_)=>break,
                    Message::Ping(_) | Message::Pong(_)=>continue,
                };
                if !matches!(result,Ok(Ok(()))) {break;}
            }
            frame=rx.recv(), if unacked<65536=>{
                let Some((kind,bytes))=frame else {break;};
                let message=if kind==2 {
                    let code=i32::from_be_bytes(bytes.try_into().unwrap());
                    Message::Text(serde_json::json!({"type":"exit","code":code}).to_string().into())
                } else {unacked+=bytes.len();Message::Binary(bytes.into())};
                if !matches!(tokio::time::timeout(Duration::from_secs(5),socket.send(message)).await,Ok(Ok(()))) {break;}
                if kind==2 {break;}
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.send(Message::Close(None))).await;
    // Dropping both upstream halves ends the guest session and releases its process group.
}
