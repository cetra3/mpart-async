use bytes::{Bytes, BytesMut};
use futures_core::Stream;
use log::debug;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{collections::VecDeque, convert::Infallible};

/// The main `MultipartRequest` struct for sending Multipart submissions to servers
pub struct MultipartRequest<S> {
    boundary: String,
    items: VecDeque<MultipartItems<S>>,
    state: Option<State<S>>,
    written: usize,
}

enum State<S> {
    WritingField(MultipartField),
    WritingStream(MultipartStream<S>),
    WritingStreamHeader(MultipartStream<S>),
    Finished,
}

/// The enum for multipart items which is either a field or a stream
pub enum MultipartItems<S> {
    /// MultipartField variant
    Field(MultipartField),
    /// MultipartStream variant
    Stream(MultipartStream<S>),
}

/// A stream which is part of a `MultipartRequest` and used to stream out bytes
pub struct MultipartStream<S> {
    name: String,
    filename: String,
    content_type: String,
    stream: S,
}

/// A MultipartField which is part of a `MultipartRequest` and used to add a standard text field
pub struct MultipartField {
    name: String,
    value: String,
}

impl<S> MultipartStream<S> {
    /// Construct a new MultipartStream providing name, filename & content_type
    pub fn new<I: Into<String>>(name: I, filename: I, content_type: I, stream: S) -> Self {
        MultipartStream {
            name: name.into(),
            filename: filename.into(),
            content_type: content_type.into(),
            stream,
        }
    }

    fn write_header(&self, boundary: &str) -> Bytes {
        let mut buf = BytesMut::new();

        buf.extend_from_slice(b"--");
        buf.extend_from_slice(boundary.as_bytes());
        buf.extend_from_slice(b"\r\n");

        buf.extend_from_slice(b"Content-Disposition: form-data; name=\"");
        buf.extend_from_slice(self.name.as_bytes());
        buf.extend_from_slice(b"\"; filename=\"");
        buf.extend_from_slice(self.filename.as_bytes());
        buf.extend_from_slice(b"\"\r\n");
        buf.extend_from_slice(b"Content-Type: ");
        buf.extend_from_slice(self.content_type.as_bytes());
        buf.extend_from_slice(b"\r\n");

        buf.extend_from_slice(b"\r\n");

        buf.freeze()
    }
}

impl MultipartField {
    /// Construct a new MultipartField given a name and value
    pub fn new<I: Into<String>>(name: I, value: I) -> Self {
        MultipartField {
            name: name.into(),
            value: value.into(),
        }
    }

    fn get_bytes(&self, boundary: &str) -> Bytes {
        let mut buf = BytesMut::new();

        buf.extend_from_slice(b"--");
        buf.extend_from_slice(boundary.as_bytes());
        buf.extend_from_slice(b"\r\n");

        buf.extend_from_slice(b"Content-Disposition: form-data; name=\"");
        buf.extend_from_slice(self.name.as_bytes());
        buf.extend_from_slice(b"\"\r\n");

        buf.extend_from_slice(b"\r\n");

        buf.extend_from_slice(self.value.as_bytes());

        buf.extend_from_slice(b"\r\n");

        buf.freeze()
    }
}

impl<E, S> MultipartRequest<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    /// Construct a new MultipartRequest with a given Boundary
    ///
    /// If you want a boundary generated automatically, then you can use `MultipartRequest::default()`
    pub fn new<I: Into<String>>(boundary: I) -> Self {
        let items = VecDeque::new();

        let state = None;

        MultipartRequest {
            boundary: boundary.into(),
            items,
            state,
            written: 0,
        }
    }

    fn next_item(&mut self) -> State<S> {
        match self.items.pop_front() {
            Some(MultipartItems::Field(new_field)) => State::WritingField(new_field),
            Some(MultipartItems::Stream(new_stream)) => State::WritingStreamHeader(new_stream),
            None => State::Finished,
        }
    }

    /// Add a raw Stream to the Multipart request
    ///
    /// The Stream should return items of `Result<Bytes, Error>`
    pub fn add_stream<I: Into<String>>(
        &mut self,
        name: I,
        filename: I,
        content_type: I,
        stream: S,
    ) {
        let stream = MultipartStream::new(name, filename, content_type, stream);

        if self.state.is_some() {
            self.items.push_back(MultipartItems::Stream(stream));
        } else {
            self.state = Some(State::WritingStreamHeader(stream));
        }
    }

    /// Add a Field to the Multipart request
    pub fn add_field<I: Into<String>>(&mut self, name: I, value: I) {
        let field = MultipartField::new(name, value);

        if self.state.is_some() {
            self.items.push_back(MultipartItems::Field(field));
        } else {
            self.state = Some(State::WritingField(field));
        }
    }

    /// Gets the boundary for the MultipartRequest
    ///
    /// This is useful for supplying the `Content-Type` header
    pub fn get_boundary(&self) -> &str {
        &self.boundary
    }

    fn write_ending(&self) -> Bytes {
        let mut buf = BytesMut::new();

        buf.extend_from_slice(b"--");
        buf.extend_from_slice(self.boundary.as_bytes());

        buf.extend_from_slice(b"--\r\n");

        buf.freeze()
    }
}

#[cfg(feature = "filestream")]
use crate::filestream::FileStream;
#[cfg(feature = "filestream")]
use std::path::PathBuf;

#[cfg(feature = "filestream")]
impl MultipartRequest<FileStream> {
    /// Add a FileStream to a MultipartRequest given a path to a file
    ///
    /// This will guess the Content Type based upon the path (i.e, .jpg will be `image/jpeg`)
    pub fn add_file<I: Into<String>, P: Into<PathBuf>>(&mut self, name: I, path: P) {
        let buf = path.into();

        let name = name.into();

        let filename = buf
            .file_name()
            .expect("Should be a valid file")
            .to_string_lossy()
            .to_string();
        let content_type = mime_guess::MimeGuess::from_path(&buf)
            .first_or_octet_stream()
            .to_string();
        let stream = FileStream::new(buf);

        self.add_stream(name, filename, content_type, stream);
    }
}

impl<E, S> Default for MultipartRequest<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    fn default() -> Self {
        let mut rng = thread_rng();

        let boundary: String = (&mut rng)
            .sample_iter(Alphanumeric)
            .take(60)
            .map(char::from)
            .collect();

        let items = VecDeque::new();

        let state = None;

        MultipartRequest {
            boundary,
            items,
            state,
            written: 0,
        }
    }
}

impl<E, S: Stream> Stream for MultipartRequest<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        debug!("Poll hit");

        let self_ref = self.get_mut();

        let mut bytes = None;

        let mut new_state = None;

        let mut waiting = false;

        if let Some(state) = self_ref.state.take() {
            match state {
                State::WritingStreamHeader(stream) => {
                    debug!("Writing Stream Header for:{}", &stream.filename);
                    bytes = Some(stream.write_header(&self_ref.boundary));

                    new_state = Some(State::WritingStream(stream));
                }
                State::WritingStream(mut stream) => {
                    debug!("Writing Stream Body for:{}", &stream.filename);

                    match Pin::new(&mut stream.stream).poll_next(cx) {
                        Poll::Pending => {
                            waiting = true;
                            new_state = Some(State::WritingStream(stream));
                        }
                        Poll::Ready(Some(Ok(some_bytes))) => {
                            bytes = Some(some_bytes);
                            new_state = Some(State::WritingStream(stream));
                        }
                        Poll::Ready(None) => {
                            let mut buf = BytesMut::new();
                            /*
                                This is a special case that we want to include \r\n and then the next item
                            */
                            buf.extend_from_slice(b"\r\n");

                            match self_ref.next_item() {
                                State::WritingStreamHeader(stream) => {
                                    debug!("Writing new Stream Header");
                                    buf.extend_from_slice(&stream.write_header(&self_ref.boundary));
                                    new_state = Some(State::WritingStream(stream));
                                }
                                State::Finished => {
                                    debug!("Writing new Stream Finished");
                                    buf.extend_from_slice(&self_ref.write_ending());
                                }
                                State::WritingField(field) => {
                                    debug!("Writing new Stream Field");
                                    buf.extend_from_slice(&field.get_bytes(&self_ref.boundary));
                                    new_state = Some(self_ref.next_item());
                                }
                                _ => (),
                            }

                            bytes = Some(buf.freeze())
                        }
                        an_error @ Poll::Ready(Some(Err(_))) => return an_error,
                    }
                }
                State::Finished => {
                    debug!("Writing Stream Finished");
                    bytes = Some(self_ref.write_ending());
                }
                State::WritingField(field) => {
                    debug!("Writing Field: {}", &field.name);
                    bytes = Some(field.get_bytes(&self_ref.boundary));
                    new_state = Some(self_ref.next_item());
                }
            }
        }

        if let Some(state) = new_state {
            self_ref.state = Some(state);
        }

        if waiting {
            return Poll::Pending;
        }

        if let Some(ref bytes) = bytes {
            debug!("Bytes: {}", bytes.len());
            self_ref.written += bytes.len();
        } else {
            debug!(
                "No bytes to write, finished stream, total bytes:{}",
                self_ref.written
            );
        }

        Poll::Ready(bytes.map(|bytes| Ok(bytes)))
    }
}

/// A Simple In-Memory Stream that can be used to store bytes
#[derive(Clone)]
pub struct ByteStream {
    bytes: Option<Bytes>,
}

impl ByteStream {
    /// Create a new ByteStream based upon the byte slice (note: this will copy from the slice)
    pub fn new(bytes: &[u8]) -> Self {
        let mut buf = BytesMut::new();

        buf.extend_from_slice(bytes);

        ByteStream {
            bytes: Some(buf.freeze()),
        }
    }
}

impl Stream for ByteStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.as_mut().bytes.take().map(Ok))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[test]
    fn sets_boundary() {
        let req: MultipartRequest<ByteStream> = MultipartRequest::new("AaB03x");
        assert_eq!(req.get_boundary(), "AaB03x");
    }

    #[test]
    fn writes_field_header() {
        let field = MultipartField::new("field_name", "field_value");

        let input: &[u8] = b"--AaB03x\r\n\
                Content-Disposition: form-data; name=\"field_name\"\r\n\
                \r\n\
                field_value\r\n";

        let bytes = field.get_bytes("AaB03x");

        assert_eq!(&bytes[..], input);
    }

    #[test]
    fn writes_stream_header() {
        let stream = MultipartStream::new("file", "test.txt", "text/plain", ByteStream::new(b""));

        let input: &[u8] = b"--AaB03x\r\n\
                Content-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\n\
                Content-Type: text/plain\r\n\
                \r\n";

        let bytes = stream.write_header("AaB03x");

        assert_eq!(&bytes[..], input);
    }

    #[tokio::test]
    async fn writes_fields() {
        let mut req: MultipartRequest<ByteStream> = MultipartRequest::new("AaB03x");

        req.add_field("name1", "value1");
        req.add_field("name2", "value2");

        let input: &[u8] = b"--AaB03x\r\n\
                Content-Disposition: form-data; name=\"name1\"\r\n\
                \r\n\
                value1\r\n\
                --AaB03x\r\n\
                Content-Disposition: form-data; name=\"name2\"\r\n\
                \r\n\
                value2\r\n\
                --AaB03x--\r\n";

        let output = req
            .fold(BytesMut::new(), |mut buf, result| async {
                if let Ok(bytes) = result {
                    buf.extend_from_slice(&bytes);
                };

                buf
            })
            .await;

        assert_eq!(&output[..], input);
    }

    #[tokio::test]
    async fn writes_streams() {
        let mut req: MultipartRequest<ByteStream> = MultipartRequest::new("AaB03x");

        let stream = ByteStream::new(b"Lorem Ipsum\n");

        req.add_stream("file", "test.txt", "text/plain", stream);

        let input: &[u8] = b"--AaB03x\r\n\
                Content-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\n\
                Content-Type: text/plain\r\n\
                \r\n\
                Lorem Ipsum\n\r\n\
                --AaB03x--\r\n";

        let output = req
            .fold(BytesMut::new(), |mut buf, result| async {
                if let Ok(bytes) = result {
                    buf.extend_from_slice(&bytes);
                };

                buf
            })
            .await;

        assert_eq!(&output[..], input);
    }

    #[tokio::test]
    async fn writes_streams_and_fields() {
        let mut req: MultipartRequest<ByteStream> = MultipartRequest::new("AaB03x");

        let stream = ByteStream::new(b"Lorem Ipsum\n");

        req.add_stream("file", "text.txt", "text/plain", stream);
        req.add_field("name1", "value1");
        req.add_field("name2", "value2");

        let input: &[u8] = b"--AaB03x\r\n\
                Content-Disposition: form-data; name=\"file\"; filename=\"text.txt\"\r\n\
                Content-Type: text/plain\r\n\
                \r\n\
                Lorem Ipsum\n\r\n\
                --AaB03x\r\n\
                Content-Disposition: form-data; name=\"name1\"\r\n\
                \r\n\
                value1\r\n\
                --AaB03x\r\n\
                Content-Disposition: form-data; name=\"name2\"\r\n\
                \r\n\
                value2\r\n\
                --AaB03x--\r\n";

        let output = req
            .fold(BytesMut::new(), |mut buf, result| async {
                if let Ok(bytes) = result {
                    buf.extend_from_slice(&bytes);
                };

                buf
            })
            .await;

        assert_eq!(&output[..], input);
    }
}
