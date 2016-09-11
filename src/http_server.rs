use std::marker;
use std::collections::HashMap;

use solicit::http::session::StreamState;
use solicit::http::connection::HttpConnection;
use solicit::http::connection::EndStream;
use solicit::http::connection::SendFrame;
use solicit::http::frame::*;
use solicit::http::StreamId;
use solicit::http::HttpScheme;
use solicit::http::HttpError;
use solicit::http::Header;
use hpack;

use futures;
use futures::Future;
use futures::stream::Stream;

use tokio_core;
use tokio_core::net::TcpStream;
use tokio_core::io as tokio_io;
use tokio_core::io::Io;
use tokio_core::io::ReadHalf;
use tokio_core::io::WriteHalf;
use tokio_core::reactor;

use futures_misc::*;

use solicit_async::*;
use solicit_misc::*;
use http_common::*;


pub trait HttpServerHandlerFactory: Send + 'static {
    fn new_request(&mut self, req: HttpStreamStreamSend) -> HttpStreamStreamSend;
}

struct GrpcHttpServerStream<F : HttpServerHandlerFactory> {
    state: StreamState,
    request_handler: Option<tokio_core::channel::Sender<ResultOrEof<HttpStreamPart, HttpError>>>,
    _marker: marker::PhantomData<F>,
}

impl<F : HttpServerHandlerFactory> GrpcHttpServerStream<F> {
    fn _close(&mut self) {
        self.set_state(StreamState::Closed);
    }

    fn _close_local(&mut self) {
        let next = match self.state() {
            StreamState::HalfClosedRemote => StreamState::Closed,
            _ => StreamState::HalfClosedLocal,
        };
        self.set_state(next);
    }

    fn close_remote(&mut self) {
        let next = match self.state() {
            StreamState::HalfClosedLocal => StreamState::Closed,
            _ => StreamState::HalfClosedRemote,
        };
        self.set_state(next);
    }

    fn _is_closed(&self) -> bool {
        self.state() == StreamState::Closed
    }

    fn _is_closed_local(&self) -> bool {
        match self.state() {
            StreamState::HalfClosedLocal | StreamState::Closed => true,
            _ => false,
        }
    }

    fn is_closed_remote(&self) -> bool {
        match self.state() {
            StreamState::HalfClosedRemote | StreamState::Closed => true,
            _ => false,
        }
    }

    fn new_data_chunk(&mut self, data: &[u8]) {
        if let Some(ref mut sender) = self.request_handler {
            // ignore error
            sender.send(ResultOrEof::Item(HttpStreamPart::Data(data.to_owned()))).ok();
        }
    }

    fn set_headers<'n, 'v>(&mut self, headers: Vec<Header<'n, 'v>>) {
        let headers = headers.into_iter().map(|h| Header::new(h.name().to_owned(), h.value().to_owned())).collect();
        if let Some(ref mut sender) = self.request_handler {
            // ignore error
            sender.send(ResultOrEof::Item(HttpStreamPart::Headers(headers))).ok();
        }
    }

    fn set_state(&mut self, state: StreamState) {
        self.state = state;
        if self.is_closed_remote() {
            if let Some(sender) = self.request_handler.take() {
                // ignore error
                sender.send(ResultOrEof::Eof).ok();
            }
        }
    }

    fn state(&self) -> StreamState {
        self.state
    }

}

struct GrpcHttpServerSessionState<F : HttpServerHandlerFactory> {
    factory: F,
    streams: HashMap<StreamId, GrpcHttpServerStream<F>>,
    to_write_tx: tokio_core::channel::Sender<ServerToWriteMessage<F>>,
    loop_handle: reactor::Handle,
    decoder: hpack::Decoder<'static>,
}

impl<F : HttpServerHandlerFactory> GrpcHttpServerSessionState<F> {
    fn insert_stream(&mut self, stream_id: StreamId, stream: GrpcHttpServerStream<F>) -> &mut GrpcHttpServerStream<F> {
        self.streams.insert(stream_id, stream);
        self.streams.get_mut(&stream_id).unwrap()
    }

    fn _get_stream_ref(&self, stream_id: StreamId) -> Option<&GrpcHttpServerStream<F>> {
        self.streams.get(&stream_id)
    }

    fn get_stream_mut(&mut self, stream_id: StreamId) -> Option<&mut GrpcHttpServerStream<F>> {
        self.streams.get_mut(&stream_id)
    }

    fn remove_stream(&mut self, stream_id: StreamId) -> Option<GrpcHttpServerStream<F>> {
        self.streams.remove(&stream_id)
    }

    fn new_request(&mut self, stream_id: StreamId)
        -> tokio_core::channel::Sender<ResultOrEof<HttpStreamPart, HttpError>>
    {
        let (req_tx, req_rx) = tokio_core::channel::channel(&self.loop_handle).unwrap();

        let req_rx = req_rx.map_err(HttpError::from);
        let req_rx = stream_with_eof_and_error(req_rx);

        let response = self.factory.new_request(Box::new(req_rx));

        {
            let to_write_tx = self.to_write_tx.clone();
            let to_write_tx2 = to_write_tx.clone();

            let process_response = response.for_each(move |part: HttpStreamPart| {
                to_write_tx.send(ServerToWriteMessage::ResponsePart(stream_id, part)).unwrap();
                Ok(())
            }).and_then(move |()| {
                to_write_tx2.send(ServerToWriteMessage::ResponseStreamEnd(stream_id)).unwrap();
                Ok(())
            }).map_err(|e| panic!("{:?}", e)); // TODO: handle

            self.loop_handle.spawn(process_response);
        }

        req_tx
    }

    fn get_or_create_stream(&mut self, stream_id: StreamId) -> &mut GrpcHttpServerStream<F> {
        if self.get_stream_mut(stream_id).is_some() {
            // https://github.com/rust-lang/rust/issues/36403
            return self.get_stream_mut(stream_id).unwrap();
        }

        let req_tx = self.new_request(stream_id);

        // New stream initiated by the client
        let stream = GrpcHttpServerStream {
            state: StreamState::Open,
            request_handler: Some(req_tx),
            _marker: marker::PhantomData,
        };
        self.insert_stream(stream_id, stream)
    }

}


struct ServerInner<F : HttpServerHandlerFactory> {
    conn: HttpConnection,
    session_state: GrpcHttpServerSessionState<F>,
}

struct ServerReadLoop<F>
    where F : HttpServerHandlerFactory
{
    read: ReadHalf<TcpStream>,
    inner: TaskRcMut<ServerInner<F>>,
}


struct ServerReadToWriteMessage {
    buf: Vec<u8>,
}

enum ServerToWriteMessage<F : HttpServerHandlerFactory> {
    _Dummy(F),
    FromRead(ServerReadToWriteMessage),
    ResponsePart(StreamId, HttpStreamPart),
    ResponseStreamEnd(StreamId),
}


impl<F : HttpServerHandlerFactory> ServerReadLoop<F> {
    fn recv_raw_frame(self) -> HttpFuture<(Self, RawFrame<'static>)> {
        let ServerReadLoop { read, inner } = self;
        Box::new(recv_raw_frame(read)
            .map(|(read, frame)| (ServerReadLoop { read: read, inner: inner}, frame))
            .map_err(HttpError::from))
    }

    fn read_process_frame(self) -> HttpFuture<Self> {
        Box::new(self.recv_raw_frame()
            .and_then(move |(rl, frame)| rl.process_raw_frame(frame)))
    }

    fn process_settings_global(self, _frame: SettingsFrame) -> HttpFuture<Self> {
        // TODO: apply settings
        Box::new(futures::finished(self))
    }

    fn send_frame<R : FrameIR>(self, frame: R) -> HttpFuture<Self> {
        self.inner.with(|inner: &mut ServerInner<F>| {
            let mut send_buf = VecSendFrame(Vec::new());
            send_buf.send_frame(frame).unwrap();
            inner.session_state.to_write_tx.send(ServerToWriteMessage::FromRead(ServerReadToWriteMessage { buf: send_buf.0 }))
                .expect("read to write");
        });

        Box::new(futures::finished(self))
    }

    fn process_ping(self, frame: PingFrame) -> HttpFuture<Self> {
        if frame.is_ack() {
            Box::new(futures::finished(self))
        } else {
            self.send_frame(PingFrame::new_ack(frame.opaque_data()))
        }
    }

    fn process_special_frame(self, frame: HttpFrameConn) -> HttpFuture<Self> {
        match frame {
            HttpFrameConn::Settings(f) => self.process_settings_global(f),
            HttpFrameConn::Ping(f) => self.process_ping(f),
            HttpFrameConn::Goaway(_f) => panic!("TODO"),
            HttpFrameConn::WindowUpdate(_f) => panic!("TODO"),
        }
    }

    fn process_data_frame(self, frame: DataFrame) -> HttpFuture<Self> {
        self.inner.with(move |inner: &mut ServerInner<F>| {
            let stream = inner.session_state.get_or_create_stream(frame.get_stream_id());

            // TODO: decrease window

            stream.new_data_chunk(&frame.data.as_ref());

            if frame.is_set(DataFlag::EndStream) {
                stream.close_remote()
            }

            // TODO: drop stream if closed on both ends
        });

        Box::new(futures::finished(self))
    }

    fn process_headers_frame(self, frame: HeadersFrame) -> HttpFuture<Self> {
        self.inner.with(move |inner: &mut ServerInner<F>| {
            let headers = inner.session_state.decoder
                                   .decode(&frame.header_fragment())
                                   .map_err(HttpError::CompressionError).unwrap();
            let headers = headers.into_iter().map(|h| h.into()).collect();

            let stream = inner.session_state.get_or_create_stream(frame.get_stream_id());

            stream.set_headers(headers);

            if frame.is_end_of_stream() {
                stream.close_remote();
            }

            // TODO: drop stream if closed on both ends
        });

        Box::new(futures::finished(self))    }

    fn process_rst_stream_frame(self, frame: RstStreamFrame) -> HttpFuture<Self> {
        self.inner.with(move |inner: &mut ServerInner<F>| {
            // TODO: check actually removed
            inner.session_state.remove_stream(frame.get_stream_id());
        });

        Box::new(futures::finished(self))
    }

    fn process_window_update_frame(self, _frame: WindowUpdateFrame) -> HttpFuture<Self> {
        // TODO
        Box::new(futures::finished(self))
    }

    fn process_stream_frame(self, frame: HttpFrameStream) -> HttpFuture<Self> {
        match frame {
            HttpFrameStream::Data(data) => self.process_data_frame(data),
            HttpFrameStream::Headers(headers) => self.process_headers_frame(headers),
            HttpFrameStream::RstStream(rst) => self.process_rst_stream_frame(rst),
            HttpFrameStream::WindowUpdate(window_update) => self.process_window_update_frame(window_update),
        }
    }

    fn process_raw_frame(self, raw_frame: RawFrame) -> HttpFuture<Self> {
        let frame = HttpFrameClassified::from_raw(&raw_frame).unwrap();
        match frame {
            HttpFrameClassified::Conn(f) => self.process_special_frame(f),
            HttpFrameClassified::Stream(f) => self.process_stream_frame(f),
            HttpFrameClassified::Unknown(_f) => panic!("TODO"),
        }
    }

    fn run(self) -> HttpFuture<()> {
        let stream = stream_repeat(());

        let future = stream.fold(self, |rl, _| {
            rl.read_process_frame()
        });

        Box::new(future.map(|_| ()))
    }
}

struct ServerWriteLoop<F>
    where F : HttpServerHandlerFactory
{
    write: WriteHalf<TcpStream>,
    inner: TaskRcMut<ServerInner<F>>,
}

impl<F : HttpServerHandlerFactory> ServerWriteLoop<F> {
    fn _loop_handle(&self) -> reactor::Handle {
        self.inner.with(move |inner: &mut ServerInner<F>| inner.session_state.loop_handle.clone())
    }

    fn _to_write_tx(&self) -> tokio_core::channel::Sender<ServerToWriteMessage<F>> {
        self.inner.with(move |inner: &mut ServerInner<F>| inner.session_state.to_write_tx.clone())
    }

    fn write_all(self, buf: Vec<u8>) -> HttpFuture<Self> {
        let ServerWriteLoop { write, inner } = self;

        Box::new(tokio_io::write_all(write, buf)
            .map(move |(write, _)| ServerWriteLoop { write: write, inner: inner })
            .map_err(HttpError::from))
    }

    fn process_from_read(self, message: ServerReadToWriteMessage) -> HttpFuture<Self> {
        self.write_all(message.buf)
    }

    fn process_response_part(self, stream_id: StreamId, part: HttpStreamPart) -> HttpFuture<Self> {
        let send_buf = self.inner.with(move |inner: &mut ServerInner<F>| {
            inner.conn.send_part_to_vec(stream_id, part, EndStream::No).unwrap()
        });
        self.write_all(send_buf)
    }

    fn process_response_end(self, stream_id: StreamId) -> HttpFuture<Self> {
        println!("http server: process_response_end");
        let send_buf = self.inner.with(move |inner: &mut ServerInner<F>| {
            inner.conn.send_end_of_stream_to_vec(stream_id).unwrap()
        });
        self.write_all(send_buf)
    }

    fn process_message(self, message: ServerToWriteMessage<F>) -> HttpFuture<Self> {
        match message {
            ServerToWriteMessage::FromRead(from_read) => self.process_from_read(from_read),
            ServerToWriteMessage::ResponsePart(stream_id, response) => self.process_response_part(stream_id, response),
            ServerToWriteMessage::ResponseStreamEnd(stream_id) => self.process_response_end(stream_id),
            ServerToWriteMessage::_Dummy(..) => panic!(),
        }
    }

    fn run(self, requests: HttpStream<ServerToWriteMessage<F>>) -> HttpFuture<()> {
        let requests = requests.map_err(HttpError::from);
        Box::new(requests
            .fold(self, move |wl, message: ServerToWriteMessage<F>| {
                wl.process_message(message)
            })
            .map(|_| ()))
    }
}




pub struct HttpServerConnectionAsync<F : HttpServerHandlerFactory> {
    _marker: marker::PhantomData<F>,
}

impl<F : HttpServerHandlerFactory> HttpServerConnectionAsync<F> {
    pub fn new(lh: reactor::Handle, socket: TcpStream, factory : F) -> HttpFuture<()> {
        let (to_write_tx, to_write_rx) = tokio_core::channel::channel::<ServerToWriteMessage<F>>(&lh).unwrap();

        let handshake = server_handshake(socket);

        let run = handshake.and_then(move |socket| {
            let (read, write) = socket.split();

            let inner = TaskRcMut::new(ServerInner {
                conn: HttpConnection::new(HttpScheme::Http),
                session_state: GrpcHttpServerSessionState {
                    streams: HashMap::new(),
                    factory: factory,
                    to_write_tx: to_write_tx.clone(),
                    loop_handle: lh,
                    decoder : hpack::Decoder::new(),
                },
            });

            let run_write = ServerWriteLoop { write: write, inner: inner.clone() }.run(Box::new(to_write_rx.map_err(HttpError::from)));
            let run_read = ServerReadLoop { read: read, inner: inner.clone() }.run();

            run_write.join(run_read).map(|_| ())
        });

        Box::new(run.then(|x| { println!("server: end"); x }))
    }
}
