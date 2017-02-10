//! Implements the traits that are required to hook up low level Tokio IO to the Tokio
//! protocol API.
//!
//! The main struct that it exposes is the `H2ClientTokioTransport`, which is the bridge between
//! the semantic representation of an HTTP/2 request/response and the lower-level IO.
//!
//! Also exposes the `H2ClientTokioProto` that allows an existing `Io` instance to be bound
//! to the HTTP/2 Tokio transport, as implemented by `H2ClientTokioTransport`.

use super::{HttpRequestHeaders, HttpRequestBody, HttpResponseHeaders, HttpResponseBody};

use io::{FrameSender, FrameReceiver};

use std::rc::Rc;
use std::cell::RefCell;
use std::io::{self, Read};
use std::collections::{HashMap,VecDeque};

use futures::{Async, AsyncSink, Future, Poll, StartSend};
use futures::future::{self};
use futures::sink::Sink;
use futures::stream::{Stream};
use futures::task;

use tokio_core::io::{Io, self as tokio_io};
use tokio_proto::streaming::multiplex::{ClientProto, Transport, Frame};

use solicit::http::{
    HttpResult, HttpScheme,
    Header, StaticHeader, OwnedHeader,
    StreamId
};
use solicit::http::connection::{HttpConnection, SendStatus};
use solicit::http::session::{
    Client as ClientMarker,
    Stream as SolicitStream,
    DefaultSessionState,
    SessionState,
    StreamDataError, StreamDataChunk,
    StreamState,
};
use solicit::http::client::{self, ClientConnection, RequestStream};

/// An enum that represents different response parts that can be generated by an HTTP/2 stream
/// for an associated request.
enum ResponseChunk {
    /// Yielded by the stream when it first receives the response headers.
    Headers(HttpResponseHeaders),
    /// Yielded by the stream for each body chunk. It wraps the actual byte chunk.
    Body(HttpResponseBody),
    /// Signals that there will be no more body chunks yielded by the stream.
    EndOfBody,
}

/// A helper struct that is used by the `H2Stream` to place its `ResponseChunk`s into a shared
/// buffer of `ResponseChunk`s that the `H2ClientTokioTransport` can yield.
#[derive(Clone)]
struct ResponseChunkSender {
    request_id: u64,
    result_stream: Rc<RefCell<Vec<(u64, ResponseChunk)>>>,
}

impl ResponseChunkSender {
    /// Places the given `ResponseChunk` into the shared buffer.
    pub fn send_chunk(&mut self, chunk: ResponseChunk) {
        self.result_stream.borrow_mut().push((self.request_id, chunk));
    }
}

/// A helper struct that exposes the receiving end of the shared buffer of `ResponseChunk`s that
/// the `H2ClientTokioTransport` should yield.
struct ResponseChunkReceiver {
    ready_responses: Rc<RefCell<Vec<(u64, ResponseChunk)>>>,
}

impl ResponseChunkReceiver {
    /// Creates a new `ResponseChunkReceiver`
    pub fn new() -> ResponseChunkReceiver {
        ResponseChunkReceiver {
            ready_responses: Rc::new(RefCell::new(vec![])),
        }
    }

    /// Creates a `ResponseChunkSender` that is bound to a Tokio request with the given ID.
    pub fn get_sender(&self, request_id: u64) -> ResponseChunkSender {
        ResponseChunkSender {
            request_id: request_id,
            result_stream: self.ready_responses.clone(),
        }
    }

    /// Gets the next `ResponseChunk` that is available in the shared buffer. If there is no
    /// available chunk, it returns `None`.
    pub fn get_next_chunk(&mut self) -> Option<(u64, ResponseChunk)> {
        let mut ready_responses = self.ready_responses.borrow_mut();
        if !ready_responses.is_empty() {
            Some(ready_responses.remove(0))
        } else {
            None
        }
    }
}

/// A struct that represents an HTTP/2 stream.
/// Each HTTP/2 stream corresponds to a single (Tokio/HTTP) request.
///
/// This struct implements the `solicit` `Stream` trait. This allows us to provide a custom handler
/// for various events that occur on individual streams, without implementing the entire session
/// layer. Most importantly, instead of accumulating the entire body of the response, this `Stream`
/// implementation will yield the chunks immediately into a shared buffer of `ResponseChunk`s. The
/// `H2ClientTokioTransport` will then, in turn, provide this response chunk to Tokio as soon as
/// possible. Tokio will then handle adapting this to a `futures` `Stream` of chunks that will
/// finally be available to external clients.
///
/// All `H2Stream` instances can share the same underlying buffer of `ResponseChunk`s (which is
/// abstracted away by the `ResponseChunkSender`). This is possible because there can be no
/// concurrency between processing streams and yielding results, as it all happens on the event
/// loop.
struct H2Stream {
    /// The ID of the stream, if already assigned by the connection.
    stream_id: Option<StreamId>,
    /// The current stream state.
    state: StreamState,

    /// The outgoing data associated to the stream. The `Cursor` points into the `Vec` at the
    /// position where the data has been sent out.
    out_buf: Option<io::Cursor<Vec<u8>>>,
    /// A queue of data chunks that should be sent after the current out buffer is exhausted.
    out_queue: VecDeque<Vec<u8>>,
    /// A boolean indicating whether the stream should be closed (locally) after the out buffer
    /// and queue have been cleared out.
    should_close: bool,

    /// A `ResponseChunkSender` that allows the stream to notify the `H2ClientTokioTransport` when
    /// it has received a relevant part of the response.
    sender: ResponseChunkSender,
}

impl H2Stream {
    /// Create a new `H2Stream` for a Tokio request with the given ID, which will place all
    /// `ResponseChunk`s that it generates due to incoming h2 stream events.
    pub fn new(sender: ResponseChunkSender) -> H2Stream {
        H2Stream {
            stream_id: None,
            state: StreamState::Open,

            out_buf: None,
            out_queue: VecDeque::new(),
            should_close: false,

            sender: sender,
        }
    }

    /// Add a chunk of data that the h2 stream should send to the server. Fails if the stream has
    /// already been instructed that it should be locally closed (via `set_should_close`) even if
    /// it still hasn't actually become locally closed (i.e. not everything that's been buffered
    /// has been sent out to the server yet).
    pub fn add_data(&mut self, data: Vec<u8>) -> Result<(), ()> {
        if self.should_close {
            // Adding data after we already closed the stream is not valid, because we cannot make
            // sure to send it.
            return Err(())
        }

        self.out_queue.push_back(data);

        Ok(())
    }

    /// Places the stream in a state where once the previously buffered chunks have been sent, the
    /// stream will be closed. No more chunks should be queued after this is called.
    pub fn set_should_close(&mut self) {
        self.should_close = true;
    }

    /// Prepare the `out_buf` by placing the next element off the `out_queue` in it, if we have
    /// exhausted the previous buffer. If the buffer hasn't yet been exhausted, it has no effect.
    fn prepare_out_buf(&mut self) {
        if self.out_buf.is_none() {
            self.out_buf = self.out_queue.pop_front().map(|vec| io::Cursor::new(vec));
        }
    }
}

impl SolicitStream for H2Stream {
    fn new_data_chunk(&mut self, data: &[u8]) {
        let body_chunk = ResponseChunk::Body(HttpResponseBody { body: data.to_vec() });
        self.sender.send_chunk(body_chunk);
    }

    fn set_headers<'n, 'v>(&mut self, headers: Vec<Header<'n, 'v>>) {
        let new_headers = headers.into_iter().map(|h| {
            let owned: OwnedHeader = h.into();
            owned.into()
        });

        let header_chunk = ResponseChunk::Headers(HttpResponseHeaders {
            headers: new_headers.collect(),
        });
        self.sender.send_chunk(header_chunk);
    }

    fn set_state(&mut self, state: StreamState) {
        self.state = state;

        // If we've transitioned into a state where the stream is closed on the remote end,
        // it means that there can't be more body chunks incoming...
        if self.is_closed_remote() {
            self.sender.send_chunk(ResponseChunk::EndOfBody);
        }
    }

    fn state(&self) -> StreamState {
        self.state
    }

    fn get_data_chunk(&mut self, buf: &mut [u8]) -> Result<StreamDataChunk, StreamDataError> {
        if self.is_closed_local() {
            return Err(StreamDataError::Closed);
        }

        // First make sure we have something in the out buffer, if at all possible.
        self.prepare_out_buf();

        // Now try giving out as much of it as we can.
        let mut out_buf_exhausted = false;
        let chunk = match self.out_buf.as_mut() {
            // No data associated to the stream, but it's open => nothing available for writing
            None => {
                if self.should_close {
                    StreamDataChunk::Last(0)
                } else {
                    StreamDataChunk::Unavailable
                }
            },
            Some(d) => {
                let read = d.read(buf)?;
                out_buf_exhausted = (d.position() as usize) == d.get_ref().len();

                if self.should_close && out_buf_exhausted && self.out_queue.is_empty() {
                    StreamDataChunk::Last(read)
                } else {
                    StreamDataChunk::Chunk(read)
                }
            }
        };

        if out_buf_exhausted {
            self.out_buf = None;
        }

        // Transition the stream state to locally closed if we've extracted the final data chunk.
        if let StreamDataChunk::Last(_) = chunk {
            self.close_local()
        }

        Ok(chunk)
    }
}


/// A type alias for the Frame type that we need to yield to Tokio from the Transport impl's
/// `Stream`.
type TokioResponseFrame = Frame<HttpResponseHeaders, HttpResponseBody, io::Error>;

/// Implements the Tokio Transport trait -- a layer that translates between the HTTP request
/// and response semantic representations (the Http{Request,Response}{Headers,Body} structs)
/// and the lower-level IO required to drive HTTP/2.
///
/// It handles mapping Tokio-level request IDs to HTTP/2 stream IDs, so that once a response
/// is received, it can notify the correct Tokio request.
///
/// It holds the HTTP/2 connection state, as a `ClientConnection` using `DefaultStream`s.
///
/// The HTTP/2 connection is fed by the `FrameSender` and `FrameReceiver`.
///
/// To satisfy the Tokio Transport trait, this struct implements a `Sink` and a `Stream` trait.
/// # Sink
///
/// As a `Sink`, it acts as a `Sink` of Tokio request frames. These frames do not necessarily
/// have a 1-1 mapping to HTTP/2 frames, but it is the task of this struct to do the required
/// mapping.
///
/// For example, a Tokio frame that signals the start of a new request, `Frame::Message`, will be
/// handled by constructing a new HTTP/2 stream in the HTTP/2 session state and instructing the
/// HTTP/2 client connection to start a new request, which will queue up the required HEADERS
/// frames, signaling the start of a new request.
///
/// When handling body "chunk" Tokio frames, i.e. the `Frame::Body` variant, it will notify the
/// appropriate stream (by mapping the Tokio request ID to the matching HTTP/2 stream ID).
///
/// # Stream
///
/// As a `Stream`, the struct acts as a `Stream` of `TokioResponseFrame`s. Tokio itself internally
/// knows how to interpret these frames in order to produce:
/// 
///   1. A `Future` that resolves once the response headers come in
///   2. A `Stream` that will yield response body chunks
///
/// Therefore, the job of this struct is to feed the HTTP/2 frames read by the `receiver` from the
/// underlying raw IO to the ClientConnection and subsequently to interpret the new state of the
/// HTTP/2 session in order to produce `TokioResponseFrame`s.
pub struct H2ClientTokioTransport<T: Io + 'static> {
    sender: FrameSender<T>,
    receiver: FrameReceiver<T>,
    conn: ClientConnection<DefaultSessionState<ClientMarker, H2Stream>>,
    ready_responses: ResponseChunkReceiver,

    // TODO: Should use a bijective map here to simplify...
    h2stream_to_tokio_request: HashMap<u32, u64>,
    tokio_request_to_h2stream: HashMap<u64, u32>,
}

impl<T> H2ClientTokioTransport<T> where T: Io + 'static {
    /// Create a new `H2ClientTokioTransport` that will use the given `Io` for its underlying raw
    /// IO needs.
    fn new(io: T) -> H2ClientTokioTransport<T> {
        let (read, write) = io.split();
        H2ClientTokioTransport {
            sender: FrameSender::new(write),
            receiver: FrameReceiver::new(read),
            conn: ClientConnection::with_connection(
                HttpConnection::new(HttpScheme::Http),
                DefaultSessionState::<ClientMarker, H2Stream>::new()),
            ready_responses: ResponseChunkReceiver::new(),
            h2stream_to_tokio_request: HashMap::new(),
            tokio_request_to_h2stream: HashMap::new(),
        }
    }

    /// Kicks off a new HTTP request.
    ///
    /// It will set up the HTTP/2 session state appropriately (start tracking a new stream)
    /// and queue up the required HTTP/2 frames onto the connection.
    ///
    /// Also starts tracking the mapping between the Tokio request ID (`request_id`) and the HTTP/2
    /// stream ID that it ends up getting assigned to.
    fn start_request(&mut self, request_id: u64, headers: Vec<StaticHeader>, has_body: bool) {
        let request = self.prepare_request(request_id, headers, has_body);

        // Start the request, obtaining the h2 stream ID.
        let stream_id = self.conn.start_request(request, &mut self.sender)
            .ok()
            .expect("queuing a send should work");

        // The ID has been assigned to the stream, so attach it to the stream instance too.
        // TODO(mlalic): The `solicit::Stream` trait should grow an `on_id_assigned` method which
        //               would be called by the session (i.e. the `ClientConnection` in this case).
        //               Indeed, this is slightly awkward...
        self.conn.state.get_stream_mut(stream_id).expect("stream _just_ created").stream_id = Some(stream_id);

        // Now that the h2 request has started, we can keep the mapping of h2 stream ID to the
        // Tokio request ID, so that when the response starts coming in, we can figure out which
        // Tokio request it belongs to...
        debug!("started new request; tokio request={}, h2 stream id={}", request_id, stream_id);
        self.h2stream_to_tokio_request.insert(stream_id, request_id);
        self.tokio_request_to_h2stream.insert(request_id, stream_id);
    }

    /// Prepares a new RequestStream with the given headers. If the request won't have any body, it
    /// immediately closes the stream on the local end to ensure that the peer doesn't expect any
    /// data to come in on the stream.
    fn prepare_request(&mut self, request_id: u64, headers: Vec<StaticHeader>, has_body: bool)
            -> RequestStream<'static, 'static, H2Stream> {
        let mut stream = H2Stream::new(self.ready_responses.get_sender(request_id));
        if !has_body {
            stream.close_local();
        }

        RequestStream {
            stream: stream,
            headers: headers,
        }
    }

    /// Handles all frames currently found in the in buffer. After this completes, the buffer will
    /// no longer contain these frames and they will have been seen by the h2 connection, with all
    /// of their effects being reported to the h2 session.
    fn handle_new_frames(&mut self) {
        // We have new data. Let's try parsing and handling as many h2
        // frames as we can!
        while let Some(bytes_to_discard) = self.handle_next_frame() {
            // So far, the frame wasn't copied out of the original input buffer.
            // Now, we'll simply discard from the input buffer...
            self.receiver.discard_frame(bytes_to_discard);
        }
    }

    /// Handles the next frame in the in buffer (if any) and returns its size in bytes. These bytes
    /// can now safely be discarded from the in buffer, as they have been processed by the h2
    /// connection.
    fn handle_next_frame(&mut self) -> Option<usize> {
        match self.receiver.get_next_frame() {
            None => None,
            Some(mut frame_container) => {
                // Give the frame_container to the conn...
                self.conn
                    .handle_next_frame(&mut frame_container, &mut self.sender)
                    .expect("fixme: handle h2 protocol errors gracefully");

                Some(frame_container.len())
            },
        }
    }

    /// Cleans up all closed streams.
    fn handle_closed_streams(&mut self) {
        // Simply let them get dropped.
        let done = self.conn.state.get_closed();
        debug!("Number of streams that got closed = {}", done.len());
    }

    /// Try to read more data off the socket and handle any HTTP/2 frames that we might
    /// successfully obtain.
    fn try_read_more(&mut self) -> io::Result<()> {
        let total_read = self.receiver.try_read()?;

        if total_read > 0 {
            self.handle_new_frames();

            // After processing frames, let's see if there are any streams that have been completed
            // as a result...
            self.handle_closed_streams();

            // Make sure to issue a write for anything that might have been queued up
            // during the processing of the frames...
            self.sender.try_write()?;
        }

        Ok(())
    }

    /// Dequeue the next response frame off the `ready_responses` queue. As a `Stream` can only
    /// yield a frame at a time, while we can resolve multiple streams (i.e. requests) in the same
    /// stream poll, we need to keep a queue of frames that the `Stream` can yield.
    fn get_next_response_frame(&mut self) -> Option<TokioResponseFrame> {
        self.ready_responses.get_next_chunk().map(|(request_id, response)| {
            match response {
                ResponseChunk::Headers(headers) => {
                    trace!("Yielding a headers frame for request {}", request_id);
                    Frame::Message {
                        id: request_id,
                        message: headers,
                        body: true,
                        solo: false,
                    }
                },
                ResponseChunk::Body(body) => {
                    trace!("Yielding a body chunk for request {}", request_id);
                    Frame::Body {
                        id: request_id,
                        chunk: Some(body),
                    }
                },
                ResponseChunk::EndOfBody => {
                    trace!("Yielding an 'end of body' chunk for request {}", request_id);
                    Frame::Body {
                        id: request_id,
                        chunk: None,
                    }
                },
            }
        })
    }

    /// Add a body chunk to the request with the given Tokio ID.
    ///
    /// Currently, we assume that each request will contain only a single body chunk.
    fn add_body_chunk(&mut self, id: u64, chunk: Option<HttpRequestBody>) {
        let stream_id =
            self.tokio_request_to_h2stream
                .get(&id)
                .expect("an in-flight request needs to have an active h2 stream");

        match self.conn.state.get_stream_mut(*stream_id) {
            Some(mut stream) => {
                match chunk {
                    Some(HttpRequestBody { body }) => {
                        trace!("set data for a request stream {}", *stream_id);
                        stream.add_data(body)
                              .expect("stream unexpectedly already locally closed");
                    },
                    None => {
                        trace!("no more data for stream {}", *stream_id);
                        stream.set_should_close();
                    },
                };
            },
            None => {},
        };
    }

    /// Attempts to queue up more HTTP/2 frames onto the `sender`.
    fn try_write_next_data(&mut self) -> HttpResult<bool> {
        self.conn.send_next_data(&mut self.sender).map(|res| {
            match res {
                SendStatus::Sent => true,
                SendStatus::Nothing => false,
            }
        })
    }

    /// Try to push out some request body data onto the underlying `Io`.
    fn send_request_data(&mut self) -> Poll<(), io::Error> {
        if !self.has_pending_request_data() {
            // No more pending request data -- we're done sending all requests.
            return Ok(Async::Ready(()));
        }

        trace!("preparing a data frame");
        let has_data = self.try_write_next_data().expect("fixme: Handle protocol failure");
        if has_data {
            debug!("queued up a new data frame");

            if self.sender.try_write()? {
                trace!("wrote a full data frame without blocking");
                // HACK!? Yield to the executor, but make sure we're called back asap...
                // Ensures that we don't simply end up writing a whole lot of request
                // data (bodies) while ignoring to appropriately (timely) handle other
                // aspects of h2 communication (applying settings, sendings acks, initiating
                // _new_ requests, ping/pong, etc...).
                let task = task::park();
                task.unpark();
                Ok(Async::NotReady)
            } else {
                // Did not manage to write the entire new frame without blocking.
                // We'll get rescheduled when the socket unblocks.
                Ok(Async::NotReady)
            }
        } else {
            trace!("no stream data ready");
            // If we didn't manage to prepare a data frame, while there were still open
            // streams, it means that the stream didn't have the data ready for writing.
            // In other words, we've managed to write all Tokio requests -- i.e. anything
            // that passed through `start_send`. When there's another piece of the full
            // HTTP request body ready, we'll get it through `start_send`.
            Ok(Async::Ready(()))
        }
    }

    /// Checks whether any active h2 stream still has data that needs to be sent out to the server.
    /// Returns `true` if there is such a stream.
    ///
    /// This is done by simply checking whether all streams have transitioned into a locally closed
    /// state, which indicates that we're done transmitting from our end.
    fn has_pending_request_data(&mut self) -> bool {
        self.conn.state.iter().any(|(_id, stream)| {
            !stream.is_closed_local()
        })
    }
}

impl<T> Stream for H2ClientTokioTransport<T> where T: Io + 'static {
    type Item = Frame<HttpResponseHeaders, HttpResponseBody, io::Error>;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        trace!("polling read");

        // First, try to see if there's anything more that we can read off the socket already...
        self.try_read_more()?;

        // Now return the first response that we have ready, if any.
        // TODO: Handle eof.
        match self.get_next_response_frame() {
            None => Ok(Async::NotReady),
            Some(tokio_frame) => Ok(Async::Ready(Some(tokio_frame))),
        }
    }
}

impl<T> Sink for H2ClientTokioTransport<T> where T: Io + 'static {
    type SinkItem = Frame<HttpRequestHeaders, HttpRequestBody, io::Error>;
    type SinkError = io::Error;

    fn start_send(&mut self,
                  item: Self::SinkItem)
                  -> StartSend<Self::SinkItem, Self::SinkError> {
        match item {
            Frame::Message { id, body: has_body, message: HttpRequestHeaders { headers }, .. } => {
                debug!("start new request id={}, body={}", id, has_body);
                trace!("  headers={:?}", headers);

                self.start_request(id, headers, has_body);
            },
            Frame::Body { id, chunk } => {
                debug!("add body chunk for request id={}", id);
                self.add_body_chunk(id, chunk);
            },
            _ => {},
        }

        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        trace!("poll all requests sent?");

        // Make sure to trigger a frame flush ...
        if self.sender.try_write()? {
            // If sending everything that was queued so far worked, let's see if we can queue up
            // some data frames, if there are streams that still need to send some.
            self.send_request_data()
        } else {
            // We didn't manage to write everything from our out buffer without blocking.
            // We'll get woken up when writing to the socket is possible again.
            Ok(Async::NotReady)
        }
    }
}

impl<ReadBody, T> Transport<ReadBody> for H2ClientTokioTransport<T> where T: Io + 'static {
    fn tick(&mut self) {
        trace!("TokioTransport TICKING");
    }
}

/// A unit struct that serves to implement the `ClientProto` Tokio trait, which hooks up a
/// raw `Io` to the `H2ClientTokioTransport`.
///
/// This is _almost_ trivial, except it also is required to do protocol negotiation/initialization.
///
/// For cleartext HTTP/2, this means simply sending out the client preface bytes, for which
/// `solicit` provides a helper.
///
/// The transport is resolved only once the preface write is complete, as only after this can the
/// `solicit` `ClientConnection` take over management of the socket: once the HTTP/2 frames start
/// flowing through.
pub struct H2ClientTokioProto;

impl<T> ClientProto<T> for H2ClientTokioProto where T: 'static + Io {
    type Request = HttpRequestHeaders;
    type RequestBody = HttpRequestBody;
    type Response = HttpResponseHeaders;
    type ResponseBody = HttpResponseBody;
    type Error = io::Error;
    type Transport = H2ClientTokioTransport<T>;
    type BindTransport = Box<Future<Item=Self::Transport, Error=io::Error>>;

    fn bind_transport(&self, io: T) -> Self::BindTransport {
        let mut buf = io::Cursor::new(vec![]);
        client::write_preface(&mut buf).expect("writing to an in-memory buffer should not fail");
        let buf = buf.into_inner();

        Box::new(tokio_io::write_all(io, buf).and_then(|(io, _buf)| {
            debug!("client preface write complete");
            future::ok(H2ClientTokioTransport::new(io))
        }))
    }
}

