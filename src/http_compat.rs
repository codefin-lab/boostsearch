//! What a client may write on the request line that hyper will not read.
//!
//! OpenSearch is served by Netty, which takes a request target as it finds
//! it: the low-level Java client sends the path a caller wrote, and callers
//! write `_cat/indices` as often as `/_cat/indices`. Hyper answers a target
//! with no leading slash with a bare `400`, before the router ever sees it.
//!
//! So the bytes of a connection pass through a small HTTP/1 reader on the way
//! in, which puts the slash back. It reads only the request line and the
//! headers of each message; a body is handed straight to the caller's buffer,
//! so the bytes that make up the bulk of a load are neither copied nor looked
//! at. Anything it does not understand -- a chunked body, a header block
//! longer than it will hold -- turns it off for the rest of the connection.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Where in a message the reader is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// waiting for a request line
    Line,
    /// waiting for the end of the header block
    Headers,
    /// this many bytes of body still to come
    Body(u64),
    /// hand everything through untouched from here on
    Through,
}

/// How long a request line and a header block may be before the reader gives
/// up on understanding the connection and hands it through.
const LINE_LIMIT: usize = 16 * 1024;
const HEADERS_LIMIT: usize = 256 * 1024;

/// A connection whose request lines are read the way OpenSearch reads them.
pub struct Lenient<S> {
    inner: S,
    phase: Phase,
    /// read from the connection, not yet understood
    hold: Vec<u8>,
    /// understood, not yet handed to the caller
    out: Vec<u8>,
    out_at: usize,
}

impl<S> Lenient<S> {
    pub fn new(inner: S) -> Self {
        Lenient { inner, phase: Phase::Line, hold: Vec::new(), out: Vec::new(), out_at: 0 }
    }
}

/// Where one slice sits inside another.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `GET _cat/indices HTTP/1.1` is a request for `/_cat/indices`.
fn fix_request_line(line: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(line).ok()?;
    let mut parts = text.splitn(3, ' ');
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if version.contains(' ') || target.is_empty() {
        return None;
    }
    // an origin-form target already has its slash; the asterisk-form and the
    // absolute-form are targets of their own
    if target.starts_with('/') || target == "*" || target.contains("://") {
        return None;
    }
    Some(format!("{method} /{target} {version}").into_bytes())
}

/// How long the body of a message with these headers is, and whether it is
/// written in chunks -- which this reader does not follow.
fn body_length(block: &[u8]) -> (u64, bool) {
    let mut len = 0u64;
    let mut chunked = false;
    for line in block.split(|b| *b == b'\n') {
        let line = std::str::from_utf8(line).unwrap_or("").trim();
        let Some((name, value)) = line.split_once(':') else { continue };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "content-length" {
            len = value.parse().unwrap_or(0);
        } else if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
    }
    (len, chunked)
}

impl<S> Lenient<S> {
    /// Move whatever has been read as far along as it will go.
    fn pump(&mut self) {
        loop {
            match self.phase {
                Phase::Through => {
                    self.out.append(&mut self.hold);
                    return;
                }
                Phase::Body(0) => self.phase = Phase::Line,
                Phase::Body(n) => {
                    let take = std::cmp::min(n as usize, self.hold.len());
                    if take == 0 {
                        return;
                    }
                    self.out.extend_from_slice(&self.hold[..take]);
                    self.hold.drain(..take);
                    self.phase = Phase::Body(n - take as u64);
                }
                Phase::Line => {
                    // HTTP/2 opens with a preface and then speaks in frames,
                    // which have no request line to read: the connection is
                    // handed through from the first byte of it
                    const H2: &[u8] = b"PRI * HTTP/2.0\r\n";
                    let short = self.hold.len() < H2.len();
                    let looks_like_h2 = if short {
                        H2.starts_with(&self.hold[..])
                    } else {
                        self.hold.starts_with(H2)
                    };
                    if looks_like_h2 {
                        if short {
                            return;
                        }
                        self.phase = Phase::Through;
                        continue;
                    }
                    let Some(at) = find(&self.hold, b"\r\n") else {
                        if self.hold.len() > LINE_LIMIT {
                            self.phase = Phase::Through;
                            continue;
                        }
                        return;
                    };
                    let line: Vec<u8> = self.hold[..at].to_vec();
                    self.hold.drain(..at + 2);
                    match fix_request_line(&line) {
                        Some(fixed) => self.out.extend_from_slice(&fixed),
                        None => self.out.extend_from_slice(&line),
                    }
                    self.out.extend_from_slice(b"\r\n");
                    self.phase = Phase::Headers;
                }
                Phase::Headers => {
                    // a message with no headers at all ends the block at once
                    if self.hold.starts_with(b"\r\n") {
                        self.out.extend_from_slice(b"\r\n");
                        self.hold.drain(..2);
                        self.phase = Phase::Line;
                        continue;
                    }
                    let Some(at) = find(&self.hold, b"\r\n\r\n") else {
                        if self.hold.len() > HEADERS_LIMIT {
                            self.phase = Phase::Through;
                            continue;
                        }
                        return;
                    };
                    let block: Vec<u8> = self.hold[..at + 4].to_vec();
                    self.hold.drain(..at + 4);
                    let (len, chunked) = body_length(&block);
                    self.out.extend_from_slice(&block);
                    self.phase = if chunked { Phase::Through } else { Phase::Body(len) };
                }
            }
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Lenient<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        loop {
            // what is already understood goes out first
            if me.out_at < me.out.len() {
                let take = std::cmp::min(buf.remaining(), me.out.len() - me.out_at);
                buf.put_slice(&me.out[me.out_at..me.out_at + take]);
                me.out_at += take;
                if me.out_at == me.out.len() {
                    me.out.clear();
                    me.out_at = 0;
                }
                return Poll::Ready(Ok(()));
            }
            // a body, with nothing held back, is read straight into the
            // caller's buffer: the bytes of a bulk load are never copied
            if me.hold.is_empty() {
                match me.phase {
                    Phase::Through => {
                        return Pin::new(&mut me.inner).poll_read(cx, buf);
                    }
                    Phase::Body(n) if n > 0 => {
                        let room = std::cmp::min(buf.remaining() as u64, n) as usize;
                        let mut narrowed = buf.take(room);
                        let before = narrowed.filled().len();
                        match Pin::new(&mut me.inner).poll_read(cx, &mut narrowed) {
                            Poll::Ready(Ok(())) => {
                                let read = narrowed.filled().len() - before;
                                unsafe { buf.assume_init(read) };
                                buf.advance(read);
                                me.phase = Phase::Body(n - read as u64);
                                return Poll::Ready(Ok(()));
                            }
                            other => return other,
                        }
                    }
                    _ => {}
                }
            }
            // otherwise read some more and see how far it gets
            let mut scratch = [0u8; 8192];
            let mut sb = ReadBuf::new(&mut scratch);
            match Pin::new(&mut me.inner).poll_read(cx, &mut sb) {
                Poll::Ready(Ok(())) => {
                    let read = sb.filled().len();
                    if read == 0 {
                        // the connection ended mid-message: hand back what
                        // was held rather than losing it
                        if !me.hold.is_empty() {
                            me.out.append(&mut me.hold);
                            continue;
                        }
                        return Poll::Ready(Ok(()));
                    }
                    me.hold.extend_from_slice(&sb.filled()[..read]);
                    me.pump();
                }
                other => return other,
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Lenient<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// A listener whose connections read request lines leniently.
pub struct LenientListener(pub tokio::net::TcpListener);

impl axum::serve::Listener for LenientListener {
    type Io = Lenient<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.0.accept().await {
                Ok((stream, addr)) => {
                    // an answer goes out as soon as it is written: waiting to
                    // fill a packet costs a request more than the packet saves
                    let _ = stream.set_nodelay(true);
                    return (Lenient::new(stream), addr);
                }
                Err(e) => {
                    if !matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionRefused
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::ConnectionReset
                    ) {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.0.local_addr()
    }
}

/// Who is on the other end of a connection.
///
/// The address type is this crate's own so that the listener above can say
/// how to read it; it carries nothing but the address, and the security layer
/// reads it as one.
#[derive(Clone, Copy, Debug)]
pub struct Peer(pub std::net::SocketAddr);

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, LenientListener>>
    for Peer
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, LenientListener>) -> Self {
        Peer(*stream.remote_addr())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_without_a_slash_gains_one() {
        assert_eq!(
            fix_request_line(b"GET _cat/indices HTTP/1.1").as_deref(),
            Some(&b"GET /_cat/indices HTTP/1.1"[..])
        );
        assert!(fix_request_line(b"GET /_cat/indices HTTP/1.1").is_none());
        assert!(fix_request_line(b"OPTIONS * HTTP/1.1").is_none());
        assert!(fix_request_line(b"GET http://host/_cat HTTP/1.1").is_none());
        assert!(fix_request_line(b"PRI * HTTP/2.0").is_none());
    }

    #[test]
    fn a_body_is_measured_from_the_headers() {
        let block = b"Host: x\r\nContent-Length: 42\r\n\r\n";
        assert_eq!(body_length(block), (42, false));
        let chunked = b"Transfer-Encoding: chunked\r\n\r\n";
        assert_eq!(body_length(chunked), (0, true));
    }

    #[tokio::test]
    async fn a_frame_stream_is_handed_through() {
        use tokio::io::AsyncReadExt;
        let mut raw: Vec<u8> = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
        // a settings frame, which has no request line in it at all
        raw.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 0, 0]);
        let mut s = Lenient::new(std::io::Cursor::new(raw.clone()));
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.expect("the cursor reads to its end");
        assert_eq!(out, raw);
    }

    #[tokio::test]
    async fn the_stream_puts_the_slash_back() {
        use tokio::io::AsyncReadExt;
        let raw: &[u8] = b"POST _bulk HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello\
                           GET /_cat HTTP/1.1\r\nHost: x\r\n\r\n";
        let mut s = Lenient::new(std::io::Cursor::new(raw.to_vec()));
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.expect("the cursor reads to its end");
        let text = String::from_utf8(out).expect("the fixture is text");
        assert!(text.starts_with("POST /_bulk HTTP/1.1\r\n"), "{text}");
        assert!(text.contains("hello"), "{text}");
        assert!(text.contains("GET /_cat HTTP/1.1"), "{text}");
    }
}
