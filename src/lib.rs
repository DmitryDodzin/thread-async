#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(feature = "std", feature(maybe_uninit_slice))]
#![cfg_attr(not(feature = "std"), no_std)]
#![doc = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))]

extern crate alloc;

use alloc::{sync::Arc, task::Wake};
use core::{
  future::Future,
  pin::Pin,
  task::{Context, Poll, Waker},
};

use pin_project_lite::pin_project;

pub trait ParkApi {
  fn park();
}

#[cfg(feature = "std")]
mod std_thread {
  use std::thread::Thread;

  use super::*;

  pub struct ThreadWaker(pub Thread);

  impl ThreadWaker {
    pub fn current() -> Self {
      ThreadWaker(std::thread::current())
    }
  }

  impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
      self.0.unpark();
    }
  }

  pub struct ThreadParkApi(());

  impl ParkApi for ThreadParkApi {
    #[inline]
    fn park() {
      std::thread::park();
    }
  }
}

pin_project! {
  /// The main abstraction of `thread-async` that implements [`Future`] so it can be seemlesly used in
  /// async runtime (like tokio)
  #[derive(Debug, Clone)]
  pub struct ThreadFuture<F> {
    #[pin]
    inner: F,
  }
}

impl<F> ThreadFuture<F>
where
  F: Future,
{
  pub fn new(inner: F) -> Self {
    ThreadFuture { inner }
  }
}

impl<F> Future for ThreadFuture<F>
where
  F: Future,
{
  type Output = F::Output;

  fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
    self.project().inner.poll(ctx)
  }
}

#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
impl<F> ThreadFuture<F>
where
  F: Future,
{
  pub fn wait(self) -> F::Output {
    let waker = Arc::new(std_thread::ThreadWaker::current());

    ThreadFuture::wait_with::<std_thread::ThreadParkApi>(self, waker.into())
  }
}

impl<F> ThreadFuture<F>
where
  F: Future,
{
  pub fn wait_with<P: ParkApi>(self, waker: Waker) -> F::Output {
    let mut fut = std::pin::pin!(self.inner);
    let mut cx = Context::from_waker(&waker);

    loop {
      match fut.as_mut().poll(&mut cx) {
        Poll::Ready(res) => return res,
        Poll::Pending => P::park(),
      }
    }
  }
}

pub trait ThreadFutureExt: Future + Sized {
  fn thread_awaiter(self) -> ThreadFuture<Self> {
    ThreadFuture::new(self)
  }

  #[cfg(feature = "std")]
  #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
  fn thread_await(self) -> Self::Output {
    self.thread_awaiter().wait()
  }

  fn thread_await_with<P: ParkApi>(self, waker: Waker) -> Self::Output {
    self.thread_awaiter().wait_with::<P>(waker)
  }

  #[cfg(feature = "std")]
  #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
  fn spawn_thread_await(self) -> std::thread::JoinHandle<Self::Output>
  where
    Self: Send + 'static,
    Self::Output: Send + 'static,
  {
    std::thread::spawn(|| self.thread_awaiter().wait())
  }
}

impl<T> ThreadFutureExt for T where T: Future {}

#[cfg(test)]
mod tests {

  use std::mem::MaybeUninit;

  use async_std::{io, io::WriteExt, net::TcpStream};
  use bytes::Bytes;
  use http_body_util::{BodyExt, Empty};
  use hyper::Request;

  pin_project! {
    #[derive(Debug)]
    pub struct AsyncIo<T> {
      #[pin]
      inner: T,
    }
  }

  impl<T> AsyncIo<T> {
    pub fn new(inner: T) -> Self {
      Self { inner }
    }
  }

  impl<T> hyper::rt::Read for AsyncIo<T>
  where
    T: async_std::io::Read,
  {
    fn poll_read(
      self: Pin<&mut Self>,
      cx: &mut Context<'_>,
      mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
      let n = unsafe {
        match async_std::io::Read::poll_read(
          self.project().inner,
          cx,
          MaybeUninit::slice_assume_init_mut(buf.as_mut()),
        ) {
          Poll::Ready(Ok(amount)) => amount,
          Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
          Poll::Pending => return Poll::Pending,
        }
      };

      unsafe {
        buf.advance(n);
      }
      Poll::Ready(Ok(()))
    }
  }

  impl<T> hyper::rt::Write for AsyncIo<T>
  where
    T: async_std::io::Write,
  {
    fn poll_write(
      self: Pin<&mut Self>,
      cx: &mut Context<'_>,
      buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
      async_std::io::Write::poll_write(self.project().inner, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
      async_std::io::Write::poll_flush(self.project().inner, cx)
    }

    fn poll_shutdown(
      self: Pin<&mut Self>,
      cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
      async_std::io::Write::poll_close(self.project().inner, cx)
    }

    fn is_write_vectored(&self) -> bool {
      false
    }

    fn poll_write_vectored(
      self: Pin<&mut Self>,
      cx: &mut Context<'_>,
      bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize, std::io::Error>> {
      async_std::io::Write::poll_write_vectored(self.project().inner, cx, bufs)
    }
  }

  use super::*;

  struct Client;

  impl Client {
    async fn _inner_get(&self, url: hyper::Uri) -> Result<(), Box<dyn std::error::Error>> {
      let host = url.host().expect("uri has no host");
      let port = url.port_u16().unwrap_or(80);
      let addr = format!("{}:{}", host, port);

      let stream = TcpStream::connect(addr).await?;
      let io = AsyncIo::new(stream);

      let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
      let conn = conn.spawn_thread_await();

      let authority = url.authority().unwrap();

      let req = Request::builder()
        .method("POST")
        .header(hyper::header::HOST, authority.as_str())
        .header(hyper::header::CONTENT_TYPE, "plain/text")
        .uri(url)
        .body(Empty::<Bytes>::new())?;

      let mut res = sender.send_request(req).await?;

      println!("Response: {}", res.status());
      println!("Headers: {:#?}\n", res.headers());

      // Stream the body, writing each chunk to stdout as we get it
      // (instead of buffering and printing at the end).
      while let Some(next) = res.frame().await {
        let frame = next?;
        if let Some(chunk) = frame.data_ref() {
          io::stdout().write_all(chunk).await?;
        }
      }

      println!("\n\nDone!");

      drop(sender);

      let _ = conn.join();

      Ok(())
    }

    fn get(
      &self,
      url: hyper::Uri,
    ) -> ThreadFuture<impl Future<Output = Result<(), Box<dyn std::error::Error>>> + '_> {
      self._inner_get(url).thread_awaiter()
    }
  }

  #[test]
  fn happy() -> Result<(), Box<dyn std::error::Error>> {
    Client
      .get("http://echo.free.beeceptor.com/?foo=bar".parse()?)
      .thread_await()?;

    Ok(())
  }
}
