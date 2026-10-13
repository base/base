//! Local HTTP responses for CRL admission and revocation tests.

use std::{
    io::{Read, Write},
    net::{Shutdown, TcpListener},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// HTTP fixture routed through an allowlisted CRL hostname without external requests.
#[derive(Debug)]
pub struct CrlTestServer {
    /// Client resolving the CRL hostname to this fixture.
    pub client: reqwest::Client,
    port: u16,
    stop: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
}

impl CrlTestServer {
    /// Serves the indexed raw HTTP responses until dropped.
    pub fn new(responses: Vec<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let task_stop = Arc::clone(&stop);
        let task = thread::spawn(move || {
            while !task_stop.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                };
                // macOS inherits the listener's nonblocking mode on accepted sockets.
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                while !request.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buffer[..n]),
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let Some(index) = request.split_whitespace().nth(1).and_then(|path| {
                    if path.starts_with("/crl/ab4960cc-") {
                        Some(0)
                    } else if path.starts_with("/crl/06d48f8e-") {
                        Some(1)
                    } else {
                        path.get(1..2).and_then(|index| index.parse::<usize>().ok())
                    }
                }) else {
                    continue;
                };
                let _ = stream.write_all(&responses[index]);
                let _ = stream.shutdown(Shutdown::Write);
                while stream.read(&mut buffer).is_ok_and(|n| n != 0) {}
            }
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .resolve("nitro-enclave.amazonaws.com", addr)
            .resolve("aws-nitro-enclaves-crl.s3.amazonaws.com", addr)
            .resolve("crl-us-east-1-aws-nitro-enclaves.s3.us-east-1.amazonaws.com", addr)
            .build()
            .unwrap();
        Self { client, port: addr.port(), stop, task: Some(task) }
    }

    /// URL selecting one response.
    pub fn url(&self, index: usize) -> String {
        format!("http://nitro-enclave.amazonaws.com:{}/{index}", self.port)
    }

    /// Successful HTTP response with a fixed body length.
    pub fn ok(body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    /// Minimal DER CRL optionally listing a certificate serial.
    pub fn der(serial: Option<&[u8]>) -> Vec<u8> {
        let algorithm = hex::decode("300a06082a8648ce3d040303").unwrap();
        let date = b"\x17\x0d240101000000Z";
        let mut tbs = algorithm.clone();
        tbs.extend_from_slice(b"\x30\x00");
        tbs.extend_from_slice(date);
        if let Some(serial) = serial {
            let mut entry = vec![2, (serial.len() + usize::from(serial[0] & 0x80 != 0)) as u8];
            if serial[0] & 0x80 != 0 {
                entry.push(0);
            }
            entry.extend_from_slice(serial);
            entry.extend_from_slice(date);
            let mut revoked = vec![0x30, (entry.len() + 2) as u8, 0x30, entry.len() as u8];
            revoked.extend(entry);
            tbs.extend(revoked);
        }
        let mut body = vec![0x30, tbs.len() as u8];
        body.extend(tbs);
        body.extend(algorithm);
        body.extend_from_slice(b"\x03\x02\x00\x00");
        let mut crl = vec![0x30, body.len() as u8];
        crl.extend(body);
        crl
    }
}

impl Drop for CrlTestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.task.take().unwrap().join().unwrap();
    }
}
