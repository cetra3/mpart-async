use bytes::Bytes;
use futures::Stream;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use httparse::Status;
use std::mem;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use thiserror::Error;

use twoway::find_bytes;

pub struct MultipartField<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<anyhow::Error>,
{
    headers: HeaderMap<HeaderValue>,
    state: Arc<Mutex<MultipartState<S, E>>>,
}

impl<S, E> MultipartField<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<anyhow::Error>,
{
    pub fn headers(&self) -> &HeaderMap<HeaderValue> {
        &self.headers
    }

    pub fn content_type<'a>(&'a self) -> Result<&'a str, MultipartError> {
        if let Some(val) = self.headers.get("content-type") {
            return val.to_str().map_err(|_| MultipartError::InvalidHeader);
        }

        Err(MultipartError::InvalidHeader)
    }

    pub fn filename<'a>(&'a self) -> Result<&'a str, MultipartError> {
        if let Some(val) = self.headers.get("content-disposition") {
            let string_val = val.to_str().map_err(|_| MultipartError::InvalidHeader)?;
            if let Some(filename) = get_dispo_param(&string_val, "filename") {
                return Ok(filename);
            }
        }

        Err(MultipartError::InvalidHeader)
    }

    pub fn name<'a>(&'a self) -> Result<&'a str, MultipartError> {
        if let Some(val) = self.headers.get("content-disposition") {
            let string_val = val.to_str().map_err(|_| MultipartError::InvalidHeader)?;
            if let Some(filename) = get_dispo_param(&string_val, "name") {
                return Ok(filename);
            }
        }

        Err(MultipartError::InvalidHeader)
    }
}

fn get_dispo_param<'a>(input: &'a str, param: &str) -> Option<&'a str> {
    if let Some(start_idx) = input.find(&param) {
        let end_param = start_idx + param.len();
        //check bounds
        if input.len() > end_param + 2 {
            if &input[end_param..end_param + 2] == "=\"" {
                let start = end_param + 2;

                if let Some(end) = &input[start..].find("\"") {
                    return Some(&input[start..start + end]);
                }
            }
        }
    }

    return None;
}

//Streams out bytes
impl<S, E> Stream for MultipartField<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<anyhow::Error>,
{
    type Item = Result<Bytes, MultipartError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let self_mut = &mut self.as_mut();

        let state = &mut self_mut
            .state
            .try_lock()
            .map_err(|_| MultipartError::InternalBorrowError)?;

        match Pin::new(&mut state.parser).poll_next(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Some(Err(err))) => {
                return Poll::Ready(Some(Err(MultipartError::Stream(err.into()))))
            }
            Poll::Ready(None) => return Poll::Ready(None),
            //If we have headers, we have reached the next file
            Poll::Ready(Some(Ok(ParseOutput::Headers(headers)))) => {
                state.next_item = Some(headers);
                return Poll::Ready(None);
            }
            Poll::Ready(Some(Ok(ParseOutput::Bytes(bytes)))) => {
                return Poll::Ready(Some(Ok(bytes)))
            }
        }
    }
}

//This is our state we use to drive the parser.  The `next_item` is there just for headers if there are more in the request
struct MultipartState<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<anyhow::Error>,
{
    parser: MultipartParser<S, E>,
    next_item: Option<HeaderMap<HeaderValue>>,
}

pub struct MultipartStream<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<anyhow::Error>,
{
    state: Arc<Mutex<MultipartState<S, E>>>,
}

impl<S, E> MultipartStream<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<anyhow::Error>,
{
    /// Construct a MultipartStream given a boundary
    pub fn new<I: Into<Bytes>>(boundary: I, stream: S) -> Self {
        Self {
            state: Arc::new(Mutex::new(MultipartState {
                parser: MultipartParser::new(boundary, stream),
                next_item: None,
            })),
        }
    }
}

impl<S, E> Stream for MultipartStream<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<anyhow::Error>,
{
    type Item = Result<MultipartField<S, E>, MultipartError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let self_mut = &mut self.as_mut();

        let state = &mut self_mut
            .state
            .try_lock()
            .map_err(|_| MultipartError::InternalBorrowError)?;

        if let Some(headers) = state.next_item.take() {
            return Poll::Ready(Some(Ok(MultipartField {
                headers,
                state: self_mut.state.clone(),
            })));
        }

        match Pin::new(&mut state.parser).poll_next(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Some(Err(err))) => {
                return Poll::Ready(Some(Err(MultipartError::Stream(err.into()))))
            }
            Poll::Ready(None) => return Poll::Ready(None),

            //If we have headers, we have reached the next file
            Poll::Ready(Some(Ok(ParseOutput::Headers(headers)))) => {
                return Poll::Ready(Some(Ok(MultipartField {
                    headers,
                    state: self_mut.state.clone(),
                })));
            }
            Poll::Ready(Some(Ok(ParseOutput::Bytes(_bytes)))) => {
                //If we are returning bytes from this stream, then there is some error
                return Poll::Ready(Some(Err(MultipartError::ShouldPollField)));
            }
        }
    }
}

#[derive(Error, Debug)]
pub enum MultipartError {
    #[error("Invalid Boundary. (expected {expected:?}, found {found:?})")]
    InvalidBoundary { expected: String, found: String },
    #[error("Incomplete Headers")]
    IncompleteHeader,
    #[error("Invalid Header Value")]
    InvalidHeader,
    #[error(
        "Tried to poll an MultipartStream when the MultipartField should be polled, try using `flatten()`"
    )]
    ShouldPollField,
    #[error("Tried to poll an MultipartField and the Mutex has already been locked")]
    InternalBorrowError,
    #[error(transparent)]
    HeaderParse(#[from] httparse::Error),
    #[error(transparent)]
    Stream(#[from] anyhow::Error),
}

//This parses the multipart and then streams out headers & bytes
pub struct MultipartParser<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<anyhow::Error>,
{
    boundary: Bytes,
    buffer: Vec<u8>,
    state: State,
    stream_finished: bool,
    stream: S,
}

impl<S, E> MultipartParser<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<anyhow::Error>,
{
    pub fn new<I: Into<Bytes>>(boundary: I, stream: S) -> Self {
        Self {
            boundary: boundary.into(),
            buffer: Vec::new(),
            state: State::ReadingBoundary,
            stream_finished: false,
            stream,
        }
    }

    //Return a poll if the stream is finished, or pending if we're waiting for our buffer
    fn return_poll(&mut self) -> Poll<Option<Result<ParseOutput, MultipartError>>> {
        if self.stream_finished {
            self.state = State::Finished;
            return Poll::Ready(None);
        } else {
            return Poll::Pending;
        }
    }
}

const NUM_HEADERS: usize = 16;

fn get_headers(buffer: &[u8]) -> Result<HeaderMap<HeaderValue>, MultipartError> {
    let mut headers = [httparse::EMPTY_HEADER; NUM_HEADERS];

    let idx = match httparse::parse_headers(&buffer, &mut headers)? {
        Status::Complete((idx, _val)) => idx,
        Status::Partial => return Err(MultipartError::IncompleteHeader),
    };

    let mut header_map = HeaderMap::with_capacity(idx);

    for header in headers.iter().take(idx) {
        if header.name != "" {
            header_map.insert(
                HeaderName::from_bytes(header.name.as_bytes())
                    .map_err(|_| MultipartError::InvalidHeader)?,
                HeaderValue::from_bytes(header.value).map_err(|_| MultipartError::InvalidHeader)?,
            );
        }
    }

    Ok(header_map)
}

impl<S, E> Stream for MultipartParser<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<anyhow::Error>,
{
    type Item = Result<ParseOutput, MultipartError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let self_mut = &mut self.as_mut();

        //The stream might be finished but we might not be
        if !self_mut.stream_finished {
            match Pin::new(&mut self_mut.stream).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Err(err))) => {
                    return Poll::Ready(Some(Err(MultipartError::Stream(err.into()))))
                }
                Poll::Ready(maybe_bytes) => {
                    match maybe_bytes {
                        Some(Ok(bytes)) => {
                            self_mut.buffer.extend_from_slice(&bytes);
                        }
                        Some(Err(_)) => {
                            //Unreachable, covered by match statement above.
                            unreachable!();
                        }
                        None => {
                            self_mut.stream_finished = true;

                            if self_mut.buffer.len() == 0 {
                                return Poll::Ready(None);
                            }
                        }
                    }
                }
            }
        }

        let boundary_len = self_mut.boundary.len();

        match self_mut.state {
            State::ReadingBoundary => {
                //If the buffer is too small
                if self_mut.buffer.len() < boundary_len + 4 {
                    return self_mut.return_poll();
                }

                //If the buffer starts with `--<boundary>\r\n`
                if &self_mut.buffer[..2] == b"--"
                    && &self_mut.buffer[2..boundary_len + 2] == &self_mut.boundary
                    && &self_mut.buffer[boundary_len + 2..boundary_len + 4] == b"\r\n"
                {
                    //remove the boundary from the buffer, returning the tail
                    self_mut.buffer = self_mut.buffer.split_off(boundary_len + 4);
                    self_mut.state = State::ReadingHeader;
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                } else {
                    let expected =
                        format!("--{}\\r\\n", String::from_utf8_lossy(&self_mut.boundary));
                    let found =
                        String::from_utf8_lossy(&self_mut.buffer[..boundary_len + 4]).to_string();

                    let error = MultipartError::InvalidBoundary { expected, found };

                    //There is some error with the boundary...
                    return Poll::Ready(Some(Err(error)));
                }
            }
            State::ReadingHeader => {
                if let Some(end) = find_bytes(&self_mut.buffer, b"\r\n\r\n") {
                    //Need to include the end header bytes for the parse to work
                    let end = end + 4;

                    let header_map = match get_headers(&self_mut.buffer[0..end]) {
                        Ok(headers) => headers,
                        Err(error) => {
                            self_mut.state = State::Finished;
                            return Poll::Ready(Some(Err(error)));
                        }
                    };

                    self_mut.buffer = self_mut.buffer.split_off(end);

                    self_mut.state = State::StreamingContent;
                    cx.waker().wake_by_ref();

                    return Poll::Ready(Some(Ok(ParseOutput::Headers(header_map))));
                } else {
                    return self_mut.return_poll();
                }
            }

            State::StreamingContent => {
                //We want to check the value of the buffer to see if there looks like there is an `end` boundary.
                if let Some(idx) = find_bytes(&self_mut.buffer, b"\r") {
                    //Check the length has enough packets for us
                    if self_mut.buffer.len() < idx + 6 + boundary_len {
                        //If the inner stream is finished, we should finish here too
                        return self_mut.return_poll();
                    }

                    //The start of the boundary is 4 chars. i.e, after `\r\n--`
                    let start_boundary = idx + 4;

                    if &self_mut.buffer[idx..start_boundary] == b"\r\n--"
                        && &self_mut.buffer[start_boundary..start_boundary + boundary_len]
                            == self_mut.boundary
                    {
                        //We want the chars after the boundary basically
                        let after_boundary = &self_mut.buffer
                            [start_boundary + boundary_len..start_boundary + boundary_len + 2];

                        if after_boundary == b"\r\n" {
                            let mut other_bytes = self_mut.buffer.split_off(idx);

                            //Remove the boundary-related bytes from the start of the buffer
                            other_bytes = other_bytes.split_off(6 + boundary_len);

                            //Return the bytes up to the boundary, we're finished and need to go back to reading headers
                            let return_bytes =
                                Bytes::from(mem::replace(&mut self_mut.buffer, other_bytes));

                            //Replace the buffer with the extra bytes
                            self_mut.state = State::ReadingHeader;
                            cx.waker().wake_by_ref();

                            return Poll::Ready(Some(Ok(ParseOutput::Bytes(return_bytes))));
                        } else if after_boundary == b"--" {
                            //We're at the end, just truncate the bytes
                            self_mut.buffer.truncate(idx);
                            self_mut.state = State::Finished;

                            return Poll::Ready(Some(Ok(ParseOutput::Bytes(Bytes::from(
                                mem::replace(&mut self_mut.buffer, Vec::new()),
                            )))));
                        }
                    }
                }

                //If we didn't find an `\r` in any of the multiparts, just take out the buffer and return as bytes
                let buffer = mem::replace(&mut self_mut.buffer, Vec::new());

                return Poll::Ready(Some(Ok(ParseOutput::Bytes(Bytes::from(buffer)))));
            }
            State::Finished => return Poll::Ready(None),
        }
    }
}

#[derive(Debug, PartialEq)]
enum State {
    ReadingBoundary,
    ReadingHeader,
    StreamingContent,
    Finished,
}

#[derive(Debug)]
pub enum ParseOutput {
    Headers(HeaderMap<HeaderValue>),
    Bytes(Bytes),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ByteStream;
    use futures::executor::block_on;
    use futures::StreamExt;

    #[test]
    fn read_stream() {
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

        let mut stream = MultipartStream::new("AaB03x", ByteStream::new(input));

        if let Some(Ok(mut mpart_field)) = block_on(stream.next()) {
            assert_eq!(mpart_field.name().ok(), Some("file"));
            assert_eq!(mpart_field.filename().ok(), Some("text.txt"));

            if let Some(Ok(bytes)) = block_on(mpart_field.next()) {
                assert_eq!(bytes, Bytes::from(b"Lorem Ipsum\n" as &[u8]));
            }
        } else {
            panic!("First value should be a field")
        }
    }

    #[test]
    fn read_filename() {
        let input = "form-data; name=\"file\"; filename=\"text.txt\"";
        let name = get_dispo_param(input, "name");
        let filename = get_dispo_param(input, "filename");

        assert_eq!(name, Some("file"));
        assert_eq!(filename, Some("text.txt"));
    }

    #[test]
    fn reads_streams_and_fields() {
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

        let mut read = MultipartParser::new("AaB03x", ByteStream::new(input));

        if let Some(Ok(ParseOutput::Headers(val))) = block_on(read.next()) {
            println!("Headers:{:?}", val);
        } else {
            panic!("First value should be a header")
        }

        if let Some(Ok(ParseOutput::Bytes(bytes))) = block_on(read.next()) {
            assert_eq!(&*bytes, b"Lorem Ipsum\n");
        } else {
            panic!("Second value should be bytes")
        }

        if let Some(Ok(ParseOutput::Headers(val))) = block_on(read.next()) {
            println!("Headers:{:?}", val);
        } else {
            panic!("Third value should be a header")
        }

        if let Some(Ok(ParseOutput::Bytes(bytes))) = block_on(read.next()) {
            assert_eq!(&*bytes, b"value1");
        } else {
            panic!("Fourth value should be bytes")
        }

        if let Some(Ok(ParseOutput::Headers(val))) = block_on(read.next()) {
            println!("Headers:{:?}", val);
        } else {
            panic!("Fifth value should be a header")
        }

        if let Some(Ok(ParseOutput::Bytes(bytes))) = block_on(read.next()) {
            assert_eq!(&*bytes, b"value2");
        } else {
            panic!("Sixth value should be bytes")
        }

        assert!(block_on(read.next()).is_none());
    }

    #[test]
    fn unfinished_header() {
        let input: &[u8] = b"--AaB03x\r\n\
                Content-Disposition: form-data; name=\"file\"; filename=\"text.txt\"\r\n\
                Content-Type: text/plain";
        let mut read = MultipartParser::new("AaB03x", ByteStream::new(input));

        assert!(block_on(read.next()).is_none());
    }
    #[test]
    fn unfinished_second_header() {
        let input: &[u8] = b"--AaB03x\r\n\
                Content-Disposition: form-data; name=\"file\"; filename=\"text.txt\"\r\n\
                Content-Type: text/plain\r\n\
                \r\n\
                Lorem Ipsum\n\r\n\
                --AaB03x\r\n\
                Content-Disposition: form-data; name=\"name1\"";

        let mut read = MultipartParser::new("AaB03x", ByteStream::new(input));

        if let Some(Ok(ParseOutput::Headers(val))) = block_on(read.next()) {
            println!("Headers:{:?}", val);
        } else {
            panic!("First value should be a header")
        }

        if let Some(Ok(ParseOutput::Bytes(bytes))) = block_on(read.next()) {
            assert_eq!(&*bytes, b"Lorem Ipsum\n");
        } else {
            panic!("Second value should be bytes")
        }

        assert!(block_on(read.next()).is_none());
    }
    #[test]
    fn invalid_header() {
        let input: &[u8] = b"--AaB03x\r\n\
                I am a bad header\r\n\
                \r\n";

        let mut read = MultipartParser::new("AaB03x", ByteStream::new(input));

        let val = block_on(read.next()).unwrap();

        match val {
            Err(MultipartError::HeaderParse(err)) => {
                //all good
                println!("{}", err);
            }
            val => {
                panic!("Expecting Parse Error, Instead got:{:?}", val);
            }
        }
    }

    #[test]
    fn invalid_boundary() {
        let input: &[u8] = b"--InvalidBoundary\r\n\
                Content-Disposition: form-data; name=\"file\"; filename=\"text.txt\"\r\n\
                Content-Type: text/plain\r\n\
                \r\n\
                Lorem Ipsum\n\r\n\
                --InvalidBoundary--\r\n";

        let mut read = MultipartParser::new("AaB03x", ByteStream::new(input));

        let val = block_on(read.next()).unwrap();

        match val {
            Err(MultipartError::InvalidBoundary { expected, found }) => {
                assert_eq!(expected, "--AaB03x\\r\\n");
                assert_eq!(found, "--InvalidB");
            }
            val => {
                panic!("Expecting Invalid Boundary Error, Instead got:{:?}", val);
            }
        }
    }

    #[test]
    fn zero_read() {
        use bytes::{BufMut, BytesMut};

        let input = b"\
----------------------------332056022174478975396798\r\n\
Content-Disposition: form-data; name=\"file\"\r\n\
Content-Type: application/octet-stream\r\n\
\r\n\
\r\n\
\r\n\
dolphin\n\
whale\r\n\
----------------------------332056022174478975396798--\r\n";

        let boundary = "--------------------------332056022174478975396798";

        let mut read = MultipartStream::new(boundary, ByteStream::new(input));

        let mut part = match block_on(read.next()).unwrap() {
            Ok(mf) => {
                assert_eq!(mf.name().unwrap(), "file");
                assert_eq!(mf.content_type().unwrap(), "application/octet-stream");
                mf
            }
            Err(e) => panic!("unexpected: {}", e),
        };

        let mut buffer = BytesMut::new();

        loop {
            match block_on(part.next()) {
                Some(Ok(bytes)) => buffer.put(bytes),
                Some(Err(e)) => panic!("unexpected {}", e),
                None => break,
            }
        }

        let nth = block_on(read.next());
        assert!(nth.is_none());

        assert_eq!(buffer.as_ref(), b"\r\n\r\ndolphin\nwhale");
    }
}
