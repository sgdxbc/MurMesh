use std::{collections::BTreeSet, fmt::Debug, time::Duration};

use derive_where::derive_where;

use crate::{
    event::{ScheduleEvent, SendEvent, TimerId},
    net::events::Cast,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[derive_where(Default)]
pub struct Schedule<M> {
    envelops: Vec<TimerEnvelop<M>>,
    count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TimerEnvelop<M> {
    id: u32,
    period: Duration,
    event: M,
}

impl<M> Schedule<M> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<M: Into<N>, N> ScheduleEvent<M> for Schedule<N> {
    fn set(&mut self, period: Duration, event: M) -> anyhow::Result<TimerId> {
        self.count += 1;
        let id = self.count;
        let envelop = TimerEnvelop {
            id,
            event: event.into(),
            period,
        };
        self.envelops.push(envelop);
        Ok(TimerId(id))
    }

    fn set_internal(
        &mut self,
        _: Duration,
        _: impl FnMut() -> M + Send + 'static,
    ) -> anyhow::Result<TimerId> {
        anyhow::bail!("unimplemented")
    }

    fn unset(&mut self, TimerId(id): TimerId) -> anyhow::Result<()> {
        self.remove(id)?;
        Ok(())
    }
}

impl<M> Schedule<M> {
    fn remove(&mut self, id: u32) -> anyhow::Result<TimerEnvelop<M>> {
        let Some(pos) = self.envelops.iter().position(|envelop| envelop.id == id) else {
            anyhow::bail!("missing timer of {:?}", TimerId(id))
        };
        Ok(self.envelops.remove(pos))
    }

    pub fn tick(&mut self, TimerId(id): TimerId) -> anyhow::Result<()> {
        let ticked = self.remove(id)?;
        self.envelops.push(ticked);
        Ok(())
    }
}

impl<M: Clone> Schedule<M> {
    pub fn events(&self) -> impl Iterator<Item = (TimerId, M)> + '_ {
        let mut limit = Duration::MAX;
        self.envelops.iter().map_while(move |envelop| {
            if envelop.period >= limit {
                return None;
            }
            limit = envelop.period;
            Some((TimerId(envelop.id), envelop.event.clone()))
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[derive_where(Default)]
pub struct Network<A, M> {
    messages: BTreeSet<(A, M)>,
}

impl<A, M> Network<A, M> {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<A: Ord + Debug, M: Into<N>, N: Ord> SendEvent<Cast<A, M>> for Network<A, N> {
    fn send(&mut self, Cast(remote, message): Cast<A, M>) -> anyhow::Result<()> {
        self.messages.insert((remote, message.into()));
        Ok(())
    }
}

impl<A: Clone, M: Clone> Network<A, M> {
    pub fn events(&self) -> impl Iterator<Item = (A, M)> + '_ {
        self.messages.iter().cloned()
    }
}
