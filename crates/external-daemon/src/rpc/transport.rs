//! Transport-independent RPC framing, workers, connection handling, and options.

#![cfg_attr(not(unix), allow(dead_code))]

use std::{
    io::{self, BufRead, BufReader, Read, Write},
    sync::atomic::{AtomicBool, Ordering},
    thread::JoinHandle,
    time::Duration,
};

use super::{
    RpcError, RpcErrorCode, RpcOutcome, RpcResponse, RpcService,
    MAX_REQUEST_FRAME_BYTES, MAX_RESPONSE_FRAME_BYTES,
};

#[derive(Debug, Clone, Copy)]
pub struct ServerOptions {
    pub max_connections: usize,
    pub connection_timeout: Duration,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            max_connections: 16,
            connection_timeout: Duration::from_secs(1),
        }
    }
}

pub(crate) fn reap_finished_workers(workers: &mut Vec<JoinHandle<()>>) {
    let mut index = 0;
    while index < workers.len() {
        if workers[index].is_finished() {
            let worker = workers.swap_remove(index);
            let _ = worker.join();
        } else {
            index += 1;
        }
    }
}

pub(crate) fn write_busy<W: Write>(mut writer: W) {
    let response = RpcResponse::error(
        None,
        RpcError::new(RpcErrorCode::Conflict, "server connection limit reached"),
    );
    let _ = write_response(&mut writer, response);
}

pub(crate) fn write_response<W: Write>(writer: &mut W, mut response: RpcResponse) -> io::Result<()> {
    let mut frame = serde_json::to_vec(&response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if frame.len() + 1 > MAX_RESPONSE_FRAME_BYTES {
        response = RpcResponse {
            request_id: response
                .request_id
                .filter(|request_id| !request_id.is_empty() && request_id.len() <= 128),
            outcome: RpcOutcome::Error {
                error: RpcError::new(RpcErrorCode::Oversized, "response frame exceeds cap"),
            },
        };
        frame = serde_json::to_vec(&response)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if frame.len() + 1 > MAX_RESPONSE_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "oversized fallback response exceeds cap",
            ));
        }
    }
    frame.push(b'\n');
    writer.write_all(&frame)
}

pub(crate) fn read_limited_frame<R: BufRead>(reader: &mut R, cap: usize) -> io::Result<Vec<u8>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if frame.is_empty() {
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "empty frame"))
            } else {
                Ok(frame)
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.unwrap_or(available.len());
        if frame.len().saturating_add(take) > cap {
            reader.consume(take + usize::from(newline.is_some()));
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame exceeds cap",
            ));
        }
        frame.extend_from_slice(&available[..take]);
        reader.consume(take + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(frame);
        }
    }
}

pub(crate) fn handle_connection<S, D>(
    mut stream: S,
    service: &RpcService,
    shutdown: &AtomicBool,
    peer_disconnected: D,
) where
    S: Read + Write,
    D: Fn(&S) -> bool,
{
    let response = {
        let mut reader = BufReader::new(&mut stream);
        match read_limited_frame(&mut reader, MAX_REQUEST_FRAME_BYTES - 1) {
            Ok(frame) => service.handle_bytes_interruptible(&frame, &|| {
                shutdown.load(Ordering::Acquire) || peer_disconnected(*reader.get_ref())
            }),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => RpcResponse::error(
                None,
                RpcError::new(RpcErrorCode::Oversized, "request frame exceeds cap"),
            ),
            Err(_) => return,
        }
    };
    let _ = write_response(&mut stream, response);
}
