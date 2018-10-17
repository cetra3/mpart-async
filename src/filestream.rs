extern crate tokio_codec;
extern crate tokio_fs;

use self::tokio_fs::file::{File, OpenFuture};
use self::tokio_codec::{BytesCodec, FramedRead};
use std::path::PathBuf;
use futures::{task, Future, Stream, Poll, Async};

use std::io::Error;

use bytes::Bytes;


// Convenience wrapper around streaming out files.  Requires tokio
pub struct FileStream {
    inner: Option<FramedRead<File, BytesCodec>>,
    file: OpenFuture<PathBuf>
}

impl FileStream {
    pub fn new<P: Into<PathBuf>>(file: P) -> Self {
        FileStream {
            file: File::open(file.into()),
            inner: None
        }
    }
}

impl Stream for FileStream {
    type Item = Bytes;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {

        if let Some(ref mut stream) = self.inner {
            if let Async::Ready(bytes_mut) = stream.poll()? {
                return Ok(Async::Ready(bytes_mut.map(|bytes| bytes.into())));
            }
        } else {
            if let Async::Ready(file) = self.file.poll()? {
                self.inner = Some(FramedRead::new(file, BytesCodec::new()));
                task::current().notify();
            }
        }
        return Ok(Async::NotReady)
    }
}

#[cfg(test)]
mod tests {
    extern crate tokio;

    use self::tokio::runtime::Runtime;
    use super::FileStream;
    use futures::{Future, Stream};

    #[test]
    fn new() {
        let mut rt = Runtime::new().expect("new rt");

        let fs = FileStream::new("Cargo.toml").concat2().and_then(|bytes| {
            assert_eq!(bytes, &include_bytes!("../Cargo.toml")[..]);
            Ok(())
        });

        rt.block_on(fs).unwrap();
    }
}
