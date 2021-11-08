//! Service that serves a file.

use super::{check_modified_headers, AsyncReadBody};
use crate::services::fs::DEFAULT_CAPACITY;
use bytes::Bytes;
use futures_util::ready;
use headers::{HeaderMapExt, IfModifiedSince, IfUnmodifiedSince, LastModified};
use http::{header, HeaderValue, Request, Response, StatusCode};
use http_body::{combinators::BoxBody, Body, Empty};
use mime::Mime;
use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
};
use tokio::fs::File;
use tower_service::Service;

/// Service that serves a file.
#[derive(Clone, Debug)]
pub struct ServeFile {
    path: PathBuf,
    mime: HeaderValue,
    buf_chunk_size: usize,
}

impl ServeFile {
    /// Create a new [`ServeFile`].
    ///
    /// The `Content-Type` will be guessed from the file extension.
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        let guess = mime_guess::from_path(&path);
        let mime = guess
            .first_raw()
            .map(|mime| HeaderValue::from_static(mime))
            .unwrap_or_else(|| {
                HeaderValue::from_str(mime::APPLICATION_OCTET_STREAM.as_ref()).unwrap()
            });

        let path = path.as_ref().to_owned();

        Self {
            path,
            mime,
            buf_chunk_size: DEFAULT_CAPACITY,
        }
    }

    /// Create a new [`ServeFile`] with a specific mime type.
    ///
    /// # Panics
    ///
    /// Will panic if the mime type isn't a valid [header value].
    ///
    /// [header value]: https://docs.rs/http/latest/http/header/struct.HeaderValue.html
    pub fn new_with_mime<P: AsRef<Path>>(path: P, mime: &Mime) -> Self {
        let mime = HeaderValue::from_str(mime.as_ref()).expect("mime isn't a valid header value");
        let path = path.as_ref().to_owned();

        Self {
            path,
            mime,
            buf_chunk_size: DEFAULT_CAPACITY,
        }
    }

    /// Set a specific read buffer chunk size.
    ///
    /// The default capacity is 64kb.
    pub fn with_buf_chunk_size(mut self, chunk_size: usize) -> Self {
        self.buf_chunk_size = chunk_size;
        self
    }
}

impl<R> Service<Request<R>> for ServeFile {
    type Response = Response<ResponseBody>;
    type Error = io::Error;
    type Future = ResponseFuture;

    #[inline]
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<R>) -> Self::Future {
        let path = self.path.clone();
        let if_unmodified_since = req.headers().typed_get::<IfUnmodifiedSince>();
        let if_modified_since = req.headers().typed_get::<IfModifiedSince>();

        let open_file_future = Box::pin(async move {
            let metadata = std::fs::metadata(&path)?;
            let modified = metadata.modified().ok().map(LastModified::from);

            if let Some(status_code) =
                check_modified_headers(modified, if_unmodified_since, if_modified_since)
            {
                return Ok(Output::StatusCode(status_code));
            }

            Ok(Output::File(File::open(path).await?, modified))
        });

        ResponseFuture {
            open_file_future,
            mime: Some(self.mime.clone()),
            buf_chunk_size: self.buf_chunk_size,
        }
    }
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + Sync + 'static>>;

/// Response future of [`ServeFile`].
pub struct ResponseFuture {
    open_file_future: BoxFuture<io::Result<Output>>,
    mime: Option<HeaderValue>,
    buf_chunk_size: usize,
}

enum Output {
    File(File, Option<LastModified>),
    StatusCode(StatusCode),
}

impl Future for ResponseFuture {
    type Output = io::Result<Response<ResponseBody>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let result = ready!(Pin::new(&mut self.open_file_future).poll(cx));

        let (file, last_modified) = match result {
            Ok(output) => match output {
                Output::File(file, last_modified) => (file, last_modified),
                Output::StatusCode(status_code) => {
                    let res = Response::builder()
                        .status(status_code)
                        .body(empty_body())
                        .unwrap();

                    return Poll::Ready(Ok(res));
                }
            },
            Err(err) => {
                return Poll::Ready(
                    super::response_from_io_error(err).map(|res| res.map(ResponseBody)),
                )
            }
        };

        let chunk_size = self.buf_chunk_size;
        let body = AsyncReadBody::with_capacity(file, chunk_size).boxed();
        let body = ResponseBody(body);

        let mut res = Response::new(body);
        res.headers_mut()
            .insert(header::CONTENT_TYPE, self.mime.take().unwrap());
        if let Some(last_modified) = last_modified {
            res.headers_mut().typed_insert(last_modified);
        }

        Poll::Ready(Ok(res))
    }
}

fn empty_body() -> ResponseBody {
    let body = Empty::new().map_err(|err| match err {}).boxed();
    ResponseBody(body)
}

opaque_body! {
    /// Response body for [`ServeFile`].
    pub type ResponseBody = BoxBody<Bytes, io::Error>;
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    use http::{Request, StatusCode};
    use http_body::Body as _;
    use hyper::Body;
    use tower::ServiceExt;

    #[tokio::test]
    async fn basic() {
        let svc = ServeFile::new("../README.md");

        let res = svc.oneshot(Request::new(Body::empty())).await.unwrap();

        assert_eq!(res.headers()["content-type"], "text/markdown");

        let body = res.into_body().data().await.unwrap().unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert!(body.starts_with("# Tower HTTP"));
    }

    #[tokio::test]
    async fn with_custom_chunk_size() {
        let svc = ServeFile::new("../README.md").with_buf_chunk_size(1024 * 32);

        let res = svc.oneshot(Request::new(Body::empty())).await.unwrap();

        assert_eq!(res.headers()["content-type"], "text/markdown");

        let body = res.into_body().data().await.unwrap().unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert!(body.starts_with("# Tower HTTP"));
    }

    #[tokio::test]
    async fn returns_404_if_file_doesnt_exist() {
        let svc = ServeFile::new("../this-doesnt-exist.md");

        let res = svc.oneshot(Request::new(Body::empty())).await.unwrap();

        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert!(res.headers().get(header::CONTENT_TYPE).is_none());
    }

    #[tokio::test]
    async fn last_modified() {
        let svc = ServeFile::new("../README.md");

        let req = Request::builder().body(Body::empty()).unwrap();
        let res = svc.oneshot(req).await.unwrap();

        assert_eq!(res.status(), StatusCode::OK);

        let last_modified = res
            .headers()
            .get(header::LAST_MODIFIED)
            .expect("Missing last modified header!");

        // -- If-Modified-Since

        let svc = ServeFile::new("../README.md");
        let req = Request::builder()
            .header(header::IF_MODIFIED_SINCE, last_modified)
            .body(Body::empty())
            .unwrap();

        let res = svc.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);

        let svc = ServeFile::new("../README.md");
        let req = Request::builder()
            .header(header::IF_MODIFIED_SINCE, "Fri, 09 Aug 1996 14:21:40 GMT")
            .body(Body::empty())
            .unwrap();

        let res = svc.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // -- If-Unmodified-Since

        let svc = ServeFile::new("../README.md");
        let req = Request::builder()
            .header(header::IF_UNMODIFIED_SINCE, last_modified)
            .body(Body::empty())
            .unwrap();

        let res = svc.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let svc = ServeFile::new("../README.md");
        let req = Request::builder()
            .header(header::IF_UNMODIFIED_SINCE, "Fri, 09 Aug 1996 14:21:40 GMT")
            .body(Body::empty())
            .unwrap();

        let res = svc.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::PRECONDITION_FAILED);
    }
}
