use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

pub type DynRead = Box<dyn AsyncRead + Unpin + Send>;
pub type DynWrite = Box<dyn AsyncWrite + Unpin + Send>;

pub trait Adapter: Send + 'static {
	fn split(self: Box<Self>) -> (DynRead, DynWrite);
}

pub struct InprocAdapter {
	reader: InprocReader,
	writer: InprocWriter,
}

impl InprocAdapter {
	pub fn pair() -> (Self, Self) {
		let (a_to_b_tx, a_to_b_rx) = mpsc::unbounded_channel::<Vec<u8>>();
		let (b_to_a_tx, b_to_a_rx) = mpsc::unbounded_channel::<Vec<u8>>();
		let a = InprocAdapter {
			reader: InprocReader::new(b_to_a_rx),
			writer: InprocWriter::new(a_to_b_tx),
		};
		let b = InprocAdapter {
			reader: InprocReader::new(a_to_b_rx),
			writer: InprocWriter::new(b_to_a_tx),
		};
		(a, b)
	}
}

impl Adapter for InprocAdapter {
	fn split(self: Box<Self>) -> (DynRead, DynWrite) {
		(Box::new(self.reader), Box::new(self.writer))
	}
}

pub struct InprocReader {
	rx: mpsc::UnboundedReceiver<Vec<u8>>,
	leftover: Vec<u8>,
}

impl InprocReader {
	fn new(rx: mpsc::UnboundedReceiver<Vec<u8>>) -> Self {
		Self {
			rx,
			leftover: Vec::new(),
		}
	}
}

impl AsyncRead for InprocReader {
	fn poll_read(
		mut self: Pin<&mut Self>,
		cx: &mut TaskContext<'_>,
		buf: &mut ReadBuf<'_>,
	) -> Poll<std::io::Result<()>> {
		if !self.leftover.is_empty() {
			let n = std::cmp::min(self.leftover.len(), buf.remaining());
			let tail = self.leftover.split_off(n);
			buf.put_slice(&self.leftover);
			self.leftover = tail;
			return Poll::Ready(Ok(()));
		}
		match self.rx.poll_recv(cx) {
			Poll::Ready(Some(bytes)) => {
				let n = std::cmp::min(bytes.len(), buf.remaining());
				buf.put_slice(&bytes[..n]);
				if n < bytes.len() {
					self.leftover.extend_from_slice(&bytes[n..]);
				}
				Poll::Ready(Ok(()))
			}
			Poll::Ready(None) => Poll::Ready(Ok(())),
			Poll::Pending => Poll::Pending,
		}
	}
}

pub struct InprocWriter {
	tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl InprocWriter {
	fn new(tx: mpsc::UnboundedSender<Vec<u8>>) -> Self {
		Self { tx }
	}
}

impl AsyncWrite for InprocWriter {
	fn poll_write(
		self: Pin<&mut Self>,
		_cx: &mut TaskContext<'_>,
		buf: &[u8],
	) -> Poll<std::io::Result<usize>> {
		self
			.tx
			.send(buf.to_vec())
			.map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "inproc closed"))?;
		Poll::Ready(Ok(buf.len()))
	}
	fn poll_flush(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
		Poll::Ready(Ok(()))
	}
	fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
		Poll::Ready(Ok(()))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use tokio::io::{AsyncReadExt, AsyncWriteExt};

	#[tokio::test]
	async fn inproc_reader_drains_leftover_across_small_reads() {
		let (a, b) = InprocAdapter::pair();
		let (_ar, mut aw) = Box::new(a).split();
		let (mut br, _bw) = Box::new(b).split();

		aw.write_all(b"hello").await.unwrap();

		let mut got = Vec::new();
		let mut chunk = [0u8; 2];
		while got.len() < 5 {
			let n = br.read(&mut chunk).await.unwrap();
			assert!(n > 0, "reader makes progress");
			got.extend_from_slice(&chunk[..n]);
		}
		assert_eq!(&got, b"hello", "leftover bytes are drained across reads");
	}
}
