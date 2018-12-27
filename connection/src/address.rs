use std::{net::SocketAddr, io};
use secp256k1::{SecretKey, PublicKey};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    prelude::{Future, Stream, Poll},
    net::{TcpStream, TcpListener, tcp::{ConnectFuture, Incoming}},
};
use brontide::{BrontideStream, HandshakeError};

pub trait AbstractAddress {
    type Error;
    type Stream: AsyncRead + AsyncWrite + Send + 'static;
    type Outgoing: Future<Item=BrontideStream<Self::Stream>, Error=HandshakeError> + Send + 'static;
    type Incoming: Stream<Item=BrontideStream<Self::Stream>, Error=HandshakeError> + Send + 'static;

    fn connect(&self, local_secret: SecretKey, remote_public: PublicKey) -> Self::Outgoing;
    fn listen(&self, local_secret: SecretKey) -> Result<Self::Incoming, Self::Error>;
}

impl AbstractAddress for SocketAddr {
    type Error = io::Error;
    type Stream = TcpStream;
    type Outgoing = TcpConnection;
    type Incoming = TcpConnectionStream;

    fn connect(&self, local_secret: SecretKey, remote_public: PublicKey) -> Self::Outgoing {
        TcpConnection {
            inner: TcpStream::connect(self),
            local_secret: local_secret,
            remote_public: remote_public,
        }
    }

    fn listen(&self, local_secret: SecretKey) -> Result<Self::Incoming, Self::Error> {
        Ok(TcpConnectionStream {
            inner: TcpListener::bind(self)
                .map(TcpListener::incoming)?,
            local_secret: local_secret,
        })
    }
}

pub struct TcpConnection {
    inner: ConnectFuture,
    local_secret: SecretKey,
    remote_public: PublicKey,
}

impl Future for TcpConnection {
    type Item = BrontideStream<TcpStream>;
    type Error = HandshakeError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        use tokio::prelude::Async::*;
        match self.inner.poll() {
            Ok(Ready(stream)) => {
                BrontideStream::outgoing(
                    stream,
                    self.local_secret.clone(),
                    self.remote_public.clone()
                ).poll()
            },
            Ok(NotReady) => Ok(NotReady),
            Err(error) => Err(HandshakeError::Io(error)),
        }
    }
}

pub struct TcpConnectionStream {
    inner: Incoming,
    local_secret: SecretKey,
}

impl Stream for TcpConnectionStream {
    type Item = BrontideStream<TcpStream>;
    type Error = HandshakeError;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        use tokio::prelude::Async::*;

        match self.inner.poll() {
            Ok(Ready(Some(stream))) => BrontideStream::incoming(stream, self.local_secret.clone())
                .poll().map(|a| a.map(Some)),
            Ok(Ready(None)) => Ok(Ready(None)),
            Ok(NotReady) => Ok(NotReady),
            Err(error) => Err(HandshakeError::Io(error)),
        }
    }
}

pub enum Command<A>
where
    A: AbstractAddress,
{
    Connect {
        address: A,
        remote_public: PublicKey,
    },
}

pub struct ConnectionStream<A, C>
where
    A: AbstractAddress,
    C: Stream<Item=Command<A>, Error=()>,
{
    incoming: A::Incoming,
    outgoing: Vec<A::Outgoing>,
    control: C,
    local_secret: SecretKey,
}

impl<A, C> ConnectionStream<A, C>
where
    A: AbstractAddress,
    C: Stream<Item=Command<A>, Error=()>,
{
    pub fn new(address: &A, control: C, local_secret: SecretKey) -> Result<Self, A::Error> {
        Ok(ConnectionStream {
            incoming: address.listen(local_secret.clone())?,
            outgoing: Vec::new(),
            control: control,
            local_secret: local_secret,
        })
    }
}

#[allow(non_shorthand_field_patterns)]
impl<A, C> Stream for ConnectionStream<A, C>
where
    A: AbstractAddress,
    C: Stream<Item=Command<A>, Error=()>,
{
    type Item = BrontideStream<A::Stream>;
    type Error = HandshakeError;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        use tokio::prelude::Async::*;

        match self.control.poll().unwrap() {
            Ready(None) => Ok(Ready(None)),
            Ready(Some(command)) => match command {
                Command::Connect {
                    address: address,
                    remote_public: remote_public,
                } => {
                    let secret = self.local_secret.clone();
                    self.outgoing.push(address.connect(secret, remote_public));
                    Ok(NotReady)
                }
            },
            NotReady => {
                let incoming = self.incoming.poll()?;
                if let Ready(t) = incoming {
                    Ok(Ready(t))
                } else {
                    for r in self.outgoing.iter_mut() {
                        if let Ready(t) = r.poll()? {
                            return Ok(Ready(Some(t)))
                        }
                    }
                    Ok(NotReady)
                }
            }
        }
    }
}