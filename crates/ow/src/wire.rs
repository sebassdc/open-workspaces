use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
};

pub fn line(stream: &mut UnixStream) -> Result<Value> {
    let mut bytes = Vec::new();
    use std::os::fd::AsRawFd;
    let mut buffer = [0u8; 8192];
    loop {
        ensure!(bytes.len() < 1024 * 1024, "control message too large");
        let count = unsafe {
            libc::recv(
                stream.as_raw_fd(),
                buffer.as_mut_ptr() as *mut _,
                buffer.len(),
                libc::MSG_PEEK,
            )
        };
        if count < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        ensure!(count > 0, "control connection closed");
        let available = &buffer[..count as usize];
        let end = available.iter().position(|b| *b == b'\n');
        let take = end.map_or(available.len(), |n| n + 1);
        ensure!(
            bytes.len() + take <= 1024 * 1024,
            "control message too large"
        );
        stream.read_exact(&mut buffer[..take])?;
        bytes.extend_from_slice(&buffer[..take]);
        if end.is_some() {
            bytes.pop();
            break;
        }
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn send(stream: &mut UnixStream, value: &Value) -> Result<()> {
    stream.write_all(value.to_string().as_bytes())?;
    stream.write_all(b"\n")?;
    Ok(())
}

pub fn connect(root: &Path, request: &Value) -> Result<(UnixStream, Value)> {
    let mut stream = UnixStream::connect(root.join("control.sock"))?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(120)))?;
    send(&mut stream, request)?;
    let response = line(&mut stream)?;
    ensure!(
        response["ok"] == true,
        "{}",
        response["error"].as_str().unwrap_or("worker error")
    );
    Ok((stream, response["result"].clone()))
}

pub fn request(root: &Path, value: Value) -> Result<Value> {
    Ok(connect(root, &value)?.1)
}

pub fn response(result: anyhow::Result<Value>) -> Value {
    match result {
        Ok(result) => json!({"ok":true,"result":result}),
        Err(error) => json!({"ok":false,"error":format!("{error:#}")}),
    }
}

/// Preserve the worker error envelope so callers can distinguish a rejection from transport loss.
pub fn request_envelope(root: &Path, request: &Value) -> Result<Value> {
    let mut stream = UnixStream::connect(root.join("control.sock"))?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    send(&mut stream, request)?;
    line(&mut stream)
}

#[cfg(test)]
mod framing_tests {
    use super::*;
    #[test]
    fn line_preserves_stream_suffix() {
        let (mut read, mut write) = UnixStream::pair().unwrap();
        write.write_all(b"{\"ok\":true}\nPTY").unwrap();
        assert_eq!(line(&mut read).unwrap(), json!({"ok":true}));
        let mut suffix = [0; 3];
        read.read_exact(&mut suffix).unwrap();
        assert_eq!(&suffix, b"PTY");
    }
    #[test]
    fn bounded_bulk_binary_json() {
        let (mut read, mut write) = UnixStream::pair().unwrap();
        let expected = json!({"data":"A".repeat(350000)});
        let value = expected.clone();
        let writer = std::thread::spawn(move || send(&mut write, &value).unwrap());
        assert_eq!(line(&mut read).unwrap(), expected);
        writer.join().unwrap();
    }
    #[test]
    fn truncated_line_is_not_success() {
        let (mut read, mut write) = UnixStream::pair().unwrap();
        write.write_all(b"{}").unwrap();
        drop(write);
        assert!(line(&mut read).is_err());
    }
}
