//! BuildKit's gRPC API on `/grpc`, relayed frame by frame. Every frame passes unchanged except the request body of a
//! Solve call that names a frontend, which gains the proxy as build arguments. Header blocks are decoded, never
//! re-encoded, so a client the proxy cannot follow keeps working without build arguments.

mod frame;
mod hpack;
mod solve;

use std::collections::HashMap;
use std::io;
use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use self::frame::{
    CONTINUATION, DATA, END_HEADERS, END_STREAM, Frame, HEADERS, PADDED, RST_STREAM, SETTINGS, WINDOW_UPDATE,
};
use self::solve::SolveFields;
use crate::variables::ProxyVariables;

const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
/// Window of a new connection, and of every stream until SETTINGS change it (RFC 9113, section 6.9.2).
const DEFAULT_WINDOW: i64 = 65_535;
const SETTINGS_INITIAL_WINDOW_SIZE: u16 = 0x4;
/// Largest Solve call the relay holds to rewrite; a larger one passes unchanged.
const MAX_SOLVE: usize = 16 * 1024 * 1024;
/// Room the engine's connection window keeps after a call grows. The client believes the window larger by the added
/// bytes until later window updates withhold them, so without this room it could overrun the window meanwhile.
const SPARE_WINDOW: i64 = 64 * 1024;

/// Relays an upgraded `/grpc` connection between `client` and `engine` until both directions end.
pub(crate) async fn relay<C, E>(client: C, engine: E, variables: ProxyVariables) -> io::Result<()>
where
    C: AsyncRead + AsyncWrite + Send,
    E: AsyncRead + AsyncWrite + Send,
{
    let (mut client_read, mut client_write) = tokio::io::split(client);
    let (mut engine_read, mut engine_write) = tokio::io::split(engine);
    let mut preface = [0; PREFACE.len()];
    client_read.read_exact(&mut preface).await?;
    engine_write.write_all(&preface).await?;
    if preface != *PREFACE {
        let requests = async {
            tokio::io::copy(&mut client_read, &mut engine_write).await?;
            engine_write.shutdown().await
        };
        let responses = async {
            tokio::io::copy(&mut engine_read, &mut client_write).await?;
            client_write.shutdown().await
        };
        tokio::try_join!(requests, responses)?;
        return Ok(());
    }

    let flow = Mutex::new(Flow::new());
    tokio::try_join!(
        requests(client_read, engine_write, &flow, &variables),
        responses(engine_read, client_write, &flow),
    )?;
    Ok(())
}

async fn requests<R, W>(mut client: R, mut engine: W, flow: &Mutex<Flow>, variables: &ProxyVariables) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut calls = Calls::new(variables);
    while let Some(frame) = Frame::read(&mut client).await? {
        let frames = calls.pass(frame, &mut lock(flow));
        for frame in frames {
            frame.write(&mut engine).await?;
        }
    }
    let frames = calls.release_all(&mut lock(flow));
    for frame in frames {
        frame.write(&mut engine).await?;
    }

    engine.shutdown().await
}

async fn responses<R, W>(mut engine: R, mut client: W, flow: &Mutex<Flow>) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    while let Some(mut frame) = Frame::read(&mut engine).await? {
        let passed = lock(flow).received(&mut frame);
        if passed {
            frame.write(&mut client).await?;
        }
    }

    client.shutdown().await
}

fn lock(flow: &Mutex<Flow>) -> MutexGuard<'_, Flow> {
    flow.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the engine still accepts from the client: its flow-control windows, and the bytes that the client's view of
/// the connection window exceeds them by.
#[derive(Debug)]
struct Flow {
    connection: i64,
    initial_stream: i64,
    /// Windows of held streams.
    streams: HashMap<u32, i64>,
    /// Bytes calls grew by that later connection window updates have not withheld from the client yet.
    owed: u32,
}

impl Flow {
    fn new() -> Self {
        Self {
            connection: DEFAULT_WINDOW,
            initial_stream: DEFAULT_WINDOW,
            streams: HashMap::new(),
            owed: 0,
        }
    }

    /// Notes a frame the engine sends; returns whether the client receives it, which a window update reduced to
    /// nothing it does not.
    fn received(&mut self, frame: &mut Frame) -> bool {
        match frame.kind {
            SETTINGS if frame.flags & frame::ACK == 0 => {
                for [i0, i1, v0, v1, v2, v3] in frame.payload.as_chunks::<6>().0 {
                    if u16::from_be_bytes([*i0, *i1]) == SETTINGS_INITIAL_WINDOW_SIZE {
                        let window = i64::from(u32::from_be_bytes([*v0, *v1, *v2, *v3]));
                        let change = window - self.initial_stream;
                        self.initial_stream = window;
                        for stream in self.streams.values_mut() {
                            *stream += change;
                        }
                    }
                }
                true
            }
            WINDOW_UPDATE => {
                let Ok(&increment) = <&[u8; 4]>::try_from(&frame.payload[..]) else {
                    return true;
                };
                let increment = u32::from_be_bytes(increment) & 0x7fff_ffff;
                if frame.stream != 0 {
                    if let Some(window) = self.streams.get_mut(&frame.stream) {
                        *window += i64::from(increment);
                    }
                    return true;
                }
                self.connection += i64::from(increment);
                let withheld = increment.min(self.owed);
                self.owed -= withheld;
                frame.payload = (increment - withheld).to_be_bytes().to_vec();
                increment > withheld
            }
            _ => true,
        }
    }

    fn sent(&mut self, frames: &[Frame]) {
        for frame in frames.iter().filter(|frame| frame.kind == DATA) {
            self.connection -= i64::try_from(frame.payload.len()).unwrap_or(i64::MAX);
        }
    }

    /// Whether held `stream` may carry `grown` bytes in place of `original` ones; if so, the difference is owed.
    fn grow(&mut self, stream: u32, original: usize, grown: usize) -> bool {
        let (Ok(grown_bytes), Ok(extra)) = (i64::try_from(grown), u32::try_from(grown.saturating_sub(original))) else {
            return false;
        };
        let stream_window = self.streams.get(&stream).copied().unwrap_or(0);
        let fits = grown_bytes <= stream_window && grown_bytes + SPARE_WINDOW <= self.connection;
        if fits {
            self.owed = self.owed.saturating_add(extra);
        }

        fits
    }
}

/// A Solve call whose request body the relay holds until the client ends the stream.
struct Held {
    fields: SolveFields,
    frames: Vec<Frame>,
    body: Vec<u8>,
}

/// Header block arriving in a HEADERS frame and its CONTINUATION frames.
struct Block {
    stream: u32,
    ends_stream: bool,
    fragments: Vec<u8>,
}

/// The client's side of the relay: which streams are Solve calls, and their held request bodies.
struct Calls<'a> {
    variables: &'a ProxyVariables,
    /// `None` once a header block could not be decoded; no further call is held then.
    decoder: Option<hpack::Decoder>,
    block: Option<Block>,
    held: HashMap<u32, Held>,
}

impl<'a> Calls<'a> {
    fn new(variables: &'a ProxyVariables) -> Self {
        Self {
            variables,
            decoder: Some(hpack::Decoder::new()),
            block: None,
            held: HashMap::new(),
        }
    }

    /// The frames to send the engine for `frame` from the client.
    fn pass(&mut self, frame: Frame, flow: &mut Flow) -> Vec<Frame> {
        let mut frames = match frame.kind {
            HEADERS | RST_STREAM => self.release(frame.stream, flow),
            _ => Vec::new(),
        };
        match frame.kind {
            HEADERS | CONTINUATION => {
                self.follow_headers(&frame, flow);
                frames.push(frame);
            }
            DATA if self.held.contains_key(&frame.stream) => frames.extend(self.hold(frame, flow)),
            _ => frames.push(frame),
        }

        flow.sent(&frames);
        frames
    }

    /// Releases every held body unchanged.
    fn release_all(&mut self, flow: &mut Flow) -> Vec<Frame> {
        let streams: Vec<u32> = self.held.keys().copied().collect();
        let frames: Vec<Frame> = streams
            .into_iter()
            .flat_map(|stream| self.release(stream, flow))
            .collect();

        flow.sent(&frames);
        frames
    }

    fn release(&mut self, stream: u32, flow: &mut Flow) -> Vec<Frame> {
        flow.streams.remove(&stream);
        self.held.remove(&stream).map(|held| held.frames).unwrap_or_default()
    }

    fn hold(&mut self, frame: Frame, flow: &mut Flow) -> Vec<Frame> {
        let stream = frame.stream;
        let Some(held) = self.held.get_mut(&stream) else {
            return vec![frame];
        };
        let ends = frame.flags & END_STREAM != 0;
        let unreadable = frame.flags & PADDED != 0 || held.body.len() + frame.payload.len() > MAX_SOLVE;
        if !unreadable {
            held.body.extend_from_slice(&frame.payload);
        }
        held.frames.push(frame);
        if unreadable {
            return self.release(stream, flow);
        }
        if !ends {
            return Vec::new();
        }

        let Some(held) = self.held.remove(&stream) else {
            return Vec::new();
        };
        let grown = solve::solve_call(&held.body, held.fields, self.variables)
            .filter(|call| flow.grow(stream, held.body.len(), call.len()));
        flow.streams.remove(&stream);
        match grown {
            Some(call) => {
                tracing::debug!(stream, "passing the proxy to a build");
                Frame::body(stream, &call)
            }
            None => held.frames,
        }
    }

    /// Decodes the header blocks of the client's requests and holds the body of each Solve call.
    fn follow_headers(&mut self, frame: &Frame, flow: &mut Flow) {
        if self.decoder.is_none() {
            return;
        }
        let Some(fragment) = frame.fragment() else {
            self.lose_track();
            return;
        };
        match (frame.kind, self.block.as_mut()) {
            (HEADERS, _) => {
                self.block = Some(Block {
                    stream: frame.stream,
                    ends_stream: frame.flags & END_STREAM != 0,
                    fragments: fragment.to_vec(),
                });
            }
            (_, Some(block)) if block.stream == frame.stream => block.fragments.extend_from_slice(fragment),
            _ => {
                self.lose_track();
                return;
            }
        }
        if frame.flags & END_HEADERS == 0 {
            return;
        }

        let (Some(block), Some(decoder)) = (self.block.take(), self.decoder.as_mut()) else {
            return;
        };
        let Some(fields) = decoder.decode(&block.fragments) else {
            self.lose_track();
            return;
        };
        // A grown body would contradict a declared length.
        let sized = fields.iter().any(|(name, _)| name == b"content-length");
        let solve = fields
            .iter()
            .find(|(name, _)| name == b":path")
            .and_then(|(_, path)| std::str::from_utf8(path).ok())
            .and_then(solve::solve_fields);
        if let Some(fields) = solve.filter(|_| !block.ends_stream && !sized) {
            flow.streams.insert(block.stream, flow.initial_stream);
            self.held.insert(
                block.stream,
                Held {
                    fields,
                    frames: Vec::new(),
                    body: Vec::new(),
                },
            );
        }
    }

    fn lose_track(&mut self) {
        tracing::warn!("cannot follow a BuildKit session; its builds get no proxy");
        self.decoder = None;
        self.block = None;
    }
}

#[cfg(test)]
mod tests;
