#![allow(dead_code)]

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A throwaway redis-server on a free port, killed on drop. Returns None (skip) when redis-server is
/// not installed, unless TL_REQUIRE_REDIS is set (CI), in which case it panics.
pub struct RedisServer {
    child: Child,
    pub url: String,
}

impl RedisServer {
    pub fn start() -> Option<Self> {
        let port = TcpListener::bind("127.0.0.1:0")
            .ok()?
            .local_addr()
            .ok()?
            .port();
        let spawned = Command::new("redis-server")
            .args([
                "--port",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
                "--save",
                "",
                "--appendonly",
                "no",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(_) if std::env::var_os("TL_REQUIRE_REDIS").is_some() => {
                panic!("redis-server is not installed but TL_REQUIRE_REDIS is set")
            }
            Err(_) => {
                eprintln!("skipping: redis-server not found on PATH");
                return None;
            }
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Some(Self {
                    child,
                    url: format!("redis://127.0.0.1:{port}"),
                });
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        panic!("redis-server did not start on port {port}");
    }
}

impl Drop for RedisServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
