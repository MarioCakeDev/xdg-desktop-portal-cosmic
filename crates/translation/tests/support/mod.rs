//! A throwaway HTTP server for the transport tests: no network, no API.
//!
//! Shared by `http.rs` (DeepL) and `local_http.rs` (NLLB sidecar). Each test
//! binary includes this module once, so unused helpers are allowed.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::{self, Receiver};

/// Serve `responses` in order, one connection each; return the base URL and a
/// channel with every raw request that arrived.
pub fn serve(responses: Vec<String>) -> (String, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("local address");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = stream.read(&mut chunk).expect("read request");
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if request_is_complete(&buffer) {
                    break;
                }
            }
            sender
                .send(String::from_utf8_lossy(&buffer).to_string())
                .expect("send request");
            stream
                .write_all(response.as_bytes())
                .expect("write response");
            stream.flush().expect("flush response");
        }
    });
    (format!("http://{address}"), receiver)
}

fn request_is_complete(buffer: &[u8]) -> bool {
    let text = String::from_utf8_lossy(buffer);
    let Some(head_end) = text.find("\r\n\r\n") else {
        return false;
    };
    let content_length = text[..head_end]
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    buffer.len() >= head_end + 4 + content_length
}

pub fn http_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}
