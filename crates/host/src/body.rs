//! Unified response body type for the middleware stack.
//!
//! Every layer seam (compression, static files, Roc responses, error pages)
//! boxes into `AppBody`, and CORS requires `Default` for preflight
//! short-circuits, which the raw `BoxBody` combinator does not provide.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub struct AppBody(UnsyncBoxBody<Bytes, BoxError>);

impl AppBody {
    pub fn new<B>(body: B) -> Self
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<BoxError>,
    {
        Self(body.map_err(Into::into).boxed_unsync())
    }

    pub fn empty() -> Self {
        Self::new(Empty::new())
    }

    pub fn full(bytes: impl Into<Bytes>) -> Self {
        Self::new(Full::new(bytes.into()))
    }
}

impl Default for AppBody {
    fn default() -> Self {
        Self::empty()
    }
}

impl Body for AppBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.0).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.0.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.0.size_hint()
    }
}
