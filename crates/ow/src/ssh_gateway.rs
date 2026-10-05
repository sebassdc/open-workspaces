//! Owner-authorized raw SSH bytes over a bounded authenticated WebSocket.
use axum::{
    extract::{
        FromRequestParts, Request,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::{
    path::Path,
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{OwnedSemaphorePermit, Semaphore},
};

pub async fn upgrade(root: &Path, request: Request, expires: u64, id: String) -> Response {
    if request.uri().query().is_some()
        || request.uri().authority().is_some()
        || request.uri().scheme().is_some()
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let (mut parts, _) = request.into_parts();
    let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(w) => w,
        Err(e) => return e.into_response(),
    };
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let Ok(permit) = SLOTS
        .get_or_init(|| Arc::new(Semaphore::new(16)))
        .clone()
        .try_acquire_owned()
    else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let root = root.to_path_buf();
    let stream = match tokio::task::spawn_blocking(move || {
        crate::wire::connect(&root, &serde_json::json!({"op":"ssh","id":id}))
    })
    .await
    {
        Ok(Ok((s, _))) => s,
        _ => {
            return (
                StatusCode::CONFLICT,
                "Guest SSH unavailable; check enrollment and running state",
            )
                .into_response();
        }
    };
    if stream.set_nonblocking(true).is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let stream = match tokio::net::UnixStream::from_std(stream) {
        Ok(s) => s,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    ws.max_message_size(16384)
        .max_frame_size(16384)
        .write_buffer_size(0)
        .max_write_buffer_size(65536)
        .on_upgrade(move |socket| relay(socket, stream, expires, permit))
}
async fn relay(
    mut socket: WebSocket,
    mut stream: tokio::net::UnixStream,
    expires: u64,
    _permit: OwnedSemaphorePermit,
) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let deadline = tokio::time::sleep(Duration::from_secs(expires.saturating_sub(now).min(3600)));
    tokio::pin!(deadline);
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    let mut active = tokio::time::Instant::now();
    let mut buf = [0; 16384];
    loop {
        tokio::select! {
            _=&mut deadline=>break,
            _=tick.tick()=>{if active.elapsed()>Duration::from_secs(90){break;}if !matches!(tokio::time::timeout(Duration::from_secs(5),socket.send(Message::Ping(Vec::new().into()))).await,Ok(Ok(()))){break;}}
            message=socket.recv()=>{
                match message {
                    Some(Ok(Message::Binary(bytes))) if !bytes.is_empty()&&bytes.len()<=16384=>{
                        if !matches!(tokio::time::timeout(Duration::from_secs(5),stream.write_all(&bytes)).await,Ok(Ok(()))){break;}active=tokio::time::Instant::now();
                    }
                    Some(Ok(Message::Ping(_)|Message::Pong(_)))=>{},_=>break,
                }
            }
            n=stream.read(&mut buf)=>{
                let Ok(n)=n else{break;};if n==0{break;}
                if !matches!(tokio::time::timeout(Duration::from_secs(5),socket.send(Message::Binary(buf[..n].to_vec().into()))).await,Ok(Ok(()))){break;}active=tokio::time::Instant::now();
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.send(Message::Close(None))).await;
}
