use futures::{Future, Stream, Sink};

use tokio_core::reactor::Core;
use tokio_core::net::TcpListener;

use tokio_io::AsyncRead;

use tokio_service::{Service, NewService};

use std::io;
use std::net::SocketAddr;

use message::{self, Message, Op, Code};
use cache;
use codec::CacheCodec;
use std::sync::Arc;
use std::error::Error;
use futures::sync::oneshot;
use stats::Stats;
use time;

/// Takes a `NewService<Request=Message, Response=Message>` and servces it at `addr`.
pub fn serve<T>(addr: SocketAddr, s: T) -> io::Result<()>
where
    T: NewService<Request = Message, Response = Message, Error = io::Error> + 'static,
    <T::Instance as Service>::Future: 'static,
{
    // The primary event loop
    let mut core = Core::new()?;
    let handle = core.handle();

    // Bind to the socket
    let listener = TcpListener::bind(&addr, &handle)?;

    let connections = listener.incoming();
    // Iterate over the the stream of connections.
    let server = connections.for_each(move |(socket, _peer_addr)| {
        // Split the connection into a Sink and a Stream.
        let (writer, reader) = socket.framed(CacheCodec).split();
        let service = s.new_service().unwrap();

        // Map the service function onto each element in the stream.
        let responses = reader.and_then(move |(req_id, msg)| {
            service.call(msg).map(move |resp| (req_id, resp))
        });

        // Finally, write out all of the responses.
        let server = writer.send_all(responses).then(|_| Ok(()));
        handle.spawn(server);
        Ok(())
    });

    core.run(server)
}

/// A service middleware that dispatches requests to `cache::Cache`.
pub struct CacheService {
    pub cache: Arc<cache::Cache>,
}

impl Service for CacheService {
    type Request = Message;
    type Response = Message;
    type Error = io::Error;
    type Future = Box<Future<Item = Message, Error = io::Error>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let (snd, rcv) = oneshot::channel();

        self.cache.process(req, snd);

        // rcv is a future that resolves when snd receives a message
        rcv.map_err(|e| io::Error::new(io::ErrorKind::Other, e.description()))
            .boxed()
    }
}

impl NewService for CacheService {
    type Request = Message;
    type Response = Message;
    type Error = io::Error;
    type Instance = CacheService;

    fn new_service(&self) -> io::Result<Self::Instance> {
        Ok(CacheService { cache: self.cache.clone() })
    }
}

/// A simplistic stat collecting middleware that counts total number of requests and tracks
/// average request time.
pub struct StatService<T> {
    pub inner: T,
    pub stats: Arc<Stats>,
}

impl<T> Service for StatService<T>
    where T: Service<Request = Message, Response = Message, Error = io::Error>,
          T::Future: 'static {
    type Request = Message;
    type Response = Message;
    type Error = io::Error;
    type Future = Box<Future<Item = Message, Error = io::Error>>;

    // TODO: Clean this up
    fn call(&self, req: Self::Request) -> Self::Future {
        match req.op() {
            Op::Stats => {
                let data = self.stats.get_stats();
                Box::new(self.inner.call(req).map(|resp| match resp {
                    message::Message::Response(_, _, Some(payload)) => {
                        let len = payload.type_id();
                        let s = format!("keys: {} ", len) + data.as_ref();
                        message::response(Op::Stats, Code::Ok, Some(
                            message::payload(1, s.into_bytes())))
                    }
                    _ => message::response(Op::Stats, Code::Ok,
                                           Some(message::payload(1, data.into_bytes())))
                }))
            }
            _ => {
                let stats = self.stats.clone();
                let start_time = time::now();
                Box::new(self.inner.call(req).and_then(move|resp|{
                    stats.incr_total_requests();
                    stats.add_request_time((time::now() - start_time)
                    .num_microseconds().unwrap() as usize);
                    Ok(resp)
                }))
            }
        }
    }
}

impl<T> NewService for StatService<T>
where
    T: NewService<
        Request = Message,
        Response = Message,
        Error = io::Error,
    >,
    <T::Instance as Service>::Future: 'static,
{
    type Request = Message;
    type Response = Message;
    type Error = io::Error;
    type Instance = StatService<T::Instance>;

    fn new_service(&self) -> io::Result<Self::Instance> {
        let inner = self.inner.new_service()?;
        Ok(StatService {
            inner: inner,
            stats: self.stats.clone(),
        })
    }
}

/// A printf logger middleware.
pub struct LogService<T> {
    pub inner: T,
}

impl<T> Service for LogService<T>
    where T: Service<Request = Message, Response = Message, Error = io::Error>,
          T::Future: 'static {
    type Request = Message;
    type Response = Message;
    type Error = io::Error;
    type Future = Box<Future<Item = Message, Error = io::Error>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        println!("{}", req);
        Box::new(self.inner.call(req).and_then(|resp| {
            println!("{}", resp);
            Ok(resp)
        }))
    }
}

impl<T> NewService for LogService<T>
where
    T: NewService<
        Request = Message,
        Response = Message,
        Error = io::Error,
    >,
    <T::Instance as Service>::Future: 'static,
{
    type Request = Message;
    type Response = Message;
    type Error = io::Error;
    type Instance = LogService<T::Instance>;

    fn new_service(&self) -> io::Result<Self::Instance> {
        let inner = self.inner.new_service()?;
        Ok(LogService { inner: inner })
    }
}
