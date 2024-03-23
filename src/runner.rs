use anyhow::Result;
use async_trait::async_trait;
use rand::prelude::*;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    select,
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{sleep_until, Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

use crate::report::IterReport;

#[derive(Copy, Clone, Debug)]
pub struct BenchOpts {
    pub start:       Instant,
    pub concurrency: u32,
    pub iterations:  Option<u64>,
    pub duration:    Option<Duration>,
    pub rate:        Option<u32>, // iterations per second
}

impl BenchOpts {
    pub fn endtime(&self) -> Option<Instant> {
        self.duration.map(|d| self.start + d)
    }
}

#[async_trait]
pub trait BenchSuite: Clone {
    type RunnerState;

    async fn init(&self) -> Result<()> {
        Ok(())
    }

    async fn state(&self) -> Result<Self::RunnerState>;
    async fn bench(&mut self, rstate: &Self::RunnerState, wstate: &mut WorkerState) -> Result<IterReport>;
}

#[derive(Clone)]
pub struct Runner<BS>
where
    BS: BenchSuite,
{
    suite:   BS,
    opts:    BenchOpts,
    res_tx:  mpsc::UnboundedSender<Result<IterReport>>,
    pause:   watch::Receiver<bool>,
    cancel:  CancellationToken,
    counter: Arc<AtomicU64>,
}

pub struct WorkerState {
    rng:        StdRng,
    worker_id:  u32,
    worker_seq: u64,
    global_seq: u64,
}

impl WorkerState {
    pub fn new(worker_id: u32) -> Self {
        Self {
            rng: StdRng::from_entropy(),
            worker_id,
            worker_seq: 0,
            global_seq: 0,
        }
    }
    pub fn global_seq(&self) -> u64 {
        self.global_seq
    }

    pub fn worker_seq(&self) -> u64 {
        self.worker_seq
    }

    pub fn worker_id(&self) -> u32 {
        self.worker_id
    }

    pub fn rng(&mut self) -> &mut StdRng {
        &mut self.rng
    }
}

impl<BS> Runner<BS>
where
    BS: BenchSuite + Send + Sync + 'static,
    BS::RunnerState: Send + Sync + 'static,
{
    pub fn new(
        suite: BS,
        opts: BenchOpts,
        res_tx: mpsc::UnboundedSender<Result<IterReport>>,
        pause: watch::Receiver<bool>,
        cancel: CancellationToken,
    ) -> Self {
        Self { suite, opts, res_tx, pause, cancel, counter: Arc::default() }
    }

    async fn iteration(&mut self, rstate: &BS::RunnerState, wstate: &mut WorkerState) {
        self.wait_if_paused().await;
        let res = self.suite.bench(rstate, wstate).await;
        self.res_tx.send(res).expect("send report");
    }

    pub async fn run(self) -> Result<()> {
        self.suite.init().await?;

        match self.opts.rate {
            None => self.bench().await,
            Some(r) => self.bench_with_rate(r).await,
        }
    }

    /// Run the benchmark.
    async fn bench(self) -> Result<()> {
        let concurrency = self.opts.concurrency;
        let iterations = self.opts.iterations;
        let endtime = self.opts.endtime();

        let futures: Vec<JoinHandle<Result<()>>> = (0..concurrency)
            .map(|worker| {
                let mut b = self.clone();
                tokio::spawn(async move {
                    let rstate = b.suite.state().await?;
                    let mut wstate = WorkerState::new(worker);
                    let cancel = b.cancel.clone();

                    loop {
                        wstate.global_seq = b.counter.fetch_add(1, Ordering::Relaxed);
                        if let Some(iterations) = iterations {
                            if wstate.global_seq >= iterations {
                                break;
                            }
                        }
                        select! {
                            _ = cancel.cancelled() => break,
                            _ = b.iteration(&rstate, &mut wstate) => (),
                        }
                        wstate.worker_seq += 1;
                    }
                    Ok(())
                })
            })
            .collect();

        if let Some(endtime) = endtime {
            select! {
                _ = self.cancel.cancelled() => (),
                _ = sleep_until(endtime) => self.cancel.cancel(),
            }
        }

        for f in futures {
            f.await??;
        }

        Ok(())
    }

    /// Run the benchmark with a rate limit.
    async fn bench_with_rate(self, rate: u32) -> Result<()> {
        let concurrency = self.opts.concurrency;
        let iterations = self.opts.iterations;
        let endtime = self.opts.endtime();
        let (tx, rx) = flume::bounded(self.opts.concurrency as usize);

        let b = self.clone();
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_secs(1) / rate);
            timer.set_missed_tick_behavior(MissedTickBehavior::Burst);
            let mut iter = 0;
            loop {
                let t = timer.tick().await;
                if b.paused() {
                    continue;
                }
                if matches!(endtime, Some(endtime) if t >= endtime) {
                    break;
                }
                if matches!(iterations, Some(iterations) if iter >= iterations) {
                    break;
                }
                if tx.send_async(()).await.is_err() {
                    // receiver dropped
                    break;
                }
                iter += 1;
            }
        });

        let futures: Vec<JoinHandle<Result<()>>> = (0..concurrency)
            .map(|worker| {
                let mut b = self.clone();
                let rx = rx.clone();

                tokio::spawn(async move {
                    let rstate = b.suite.state().await?;
                    let mut wstate = WorkerState::new(worker);
                    let cancel = b.cancel.clone();

                    loop {
                        select! {
                            _ = cancel.cancelled() => break,
                            t = rx.recv_async() => match t {
                                Ok(_) => {
                                    wstate.global_seq = b.counter.fetch_add(1, Ordering::Relaxed);
                                    select! {
                                        _ = cancel.cancelled() => break,
                                        _ = b.iteration(&rstate, &mut wstate) => (),
                                    }
                                    wstate.worker_seq += 1;
                                }
                                Err(_) => break,
                            }
                        }
                    }
                    Ok(())
                })
            })
            .collect();

        for f in futures {
            f.await??;
        }

        Ok(())
    }

    fn paused(&self) -> bool {
        *self.pause.borrow()
    }

    async fn wait_if_paused(&mut self) {
        while self.paused() {
            if self.pause.changed().await.is_err() {
                return;
            }
        }
    }
}