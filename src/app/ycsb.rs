// a reduced implementation of YCSB core workload. there's no `impl App` here,
// other modules may contain implementors that work with `Op` and `Result`
//
// detailed difference with upstream
// * table name is omitted
// * only default field policy i.e. read/scan all field and update single field
// * deterministic and data integrity are not implemented
// * `insertstart`/`insertcount` are removed. the load phase is supposed to
//   bypass the evaluated protocols and directly perform on `impl App`s with
//   `startup_ops`. use seeded RNG to build store with deterministic content
// * key chooser distribution i.e. request distribution is based on
//   `recordcount` rather than `insertstart`/`insertcount`. i don't understand
//   why restrict each client to operate on nonoverlapping keys anyway
// * `operationcount` is optional
// * zipfian request distribution only ranges in up to `recordcount` instead of
//   taking the inserted keys during evaluation into account. the other
//   distributions only range up to `recordcount` (`insertstart + insertcount`
//   in upstream) as well. this change enables optional `operationcount`
// * only hashed insertion order is implemented
// * only zipfian, latest and uniform request distributions are implemented
// * zero padding length is default to 20 which corresponding to 24 byte keys,
//   matching the expectation of upstream's workload comment "1KB record (...
//   plus key)"
// * see below for discussion on zipfian parameter

use std::{
    collections::HashSet,
    hash::{BuildHasher, BuildHasherDefault},
    iter::repeat_with,
    sync::{
        atomic::{AtomicUsize, Ordering::SeqCst},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use rand::{
    distributions::{Alphanumeric, Distribution as _, Uniform},
    Rng,
};
use rand_distr::{WeightedAliasIndex, Zeta, Zipf};
use rustc_hash::FxHasher;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Op {
    Read(String),
    Scan(String, usize), // field index, max count
    Update(String, usize, String),
    Insert(String, Vec<String>),
    Delete(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Result {
    ReadOk(Vec<String>),
    NotFound,
    ScanOk(Vec<Vec<String>>),
    Ok,
}

#[derive(Debug, Clone)]
pub struct WorkloadSettings {
    pub record_count: usize,
    pub operation_count: Option<usize>,
    pub field_length: usize,
    pub field_length_distr: SettingsDistr,
    pub field_count: usize,
    pub read_proportion: f32,
    pub update_proportion: f32,
    pub insert_proportion: f32,
    pub read_modify_write_proportion: f32,
    pub scan_proportion: f32,
    pub max_scan_length: usize,
    pub scan_length_distr: SettingsDistr,
    pub request_distr: SettingsDistr,
    pub zero_padding: usize,
}

#[derive(Debug, Clone, Copy)]
pub enum SettingsDistr {
    Constant,
    Uniform,
    Zipfian,
    Latest,
}

impl WorkloadSettings {
    pub fn new(record_count: usize) -> Self {
        Self {
            record_count,
            operation_count: None,
            field_length: 100,
            field_count: 10,
            field_length_distr: SettingsDistr::Constant,
            read_proportion: 0.95,
            update_proportion: 0.05,
            insert_proportion: 0.,
            read_modify_write_proportion: 0.,
            scan_proportion: 0.,
            max_scan_length: 1000,
            scan_length_distr: SettingsDistr::Uniform,
            request_distr: SettingsDistr::Uniform,
            zero_padding: 20,
        }
    }

    pub fn new_a(record_count: usize) -> Self {
        Self {
            read_proportion: 0.5,
            update_proportion: 0.5,
            scan_proportion: 0.,
            insert_proportion: 0.,
            request_distr: SettingsDistr::Zipfian,
            ..Self::new(record_count)
        }
    }

    pub fn new_b(record_count: usize) -> Self {
        Self {
            read_proportion: 0.95,
            update_proportion: 0.05,
            scan_proportion: 0.,
            insert_proportion: 0.,
            request_distr: SettingsDistr::Zipfian,
            ..Self::new(record_count)
        }
    }

    pub fn new_c(record_count: usize) -> Self {
        Self {
            read_proportion: 1.,
            update_proportion: 0.,
            scan_proportion: 0.,
            insert_proportion: 0.,
            request_distr: SettingsDistr::Zipfian,
            ..Self::new(record_count)
        }
    }

    pub fn new_d(record_count: usize) -> Self {
        Self {
            read_proportion: 0.95,
            update_proportion: 0.,
            scan_proportion: 0.,
            insert_proportion: 0.05,
            request_distr: SettingsDistr::Latest,
            ..Self::new(record_count)
        }
    }

    pub fn new_e(record_count: usize) -> Self {
        Self {
            read_proportion: 0.,
            update_proportion: 0.,
            scan_proportion: 0.95,
            insert_proportion: 0.05,
            request_distr: SettingsDistr::Zipfian,
            max_scan_length: 100,
            scan_length_distr: SettingsDistr::Uniform,
            ..Self::new(record_count)
        }
    }

    pub fn new_f(record_count: usize) -> Self {
        Self {
            read_proportion: 0.5,
            update_proportion: 0.,
            scan_proportion: 0.,
            insert_proportion: 0.,
            read_modify_write_proportion: 0.5,
            request_distr: SettingsDistr::Zipfian,
            ..Self::new(record_count)
        }
    }
}

#[derive(Clone, derive_more::AsMut)]
pub struct Workload<R> {
    rng: R,
    settings: WorkloadSettings,

    insert_key_num: usize,            // `keysequence`
    insert_shared: Arc<InsertShared>, // `transactioninsertkeysequence`

    field_length: Gen,
    pub key_num: Gen,
    scan_len: Gen,
    transaction: WeightedAliasIndex<f32>,

    transaction_count: usize,
    rmw_update: Option<Op>,
    #[as_mut]
    pub latencies: Vec<Duration>,
    start: Option<Instant>,
}

struct InsertShared {
    next_num: AtomicUsize,
    // possibly feasible to implement with AtomicUsize as well, but too hard for me
    progress: Mutex<(usize, HashSet<usize>)>,
}

impl InsertShared {
    fn alloc(&self) -> usize {
        self.next_num.fetch_add(1, SeqCst)
    }

    fn first_unacked(&self) -> usize {
        self.progress.lock().unwrap().0
    }

    fn ack(&self, n: usize) {
        let (first_unacked, acked) = &mut *self.progress.lock().unwrap();
        assert!(n >= *first_unacked);
        if n != *first_unacked {
            let inserted = acked.insert(n);
            assert!(inserted);
            return;
        }
        while {
            *first_unacked += 1;
            acked.remove(first_unacked)
        } {}
    }
}

#[derive(Clone)]
pub enum Gen {
    Constant(usize),
    Uniform(Uniform<usize>),
    ScrambledZipf(GenScrambledZipf),
    Zipf(Zipf<f32>),
}

#[derive(Clone)]
pub struct GenScrambledZipf {
    min: usize,
    item_count: usize,
    zeta: Zeta<f32>,
}

impl Gen {
    fn new(distr: SettingsDistr, n: usize, scrambled: bool) -> anyhow::Result<Self> {
        Ok(match distr {
            SettingsDistr::Constant => Self::Constant(n),
            SettingsDistr::Uniform => Self::Uniform(Uniform::new(0, n)),
            SettingsDistr::Zipfian if scrambled => {
                Self::ScrambledZipf(GenScrambledZipf {
                    min: 0, // only for key chooser
                    item_count: n,
                    // according to https://stackoverflow.com/a/41448684 upstream's
                    // `ZipfianGenerator` effectively set this `a` parameter to 100
                    // however during testing, that results in only `min` i.e. 1 is ever yielded
                    // that means only single key will every be accessed, which hardly be expected
                    // while tuning the parameter, i realize the absolute number of different values
                    // that will be yielded is controlled solely by this parameter, not by e.g. `n`
                    // yet to investigate into this, but suspect the hotspot distribution is to
                    // solve this issue
                    zeta: Zeta::new(6.)?,
                })
            }
            SettingsDistr::Zipfian => Self::Zipf(Zipf::new(n as _, 6.)?),
            SettingsDistr::Latest => anyhow::bail!("unimplemented"),
        })
    }

    fn gen(&self, rng: &mut impl Rng) -> usize {
        match self {
            Self::Constant(n) => *n,
            Self::Uniform(uniform) => uniform.sample(rng),
            Self::ScrambledZipf(gen) => gen.gen(rng),
            Self::Zipf(zipf) => zipf.sample(rng) as _,
        }
    }
}

impl GenScrambledZipf {
    fn gen(&self, rng: &mut impl Rng) -> usize {
        let mut r;
        while {
            r = self.zeta.sample(rng);
            r > u64::MAX as f32
        } {}
        // println!("{r}");
        self.min
            + BuildHasherDefault::<FxHasher>::default().hash_one(r as u64) as usize
                % self.item_count
    }
}

#[derive(Clone, Copy)]
enum Transaction {
    Read,
    Update,
    Insert,
    Scan,
    ReadModifyWrite,
}

impl<R> Workload<R> {
    pub fn new(rng: R, settings: WorkloadSettings) -> anyhow::Result<Self> {
        Ok(Self {
            rng,
            insert_key_num: 0,
            insert_shared: Arc::new(InsertShared {
                next_num: AtomicUsize::new(settings.record_count),
                progress: Mutex::new((settings.record_count, Default::default())),
            }),
            field_length: Gen::new(settings.field_length_distr, settings.field_length, false)?,
            key_num: if matches!(settings.request_distr, SettingsDistr::Latest) {
                Gen::new(SettingsDistr::Zipfian, settings.record_count, false)
            } else {
                Gen::new(settings.request_distr, settings.record_count, true)
            }?,
            scan_len: Gen::new(settings.scan_length_distr, settings.max_scan_length, false)?,
            transaction: WeightedAliasIndex::new(vec![
                settings.read_proportion,
                settings.update_proportion,
                settings.insert_proportion,
                settings.scan_proportion,
                settings.read_modify_write_proportion,
            ])?,
            settings,
            transaction_count: 0,
            rmw_update: None,
            latencies: Default::default(),
            start: None,
        })
    }

    const TRANSACTIONS: [Transaction; 5] = [
        Transaction::Read,
        Transaction::Update,
        Transaction::Insert,
        Transaction::Scan,
        Transaction::ReadModifyWrite,
    ];

    fn build_key_name(&self, key_num: usize) -> String {
        let key = BuildHasherDefault::<FxHasher>::default()
            .hash_one(key_num)
            .to_string();
        let mut pre_key = String::from("user");
        for _ in 0..self.settings.zero_padding - key.len() {
            pre_key += "0"
        }
        pre_key + &key
    }
}

impl<R: Clone> Workload<R> {
    pub fn clone_reseed(&self, rng: R) -> Self {
        let mut workload = self.clone();
        workload.rng = rng;
        workload
    }
}

impl<R: Rng> Workload<R> {
    fn build_value(&mut self) -> String {
        let field_len = self.field_length.gen(&mut self.rng);
        assert_ne!(field_len, 0);
        repeat_with(|| char::from(Alphanumeric.sample(&mut self.rng)))
            .take(field_len)
            .collect()
    }

    fn startup_insert(&mut self) -> Op {
        let key = self.build_key_name(self.insert_key_num);
        self.insert_key_num += 1;
        let value = (0..self.settings.field_count)
            .map(|_| self.build_value())
            .collect();
        Op::Insert(key, value)
    }

    pub fn startup_ops(&mut self) -> impl Iterator<Item = Op> + '_ {
        let record_count = self.settings.record_count;
        repeat_with(|| self.startup_insert()).take(record_count)
    }

    fn key_num(&mut self) -> usize {
        if matches!(self.settings.request_distr, SettingsDistr::Latest) {
            return self.insert_shared.first_unacked() - self.key_num.gen(&mut self.rng);
        }
        // probably never reiterate after the simplification i made
        let mut key_num;
        while {
            key_num = self.key_num.gen(&mut self.rng);
            key_num >= self.insert_shared.first_unacked()
        } {}
        key_num
    }
}

impl<R: Rng> crate::workload::Workload for Workload<R> {
    type Op = Op;
    type Result = Result;
    type OpContext = Option<usize>;

    fn next_op(&mut self) -> anyhow::Result<Option<(Self::Op, Self::OpContext)>> {
        let mut key_num = 0;
        let op = 'op: {
            if let Some(op) = self.rmw_update.take() {
                break 'op Some(op);
            }
            if Some(self.transaction_count) == self.settings.operation_count {
                break 'op None;
            }
            let transaction = Self::TRANSACTIONS[self.transaction.sample(&mut self.rng)];
            let field = if !matches!(transaction, Transaction::Insert | Transaction::Read) {
                self.rng.gen_range(0..self.settings.field_count)
            } else {
                0
            };
            key_num = if matches!(transaction, Transaction::Insert) {
                self.insert_shared.alloc()
            } else {
                self.key_num()
            };
            let key_name = self.build_key_name(key_num);
            self.start = Some(Instant::now());
            Some(match transaction {
                Transaction::Read => Op::Read(key_name),
                Transaction::Update => Op::Update(key_name, field, self.build_value()),
                Transaction::Insert => Op::Insert(
                    key_name,
                    vec![self.build_value(); self.settings.field_count],
                ),
                Transaction::Scan => Op::Scan(key_name, self.scan_len.gen(&mut self.rng)),
                Transaction::ReadModifyWrite => {
                    let op = Op::Read(key_name.clone());
                    let value = self.build_value();
                    self.rmw_update = Some(Op::Update(key_name, field, value));
                    op
                }
            })
        };
        Ok(if let Some(op) = op {
            let attach = if matches!(op, Op::Insert(..)) {
                Some(key_num)
            } else {
                None
            };
            Some((op, attach))
        } else {
            None
        })
    }

    fn on_result(&mut self, result: Self::Result, key_num: Self::OpContext) -> anyhow::Result<()> {
        anyhow::ensure!(!matches!(result, Result::NotFound), "unexpected NotFound");
        if let Some(key_num) = key_num {
            self.insert_shared.ack(key_num)
        }
        if self.rmw_update.is_none() {
            let Some(start) = self.start.take() else {
                anyhow::bail!("missing start instant")
            };
            self.latencies.push(start.elapsed())
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use rand::thread_rng;

    use crate::app::{App, BTreeMap};

    use super::*;

    #[test]
    fn startup() -> anyhow::Result<()> {
        let mut app = BTreeMap::new();
        let mut workload = Workload::new(thread_rng(), WorkloadSettings::new(100))?;
        for op in workload.startup_ops() {
            app.execute(&serde_json::to_vec(&op)?)?;
        }
        assert_eq!(app.0.len(), 100);
        Ok(())
    }

    #[test]
    fn zipf() -> anyhow::Result<()> {
        let mut settings = WorkloadSettings::new(10_000);
        settings.request_distr = SettingsDistr::Zipfian;
        let mut workload = Workload::new(thread_rng(), settings)?;
        let mut counts = HashMap::<_, usize>::new();
        for _ in 0..1_000_000 {
            *counts.entry(workload.key_num()).or_default() += 1
        }
        let mut counts = counts.into_iter().collect::<Vec<_>>();
        counts.sort_unstable_by_key(|(_, n)| *n);
        assert!(counts.last().unwrap().1 > 1_000_000 / 100 * 95);
        Ok(())
    }
}

// cSpell:words zipf zipfian ycsb hasher rustc nonoverlapping hotspot
// cSpell:ignore insertstart insertcount recordcount operationcount keysequence
// cSpell:ignore transactioninsertkeysequence
