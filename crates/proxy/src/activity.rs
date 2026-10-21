use super::server::{Body, BoxError};
use hyper::body::{Body as HttpBody, Frame, SizeHint};
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::watch;

/// A response stays active until its body ends or the client drops it.
pub(crate) struct ActiveBody {
    inner: Body,
    activity: Option<Activity>,
}

impl ActiveBody {
    /// Keeps SSE outside the downstream idle timeout.
    pub(crate) fn new(inner: Body, activity: Activity) -> Self {
        Self {
            inner,
            activity: Some(activity),
        }
    }
}

pub(crate) struct Activity(watch::Sender<usize>);

impl Activity {
    /// Marks request handling active before dialing or receiving upstream headers.
    pub(crate) fn new(sender: watch::Sender<usize>) -> Self {
        sender.send_modify(|count| *count += 1);
        Self(sender)
    }
}

impl Drop for Activity {
    /// Notifies the connection driver when the last active response is released.
    fn drop(&mut self) {
        self.0.send_modify(|count| *count -= 1);
    }
}

impl HttpBody for ActiveBody {
    type Data = bytes::Bytes;
    type Error = BoxError;

    /// Releases activity at end-of-stream even when Hyper retains the body wrapper.
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let result = Pin::new(&mut self.inner).poll_frame(cx);
        if matches!(result, Poll::Ready(None | Some(Err(_)))) || self.inner.is_end_stream() {
            self.activity.take();
        }
        result
    }

    /// Preserves Hyper's content-length and empty-body handling.
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }

    /// Delegates framing metadata without buffering any response bytes.
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}
