use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::pin::Pin;

use futures::stream::FuturesUnordered;
use futures::StreamExt;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use log::{error, trace, warn};

use crate::Action;
use crate::Action::None;

#[must_use = "TcpBridge does nothing unless extracted from / input to"]
pub struct TcpBridge {
    /// This is the bridged port
    port: u16,
    streams: HashMap<u16, TcpStream>,
    opt_listener: Option<TcpListener>,
    closing: bool,
}

/// constructors
impl TcpBridge {
    /// Creates a listening [TcpBridge] which accepts connections to `port`
    ///
    /// # Cancel safety
    /// This method is to be assumed not cancel safe.
    /// The cancel safety of this method cannot be garanteed since the underlying
    /// `bind` future does not make any statements about cancel safety
    pub async fn accepting_from(port: u16) -> TcpBridge {
        let addr_l = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
        let listener = TcpListener::bind(addr_l)
            .await
            .expect("could not bind vlan socket");

        TcpBridge {
            port,
            streams: HashMap::new(),
            opt_listener: Some(listener),
            closing: false,
        }
    }

    /// Creates an emitting [TcpBridge] which emits connections to `port`
    pub fn emit_to(port: u16) -> TcpBridge {
        TcpBridge {
            port,
            streams: HashMap::new(),
            opt_listener: std::option::Option::None,
            closing: false,
        }
    }
}

/// extract logic
impl TcpBridge {
    /// Wait for an action to occur which can the transfered to the other half of this Bridge
    ///
    /// # Cancel safety
    /// This method is cancel safe.
    /// After a returning action has been selected all internal futures will be dropped.
    /// Underlying futures are all cancel safe, hence this function may be used in [tokio::select]
    pub async fn extract<'a>(&mut self, buf: &'a mut [u8]) -> Action<'a> {
        if self.is_closed() {
            return Action::None;
        }
        let id = self.port;

        // create arena allocation to avoid boxing futures individually
        let mut read_arena = Vec::with_capacity(self.streams.len());
        for (v_port, socket) in self.streams.iter() {
            let read_future = async {
                trace!("poll reading in {id}");
                _ = socket.readable().await;
                trace!("ready reading in {id}");
                Extractable::Read(*v_port)
            };
            read_arena.push(read_future);
        }

        struct ListFut<'a>(Option<&'a TcpListener>);
        impl<'a> Future for ListFut<'a> {
            type Output = Extractable;

            fn poll(
                self: Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Self::Output> {
                use std::task::Poll::*;
                let Some(list) = self.0 else { return Pending };

                match list.poll_accept(cx) {
                    std::task::Poll::Pending => std::task::Poll::Pending,
                    std::task::Poll::Ready(ready) => match ready {
                        Err(err) => Ready(Extractable::Error(err)),
                        Ok((stream, addr)) => Ready(Extractable::New(stream, addr.port())),
                    },
                }
            }
        }
        let mut listen_future = ListFut(self.opt_listener.as_ref());

        // create futures aggregation (must happen after allocation of the futures because of drop order)
        let mut futures = FuturesUnordered::<Pin<&mut dyn Future<Output = Extractable>>>::new();

        let listen_future_pin = unsafe { Pin::new_unchecked(&mut listen_future) };
        futures.push(listen_future_pin);
        for future in read_arena.iter_mut() {
            let future = unsafe { Pin::new_unchecked(future) };
            futures.push(future);
        }

        if let Some(extraction) = futures.next().await {
            drop(futures);
            drop(read_arena);
            return match extraction {
                Extractable::Error(err) => {
                    self.closing = true;
                    self.opt_listener.take();
                    Action::AcceptError(err)
                }
                Extractable::New(socket, v_port) => {
                    if self.streams.insert(v_port, socket).is_some() {
                        error!("cannot accept a socket into a v_port that already has a socket");
                        panic!("this should not happen")
                    }
                    Action::Connect(v_port)
                }
                Extractable::Read(v_port) => self.extract_from(v_port, buf),
            };
        }

        None
    }

    fn extract_from<'a>(&mut self, v_port: u16, buf: &'a mut [u8]) -> Action<'a> {
        let Some(socket) = self.streams.get_mut(&v_port) else {
            panic!("should not be able to happen")
        };

        match socket.try_read(buf) {
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => None,
            Err(err) => {
                // erorr means socket dead, remove and drop the stream
                self.streams
                    .remove(&v_port)
                    .expect("there was no socket to close");
                Action::Error(v_port, err)
            }
            Ok(0) => {
                // 0 means EOF, remove and drop the stream
                self.streams
                    .remove(&v_port)
                    .expect("there was no socket to close");
                Action::Data(v_port, &[])
            }
            Ok(count) => Action::Data(v_port, &buf[..count]),
        }
    }
}

/// input logic
impl TcpBridge {
    /// Inputs an action produced by the other half of this Bridge
    ///
    /// # Cancel safety
    /// This method is not cancellation safe.
    /// If it is used as the event in a [tokio::select] statement and
    /// some other branch completes first, then the provided action may
    /// have been partially executed, but future calls to `input` will
    /// start over with the action
    pub async fn input<'a>(&mut self, action: Action<'a>) -> Action {
        let id = self.port;
        match action {
            None => None,
            Action::AcceptError(err) => {
                assert!(
                    self.opt_listener.is_none(),
                    "cannot receive AcceptError when were the listening side"
                );
                trace!("input AcceptError({err}) in {id}, remote wont accept any more connections");
                self.closing = true;
                Action::None
            }
            Action::Error(v_port, err) => {
                trace!("input Error({v_port}, {err}) in {id}");
                self.input_error(v_port, err);
                Action::None
            }
            Action::Connect(v_port) => {
                trace!("input Connect({v_port})");
                self.connect_new(v_port).await
            }
            Action::Data(v_port, data) => {
                trace!("input Data({v_port}, len()={})", data.len());
                self.write_to(v_port, data).await
            }
        }
    }

    pub fn input_error(&mut self, v_port: u16, _err: io::Error) {
        let stream = self.streams.remove(&v_port).or_else(|| {
            warn!("got ReadError for already closed v_port, {v_port}");
            std::option::Option::None
        });
        drop(stream);
    }

    /// Writes the given `data` to the [TcpStream] associated with `v_port`
    ///
    /// # Cancel safety
    /// This method is not cancellation safe.
    async fn write_to<'a>(&mut self, v_port: u16, data: &'a [u8]) -> Action {
        if data.is_empty() {
            // empty a.k.a. len() == 0 means EOF on other side
            // remove and close stream
            self.streams
                .remove(&v_port)
                .expect("there was no socket to close");
            return Action::None;
        }

        let Some(local_tx) = self.streams.get_mut(&v_port) else {
            warn!("got Data for already closed v_port, {v_port}, emitting NotConnected Error");
            return Action::Error(v_port, io::Error::new(io::ErrorKind::NotConnected, "err"));
        };

        match local_tx.write_all(data).await {
            Err(err) => Action::Error(v_port, err),
            Ok(()) => None,
        }
    }

    /// Creates a new [TcpStream] and associates it with `v_port`.
    ///
    /// The returned [Action] contains information on potential errors during
    /// the creation of this [TcpStream] or its underlying [TcpSocket] and must
    /// be sent to the other half of this Bridge to inform it about the result of
    /// this action.
    ///
    /// # Cancel safety
    /// This method is to be assumed not cancel safe.
    /// The cancel safety of this method cannot be garanteed since the underlying
    /// `socket.connect` future does not make any statements about cancel safety
    async fn connect_new(&mut self, v_port: u16) -> Action {
        assert!(self.opt_listener.is_none()); // ensure we are not the listening side

        let addr_l = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, self.port));
        let stream = match TcpStream::connect(addr_l).await {
            Ok(stream) => stream,
            Err(err) => return Action::Error(v_port, err),
        };

        self.streams.insert(v_port, stream);

        None
    }
}

/// info getters
impl TcpBridge {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn is_listening(&self) -> bool {
        self.opt_listener.is_some()
    }

    pub fn active_connections(&self) -> usize {
        self.streams.len()
    }

    pub fn is_closed(&self) -> bool {
        self.closing & !self.is_listening() & (self.active_connections() == 0)
    }

    pub fn close(&mut self) {
        self.closing = true;
        self.opt_listener.take();
    }
}

#[derive(Debug)]
enum Extractable {
    Read(u16),
    New(TcpStream, u16),
    Error(io::Error),
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        net::{Ipv4Addr, SocketAddr, SocketAddrV4},
        time::Duration,
    };

    use log::trace;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        select,
        sync::oneshot,
        task::JoinHandle,
    };

    use crate::{Action, TcpBridge};

    #[tokio::test]
    async fn bridge_test() {
        _ = env_logger::try_init();

        const SERVER_PORT: u16 = 9999;
        const BRIDGE_PORT: u16 = 10000;
        let (kill_tx, kill_rx) = oneshot::channel::<()>();
        let mut listening = TcpBridge::accepting_from(BRIDGE_PORT).await;
        let mut emitting = TcpBridge::emit_to(SERVER_PORT);
        let mut server_handle = tokio::spawn(dummy_server(kill_rx, SERVER_PORT));

        let mut listen_buf = [0u8; 4092];
        let mut emit_buf = [0u8; 4092];

        let mut client_handle = tokio::spawn(dummy_client(BRIDGE_PORT));

        // give time for the server dummy server to start
        tokio::time::sleep(Duration::from_millis(200)).await; // purely for better readable logs

        loop {
            let closed_l = listening.is_closed();
            let closed_e = emitting.is_closed();
            if closed_l | closed_e {
                trace!("closed, Listening: {closed_l}, Emitting: {closed_e}");
                break;
            }
            trace!("loop extract");
            select! {
                Err(err) = &mut server_handle => Err(err).expect("dummy server error"),
                Err(err) = &mut client_handle => Err(err).expect("dummy client error"),
                listen_action = listening.extract(&mut listen_buf) => {
                    handle_bridge_action(&mut listening, &mut emitting, listen_action).await.expect("snding action into emitting failed")
                },
                emitting_axtion = emitting.extract(&mut emit_buf), if emitting.active_connections() > 0 => {
                    //after first time connecting set emitting to close in order to terminate after the client finished
                    emitting.close();
                    handle_bridge_action(&mut emitting, &mut listening, emitting_axtion).await.expect("sending action into listening failed")},
            }
        }

        trace!("wait on client handle end");
        client_handle
            .await
            .expect("err joining on dummy server")
            .expect("dummy server error");
        trace!("end server");
        kill_tx.send(()).expect("dummy server already failed");
        trace!("wait on server end");
        server_handle
            .await
            .expect("err joining on dummy server")
            .expect("dummy server error");
        trace!("end");
    }

    async fn handle_bridge_action<'a>(
        own: &mut TcpBridge,
        other: &mut TcpBridge,
        action: Action<'a>,
    ) -> io::Result<()> {
        trace!("bridge action in {}", own.port);

        if let Action::None = action {
            return Ok(());
        }

        let other_id = other.port();
        let response = other.input(action).await;
        trace!("input to {} result: {response:?}", other_id);
        if let Action::None = response {
            return Ok(());
        }

        let Action::Error(v_port, err) = response else {
            // TODO make this a compile time constraint
            panic!("input is not allowed to return anything but None or Error");
        };

        trace!("input Error({v_port}, {err} to {}", own.port);
        own.input_error(v_port, err);

        Ok(())
    }

    async fn dummy_client(port: u16) -> io::Result<()> {
        let addr_l = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
        let mut stream = TcpStream::connect(addr_l).await?;

        for i in (1..=50).rev() {
            trace!("dummy client send {i}");
            stream.write_u128_le(i).await?;
            assert_eq!(stream.read_u128_le().await?, i);
        }

        stream.write_u128_le(0).await?;
        let mut buf = [0];
        assert_eq!(stream.read(&mut buf).await?, 0);

        trace!("dummy client end");
        Ok(())
    }

    async fn dummy_server(mut kill_rx: oneshot::Receiver<()>, port: u16) -> io::Result<()> {
        trace!("start dummy server");
        let addr_l = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
        let listener = TcpListener::bind(addr_l).await?;
        let mut clients = Vec::<JoinHandle<io::Result<()>>>::new();

        loop {
            select! {
                _ = &mut kill_rx => break,
                Ok((stream, _addr)) = listener.accept() => clients.push(tokio::spawn(echo_chamber(stream))),
            }
        }

        for client in clients {
            trace!("waiting on client to finish doing stuff");
            client
                .await
                .expect("join error")
                .expect("dummy server handleing error");
        }

        trace!("dummy server end");
        Ok(())
    }

    /// continously reads 16 bytes then writes them back to the stream
    /// terminates upon receiving `[0x00; 16]` before it is echoed
    async fn echo_chamber(mut stream: TcpStream) -> io::Result<()> {
        trace!("start echo chamber");
        loop {
            let number = stream.read_u128_le().await?;

            if number == 0 {
                trace!("end echo chamber");
                return Ok(());
            }

            trace!("echo {number}");
            stream.write_u128_le(number).await?;
        }
    }
}
