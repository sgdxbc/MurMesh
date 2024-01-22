use std::{collections::HashMap, fmt::Debug, time::Duration};

use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
};

pub trait SendEvent<M> {
    fn send(&self, event: M) -> anyhow::Result<()>;
}

pub trait OnEvent<M> {
    fn on_event(&mut self, event: M, timer: &mut dyn Timer<M>) -> anyhow::Result<()>;
}

pub type TimerId = u32;

pub trait Timer<M> {
    fn set_internal(&mut self, duration: Duration, event: M) -> anyhow::Result<TimerId>;

    fn unset(&mut self, timer_id: TimerId) -> anyhow::Result<()>;
}

impl<M> dyn Timer<M> + '_ {
    pub fn set(&mut self, duration: Duration, event: impl Into<M>) -> anyhow::Result<u32> {
        self.set_internal(duration, event.into())
    }
}

#[derive(Debug, derive_more::From)]
enum SessionEvent<M> {
    Timer(TimerId, M),
    Other(M),
}

#[derive(Debug)]
pub struct SessionSender<M>(UnboundedSender<SessionEvent<M>>);

impl<M> Clone for SessionSender<M> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<M> PartialEq for SessionSender<M> {
    fn eq(&self, other: &Self) -> bool {
        self.0.same_channel(&other.0)
    }
}

impl<M> Eq for SessionSender<M> {}

impl<M: Into<N>, N> SendEvent<M> for SessionSender<N> {
    fn send(&self, event: M) -> anyhow::Result<()> {
        self.0
            .send(event.into().into())
            .map_err(|_| anyhow::anyhow!("receiver closed"))
    }
}

pub struct Session<M> {
    sender: UnboundedSender<SessionEvent<M>>,
    receiver: UnboundedReceiver<SessionEvent<M>>,
    timer_id: TimerId,
    timers: HashMap<TimerId, JoinHandle<()>>,
}

impl<M> Debug for Session<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("timer_id", &self.timer_id)
            .field("timers", &self.timers)
            .finish_non_exhaustive()
    }
}

impl<M> Session<M> {
    pub fn new() -> Self {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        Self {
            sender,
            receiver,
            timer_id: 0,
            timers: Default::default(),
        }
    }
}

impl<M> Default for Session<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M> Session<M> {
    pub fn sender(&self) -> SessionSender<M> {
        SessionSender(self.sender.clone())
    }

    pub async fn run(&mut self, state: &mut impl OnEvent<M>) -> anyhow::Result<()>
    where
        M: Send + 'static,
    {
        loop {
            let event = match self
                .receiver
                .recv()
                .await
                .ok_or(anyhow::anyhow!("channel closed"))?
            {
                SessionEvent::Timer(timer_id, event) => {
                    if self.timers.remove(&timer_id).is_some() {
                        event
                    } else {
                        // unset/timeup contention, force to skip timer as long as it has been unset
                        // this could happen because of stalled timers in event waiting list
                        // another approach has been taken previously, by passing the timer events
                        // with a shared mutex state `timeouts`
                        // that should (probably) avoid this case in a single-thread runtime, but
                        // since tokio does not offer a generally synchronous `abort`, the following
                        // sequence is still possible in multithreading runtime
                        //   event loop lock `timeouts`
                        //   event callback `unset` timer which calls `abort`
                        //   event callback returns, event loop unlock `timeouts`
                        //   timer coroutine keep alive, lock `timeouts` and push event into it
                        //   timer coroutine finally get aborted
                        // the (probably) only solution is to implement a synchronous abort, block
                        // in `unset` call until timer coroutine replies with somehow promise of not
                        // sending timer event anymore, i don't feel that worth
                        // anyway, as long as this fallback presents the `abort` is logically
                        // redundant, just for hopefully better performance
                        // (so wish i have direct access to the timer wheel...)
                        continue;
                    }
                }
                SessionEvent::Other(event) => event,
            };
            state.on_event(
                event,
                &mut SessionTimer {
                    timer_id: &mut self.timer_id,
                    timers: &mut self.timers,
                    sender: &self.sender,
                } as _,
            )?
        }
    }
}

#[derive(Debug)]
pub struct SessionTimer<'a, M> {
    timer_id: &'a mut TimerId,
    timers: &'a mut HashMap<TimerId, JoinHandle<()>>,
    sender: &'a UnboundedSender<SessionEvent<M>>,
}

impl<M: Send + 'static> Timer<M> for SessionTimer<'_, M> {
    fn set_internal(&mut self, duration: Duration, event: M) -> anyhow::Result<TimerId> {
        *self.timer_id += 1;
        let timer_id = *self.timer_id;
        let sender = self.sender.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(duration).await;
            sender.send(SessionEvent::Timer(timer_id, event)).unwrap();
        });
        self.timers.insert(timer_id, timer);
        Ok(timer_id)
    }

    fn unset(&mut self, timer_id: TimerId) -> anyhow::Result<()> {
        self.timers
            .remove(&timer_id)
            .ok_or(anyhow::anyhow!("timer not exists"))?
            .abort();
        Ok(())
    }
}