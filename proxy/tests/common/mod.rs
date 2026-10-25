#![allow(dead_code)]

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A throwaway redis-server on a free port, killed on drop. Returns None (skip) when redis-server is
/// not installed, unless TOKENREIN_REQUIRE_REDIS is set (CI), in which case it panics.
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
            Err(_) if std::env::var_os("TOKENREIN_REQUIRE_REDIS").is_some() => {
                panic!("redis-server is not installed but TOKENREIN_REQUIRE_REDIS is set")
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

/// A throwaway Postgres cluster (initdb + postgres) on a free port, deleted on drop. Binaries come
/// from TOKENREIN_PG_BIN, PATH, or the usual Homebrew / Debian locations. Returns None (skip) when they
/// are not installed, unless TOKENREIN_REQUIRE_POSTGRES is set (CI), in which case it panics.
pub struct PostgresServer {
    child: Child,
    dir: std::path::PathBuf,
    bin: std::path::PathBuf,
    pub url: String,
}

fn pg_bin_dir() -> Option<std::path::PathBuf> {
    let has_initdb = |d: &std::path::Path| d.join("initdb").is_file();
    if let Some(d) = std::env::var_os("TOKENREIN_PG_BIN") {
        return Some(d.into());
    }
    if let Some(path) = std::env::var_os("PATH")
        && let Some(d) = std::env::split_paths(&path).find(|d| has_initdb(d))
    {
        return Some(d);
    }
    let mut candidates: Vec<std::path::PathBuf> = ["/opt/homebrew/opt", "/usr/local/opt"]
        .iter()
        .flat_map(|root| {
            (14..=19)
                .rev()
                .map(move |v| std::path::Path::new(root).join(format!("postgresql@{v}/bin")))
        })
        .collect();
    if let Ok(entries) = std::fs::read_dir("/usr/lib/postgresql") {
        let mut versions: Vec<_> = entries.flatten().map(|e| e.path().join("bin")).collect();
        versions.sort();
        candidates.extend(versions.into_iter().rev());
    }
    candidates.into_iter().find(|d| has_initdb(d))
}

impl PostgresServer {
    pub fn start() -> Option<Self> {
        let Some(bin) = pg_bin_dir() else {
            if std::env::var_os("TOKENREIN_REQUIRE_POSTGRES").is_some() {
                panic!(
                    "Postgres binaries (initdb) not found but TOKENREIN_REQUIRE_POSTGRES is set"
                );
            }
            eprintln!("skipping: Postgres (initdb) not found");
            return None;
        };
        let dir = std::env::temp_dir().join(format!("tl-pg-{}", uuid_like()));
        let ok = Command::new(bin.join("initdb"))
            .args(["-D", dir.to_str().unwrap(), "-U", "postgres", "-A", "trust"])
            .args(["-E", "UTF8", "--locale=C", "--no-sync"])
            .env("LC_ALL", "C")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "initdb failed in {}", dir.display());
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let child = Command::new(bin.join("postgres"))
            .args(["-D", dir.to_str().unwrap(), "-p", &port.to_string()])
            .args([
                "-c",
                "listen_addresses=127.0.0.1",
                "-c",
                "unix_socket_directories=",
            ])
            .args(["-c", "fsync=off", "-c", "synchronous_commit=off"])
            .args(["-c", "full_page_writes=off"])
            // Without a valid locale, Postgres on macOS refuses to start.
            .env("LC_ALL", "C")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("starting postgres");
        let server = Self {
            child,
            dir,
            bin: bin.clone(),
            url: format!("postgres://postgres@127.0.0.1:{port}/postgres"),
        };
        // Accepting TCP connections comes before accepting queries; pg_isready knows the difference.
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let ready = Command::new(bin.join("pg_isready"))
                .args([
                    "-h",
                    "127.0.0.1",
                    "-p",
                    &port.to_string(),
                    "-U",
                    "postgres",
                    "-q",
                ])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ready {
                return Some(server);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("postgres did not start on port {port}");
    }
}

fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!(
        "{}-{nanos}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    )
}

impl Drop for PostgresServer {
    fn drop(&mut self) {
        // Stop it properly: a killed postmaster leaves its System V shared memory segment
        // behind, and macOS allows only a few dozen of those.
        let stopped = Command::new(self.bin.join("pg_ctl"))
            .args([
                "stop",
                "-D",
                self.dir.to_str().unwrap(),
                "-m",
                "immediate",
                "-w",
            ])
            .env("LC_ALL", "C")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !stopped {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
