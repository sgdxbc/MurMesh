use std::{fmt::Debug, hash::Hash, net::SocketAddr};

use bytes::Bytes;
use serde::{de::DeserializeOwned, Serialize};

use crate::event::SendEvent;

pub mod combinators;
pub mod task {
    pub mod udp;
}

pub mod events {
    // probably called `Send` in any sane codebase, but that terribly conflicts with
    // std::marker::Send
    #[derive(Debug)]
    pub struct Cast<A, M>(pub A, pub M);

    #[derive(Debug)]
    pub struct Recv<M>(pub M);
}

pub trait SendMessage<A, M> {
    fn send(&mut self, remote: A, message: M) -> anyhow::Result<()>;
}

impl<E: SendEvent<events::Cast<A, M>>, A, M> SendMessage<A, M> for E {
    fn send(&mut self, remote: A, message: M) -> anyhow::Result<()> {
        SendEvent::send(self, events::Cast(remote, message))
    }
}

pub trait Addr:
    Debug + Clone + Eq + Ord + Hash + Serialize + DeserializeOwned + Send + Sync + 'static
{
}

impl Addr for u8 {}
impl Addr for SocketAddr {}

pub fn send_bytes(
    mut sender: impl SendEvent<events::Recv<Bytes>>,
) -> impl FnMut(&[u8]) -> anyhow::Result<()> {
    move |buf| sender.send(events::Recv(Bytes::copy_from_slice(buf)))
}
