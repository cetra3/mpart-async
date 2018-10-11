extern crate bytes;
extern crate failure;
extern crate futures;
extern crate rand;
#[macro_use]
extern crate log;


use bytes::{Bytes, BytesMut};
use failure::Error as FailError;
use futures::Stream;
use futures::{Async, Poll};
use rand::{distributions::Alphanumeric, rngs::SmallRng, FromEntropy, Rng};

pub struct ByteStream {
    bytes: Option<Bytes>
}

impl ByteStream {
    pub fn new(bytes: &[u8]) -> Self {

        let mut buf = BytesMut::new();

        buf.extend_from_slice(bytes);

        ByteStream {
            bytes: Some(buf.freeze())
        }

    }
}

impl Stream for ByteStream
{
    type Item = Bytes;
    type Error = FailError;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {

        Ok(Async::Ready(self.bytes.take()))
    }

}

pub struct MultipartRequest<S> {
    boundary: String,
    items: Vec<MultipartItems<S>>,
    state: Option<State<S>>,
    written: usize
}

enum State<S> {
    WritingField(MultipartField),
    WritingStream(MultipartStream<S>),
    WritingStreamHeader(MultipartStream<S>),
    WritingFinished,
}

enum MultipartItems<S> {
    Field(MultipartField),
    Stream(MultipartStream<S>),
}

pub struct MultipartStream<S> {
    name: String,
    filename: String,
    content_type: String,
    stream: S,
}

pub struct MultipartField {
    name: String,
    value: String,
}

impl<S> MultipartStream<S> {
    pub fn new<I: Into<String>>(name: I, filename: I, content_type: I, stream: S) -> Self {
        MultipartStream {
            name: name.into(),
            filename: filename.into(),
            content_type: content_type.into(),
            stream,
        }
    }

    pub fn write_header(&self, boundary: &str) -> Bytes {
        let mut buf = BytesMut::new();

        buf.extend_from_slice(b"--");
        buf.extend_from_slice(&boundary.as_bytes());
        buf.extend_from_slice(b"\r\n");

        buf.extend_from_slice(b"Content-Disposition: form-data; name=\"");
        buf.extend_from_slice(&self.name.as_bytes());
        buf.extend_from_slice(b"\"; filename=\"");
        buf.extend_from_slice(&self.filename.as_bytes());
        buf.extend_from_slice(b"\"\r\n");
        buf.extend_from_slice(b"Content-Type: ");
        buf.extend_from_slice(&self.content_type.as_bytes());
        buf.extend_from_slice(b"\r\n");

        buf.extend_from_slice(b"\r\n");

        buf.freeze()
    }
}

impl MultipartField {
    pub fn new<I: Into<String>>(name: I, value: I) -> Self {
        MultipartField {
            name: name.into(),
            value: value.into(),
        }
    }

    fn get_bytes(&self, boundary: &str) -> Bytes {
        let mut buf = BytesMut::new();

        buf.extend_from_slice(b"--");
        buf.extend_from_slice(&boundary.as_bytes());
        buf.extend_from_slice(b"\r\n");

        buf.extend_from_slice(b"Content-Disposition: form-data; name=\"");
        buf.extend_from_slice(&self.name.as_bytes());
        buf.extend_from_slice(b"\"\r\n");

        buf.extend_from_slice(b"\r\n");

        buf.extend_from_slice(&self.value.as_bytes());

        buf.extend_from_slice(b"\r\n");

        buf.freeze()
    }
}

impl<S> MultipartRequest<S> {
    pub fn new<I: Into<String>>(boundary: I) -> Self {
        let items = Vec::new();

        let state = None;

        MultipartRequest {
            boundary: boundary.into(),
            items,
            state,
            written: 0
        }
    }

    fn next_item(&mut self) -> State<S> {

        match self.items.pop() {
            Some(MultipartItems::Field(new_field)) => {
                State::WritingField(new_field)
            }
            Some(MultipartItems::Stream(new_stream)) => {
                State::WritingStreamHeader(new_stream)
            }
            None => {
                State::WritingFinished
            }
        }
    }

    pub fn add_stream<I: Into<String>>(&mut self, name: I, filename: I, content_type: I, stream: S) {
        let stream = MultipartStream::new(name, filename, content_type, stream);

        if self.state.is_some() {
            self.items.push(MultipartItems::Stream(stream));
        } else {
            self.state = Some(State::WritingStreamHeader(stream));
        }

    }

    pub fn add_field<I: Into<String>>(&mut self, name: I, value: I) {
        let field = MultipartField::new(name, value);

        if self.state.is_some() {
            self.items.push(MultipartItems::Field(field));
        } else {
            self.state = Some(State::WritingField(field));
        }

    }

    pub fn get_boundary(&self) -> &str {
        &self.boundary
    }

    fn write_ending(&self) -> Bytes {
        let mut buf = BytesMut::new();

        buf.extend_from_slice(b"--");
        buf.extend_from_slice(&self.boundary.as_bytes());

        buf.extend_from_slice(b"--\r\n");

        buf.freeze()
    }
}

impl<S> Default for MultipartRequest<S> {
    fn default() -> Self {
        let mut rng = SmallRng::from_entropy();
        let boundary: String = rng.sample_iter(&Alphanumeric).take(60).collect();

        let items = Vec::new();

        let state = None;

        MultipartRequest {
            boundary,
            items,
            state,
            written: 0
        }
    }
}

impl<S> Stream for MultipartRequest<S>
where
    S: Stream<Item = Bytes>,
{
    type Item = Bytes;
    type Error = S::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {

        debug!("Poll hit");

        let mut bytes = None;

        let mut new_state = None;

        let mut waiting = false;

        if let Some(state) = self.state.take() {
            match state {
                State::WritingStreamHeader(stream) => {
                    debug!("Writing Stream Header for:{}", &stream.filename);
                    bytes = Some(stream.write_header(&self.boundary));

                    new_state = Some(State::WritingStream(stream));
                }
                State::WritingStream(mut stream) => {
                    debug!("Writing Stream Body for:{}", &stream.filename);
                    match stream.stream.poll() {
                        Ok(Async::NotReady) => {
                            waiting = true;
                            new_state = Some(State::WritingStream(stream));
                        },
                        Ok(Async::Ready(stream_bytes)) => {
                            match stream_bytes {
                                some_bytes @ Some(_) => {
                                    bytes = some_bytes;
                                    new_state = Some(State::WritingStream(stream));
                                },
                                None => {
                                debug!("Writing Stream Body Finished");

                                    /*
                                        This is a special case that we want to include \r\n and then the next item
                                    */

                                    //Need to add an \r\n to end of stream
                                    let mut buf = BytesMut::new();

                                    buf.extend_from_slice(b"\r\n");

                                    match self.next_item() {
                                        State::WritingStreamHeader(stream) => {
                                            debug!("Writing new Stream Header");
                                            buf.extend_from_slice(&stream.write_header(&self.boundary));
                                            new_state = Some(State::WritingStream(stream));
                                        },
                                        State::WritingFinished => {
                                            debug!("Writing new Stream Finished");
                                            buf.extend_from_slice(&self.write_ending());
                                        },
                                        State::WritingField(field) => {
                                            debug!("Writing new Stream Field");
                                            buf.extend_from_slice(&field.get_bytes(&self.boundary));
                                            new_state = Some(self.next_item());
                                        },
                                        _ => ()
                                    }

                                    bytes = Some(buf.freeze())
                                }
                            }

                        }
                        Err(err) => {
                            return Err(err)
                        }
                    }

                }
                State::WritingFinished => {
                    debug!("Writing Stream Finished");
                    bytes = Some(self.write_ending());
                }
                State::WritingField(field) => {
                    debug!("Writing Field: {}", &field.name);
                    bytes = Some(field.get_bytes(&self.boundary));
                    new_state = Some(self.next_item());
                }
            }
        }

        if let Some(state) = new_state {
            self.state = Some(state);
        }

        if waiting {
            return Ok(Async::NotReady)
        }

        if let Some(ref bytes) = bytes {
            debug!("Bytes: {}", bytes.len());
            self.written += bytes.len();
        } else {
            debug!("No bytes to write, finished stream, total bytes:{}", self.written);
        }
        return Ok(Async::Ready(bytes));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn writes_fields() {
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

        let output = req.wait().fold(BytesMut::new(), |mut buf, result| {
            if let Ok(bytes) = result {
                buf.extend_from_slice(&bytes);
            };

            buf
        });

        assert_eq!(&output[..], input);
    }

    #[test]
    fn writes_streams() {
        let mut req: MultipartRequest<ByteStream> = MultipartRequest::new("AaB03x");

        let stream = ByteStream::new(b"Lorem Ipsum\n");

        req.add_stream("file", "test.txt", "text/plain", stream);

        let input: &[u8] = b"--AaB03x\r\n\
                Content-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\n\
                Content-Type: text/plain\r\n\
                \r\n\
                Lorem Ipsum\n\r\n\
                --AaB03x--\r\n";

        let output = req.wait().fold(BytesMut::new(), |mut buf, result| {
            if let Ok(bytes) = result {
                buf.extend_from_slice(&bytes);
            };

            buf
        });

        assert_eq!(&output[..], input);
    }

    #[test]
    fn writes_streams_and_fields() {

        let mut req: MultipartRequest<ByteStream> = MultipartRequest::new("AaB03x");

        let stream = ByteStream::new(b"Lorem Ipsum\n");

        req.add_stream("file", "text.txt", "text/plain", stream);
        req.add_field("name2", "value2");
        req.add_field("name1", "value1");

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

        let output = req.wait().fold(BytesMut::new(), |mut buf, result| {
            if let Ok(bytes) = result {
                buf.extend_from_slice(&bytes);
            };

            buf
        });

        assert_eq!(&output[..], input);

    }
   

}
