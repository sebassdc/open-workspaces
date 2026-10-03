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
    let mut byte = [0u8];
    loop {
        ensure!(bytes.len() < 1024 * 1024, "control message too large");
        ensure!(stream.read(&mut byte)? == 1, "control connection closed");
        if byte[0] == b'\n' {
            break;
        }
        bytes.push(byte[0]);
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
