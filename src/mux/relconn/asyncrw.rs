use async_channel::{Receiver, Sender};
use bytes::Bytes;
use smol::prelude::*;
use std::io::{Read, Write};
use std::{
    pin::Pin,
    task::{Context, Poll},
};

/// Wraps a Receiver<Bytes> to implement AsyncRead.
pub struct BytesReader {
    inner: Receiver<Bytes>,
    read_buffer: std::io::Cursor<Bytes>,
}

impl BytesReader {
    pub fn new(inner: Receiver<Bytes>) -> Self {
        BytesReader {
            inner,
            read_buffer: std::io::Cursor::new(Bytes::new()),
        }
    }
}

fn to_ioerror<T: Into<Box<dyn std::error::Error + Send + Sync>>>(val: T) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::ConnectionAborted, val)
}

impl AsyncRead for BytesReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        while self.read_buffer.get_ref().len() - self.read_buffer.position() as usize == 0 {
            let res = smol::ready!(Pin::new(&mut self.inner).poll_next(cx));
            match res {
                Some(res) => self.read_buffer = std::io::Cursor::new(res),
                None => return Poll::Ready(Err(to_ioerror("bad"))),
            }
        }
        Poll::Ready(self.read_buffer.read(buf))
    }
}

/// Wraps a Sender<Bytes> to implement AsyncWrite.
/// Horrible but probably correct implementation that uses the blocking crate. Fix later.
pub struct BytesWriter {
    wrapped: smol::Unblock<BytesWriterInner>,
}

impl BytesWriter {
    fn project(self: Pin<&mut Self>) -> Pin<&mut impl AsyncWrite> {
        unsafe { self.map_unchecked_mut(|m| &mut m.wrapped) }
    }

    pub fn new(inner: Sender<Bytes>) -> Self {
        BytesWriter {
            wrapped: smol::Unblock::with_capacity(128 * 1024, BytesWriterInner { chan: inner }),
        }
    }
}

impl AsyncWrite for BytesWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.project().poll_write(cx, buf)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.project().poll_close(cx)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.project().poll_flush(cx)
    }
}

#[derive(Clone)]
struct BytesWriterInner {
    chan: Sender<Bytes>,
}

impl Write for BytesWriterInner {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let bts = Bytes::copy_from_slice(buf);
        smol::block_on(async { self.chan.send(bts).await })
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
