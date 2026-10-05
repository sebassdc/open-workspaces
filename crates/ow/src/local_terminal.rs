//! Local client for the existing guest PTY framing contract.
use crate::wire;
use std::{path::Path, time::Duration};
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
