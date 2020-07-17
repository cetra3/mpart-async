use futures_core::Stream;
use std::path::PathBuf;
use tokio::fs::File;
use tokio_util::codec::{BytesCodec, FramedRead};

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use std::io::Error;

use bytes::Bytes;

/// Convenience wrapper around streaming out files.  Requires tokio
///
/// You can also add this to a `MultipartRequest` using the [`add_file`](../client/struct.MultipartRequest.html#method.add_file) method:
/// ```no_run
/// # use mpart_async::client::MultipartRequest;
/// # fn main() {
/// let mut req = MultipartRequest::default();
/// req.add_file("file", "/path/to/file");
/// # }
/// ```
pub struct FileStream {
    inner: Option<FramedRead<File, BytesCodec>>,
    file: Pin<Box<dyn Future<Output = Result<File, Error>> + Send + Sync>>,
}

impl FileStream {
    /// Create a new FileStream from a file path
    pub fn new<P: Into<PathBuf>>(file: P) -> Self {
        FileStream {
            file: Box::pin(File::open(file.into())),
            inner: None,
        }
    }
}

impl Stream for FileStream {
    type Item = Result<Bytes, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(ref mut stream) = self.inner {
            return Pin::new(stream)
                .poll_next(cx)
                .map(|val| val.map(|val| val.map(|val| val.freeze())));
        } else {
            if let Poll::Ready(file_result) = self.file.as_mut().poll(cx) {
                match file_result {
                    Ok(file) => {
                        self.inner = Some(FramedRead::new(file, BytesCodec::new()));
                        cx.waker().wake_by_ref();
                    }
                    Err(err) => {
                        return Poll::Ready(Some(Err(err)));
                    }
                }
            }
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::FileStream;
    use bytes::Bytes;
    use std::io::Error;
    use tokio::stream::StreamExt;

    #[tokio::test]
    async fn streams_file() -> Result<(), Error> {
        let bytes = FileStream::new("Cargo.toml")
            .collect::<Result<Bytes, Error>>()
            .await?;

        assert_eq!(bytes, &include_bytes!("../Cargo.toml")[..]);

        Ok(())
    }
}
