use std::pin::Pin;

use futures::{
    stream::{Fuse, FusedStream, Stream, StreamExt},
    task::{Context, Poll}};
use pin_project_lite::pin_project;

pin_project! {
    pub struct ShuttingDownStream<St, CSt> {
        #[pin]
        stream: Fuse<St>,
        #[pin]
        control_stream: Fuse<CSt>,
    }
}

/// Adapter for controlling primary stream with control stream.
/// Stream item for adapter is a tuple of item from primary list
/// and bool field indicating if control stream is closed.
impl<St: Stream, CSt: Stream> ShuttingDownStream<St, CSt> {
    pub fn new(stream: St, control_stream: CSt) -> Self {
        Self { stream: stream.fuse(), control_stream: control_stream.fuse() }
    }
}

impl<St, CSt> FusedStream for ShuttingDownStream<St, CSt>
    where
        St: Stream,
        CSt: Stream,
{
    fn is_terminated(&self) -> bool {
        self.stream.is_terminated() || self.control_stream.is_terminated()
    }
}

impl<St: Stream, CSt: Stream> Stream for ShuttingDownStream<St, CSt> {
    type Item = (St::Item, bool);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        let b = this.control_stream.as_mut().poll_next(cx);

        if let Poll::Ready(Some(entry)) = this.stream.as_mut().poll_next(cx) {
            return Poll::Ready(Some((entry, b.is_ready())));
        } else if b.is_ready() {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}