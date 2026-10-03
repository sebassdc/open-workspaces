//! Disposable guest HTTP workload with a counter stored only in process memory.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let arguments: Vec<_> = std::env::args().collect();
    let memory_mib: usize = if arguments.get(1).map(String::as_str) == Some("--memory-mib") {
        arguments.get(2).and_then(|value| value.parse().ok()).filter(|size| *size <= 128)
            .ok_or_else(|| std::io::Error::other("memory fixture requires 0–128 MiB"))?
    } else { 0 };
    let mut memory = vec![0xabu8; memory_mib * 1024 * 1024];
    let listener = TcpListener::bind("0.0.0.0:8080")?;
    let mut counter: u64 = 0;
    for connection in listener.incoming() {
        let mut stream = connection?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut request = [0_u8; 4096];
        let count = stream.read(&mut request).unwrap_or(0);
        if count == 0 {
            continue;
        }
        if request[..count].starts_with(b"GET /dirty ") {
            for page in memory.chunks_mut(4096) { page[0] = page[0].wrapping_add(1); }
        }
        // Read each page on every request so the clean-page profile is actually faulted in.
        let checksum: u64 = memory.chunks(4096).map(|page| u64::from(page[0])).sum();
        std::hint::black_box(checksum);
        counter += 1;
        let body = format!("open-workspaces guest HTTP ready\n\nProcess counter: {counter}\n\nThis counter lives in the guest process memory.\nRefresh to increment it; snapshots and forks preserve its captured value.\n");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nX-Counter: {}\r\nConnection: close\r\n\r\n{}",
            body.len(), counter, body
        );
        // A disconnected client must not kill the test workload.
        let _ = stream.write_all(response.as_bytes());
    }
    Ok(())
}
