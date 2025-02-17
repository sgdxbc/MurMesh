use std::{
    any::Any,
    convert::identity,
    fmt::{Debug, Display},
    hash::{BuildHasherDefault, Hash},
    iter::repeat,
    num::NonZeroUsize,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::SeqCst},
        Arc, Barrier, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

use crossbeam_queue::SegQueue;
use derive_where::derive_where;
use rand::{seq::IteratorRandom as _, thread_rng};
use rustc_hash::FxHasher;
use scc::HashMap;

use crate::event::SendEvent;

// use scc::HashIndex as HashMap;

pub mod state;

pub trait State: SendEvent<Self::Event> {
    type Event;

    fn events(&self) -> impl Iterator<Item = Self::Event> + '_;
}

// the alternative `State` interface
//   trait State = OnEvent<C> where C: Context<Self::Event>
//   pub trait Context<M> {
//       fn register(&mut self, event: M) -> anyhow::Result<()>;
//   }
// the custom `events` method can be removed then, results in more compact
// interface
//
// the downside of this alternation
// * bootstrapping. the every first event(s) that applied to the initial state
//   is hard to be provided
// * it doesn't fit the current searching workflows. it may be possible to
//   adjust the workflows to maintain a buffer of not yet applied events, but
//   in my opinion that complicates things

fn step<S: State>(state: &mut S, event: S::Event) -> anyhow::Result<()> {
    // TODO revise whether this panic safety reasoning is correct
    catch_unwind(AssertUnwindSafe(|| state.send(event)))
        .map_err(error_from_panic)
        .and_then(identity)
}

#[derive(Debug, Clone)]
pub struct Settings<I, G, P> {
    pub invariant: I,
    pub goal: G,
    pub prune: P,
    pub max_depth: Option<NonZeroUsize>,
}

pub enum SearchResult<S, E> {
    Err(Vec<(E, S)>, E, anyhow::Error),
    InvariantViolation(Vec<(E, S)>, anyhow::Error),
    GoalFound(S),
    SpaceExhausted,
    Timeout,
}

impl<S, E> Debug for SearchResult<S, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Err(_, _, err) => write!(f, "Err({err})"),
            Self::InvariantViolation(_, err) => write!(f, "InvariantViolation({err:?})"),
            Self::GoalFound(_) => write!(f, "GoalFound"),
            Self::SpaceExhausted => write!(f, "SpaceExhausted"),
            Self::Timeout => write!(f, "Timeout"),
        }
    }
}

impl<S: Debug, E: Debug> Display for SearchResult<S, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Err(trace, event, err) => {
                for (event, state) in trace {
                    writeln!(f, "-> {event:?}")?;
                    writeln!(f, "{state:?}")?
                }
                writeln!(f, "-> {event:?}")?;
                write!(f, "{err}")
            }
            Self::InvariantViolation(trace, err) => {
                for (event, state) in trace {
                    writeln!(f, "-> {event:?}")?;
                    writeln!(f, "{state:?}")?
                }
                write!(f, "{err}")
            }
            result => write!(f, "{result:?}"),
        }
    }
}

pub fn breadth_first<S, I, G, P>(
    initial_state: S,
    settings: Settings<I, G, P>,
    num_worker: NonZeroUsize,
    max_duration: impl Into<Option<Duration>>,
) -> anyhow::Result<SearchResult<S, S::Event>>
where
    S: State + Clone + Eq + Hash + Send + Sync + 'static,
    S::Event: Clone + Send + Sync,
    I: Fn(&S) -> anyhow::Result<()> + Clone + Send + 'static,
    G: Fn(&S) -> bool + Clone + Send + 'static,
    P: Fn(&S) -> bool + Clone + Send + 'static,
{
    let discovered = Arc::new(HashMap::with_hasher(
        BuildHasherDefault::<FxHasher>::default(),
    ));
    let queue = Arc::new(SegQueue::new());
    let pushing_queue = Arc::new(SegQueue::new());
    let depth = Arc::new(AtomicUsize::new(0));
    let depth_barrier = Arc::new(Barrier::new(num_worker.get()));
    let search_finished = Arc::new((Mutex::new(None), Condvar::new(), AtomicBool::new(false)));

    let initial_state = Arc::new(initial_state);
    queue.push(initial_state.clone());
    discovered
        .insert(
            initial_state,
            StateInfo {
                prev: None,
                depth: 0,
            },
        )
        .map_err(|_| "empty discovered map at initial")
        .unwrap();

    let result = search_internal(
        max_duration,
        repeat({
            let discovered = discovered.clone();
            let depth = depth.clone();
            let search_finished = search_finished.clone();
            move || {
                breath_first_worker(
                    settings,
                    discovered,
                    queue,
                    pushing_queue,
                    depth,
                    depth_barrier,
                    search_finished,
                )
            }
        })
        .take(num_worker.get()),
        {
            let discovered = discovered.clone();
            move |elapsed| {
                format!(
                    "Explored: {}, Depth {} ({:.2}s, {:.2}K states/s)",
                    discovered.len(),
                    depth.load(SeqCst),
                    elapsed.as_secs_f32(),
                    discovered.len() as f32 / elapsed.as_secs_f32() / 1000.
                )
            }
        },
        search_finished,
    )?;
    // println!("search internal done");

    let Some(result) = result else {
        return Ok(SearchResult::Timeout);
    };
    let result = match result {
        SearchWorkerResult::Error(state, event, err) => {
            SearchResult::Err(trace(&discovered, state), event, err)
        }
        SearchWorkerResult::InvariantViolation(state, err) => {
            SearchResult::InvariantViolation(trace(&discovered, state), err)
        }
        SearchWorkerResult::GoalFound(state) => SearchResult::GoalFound(state),
        SearchWorkerResult::SpaceExhausted => SearchResult::SpaceExhausted,
    };
    // println!("search exit");
    Ok(result)
}

// the discussion above on `S` and `T` also applies here
pub fn random_depth_first<S, I, G, P>(
    initial_state: S,
    settings: Settings<I, G, P>,
    num_worker: NonZeroUsize,
    max_duration: impl Into<Option<Duration>>,
) -> anyhow::Result<SearchResult<S, S::Event>>
where
    S: State + Clone + Eq + Hash + Send + Sync + 'static,
    S::Event: Clone + Send + Sync,
    I: Fn(&S) -> anyhow::Result<()> + Clone + Send + 'static,
    G: Fn(&S) -> bool + Clone + Send + 'static,
    P: Fn(&S) -> bool + Clone + Send + 'static,
{
    let num_probe = Arc::new(AtomicU32::new(0));
    let num_state = Arc::new(AtomicU32::new(0));
    let search_finished = Arc::new((Mutex::new(None), Condvar::new(), AtomicBool::new(false)));

    let result = search_internal(
        max_duration,
        {
            let num_probe = num_probe.clone();
            let num_state = num_state.clone();
            let search_finished = search_finished.clone();
            let settings = settings.clone();
            let initial_state = initial_state.clone();
            repeat(move || {
                random_depth_first_worker(
                    settings,
                    initial_state,
                    num_probe,
                    num_state,
                    search_finished,
                )
            })
            .take(num_worker.get())
        },
        move |elapsed| {
            format!(
                "Explored: {}, Num Probes: {} ({:.2}s, {:.2}K explored/s)",
                num_state.load(SeqCst),
                num_probe.load(SeqCst),
                elapsed.as_secs_f32(),
                num_state.load(SeqCst) as f32 / elapsed.as_secs_f32() / 1000.
            )
        },
        search_finished,
    )?;
    Ok(result.unwrap_or(SearchResult::Timeout))
}

fn error_from_panic(err: Box<dyn Any + Send>) -> anyhow::Error {
    if let Ok(err) = err.downcast::<anyhow::Error>() {
        *err
    } else {
        anyhow::format_err!("unknown join error")
    }
}

fn search_internal<R: Send + 'static, F: FnOnce() + Send + 'static>(
    max_duration: impl Into<Option<Duration>>,
    workers: impl Iterator<Item = F>,
    status: impl Fn(Duration) -> String + Send + 'static,
    search_finished: SearchFinished<R>,
) -> anyhow::Result<Option<R>>
where
{
    let max_duration = max_duration.into();

    let mut worker_tasks = Vec::new();
    for worker in workers {
        worker_tasks.push(std::thread::spawn(worker))
    }
    let status_worker = std::thread::spawn({
        let search_finished = search_finished.clone();
        move || status_worker(status, search_finished)
    });

    let result = search_finished
        .0
        .lock()
        .map_err(|err| anyhow::format_err!(err.to_string()))?;
    let result = if let Some(max_duration) = max_duration {
        search_finished
            .1
            .wait_timeout_while(result, max_duration, |result| result.is_none())
            .map_err(|err| anyhow::format_err!(err.to_string()))?
            .0
            .take()
    } else {
        search_finished
            .1
            .wait_while(result, |result| result.is_none())
            .map_err(|err| anyhow::format_err!(err.to_string()))?
            .take()
    };
    std::thread::sleep(Duration::from_millis(20));
    search_finished.2.store(true, SeqCst);
    search_finished.1.notify_all();
    // println!("search finished");
    for worker in worker_tasks {
        worker.join().map_err(error_from_panic)?;
    }
    // println!("worker joined");
    status_worker.join().map_err(error_from_panic)?;
    // println!("status worker joined");
    Ok(result)
}

type SearchFinished<R> = Arc<(Mutex<Option<R>>, Condvar, AtomicBool)>;

fn status_worker<R>(status: impl Fn(Duration) -> String, search_finished: SearchFinished<R>) {
    let start = Instant::now();
    let mut result = search_finished.0.lock().unwrap();
    let mut wait_result;
    while {
        (result, wait_result) = search_finished
            .1
            .wait_timeout_while(result, Duration::from_secs(5), |_| {
                !search_finished.2.load(SeqCst)
            })
            .unwrap();
        wait_result.timed_out()
    } {
        println!("{}", status(start.elapsed()))
    }
    println!("{}", status(start.elapsed()))
}

#[derive_where(Clone; E)]
struct StateInfo<S, E> {
    prev: Option<(E, Arc<S>)>,
    #[allow(unused)]
    depth: usize, // to assert trace correctness?
}

type Discovered<S, E> = HashMap<Arc<S>, StateInfo<S, E>, BuildHasherDefault<FxHasher>>;

fn trace<S: Eq + Hash + Clone, E: Clone>(discovered: &Discovered<S, E>, target: S) -> Vec<(E, S)> {
    let info = discovered.get(&target).unwrap();
    let Some((prev_event, prev_state)) = &info.get().prev else {
        return Vec::new();
    };
    let prev_state = S::clone(prev_state);
    let prev_event = prev_event.clone();
    drop(info);
    let mut trace = trace(discovered, prev_state);
    trace.push((prev_event, target));
    trace
}

enum SearchWorkerResult<S, E> {
    Error(S, E, anyhow::Error),
    InvariantViolation(S, anyhow::Error),
    GoalFound(S),
    SpaceExhausted,
}

fn breath_first_worker<S, I, G, P>(
    settings: Settings<I, G, P>,
    discovered: Arc<Discovered<S, S::Event>>,
    mut queue: Arc<SegQueue<Arc<S>>>,
    mut pushing_queue: Arc<SegQueue<Arc<S>>>,
    depth: Arc<AtomicUsize>,
    depth_barrier: Arc<Barrier>,
    search_finished: SearchFinished<SearchWorkerResult<S, S::Event>>,
) where
    S: State + Clone + Eq + Hash + Send + Sync + 'static,
    S::Event: Clone + Send + Sync,
    I: Fn(&S) -> anyhow::Result<()>,
    G: Fn(&S) -> bool,
    P: Fn(&S) -> bool,
    // T: Debug,
    // S::Event: Debug,
{
    let search_finish = |result| {
        search_finished.0.lock().unwrap().get_or_insert(result);
        search_finished.2.store(true, SeqCst);
        search_finished.1.notify_all()
    };
    for local_depth in 0.. {
        // println!("start depth {local_depth}");
        'depth: while let Some(state) = queue.pop() {
            // TODO check initial state
            // println!("check events");
            for event in state.events() {
                // println!("step {event:?}");
                let mut next_state = S::clone(&state);
                if let Err(err) = step(&mut next_state, event.clone()) {
                    search_finish(SearchWorkerResult::Error(S::clone(&state), event, err));
                    break 'depth;
                }
                let next_state = Arc::new(next_state);
                // do not replace a previously-found state, which may be reached with a shorter
                // trace from initial state
                let mut inserted = false;
                discovered.entry(next_state.clone()).or_insert_with(|| {
                    inserted = true;
                    StateInfo {
                        prev: Some((event, state.clone())),
                        depth: local_depth + 1,
                    }
                });
                // println!("dry state {next_dry_state:?} inserted {inserted}");
                if !inserted {
                    continue;
                }
                // println!("check invariant");
                if let Err(err) = (settings.invariant)(&next_state) {
                    search_finish(SearchWorkerResult::InvariantViolation(
                        S::clone(&next_state),
                        err,
                    ));
                    break 'depth;
                }
                // println!("check goal");
                if (settings.goal)(&next_state) {
                    search_finish(SearchWorkerResult::GoalFound(S::clone(&next_state)));
                    break 'depth;
                }
                if Some(local_depth + 1) != settings.max_depth.map(Into::into)
                    && !(settings.prune)(&next_state)
                {
                    pushing_queue.push(next_state)
                }
            }
            if search_finished.2.load(SeqCst) {
                break;
            }
        }
        // println!("end depth {local_depth} pushed {}", pushing_queue.len());

        // even if the above loop breaks, this wait always traps every worker
        // so that if some worker trap here first, then other worker `search_finish()`, the former
        // worker does not stuck here
        let wait_result = depth_barrier.wait();
        // println!("barrier");
        if search_finished.2.load(SeqCst) {
            break;
        }
        // println!("continue on next depth");

        if wait_result.is_leader() {
            depth.store(local_depth + 1, SeqCst);
        }
        // one corner case: if some worker happen to perform empty check very late, and by that time
        // other workers already working on the next depth for a while and have exhausted the queue,
        // then the late worker will false positive report SpaceExhausted
        if pushing_queue.is_empty() {
            search_finish(SearchWorkerResult::SpaceExhausted);
            break;
        }
        assert_ne!(Some(local_depth + 1), settings.max_depth.map(Into::into));
        // i don't want to deal with that seriously, so just slow down the fast wakers a little bit
        std::thread::sleep(Duration::from_millis(10));
        (queue, pushing_queue) = (pushing_queue, queue)
    }
    // println!("worker exit");
}

fn random_depth_first_worker<S, I, G, P>(
    settings: Settings<I, G, P>,
    initial_state: S,
    num_probe: Arc<AtomicU32>,
    num_state: Arc<AtomicU32>,
    search_finished: SearchFinished<SearchResult<S, S::Event>>,
) where
    S: State + Clone,
    S::Event: Clone,
    I: Fn(&S) -> anyhow::Result<()>,
    G: Fn(&S) -> bool,
    P: Fn(&S) -> bool,
{
    let search_finish = |result| {
        search_finished.0.lock().unwrap().get_or_insert(result);
        search_finished.2.store(true, SeqCst);
        search_finished.1.notify_all()
    };
    let mut rng = thread_rng();
    while !search_finished.2.load(SeqCst) {
        num_probe.fetch_add(1, SeqCst);
        let mut state = initial_state.clone();
        let mut trace = Vec::new();
        // TODO check initial state
        for depth in 0.. {
            let Some(event) = state.events().choose(&mut rng).clone() else {
                break;
            };
            if let Err(err) = step(&mut state, event.clone()) {
                search_finish(SearchResult::Err(trace, event, err));
                break;
            }
            num_state.fetch_add(1, SeqCst);
            trace.push((event, state.clone()));
            if let Err(err) = (settings.invariant)(&state) {
                search_finish(SearchResult::InvariantViolation(trace, err));
                break;
            }
            // highly unpractical
            // effectively monkey-typing an OSDI paper
            if (settings.goal)(&state) {
                search_finish(SearchResult::GoalFound(state));
                break;
            }
            if (settings.prune)(&state)
                || Some(depth + 1) == settings.max_depth.map(Into::into)
                || search_finished.2.load(SeqCst)
            {
                break;
            }
        }
    }
}
