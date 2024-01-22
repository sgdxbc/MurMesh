use std::{collections::HashMap, fmt::Debug, time::Duration};

use bincode::Options as _;
use serde::{Deserialize, Serialize};

use crate::{
    app::App,
    crypto::{Crypto, DigestHash as _, Signed},
    event::{OnEvent, SendEvent, Timer, TimerId},
    net::{Addr, MessageNet, SendMessage},
    replication::Request,
    worker::Worker,
};

#[derive(Debug, Clone, Hash, Serialize, Deserialize)]
pub struct PrePrepare {
    view_num: u32,
    op_num: u32,
    digest: [u8; 32],
}

#[derive(Debug, Clone, Hash, Serialize, Deserialize)]
pub struct Prepare {
    view_num: u32,
    op_num: u32,
    digest: [u8; 32],
    replica_id: u8,
}

#[derive(Debug, Clone, Hash, Serialize, Deserialize)]
pub struct Commit {
    view_num: u32,
    op_num: u32,
    digest: [u8; 32],
    replica_id: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    seq: u32,
    result: Vec<u8>,
    view_num: u32,
    replica_id: u8,
}

pub trait ToClientNet: SendMessage<Reply> {}
impl<T: SendMessage<Reply>> ToClientNet for T {}

pub trait ToReplicaNet<A>:
    SendMessage<Request<A>, Addr = u8>
    + SendMessage<(Signed<PrePrepare>, Vec<Request<A>>), Addr = u8>
    + SendMessage<Signed<Prepare>, Addr = u8>
    + SendMessage<Signed<Commit>, Addr = u8>
{
}
impl<
        T: SendMessage<Request<A>, Addr = u8>
            + SendMessage<(Signed<PrePrepare>, Vec<Request<A>>), Addr = u8>
            + SendMessage<Signed<Prepare>, Addr = u8>
            + SendMessage<Signed<Commit>, Addr = u8>,
        A,
    > ToReplicaNet<A> for T
{
}

#[derive(Debug, Clone, derive_more::From)]
pub enum ClientEvent {
    Invoke(Vec<u8>),
    Ingress(Reply),
    ResendTimeout,
}

pub trait ClientUpcall: SendEvent<(u32, Vec<u8>)> {}
impl<T: SendEvent<(u32, Vec<u8>)>> ClientUpcall for T {}

#[derive(Debug)]
pub struct Client<N, U, A> {
    id: u32,
    addr: A,
    seq: u32,
    invoke: Option<ClientInvoke>,
    view_num: u32,
    num_replica: usize,
    num_faulty: usize,

    net: N,
    upcall: U,
}

#[derive(Debug)]
struct ClientInvoke {
    op: Vec<u8>,
    resend_timer: TimerId,
    replies: HashMap<u8, Reply>,
}

impl<N, U, A> Client<N, U, A> {
    pub fn new(id: u32, addr: A, net: N, upcall: U, num_replica: usize, num_faulty: usize) -> Self {
        Self {
            id,
            addr,
            net,
            upcall,
            num_replica,
            num_faulty,
            seq: 0,
            view_num: 0,
            invoke: Default::default(),
        }
    }
}

impl<N: ToReplicaNet<A>, U: ClientUpcall, A: Addr> OnEvent<ClientEvent> for Client<N, U, A> {
    fn on_event(
        &mut self,
        event: ClientEvent,
        timer: &mut dyn Timer<ClientEvent>,
    ) -> anyhow::Result<()> {
        match event {
            ClientEvent::Invoke(op) => self.on_invoke(op, timer),
            ClientEvent::ResendTimeout => self.on_resend_timeout(),
            ClientEvent::Ingress(reply) => self.on_ingress(reply, timer),
        }
    }
}

impl<N: ToReplicaNet<A>, U: ClientUpcall, A: Addr> Client<N, U, A> {
    fn on_invoke(&mut self, op: Vec<u8>, timer: &mut dyn Timer<ClientEvent>) -> anyhow::Result<()> {
        if self.invoke.is_some() {
            anyhow::bail!("concurrent invocation")
        }
        self.seq += 1;
        let invoke = ClientInvoke {
            op,
            resend_timer: timer.set(Duration::from_millis(1000), ClientEvent::ResendTimeout)?,
            replies: Default::default(),
        };
        self.invoke = Some(invoke);
        self.do_send()
    }

    fn on_resend_timeout(&self) -> anyhow::Result<()> {
        // TODO logging
        self.do_send()
    }

    fn on_ingress(
        &mut self,
        reply: Reply,
        timer: &mut dyn Timer<ClientEvent>,
    ) -> anyhow::Result<()> {
        if reply.seq != self.seq {
            return Ok(());
        }
        let Some(invoke) = self.invoke.as_mut() else {
            return Ok(());
        };
        invoke.replies.insert(reply.replica_id, reply.clone());
        if invoke
            .replies
            .values()
            .filter(|inserted_reply| inserted_reply.result == reply.result)
            .count()
            == self.num_faulty + 1
        {
            self.view_num = reply.view_num;
            let invoke = self.invoke.take().unwrap();
            timer.unset(invoke.resend_timer)?;
            self.upcall.send((self.id, reply.result))
        } else {
            Ok(())
        }
    }

    fn do_send(&self) -> anyhow::Result<()> {
        let request = Request {
            client_id: self.id,
            client_addr: self.addr.clone(),
            seq: self.seq,
            op: self.invoke.as_ref().unwrap().op.clone(),
        };
        // TODO broadcast on resend
        self.net
            .send((self.view_num as usize % self.num_replica) as _, &request)
    }
}

#[derive(Debug)]
pub enum ReplicaEvent<A> {
    IngressRequest(Request<A>),
    SignedPrePrepare(Signed<PrePrepare>, Vec<Request<A>>),
    IngressPrePrepare(Signed<PrePrepare>, Vec<Request<A>>),
    VerifiedPrePrepare(Signed<PrePrepare>, Vec<Request<A>>),
    SignedPrepare(Signed<Prepare>),
    IngressPrepare(Signed<Prepare>),
    VerifiedPrepare(Signed<Prepare>),
    SignedCommit(Signed<Commit>),
    IngressCommit(Signed<Commit>),
    VerifiedCommit(Signed<Commit>),
}

pub struct Replica<S, N, M, A> {
    id: u8,
    num_replica: usize,
    num_faulty: usize,

    on_request: HashMap<u32, OnRequest<A, M>>,
    requests: Vec<Request<A>>,
    view_num: u32,
    op_num: u32,
    log: Vec<LogEntry<A>>,
    prepare_quorums: HashMap<u32, HashMap<u8, Signed<Prepare>>>,
    commit_quorums: HashMap<u32, HashMap<u8, Signed<Commit>>>,
    commit_num: u32,
    app: S,
    on_verified_prepare: HashMap<u32, Vec<OnVerifiedMessage<Self>>>,
    on_verified_commit: HashMap<u32, Vec<OnVerifiedMessage<Self>>>,

    net: N,
    client_net: M,
    crypto_worker: Worker<Crypto<u8>, ReplicaEvent<A>>,
}

type OnRequest<A, N> = Box<dyn Fn(&Request<A>, &N) -> anyhow::Result<bool> + Send + Sync>;

type OnVerifiedMessage<S> = Box<dyn FnOnce(&mut S) -> anyhow::Result<()> + Send + Sync>;

#[derive(Debug)]
struct LogEntry<A> {
    view_num: u32,
    pre_prepare: Option<Signed<PrePrepare>>,
    requests: Vec<Request<A>>,
    prepares: Vec<(u8, Signed<Prepare>)>,
    commits: Vec<(u8, Signed<Commit>)>,
}

impl<A> Default for LogEntry<A> {
    fn default() -> Self {
        Self {
            view_num: Default::default(),
            pre_prepare: Default::default(),
            requests: Default::default(),
            prepares: Default::default(),
            commits: Default::default(),
        }
    }
}

impl<S, N, M, A> Debug for Replica<S, N, M, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Replica").finish_non_exhaustive()
    }
}

impl<S, N, M, A> Replica<S, N, M, A> {
    pub fn new(
        id: u8,
        app: S,
        net: N,
        client_net: M,
        crypto_worker: Worker<Crypto<u8>, ReplicaEvent<A>>,
        num_replica: usize,
        num_faulty: usize,
    ) -> Self {
        Self {
            id,
            app,
            net,
            client_net,
            crypto_worker,
            num_replica,
            num_faulty,
            on_request: Default::default(),
            requests: Default::default(),
            view_num: 0,
            op_num: 0,
            log: Default::default(),
            prepare_quorums: Default::default(),
            commit_quorums: Default::default(),
            commit_num: 0,
            on_verified_prepare: Default::default(),
            on_verified_commit: Default::default(),
        }
    }
}

impl<S: App + 'static, N: ToReplicaNet<M::Addr> + 'static, M: ToClientNet + 'static>
    OnEvent<ReplicaEvent<M::Addr>> for Replica<S, N, M, M::Addr>
{
    fn on_event(
        &mut self,
        event: ReplicaEvent<M::Addr>,
        _timer: &mut dyn Timer<ReplicaEvent<M::Addr>>,
    ) -> anyhow::Result<()> {
        match event {
            ReplicaEvent::IngressRequest(request) => self.on_ingress_request(request),
            ReplicaEvent::SignedPrePrepare(pre_prepare, requests) => {
                self.on_signed_pre_prepare(pre_prepare, requests)
            }
            ReplicaEvent::IngressPrePrepare(pre_prepare, requests) => {
                self.on_ingress_pre_prepare(pre_prepare, requests)
            }
            ReplicaEvent::VerifiedPrePrepare(pre_prepare, requests) => {
                self.on_verified_pre_prepare(pre_prepare, requests)
            }
            ReplicaEvent::SignedPrepare(prepare) => self.on_signed_prepare(prepare),
            ReplicaEvent::IngressPrepare(prepare) => self.on_ingress_prepare(prepare),
            ReplicaEvent::VerifiedPrepare(prepare) => self.on_verified_prepare(prepare),
            ReplicaEvent::SignedCommit(commit) => self.on_signed_commit(commit),
            ReplicaEvent::IngressCommit(commit) => self.on_ingress_commit(commit),
            ReplicaEvent::VerifiedCommit(commit) => self.on_verified_commit(commit),
        }
    }
}

impl<S: App + 'static, N: ToReplicaNet<M::Addr> + 'static, M: ToClientNet + 'static>
    Replica<S, N, M, M::Addr>
{
    fn is_primary(&self) -> bool {
        (self.id as usize % self.num_replica) == self.view_num as usize
    }

    fn on_ingress_request(&mut self, request: Request<M::Addr>) -> anyhow::Result<()> {
        if let Some(on_request) = self.on_request.get(&request.client_id) {
            if on_request(&request, &self.client_net)? {
                return Ok(());
            }
        }
        if !self.is_primary() {
            todo!("forward request")
        }
        // ignore resend of ongoing consensus
        self.on_request
            .insert(request.client_id, Box::new(|_, _| Ok(true)));
        self.requests.push(request);
        // TODO close batch
        Ok(())
    }

    fn on_signed_pre_prepare(
        &mut self,
        pre_prepare: Signed<PrePrepare>,
        requests: Vec<Request<M::Addr>>,
    ) -> anyhow::Result<()> {
        if pre_prepare.view_num != self.view_num {
            return Ok(());
        }
        if self.log.get(pre_prepare.op_num as usize).is_none() {
            self.log
                .resize_with(pre_prepare.op_num as usize + 1, Default::default);
        }
        let replaced = self.log[pre_prepare.op_num as usize]
            .pre_prepare
            .replace(pre_prepare.clone());
        assert!(replaced.is_none());
        self.log[pre_prepare.op_num as usize].view_num = self.view_num;
        self.log[pre_prepare.op_num as usize].requests = requests.clone();
        self.net.send_to_all(&(pre_prepare, requests))
    }

    fn on_ingress_pre_prepare(
        &mut self,
        pre_prepare: Signed<PrePrepare>,
        requests: Vec<Request<M::Addr>>,
    ) -> anyhow::Result<()> {
        if pre_prepare.view_num != self.view_num {
            if pre_prepare.view_num > self.view_num {
                todo!("state transfer to enter view")
            }
            return Ok(());
        }
        if let Some(entry) = self.log.get(pre_prepare.op_num as usize) {
            if entry.pre_prepare.is_some() {
                return Ok(());
            }
        }
        // a decent implementation probably should throttle here (as well as for prepares and
        // commits) in order to mitigate faulty proposals
        // omitted since it makes no difference in normal path
        let replica_id = (pre_prepare.view_num as usize % self.num_replica) as _;
        self.crypto_worker.submit(Box::new(move |crypto, sender| {
            if requests.sha256() == pre_prepare.digest
                && crypto.verify(&replica_id, &pre_prepare).is_ok()
            {
                sender.send(ReplicaEvent::VerifiedPrePrepare(pre_prepare, requests))
            } else {
                Ok(())
            }
        }))
    }

    fn on_verified_pre_prepare(
        &mut self,
        pre_prepare: Signed<PrePrepare>,
        requests: Vec<Request<M::Addr>>,
    ) -> anyhow::Result<()> {
        if pre_prepare.view_num != self.view_num {
            return Ok(());
        }
        if self.log.get(pre_prepare.op_num as usize).is_none() {
            self.log
                .resize_with(pre_prepare.op_num as usize + 1, Default::default);
        }
        if self.log[pre_prepare.op_num as usize].pre_prepare.is_some() {
            return Ok(());
        }
        let _ = self.log[pre_prepare.op_num as usize]
            .pre_prepare
            .insert(pre_prepare.clone());
        self.log[pre_prepare.op_num as usize].view_num = self.view_num;
        self.log[pre_prepare.op_num as usize].requests = requests;

        let prepare = Prepare {
            view_num: self.view_num,
            op_num: pre_prepare.op_num,
            digest: pre_prepare.digest,
            replica_id: self.id,
        };
        self.crypto_worker.submit(Box::new(move |crypto, sender| {
            sender.send(ReplicaEvent::SignedPrepare(crypto.sign(prepare)))
        }))?;

        if let Some(commit_quorum) = self.commit_quorums.get_mut(&pre_prepare.op_num) {
            commit_quorum.retain(|_, commit| commit.digest == pre_prepare.digest)
        }
        if let Some(prepare_quorum) = self.prepare_quorums.get_mut(&pre_prepare.op_num) {
            prepare_quorum.retain(|_, prepare| prepare.digest == pre_prepare.digest);
            if prepare_quorum.len() >= self.num_replica - self.num_faulty {
                assert!(self.log[pre_prepare.op_num as usize].prepares.is_empty());
                self.log[pre_prepare.op_num as usize].prepares = self
                    .prepare_quorums
                    .remove(&pre_prepare.op_num)
                    .unwrap()
                    .into_iter()
                    .collect();
                self.prepared(pre_prepare.op_num)?
            }
        }
        Ok(())
    }

    fn on_signed_prepare(&mut self, prepare: Signed<Prepare>) -> anyhow::Result<()> {
        if prepare.view_num != self.view_num {
            return Ok(());
        }
        self.net.send_to_all(&prepare)?;
        self.insert_prepare(prepare)?;
        Ok(())
    }

    fn on_ingress_prepare(&mut self, prepare: Signed<Prepare>) -> anyhow::Result<()> {
        if prepare.view_num != self.view_num {
            if prepare.view_num > self.view_num {
                todo!("state transfer to enter view")
            }
            return Ok(());
        }
        if let Some(entry) = self.log.get(prepare.op_num as usize) {
            if !entry.prepares.is_empty() {
                return Ok(());
            }
            if let Some(pre_prepare) = &entry.pre_prepare {
                if prepare.digest != pre_prepare.digest {
                    return Ok(());
                }
            }
        }
        let op_num = prepare.op_num;
        let do_verify = move |this: &mut Self| {
            this.crypto_worker.submit(Box::new(move |crypto, sender| {
                if crypto.verify(&prepare.replica_id, &prepare).is_ok() {
                    sender.send(ReplicaEvent::VerifiedPrepare(prepare))
                } else {
                    Ok(())
                }
            }))
        };
        if let Some(on_verified) = self.on_verified_prepare.get_mut(&op_num) {
            on_verified.push(Box::new(do_verify));
            Ok(())
        } else {
            self.on_verified_prepare.insert(op_num, Default::default());
            do_verify(self)
        }
    }

    fn on_verified_prepare(&mut self, prepare: Signed<Prepare>) -> anyhow::Result<()> {
        if prepare.view_num != self.view_num {
            return Ok(());
        }
        let op_num = prepare.op_num;
        self.insert_prepare(prepare)?;
        if let Some(on_verified) = self.on_verified_prepare.get_mut(&op_num) {
            if let Some(on_verified) = on_verified.pop() {
                on_verified(self)?;
            } else {
                // there's no pending task, remove the task list to indicate
                self.on_verified_prepare.remove(&op_num);
            }
        }
        Ok(())
    }

    fn insert_prepare(&mut self, prepare: Signed<Prepare>) -> anyhow::Result<()> {
        let prepare_quorum = self.prepare_quorums.entry(prepare.op_num).or_default();
        prepare_quorum.insert(prepare.replica_id, prepare.clone());
        if prepare_quorum.len() < self.num_replica - self.num_faulty {
            return Ok(());
        }
        let Some(entry) = self.log.get_mut(prepare.op_num as usize) else {
            // cannot match digest for now, postpone entering "prepared" until receiving pre-prepare
            return Ok(());
        };
        assert!(entry.prepares.is_empty());
        entry.prepares = self
            .prepare_quorums
            .remove(&prepare.op_num)
            .unwrap()
            .into_iter()
            .collect();
        self.prepared(prepare.op_num)
    }

    fn prepared(&mut self, op_num: u32) -> anyhow::Result<()> {
        self.on_verified_prepare.remove(&op_num);
        let digest = self.log[op_num as usize]
            .pre_prepare
            .as_ref()
            .unwrap()
            .digest;
        let commit = Commit {
            view_num: self.view_num,
            op_num,
            digest,
            replica_id: self.id,
        };
        self.crypto_worker.submit(Box::new(move |crypto, sender| {
            sender.send(ReplicaEvent::SignedCommit(crypto.sign(commit)))
        }))
    }

    fn on_signed_commit(&mut self, commit: Signed<Commit>) -> anyhow::Result<()> {
        if commit.view_num != self.view_num {
            return Ok(());
        }
        self.net.send_to_all(&commit)?;
        self.insert_commit(commit)
    }

    fn on_ingress_commit(&mut self, commit: Signed<Commit>) -> anyhow::Result<()> {
        if commit.view_num != self.view_num {
            if commit.view_num > self.view_num {
                todo!("state transfer to enter view")
            }
            return Ok(());
        }
        if let Some(entry) = self.log.get(commit.op_num as usize) {
            if !entry.commits.is_empty() {
                return Ok(());
            }
            if let Some(pre_prepare) = &entry.pre_prepare {
                if commit.digest != pre_prepare.digest {
                    return Ok(());
                }
            }
        }
        let op_num = commit.op_num;
        let do_verify = move |this: &mut Self| {
            this.crypto_worker.submit(Box::new(move |crypto, sender| {
                if crypto.verify(&commit.replica_id, &commit).is_ok() {
                    sender.send(ReplicaEvent::VerifiedCommit(commit))
                } else {
                    Ok(())
                }
            }))
        };
        if let Some(on_verified) = self.on_verified_commit.get_mut(&op_num) {
            on_verified.push(Box::new(do_verify));
            Ok(())
        } else {
            self.on_verified_commit.insert(op_num, Default::default());
            do_verify(self)
        }
    }

    fn on_verified_commit(&mut self, commit: Signed<Commit>) -> anyhow::Result<()> {
        if commit.view_num != self.view_num {
            return Ok(());
        }
        let op_num = commit.op_num;
        self.insert_commit(commit)?;
        if let Some(on_verified) = self.on_verified_commit.get_mut(&op_num) {
            if let Some(on_verified) = on_verified.pop() {
                on_verified(self)?;
            } else {
                self.on_verified_commit.remove(&op_num);
            }
        }
        Ok(())
    }

    fn insert_commit(&mut self, commit: Signed<Commit>) -> anyhow::Result<()> {
        let commit_quorum = self.commit_quorums.entry(commit.op_num).or_default();
        commit_quorum.insert(commit.replica_id, commit.clone());
        if commit_quorum.len() < self.num_replica - self.num_faulty {
            return Ok(());
        }
        let Some(entry) = self.log.get_mut(commit.op_num as usize) else {
            return Ok(());
        };
        if entry.prepares.is_empty() {
            return Ok(());
        }
        assert!(entry.commits.is_empty());
        entry.commits = self
            .commit_quorums
            .remove(&commit.op_num)
            .unwrap()
            .into_iter()
            .collect();

        while let Some(entry) = self.log.get(self.commit_num as usize + 1) {
            if entry.commits.is_empty() {
                break;
            }
            for request in &entry.requests {
                let result = self.app.execute(&request.op)?;
                let seq = request.seq;
                let reply = Reply {
                    seq,
                    result,
                    view_num: self.view_num,
                    replica_id: self.id,
                };
                let addr = request.client_addr.clone();
                let on_request = move |request: &Request<M::Addr>, net: &M| {
                    if request.seq < seq {
                        return Ok(true);
                    }
                    if request.seq == seq {
                        net.send(addr.clone(), &reply)?;
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                };
                on_request(request, &self.client_net)?;
                self.on_request
                    .insert(request.client_id, Box::new(on_request));
            }
        }
        Ok(())
    }
}

pub type ToClientMessageNet<T> = MessageNet<T, Reply>;

pub fn to_client_on_buf(sender: &impl SendEvent<Reply>, buf: &[u8]) -> anyhow::Result<()> {
    let message = bincode::options().allow_trailing_bytes().deserialize(buf)?;
    sender.send(message)
}

#[derive(Debug, Clone, Serialize, Deserialize, derive_more::From)]
pub enum ToReplica<A> {
    Request(Request<A>),
    PrePrepare(Signed<PrePrepare>, Vec<Request<A>>),
    Prepare(Signed<Prepare>),
    Commit(Signed<Commit>),
}

pub type ToReplicaMessageNet<T, A> = MessageNet<T, ToReplica<A>>;

pub fn to_replica_on_buf<A: Addr>(
    sender: &impl SendEvent<ReplicaEvent<A>>,
    buf: &[u8],
) -> anyhow::Result<()> {
    match bincode::options().allow_trailing_bytes().deserialize(buf)? {
        ToReplica::Request(message) => sender.send(ReplicaEvent::IngressRequest(message)),
        ToReplica::PrePrepare(message, requests) => {
            sender.send(ReplicaEvent::IngressPrePrepare(message, requests))
        }
        ToReplica::Prepare(message) => sender.send(ReplicaEvent::IngressPrepare(message)),
        ToReplica::Commit(message) => sender.send(ReplicaEvent::IngressCommit(message)),
    }
}