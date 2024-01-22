use std::{hash::Hash, net::SocketAddr, sync::Arc};

use bincode::Options as _;
use bytes::Bytes;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::event::SendEvent;

pub trait Addr: Send + Sync + Clone + Eq + Hash + Serialize + DeserializeOwned + 'static {}
impl<T: Send + Sync + Clone + Eq + Hash + Serialize + DeserializeOwned + 'static> Addr for T {}

pub trait SendMessage<M> {
    type Addr: Addr;

    fn send(&self, dest: Self::Addr, message: &M) -> anyhow::Result<()>;

    fn send_to_all(&self, message: &M) -> anyhow::Result<()>;
}

pub trait Buf: AsRef<[u8]> + Send + Sync + Clone + 'static {}
impl<T: AsRef<[u8]> + Send + Sync + Clone + 'static> Buf for T {}

pub trait SendBuf {
    type Addr: Addr;

    fn send(&self, dest: Self::Addr, buf: impl Buf) -> anyhow::Result<()>;

    fn send_to_all(&self, buf: impl Buf) -> anyhow::Result<()>;
}

#[derive(Debug, Clone)]
pub struct Udp(pub Arc<tokio::net::UdpSocket>);

impl Udp {
    pub async fn recv_session(
        &self,
        mut on_buf: impl FnMut(&[u8]) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let mut buf = vec![0; 1 << 16];
        loop {
            let (len, _) = self.0.recv_from(&mut buf).await?;
            on_buf(&buf[..len])?
        }
    }
}

impl SendBuf for Udp {
    type Addr = SocketAddr;

    fn send(&self, dest: Self::Addr, buf: impl Buf) -> anyhow::Result<()> {
        let socket = self.0.clone();
        tokio::spawn(async move { socket.send_to(buf.as_ref(), dest).await.unwrap() });
        Ok(())
    }

    fn send_to_all(&self, _: impl Buf) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("boradcast is not supported"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SendAddr<T>(pub T);

#[derive(Debug)]
pub struct Auto<A>(std::marker::PhantomData<A>); // TODO better name

impl<T: SendEvent<M>, M> SendMessage<M> for Auto<SendAddr<T>>
where
    SendAddr<T>: Addr,
    M: Clone,
{
    type Addr = SendAddr<T>;

    fn send(&self, dest: Self::Addr, message: &M) -> anyhow::Result<()> {
        dest.0.send(message.clone())
    }

    fn send_to_all(&self, _: &M) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("boradcast is not supported"))
    }
}

#[derive(Debug)]
pub struct MessageNet<T, M>(pub T, std::marker::PhantomData<M>);

impl<T, M> MessageNet<T, M> {
    pub fn new(raw_net: T) -> Self {
        Self(raw_net, Default::default())
    }
}

impl<T, M> From<T> for MessageNet<T, M> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: SendBuf, M: Clone + Into<N>, N: Serialize> SendMessage<M> for MessageNet<T, N> {
    type Addr = T::Addr;

    fn send(&self, dest: Self::Addr, message: &M) -> anyhow::Result<()> {
        let buf = Bytes::from(bincode::options().serialize(&message.clone().into())?);
        self.0.send(dest, buf)
    }

    fn send_to_all(&self, message: &M) -> anyhow::Result<()> {
        let buf = Bytes::from(bincode::options().serialize(&message.clone().into())?);
        self.0.send_to_all(buf)
    }
}