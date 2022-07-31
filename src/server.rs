use bytes::{Bytes, BytesMut};
use futures_core::Stream;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use httparse::Status;
use log::debug;
use pin_project_lite::pin_project;
use std::borrow::Cow;
use std::error::Error as StdError;
use std::mem;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use thiserror::Error;

use memchr::{memchr, memmem::find, memrchr};
use percent_encoding::percent_decode_str;

type AnyStdError = Box<dyn StdError + Send + Sync + 'static>;

/// A single field of a [`MultipartStream`](./struct.MultipartStream.html) which itself is a stream
///
/// This represents either an uploaded file, or a simple text value
///
/// Each field will have some headers and then a body.
/// There are no assumptions made when parsing about what headers are present, but some of the helper methods here (such as `content_type()` & `filename()`) will return an error if they aren't present.
/// The body will be returned as an inner stream of bytes from the request, but up to the end of the field.
///
/// Fields are not concurrent against their parent multipart request. This is because multipart submissions are a single http request and we don't support going backwards or skipping bytes.  In other words you can't read from multiple fields from the same request at the same time: you must wait for one field to finish being read before moving on.

pub struct MultipartField<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<AnyStdError>,
{
    headers: HeaderMap<HeaderValue>,
    state: Arc<Mutex<MultipartState<S, E>>>,
}

impl<S, E> MultipartField<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<AnyStdError>,
{
    /// Return the headers for the field
    ///
    /// You can use `self.headers.get("my-header").and_then(|val| val.to_str().ok())` to get out the header if present
    pub fn headers(&self) -> &HeaderMap<HeaderValue> {
        &self.headers
    }

    /// Return the content type of the field (if present or error)
    pub fn content_type(&self) -> Result<&str, MultipartError> {
        if let Some(val) = self.headers.get("content-type") {
            return val.to_str().map_err(|_| MultipartError::InvalidHeader);
        }

        Err(MultipartError::InvalidHeader)
    }

    /// Return the filename of the field (if present or error).
    /// The returned filename will be utf8 percent-decoded
    pub fn filename(&self) -> Result<Cow<str>, MultipartError> {
        if let Some(val) = self.headers.get("content-disposition") {
            let string_val =
                std::str::from_utf8(val.as_bytes()).map_err(|_| MultipartError::InvalidHeader)?;
            if let Some(filename) = get_dispo_param(string_val, "filename*") {
                let stripped = strip_utf8_prefix(filename);
                return Ok(stripped);
            }
            if let Some(filename) = get_dispo_param(string_val, "filename") {
                return Ok(filename);
            }
        }

        Err(MultipartError::InvalidHeader)
    }

    /// Return the name of the field (if present or error).
    /// The returned name will be utf8 percent-decoded
    pub fn name(&self) -> Result<Cow<str>, MultipartError> {
        if let Some(val) = self.headers.get("content-disposition") {
            let string_val =
                std::str::from_utf8(val.as_bytes()).map_err(|_| MultipartError::InvalidHeader)?;
            if let Some(filename) = get_dispo_param(string_val, "name") {
                return Ok(filename);
            }
        }

        Err(MultipartError::InvalidHeader)
    }
}

fn strip_utf8_prefix(string: Cow<str>) -> Cow<str> {
    if string.starts_with("UTF-8''") || string.starts_with("utf-8''") {
        let split = string.split_at(7).1;
        return Cow::from(split.to_owned());
    }

    string
}

/// This function will get a disposition param from `content-disposition` header & try to escape it if there are escaped quotes or percent encoding
fn get_dispo_param<'a>(input: &'a str, param: &str) -> Option<Cow<'a, str>> {
    debug!("dispo param:{input}, field `{param}`");
    if let Some(start_idx) = find(input.as_bytes(), param.as_bytes()) {
        debug!("Start idx found:{start_idx}");
        let end_param = start_idx + param.len();
        //check bounds
        if input.len() > end_param + 2 {
            if &input[end_param..end_param + 2] == "=\"" {
                let start = end_param + 2;

                let mut snippet = &input[start..];

                // If we encounter a `\"` in the string we need to escape it
                // This means that we need to create a new escaped string as it will be discontiguous
                let mut escaped_buffer: Option<String> = None;

                while let Some(end) = memchr(b'"', snippet.as_bytes()) {
                    // if we encounter a backslash before the quote
                    if end > 0
                        && snippet
                            .get(end - 1..end)
                            .map_or(false, |character| character == "\\")
                    {
                        // We get an existing escaped buffer or create an empty string
                        let mut buffer = escaped_buffer.unwrap_or_default();

                        // push up until the escaped quote
                        buffer.push_str(&snippet[..end - 1]);
                        // push in the quote itself
                        buffer.push('"');

                        escaped_buffer = Some(buffer);

                        // Move the buffer ahead
                        snippet = &snippet[end + 1..];
                        continue;
                    } else {
                        // we're at the end

                        // if we have something escaped
                        match escaped_buffer {
                            Some(mut escaped) => {
                                // tack on the end of the string
                                escaped.push_str(&snippet[0..end]);

                                // Double escape with percent decode
                                if escaped.contains('%') {
                                    let decoded_val =
                                        percent_decode_str(&escaped).decode_utf8_lossy();
                                    return Some(Cow::Owned(decoded_val.into_owned()));
                                }

                                return Some(Cow::Owned(escaped));
                            }
                            None => {
                                let value = &snippet[0..end];

                                // Escape with percent decode, if necessary
                                if value.contains('%') {
                                    let decoded_val = percent_decode_str(value).decode_utf8_lossy();

                                    return Some(decoded_val);
                                }

                                return Some(Cow::Borrowed(value));
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

//Streams out bytes
impl<S, E> Stream for MultipartField<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<AnyStdError>,
{
    type Item = Result<Bytes, MultipartError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let self_mut = &mut self.as_mut();

        let state = &mut self_mut
            .state
            .try_lock()
            .map_err(|_| MultipartError::InternalBorrowError)?;

        match Pin::new(&mut state.parser).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Err(err))) => {
                Poll::Ready(Some(Err(MultipartError::Stream(err.into()))))
            }
            Poll::Ready(None) => Poll::Ready(None),
            //If we have headers, we have reached the next file
            Poll::Ready(Some(Ok(ParseOutput::Headers(headers)))) => {
                state.next_item = Some(headers);
                Poll::Ready(None)
            }
            Poll::Ready(Some(Ok(ParseOutput::Bytes(bytes)))) => Poll::Ready(Some(Ok(bytes))),
        }
    }
}

//This is our state we use to drive the parser.  The `next_item` is there just for headers if there are more in the request
struct MultipartState<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<AnyStdError>,
{
    parser: MultipartParser<S, E>,
    next_item: Option<HeaderMap<HeaderValue>>,
}

/// The main `MultipartStream` struct which will contain one or more fields (a stream of streams)
///
/// You can construct this given a boundary and a stream of bytes from a server request.
///
/// **Please Note**: If you are reading in a field, you must exhaust the field's bytes before moving onto the next field
/// ```no_run
/// # use warp::Filter;
/// # use bytes::{Buf, BufMut, BytesMut};
/// # use futures_util::TryStreamExt;
/// # use futures_core::Stream;
/// # use mime::Mime;
/// # use mpart_async::server::MultipartStream;
/// # use std::convert::Infallible;
/// # #[tokio::main]
/// # async fn main() {
/// #     // Match any request and return hello world!
/// #     let routes = warp::any()
/// #         .and(warp::header::<Mime>("content-type"))
/// #         .and(warp::body::stream())
/// #         .and_then(mpart);
/// #     warp::serve(routes).run(([127, 0, 0, 1], 3030)).await;
/// # }
/// # async fn mpart(
/// #     mime: Mime,
/// #     body: impl Stream<Item = Result<impl Buf, warp::Error>> + Unpin,
/// # ) -> Result<impl warp::Reply, Infallible> {
/// #     let boundary = mime.get_param("boundary").map(|v| v.to_string()).unwrap();
/// let mut stream = MultipartStream::new(boundary, body.map_ok(|mut buf| {
///     let mut ret = BytesMut::with_capacity(buf.remaining());
///     ret.put(buf);
///     ret.freeze()
/// }));
///
/// while let Ok(Some(mut field)) = stream.try_next().await {
///     println!("Field received:{}", field.name().unwrap());
///     if let Ok(filename) = field.filename() {
///         println!("Field filename:{}", filename);
///     }
///
///     while let Ok(Some(bytes)) = field.try_next().await {
///         println!("Bytes received:{}", bytes.len());
///     }
/// }
/// #     Ok(format!("Thanks!\n"))
/// # }
/// ```

pub struct MultipartStream<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<AnyStdError>,
{
    state: Arc<Mutex<MultipartState<S, E>>>,
}

impl<S, E> MultipartStream<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<AnyStdError>,
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
    E: Into<AnyStdError>,
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
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(err))),
            Poll::Ready(None) => Poll::Ready(None),

            //If we have headers, we have reached the next file
            Poll::Ready(Some(Ok(ParseOutput::Headers(headers)))) => {
                Poll::Ready(Some(Ok(MultipartField {
                    headers,
                    state: self_mut.state.clone(),
                })))
            }
            Poll::Ready(Some(Ok(ParseOutput::Bytes(_bytes)))) => {
                //If we are returning bytes from this stream, then there is some error
                Poll::Ready(Some(Err(MultipartError::ShouldPollField)))
            }
        }
    }
}

#[derive(Error, Debug)]
/// The Standard Error Type
pub enum MultipartError {
    /// Given if the boundary is not what is expected
    #[error("Invalid Boundary. (expected {expected:?}, found {found:?})")]
    InvalidBoundary {
        /// The Expected Boundary
        expected: String,
        /// The Found Boundary
        found: String,
    },
    /// Given if when parsing the headers they are incomplete
    #[error("Incomplete Headers")]
    IncompleteHeader,
    /// Given if when trying to retrieve a field like name or filename it's not present or malformed
    #[error("Invalid Header Value")]
    InvalidHeader,
    /// Given if in the middle of polling a Field, and someone tries to poll the Stream
    #[error(
        "Tried to poll an MultipartStream when the MultipartField should be polled, try using `flatten()`"
    )]
    ShouldPollField,
    /// Given if in the middle of polling a Field, but the Mutex is in use somewhere else
    #[error("Tried to poll an MultipartField and the Mutex has already been locked")]
    InternalBorrowError,
    /// Given if there is an error when parsing headers
    #[error(transparent)]
    HeaderParse(#[from] httparse::Error),
    /// Given if there is an error in the underlying stream
    #[error(transparent)]
    Stream(#[from] AnyStdError),
    /// Given if the stream ends when reading headers
    #[error("EOF while reading headers")]
    EOFWhileReadingHeaders,
    /// Given if the stream ends when reading boundary
    #[error("EOF while reading boundary")]
    EOFWhileReadingBoundary,
    /// Given if if the stream ends when reading the body and there is no end boundary
    #[error("EOF while reading body")]
    EOFWhileReadingBody,
    /// Given if there is garbage after the boundary
    #[error("Garbage following boundary: {0:02x?}")]
    GarbageAfterBoundary([u8; 2]),
}

pin_project! {
    /// A low-level parser which `MultipartStream` uses.
    ///
    /// Returns either headers of a field or a byte chunk, alternating between the two types
    ///
    /// Unless there is an issue with using [`MultipartStream`](./struct.MultipartStream.html) you don't normally want to use this struct
    #[project = ParserProj]
    pub struct MultipartParser<S, E>
    where
        S: Stream<Item = Result<Bytes, E>>,
        E: Into<AnyStdError>,
    {
        boundary: Bytes,
        buffer: BytesMut,
        state: State,
        #[pin]
        stream: S,
    }
}

impl<S, E> MultipartParser<S, E>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: Into<AnyStdError>,
{
    /// Construct a raw parser from a boundary/stream.
    pub fn new<I: Into<Bytes>>(boundary: I, stream: S) -> Self {
        Self {
            boundary: boundary.into(),
            buffer: BytesMut::new(),
            state: State::ReadingBoundary,
            stream,
        }
    }
}

const NUM_HEADERS: usize = 16;

fn get_headers(buffer: &[u8]) -> Result<HeaderMap<HeaderValue>, MultipartError> {
    let mut headers = [httparse::EMPTY_HEADER; NUM_HEADERS];

    let idx = match httparse::parse_headers(buffer, &mut headers)? {
        Status::Complete((idx, _val)) => idx,
        Status::Partial => return Err(MultipartError::IncompleteHeader),
    };

    let mut header_map = HeaderMap::with_capacity(idx);

    for header in headers.iter().take(idx) {
        if !header.name.is_empty() {
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
    S: Stream<Item = Result<Bytes, E>>,
    E: Into<AnyStdError>,
{
    type Item = Result<ParseOutput, MultipartError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let ParserProj {
            boundary,
            buffer,
            state,
            mut stream,
        } = self.project();

        loop {
            match state {
                State::ReadingBoundary => {
                    let boundary_len = boundary.len();

                    //If the buffer is too small
                    if buffer.len() < boundary_len + 4 {
                        match futures_core::ready!(stream.as_mut().poll_next(cx)) {
                            Some(Ok(bytes)) => {
                                buffer.extend_from_slice(&bytes);
                                continue;
                            }
                            Some(Err(e)) => {
                                return Poll::Ready(Some(Err(MultipartError::Stream(e.into()))))
                            }
                            None => {
                                return Poll::Ready(Some(Err(
                                    MultipartError::EOFWhileReadingBoundary,
                                )))
                            }
                        }
                    }

                    //If the buffer starts with `--<boundary>\r\n`
                    if &buffer[..2] == b"--"
                        && buffer[2..boundary_len + 2] == *boundary
                        && &buffer[boundary_len + 2..boundary_len + 4] == b"\r\n"
                    {
                        //remove the boundary from the buffer, returning the tail
                        *buffer = buffer.split_off(boundary_len + 4);
                        *state = State::ReadingHeader;

                        //Update the boundary here to include the \r\n-- preamble for individual fields
                        let mut new_boundary = BytesMut::with_capacity(boundary_len + 4);

                        new_boundary.extend_from_slice(b"\r\n--");
                        new_boundary.extend_from_slice(boundary);

                        *boundary = new_boundary.freeze();

                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    } else {
                        let expected = format!("--{}\\r\\n", String::from_utf8_lossy(boundary));
                        let found =
                            String::from_utf8_lossy(&buffer[..boundary_len + 4]).to_string();

                        let error = MultipartError::InvalidBoundary { expected, found };

                        //There is some error with the boundary...
                        return Poll::Ready(Some(Err(error)));
                    }
                }
                State::ReadingHeader => {
                    if let Some(end) = find(buffer, b"\r\n\r\n") {
                        //Need to include the end header bytes for the parse to work
                        let end = end + 4;

                        let header_map = match get_headers(&buffer[0..end]) {
                            Ok(headers) => headers,
                            Err(error) => {
                                *state = State::Finished;
                                return Poll::Ready(Some(Err(error)));
                            }
                        };

                        *buffer = buffer.split_off(end);
                        *state = State::StreamingContent(buffer.is_empty());

                        cx.waker().wake_by_ref();

                        return Poll::Ready(Some(Ok(ParseOutput::Headers(header_map))));
                    } else {
                        match futures_core::ready!(stream.as_mut().poll_next(cx)) {
                            Some(Ok(bytes)) => {
                                buffer.extend_from_slice(&bytes);
                                continue;
                            }
                            Some(Err(e)) => {
                                return Poll::Ready(Some(Err(MultipartError::Stream(e.into()))))
                            }
                            None => {
                                return Poll::Ready(Some(Err(
                                    MultipartError::EOFWhileReadingHeaders,
                                )))
                            }
                        }
                    }
                }

                State::StreamingContent(exhausted) => {
                    let boundary_len = boundary.len();

                    if buffer.is_empty() || *exhausted {
                        *state = State::StreamingContent(false);
                        match futures_core::ready!(stream.as_mut().poll_next(cx)) {
                            Some(Ok(bytes)) => {
                                buffer.extend_from_slice(&bytes);
                                continue;
                            }
                            Some(Err(e)) => {
                                return Poll::Ready(Some(Err(MultipartError::Stream(e.into()))))
                            }
                            None => {
                                return Poll::Ready(Some(Err(MultipartError::EOFWhileReadingBody)))
                            }
                        }
                    }

                    //We want to check the value of the buffer to see if there looks like there is an `end` boundary.
                    if let Some(idx) = find(buffer, boundary) {
                        //Check the length has enough bytes for us
                        if buffer.len() < idx + 2 + boundary_len {
                            // FIXME: cannot stop the read successfully here!
                            *state = State::StreamingContent(true);
                            continue;
                        }

                        //The start of the boundary is 4 chars. i.e, after `\r\n--`
                        let end_boundary = idx + boundary_len;

                        //We want the chars after the boundary basically
                        let after_boundary = &buffer[end_boundary..end_boundary + 2];

                        if after_boundary == b"\r\n" {
                            let mut other_bytes = (*buffer).split_off(idx);

                            //Remove the boundary-related bytes from the start of the buffer
                            other_bytes = other_bytes.split_off(2 + boundary_len);

                            //Return the bytes up to the boundary, we're finished and need to go back to reading headers
                            let return_bytes = Bytes::from(mem::replace(buffer, other_bytes));

                            //Replace the buffer with the extra bytes
                            *state = State::ReadingHeader;
                            cx.waker().wake_by_ref();

                            return Poll::Ready(Some(Ok(ParseOutput::Bytes(return_bytes))));
                        } else if after_boundary == b"--" {
                            //We're at the end, just truncate the bytes
                            buffer.truncate(idx);
                            *state = State::Finished;

                            return Poll::Ready(Some(Ok(ParseOutput::Bytes(Bytes::from(
                                mem::take(buffer),
                            )))));
                        } else {
                            return Poll::Ready(Some(Err(MultipartError::GarbageAfterBoundary([
                                after_boundary[0],
                                after_boundary[1],
                            ]))));
                        }
                    } else {
                        //We need to check for partial matches by checking the last boundary_len bytes
                        let buffer_len = buffer.len();

                        //Clamp to zero if the boundary length is bigger than the buffer
                        let start_idx =
                            (buffer_len as i64 - (boundary_len as i64 - 1)).max(0) as usize;

                        let end_of_buffer = &buffer[start_idx..];

                        if let Some(idx) = memrchr(b'\r', end_of_buffer) {
                            //If to the end of the match equals the same amount of bytes
                            if end_of_buffer[idx..] == boundary[..(end_of_buffer.len() - idx)] {
                                *state = State::StreamingContent(true);

                                //we return all the bytes except for the start of our boundary
                                let mut output = buffer.split_off(idx + start_idx);
                                mem::swap(&mut output, buffer);

                                cx.waker().wake_by_ref();
                                return Poll::Ready(Some(Ok(ParseOutput::Bytes(output.freeze()))));
                            }
                        }

                        let output = mem::take(buffer);
                        return Poll::Ready(Some(Ok(ParseOutput::Bytes(output.freeze()))));
                    }
                }
                State::Finished => return Poll::Ready(None),
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum State {
    ReadingBoundary,
    ReadingHeader,
    StreamingContent(bool),
    Finished,
}

#[derive(Debug)]
/// The output from a MultipartParser
pub enum ParseOutput {
    /// Headers received in the output
    Headers(HeaderMap<HeaderValue>),
    /// Bytes received in the output
    Bytes(Bytes),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ByteStream;
    use futures_util::StreamExt;

    #[tokio::test]
    async fn read_stream() {
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

        if let Some(Ok(mut mpart_field)) = stream.next().await {
            assert_eq!(mpart_field.name().ok(), Some(Cow::Borrowed("file")));
            assert_eq!(mpart_field.filename().ok(), Some(Cow::Borrowed("text.txt")));

            if let Some(Ok(bytes)) = mpart_field.next().await {
                assert_eq!(bytes, Bytes::from(b"Lorem Ipsum\n" as &[u8]));
            }
        } else {
            panic!("First value should be a field")
        }
    }

    #[tokio::test]
    async fn read_utf_8_filename() {
        let input: &[u8] = b"--AaB03x\r\n\
                Content-Disposition: form-data; name=\"file\"; filename=\"text.txt\"; filename*=\"aous%20.txt\"\r\n\
                Content-Type: text/plain\r\n\
                \r\n\
                Lorem Ipsum\n\r\n\
                --AaB03x--\r\n";

        let mut stream = MultipartStream::new("AaB03x", ByteStream::new(input));

        let field = stream.next().await.unwrap().unwrap();
        assert_eq!(field.filename().ok(), Some(Cow::Borrowed("aous .txt")));
    }

    #[test]
    fn read_filename() {
        let input = "form-data; name=\"file\";\
                           filename=\"text%20.txt\";\
                           quoted=\"with a \\\" quote and another \\\" quote\";\
                           empty=\"\"\
                           percent_encoded=\"foo%20%3Cbar%3E\"\
                           ";
        let name = get_dispo_param(input, "name");
        let filename = get_dispo_param(input, "filename");
        let with_a_quote = get_dispo_param(input, "quoted");
        let empty = get_dispo_param(input, "empty");
        let percent_encoded = get_dispo_param(input, "percent_encoded");

        assert_eq!(name, Some(Cow::Borrowed("file")));
        assert_eq!(filename, Some(Cow::Borrowed("text .txt")));
        assert_eq!(
            with_a_quote,
            Some(Cow::Owned("with a \" quote and another \" quote".into()))
        );
        assert_eq!(empty, Some(Cow::Borrowed("")));
        assert_eq!(percent_encoded, Some(Cow::Borrowed("foo <bar>")));
    }

    #[test]
    fn read_filename_umlaut() {
        let input = "form-data; name=\"äöüß\";\
                           filename*=\"äöü ß%20.txt\";\
                           quoted=\"with a \\\" quote and another \\\" quote\";\
                           empty=\"\"\
                           percent_encoded=\"foo%20%3Cbar%3E\"\
                           ";
        let name = get_dispo_param(input, "name");
        let filename = get_dispo_param(input, "filename*");
        let with_a_quote = get_dispo_param(input, "quoted");
        let empty = get_dispo_param(input, "empty");
        let percent_encoded = get_dispo_param(input, "percent_encoded");

        assert_eq!(name, Some(Cow::Borrowed("äöüß")));
        assert_eq!(filename, Some(Cow::Borrowed("äöü ß .txt")));
        assert_eq!(
            with_a_quote,
            Some(Cow::Owned("with a \" quote and another \" quote".into()))
        );
        assert_eq!(empty, Some(Cow::Borrowed("")));
        assert_eq!(percent_encoded, Some(Cow::Borrowed("foo <bar>")));
    }

    #[tokio::test]
    async fn reads_streams_and_fields() {
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

        if let Some(Ok(ParseOutput::Headers(val))) = read.next().await {
            println!("Headers:{:?}", val);
        } else {
            panic!("First value should be a header")
        }

        if let Some(Ok(ParseOutput::Bytes(bytes))) = read.next().await {
            assert_eq!(&*bytes, b"Lorem Ipsum\n");
        } else {
            panic!("Second value should be bytes")
        }

        if let Some(Ok(ParseOutput::Headers(val))) = read.next().await {
            println!("Headers:{:?}", val);
        } else {
            panic!("Third value should be a header")
        }

        if let Some(Ok(ParseOutput::Bytes(bytes))) = read.next().await {
            assert_eq!(&*bytes, b"value1");
        } else {
            panic!("Fourth value should be bytes")
        }

        if let Some(Ok(ParseOutput::Headers(val))) = read.next().await {
            println!("Headers:{:?}", val);
        } else {
            panic!("Fifth value should be a header")
        }

        if let Some(Ok(ParseOutput::Bytes(bytes))) = read.next().await {
            assert_eq!(&*bytes, b"value2");
        } else {
            panic!("Sixth value should be bytes")
        }

        assert!(read.next().await.is_none());
    }

    #[tokio::test]
    async fn unfinished_header() {
        let input: &[u8] = b"--AaB03x\r\n\
                Content-Disposition: form-data; name=\"file\"; filename=\"text.txt\"\r\n\
                Content-Type: text/plain";
        let mut read = MultipartParser::new("AaB03x", ByteStream::new(input));

        let ret = read.next().await;

        assert!(matches!(
            ret,
            Some(Err(MultipartError::EOFWhileReadingHeaders))
        ),);
    }

    #[tokio::test]
    async fn unfinished_second_header() {
        let input: &[u8] = b"--AaB03x\r\n\
                Content-Disposition: form-data; name=\"file\"; filename=\"text.txt\"\r\n\
                Content-Type: text/plain\r\n\
                \r\n\
                Lorem Ipsum\n\r\n\
                --AaB03x\r\n\
                Content-Disposition: form-data; name=\"name1\"";

        let mut read = MultipartParser::new("AaB03x", ByteStream::new(input));

        if let Some(Ok(ParseOutput::Headers(val))) = read.next().await {
            println!("Headers:{:?}", val);
        } else {
            panic!("First value should be a header")
        }

        if let Some(Ok(ParseOutput::Bytes(bytes))) = read.next().await {
            assert_eq!(&*bytes, b"Lorem Ipsum\n");
        } else {
            panic!("Second value should be bytes")
        }

        let ret = read.next().await;

        assert!(matches!(
            ret,
            Some(Err(MultipartError::EOFWhileReadingHeaders))
        ),);
    }

    #[tokio::test]
    async fn invalid_header() {
        let input: &[u8] = b"--AaB03x\r\n\
                I am a bad header\r\n\
                \r\n";

        let mut read = MultipartParser::new("AaB03x", ByteStream::new(input));

        let val = read.next().await.unwrap();

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

    #[tokio::test]
    async fn invalid_boundary() {
        let input: &[u8] = b"--InvalidBoundary\r\n\
                Content-Disposition: form-data; name=\"file\"; filename=\"text.txt\"\r\n\
                Content-Type: text/plain\r\n\
                \r\n\
                Lorem Ipsum\n\r\n\
                --InvalidBoundary--\r\n";

        let mut read = MultipartParser::new("AaB03x", ByteStream::new(input));

        let val = read.next().await.unwrap();

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

    #[tokio::test]
    async fn zero_read() {
        use bytes::{BufMut, BytesMut};

        let input = b"----------------------------332056022174478975396798\r\n\
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

        let mut part = match read.next().await.unwrap() {
            Ok(mf) => {
                assert_eq!(mf.name().unwrap(), "file");
                assert_eq!(mf.content_type().unwrap(), "application/octet-stream");
                mf
            }
            Err(e) => panic!("unexpected: {}", e),
        };

        let mut buffer = BytesMut::new();

        loop {
            match part.next().await {
                Some(Ok(bytes)) => buffer.put(bytes),
                Some(Err(e)) => panic!("unexpected {}", e),
                None => break,
            }
        }

        let nth = read.next().await;
        assert!(nth.is_none());

        assert_eq!(buffer.as_ref(), b"\r\n\r\ndolphin\nwhale");
    }

    #[tokio::test]
    async fn r_read() {
        use std::convert::Infallible;

        //Used to ensure partial matches are working!

        #[derive(Clone)]
        pub struct SplitStream {
            packets: Vec<Bytes>,
        }

        impl SplitStream {
            pub fn new() -> Self {
                SplitStream { packets: vec![] }
            }

            pub fn add_packet<P: Into<Bytes>>(&mut self, bytes: P) {
                self.packets.push(bytes.into());
            }
        }

        impl Stream for SplitStream {
            type Item = Result<Bytes, Infallible>;

            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.as_mut().packets.is_empty() {
                    return Poll::Ready(None);
                }

                Poll::Ready(Some(Ok(self.as_mut().packets.remove(0))))
            }
        }

        use bytes::{BufMut, BytesMut};

        //This is a packet split on the boundary to test partial matching
        let input1: &[u8] = b"----------------------------332056022174478975396798\r\n\
                Content-Disposition: form-data; name=\"file\"\r\n\
                Content-Type: application/octet-stream\r\n\
                \r\n\
                \r\r\r\r\r\r\r\r\r\r\r\r\r\
                \r\n\
                ----------------------------332";

        //This is the rest of the packet
        let input2: &[u8] = b"056022174478975396798--\r\n";

        let boundary = "--------------------------332056022174478975396798";

        let mut split_stream = SplitStream::new();

        split_stream.add_packet(&*input1);
        split_stream.add_packet(&*input2);

        let mut read = MultipartStream::new(boundary, split_stream);

        let mut part = match read.next().await.unwrap() {
            Ok(mf) => {
                assert_eq!(mf.name().unwrap(), "file");
                assert_eq!(mf.content_type().unwrap(), "application/octet-stream");
                mf
            }
            Err(e) => panic!("unexpected: {}", e),
        };

        let mut buffer = BytesMut::new();

        loop {
            match part.next().await {
                Some(Ok(bytes)) => buffer.put(bytes),
                Some(Err(e)) => panic!("unexpected {}", e),
                None => break,
            }
        }

        let nth = read.next().await;
        assert!(nth.is_none());

        assert_eq!(buffer.as_ref(), b"\r\r\r\r\r\r\r\r\r\r\r\r\r");
    }

    #[test]
    fn test_strip_no_strip_necessary() {
        let name: Cow<str> = Cow::Owned("äöüß.txt".to_owned());

        let res = strip_utf8_prefix(name.clone());

        assert_eq!(res, name);
    }

    #[test]
    fn test_strip_uppercase_utf8() {
        let name: Cow<str> = Cow::Owned("UTF-8''äöüß.txt".to_owned());

        let res = strip_utf8_prefix(name);

        assert_eq!(res, "äöüß.txt");
    }

    #[test]
    fn test_strip_lowercase_utf8() {
        let name: Cow<str> = Cow::Owned("utf-8''äöüß.txt".to_owned());

        let res = strip_utf8_prefix(name);

        assert_eq!(res, "äöüß.txt");
    }
}
