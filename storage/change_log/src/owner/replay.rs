use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, bail};
use quint_connect::{Driver, State, Step, quint_run, switch};
use serde::Deserialize;

use super::*;

const SERVERS: [&str; 2] = ["A", "B"];
const MAX_RESENDS: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
struct Write {
    server: String,
    slot: u64,
    epoch: u64,
    requests: Vec<String>,
    attempt: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
struct Reply {
    write: Write,
    succeeded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
struct Pending {
    write: Write,
    unknown: bool,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
struct Stored {
    epoch: u64,
    requests: Vec<String>,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Store {
    slots: BTreeMap<u64, Stored>,
    writes_in_flight: BTreeSet<Write>,
    replies: BTreeSet<Reply>,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "tag")]
enum ModelPhase {
    Down,
    Loading,
    Fencing,
    Ready,
    Stopped,
}

#[derive(Debug, Deserialize)]
struct ModelServer {
    phase: ModelPhase,
    epoch: u64,
    next: u64,
    view: Vec<String>,
    batch: Vec<String>,
    pending: BTreeSet<Pending>,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(from = "ModelServer")]
struct Server {
    phase: ModelPhase,
    epoch: u64,
    next: u64,
    view: BTreeSet<String>,
    view_length: u64,
    batch: Vec<String>,
    pending: BTreeSet<(u64, Vec<String>, bool)>,
}

impl From<ModelServer> for Server {
    fn from(model: ModelServer) -> Server {
        // Not compared after a stop: the model keeps the view of a stopped owner, but the owner drops it.
        if model.phase == ModelPhase::Stopped {
            return Server::empty(ModelPhase::Stopped, model.epoch);
        }
        Server {
            phase: model.phase,
            epoch: model.epoch,
            next: model.next,
            view_length: u64::try_from(model.view.len()).expect("a short view"),
            view: model.view.into_iter().collect(),
            batch: model.batch,
            pending: model
                .pending
                .into_iter()
                .map(|entry| (entry.write.slot, entry.write.requests, entry.unknown))
                .collect(),
        }
    }
}

impl Server {
    fn empty(phase: ModelPhase, epoch: u64) -> Server {
        Server {
            phase,
            epoch,
            next: 0,
            view: BTreeSet::new(),
            view_length: 0,
            batch: Vec::new(),
            pending: BTreeSet::new(),
        }
    }

    fn of(owner: &Owner) -> Server {
        let epoch = owner.epoch.0;
        match &owner.phase {
            Phase::Loading { .. } => Server::empty(ModelPhase::Loading, epoch),
            Phase::Fencing { view, fence, .. } => Server {
                next: fence.slot.0 + 1,
                pending: BTreeSet::from([pending(fence)]),
                ..Server::with_view(ModelPhase::Fencing, epoch, view)
            },
            Phase::Ready(ready) => Server {
                next: ready.next.0,
                batch: ready
                    .batch
                    .iter()
                    .map(|request| name(&request.request_id))
                    .collect(),
                pending: ready.in_flight.iter().flatten().map(pending).collect(),
                ..Server::with_view(ModelPhase::Ready, epoch, &ready.view)
            },
            Phase::Stopped => Server::empty(ModelPhase::Stopped, epoch),
        }
    }

    fn with_view(phase: ModelPhase, epoch: u64, view: &View) -> Server {
        Server {
            view: view.sequences.keys().map(name).collect(),
            view_length: view.next_sequence,
            ..Server::empty(phase, epoch)
        }
    }
}

fn pending(write: &super::Write) -> (u64, Vec<String>, bool) {
    (
        write.slot.0,
        write
            .commits
            .iter()
            .map(|commit| name(&commit.request_id))
            .collect(),
        write.awaiting == Awaiting::ReadBack,
    )
}

fn name(request_id: &RequestId) -> String {
    String::from_utf8(request_id.0.to_vec()).expect("the model names requests in UTF-8")
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
struct Clients {
    acknowledged: BTreeSet<String>,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
struct ModelState {
    #[serde(rename = "seqlog_replay::seqlog::store")]
    store: Store,
    #[serde(rename = "seqlog_replay::seqlog::servers")]
    servers: BTreeMap<String, Server>,
    #[serde(rename = "seqlog_replay::seqlog::grantedEpoch")]
    granted_epoch: u64,
    #[serde(rename = "seqlog_replay::seqlog::clients")]
    clients: Clients,
}

impl State<Replay> for ModelState {
    fn from_driver(replay: &Replay) -> anyhow::Result<ModelState> {
        Ok(ModelState {
            store: Store {
                slots: replay
                    .slots
                    .iter()
                    .map(|(slot, segment)| {
                        (
                            slot.0,
                            Stored {
                                epoch: segment.epoch.0,
                                requests: names(segment),
                            },
                        )
                    })
                    .collect(),
                writes_in_flight: replay.writes_in_flight.keys().cloned().collect(),
                replies: replay.replies.clone(),
            },
            servers: SERVERS
                .iter()
                .map(|server| {
                    let state = replay
                        .owners
                        .get(*server)
                        .map_or_else(|| Server::empty(ModelPhase::Down, 0), Server::of);
                    (server.to_string(), state)
                })
                .collect(),
            granted_epoch: replay.granted_epoch,
            clients: Clients {
                acknowledged: replay.acknowledged.clone(),
            },
        })
    }
}

fn sequence_index(sequence: u64) -> usize {
    usize::try_from(sequence).expect("a short log")
}

fn names(segment: &Segment) -> Vec<String> {
    segment
        .commits
        .iter()
        .map(|commit| name(&commit.request_id))
        .collect()
}

#[derive(Default)]
struct Replay {
    owners: BTreeMap<String, Owner>,
    granted_epoch: u64,
    slots: BTreeMap<Slot, Segment>,
    writes_in_flight: BTreeMap<Write, Segment>,
    replies: BTreeSet<Reply>,
    acknowledged: BTreeSet<String>,
    passed_on: BTreeMap<String, u64>,
}

impl Driver for Replay {
    type State = ModelState;

    #[expect(non_snake_case)]
    fn step(&mut self, step: &Step) -> anyhow::Result<()> {
        switch!(step {
            init => *self = Replay::default(),
            grant(server: String) => {
                self.granted_epoch += 1;
                self.owners.insert(server.clone(), Owner::new(Epoch(self.granted_epoch)));
                self.passed_on.insert(server, 0);
            },
            crash(server: String) => {
                self.owners.remove(&server);
                self.passed_on.remove(&server);
            },
            load(server: String) => {
                let slots = self.slots.clone();
                self.run(&server, Event::Loaded { slots })?
            },
            send => {},
            receive(server: String, request: String) => self.run(&server, Event::Request(Request {
                request_id: RequestId(request.into()),
                payload: Bytes::new(),
            }))?,
            flush(server: String) => self.run(&server, Event::Flush)?,
            apply(write: Write, replyLost: bool) => self.apply(write, replyLost)?,
            receiveReply(reply: Reply) => {
                self.replies.remove(&reply);
                let slot = Slot(reply.write.slot);
                let event = if reply.succeeded {
                    Event::PutSucceeded { slot }
                } else {
                    Event::PutFailed { slot, failure: PutFailure::AlreadyExists }
                };
                self.run(&reply.write.server, event)?
            },
            timeout(server: String, p: Pending) => self.run(&server, Event::PutFailed {
                slot: Slot(p.write.slot),
                failure: PutFailure::Unknown,
            })?,
            resolve(server: String, p: Pending) => {
                let slot = Slot(p.write.slot);
                let found = self.slots.get(&slot).cloned();
                self.run(&server, Event::ReadBack { slot, found })?
            },
        })
    }
}

impl Replay {
    fn run(&mut self, server: &str, event: Event) -> anyhow::Result<()> {
        let owner = self
            .owners
            .get_mut(server)
            .context("the model has an owner")?;
        for effect in owner.step(event) {
            match effect {
                // Not sent: the model resends at most MaxResends times and then waits for a timeout, as if the network lost the resend.
                Effect::Put { attempt, .. } if attempt > MAX_RESENDS => {}
                Effect::Put {
                    slot,
                    body,
                    attempt,
                } => {
                    let segment = Segment::decode(body).expect("the owner encodes its own body");
                    let write = Write {
                        server: server.to_string(),
                        slot: slot.0,
                        epoch: segment.epoch.0,
                        requests: names(&segment),
                        attempt,
                    };
                    self.writes_in_flight.insert(write, segment);
                }
                Effect::Committed(commit) => {
                    let logged = self.log().get(sequence_index(commit.sequence)) == Some(&&commit);
                    let passed_on = self
                        .passed_on
                        .get_mut(server)
                        .context("a granted owner counts its commits")?;
                    if commit.sequence != *passed_on || !logged {
                        bail!("{server} passed on {commit:?} after {passed_on} commits");
                    }
                    *passed_on += 1;
                }
                Effect::Ack {
                    request_id,
                    sequence,
                } => {
                    let logged = self
                        .log()
                        .get(sequence_index(sequence))
                        .map(|commit| &commit.request_id);
                    if logged != Some(&request_id) {
                        bail!("{server} acknowledged {request_id:?} as {sequence}");
                    }
                    self.acknowledged.insert(name(&request_id));
                }
                Effect::Get { .. } | Effect::Stop => {}
                Effect::Alarm(alarm) => bail!("the owner raised {alarm:?}"),
            }
        }
        Ok(())
    }

    fn log(&self) -> Vec<&Commit> {
        self.slots
            .iter()
            .zip(0..)
            .take_while(|((slot, _), expected)| slot.0 == *expected)
            .flat_map(|((_, segment), _)| &segment.commits)
            .collect()
    }

    fn apply(&mut self, write: Write, reply_lost: bool) -> anyhow::Result<()> {
        let segment = self
            .writes_in_flight
            .remove(&write)
            .context("the owner sent the write")?;
        let slot = Slot(write.slot);
        let succeeded = !self.slots.contains_key(&slot);
        if succeeded {
            self.slots.insert(slot, segment);
        }
        if !reply_lost {
            self.replies.insert(Reply { write, succeeded });
        }
        Ok(())
    }
}

#[quint_run(
    spec = "../quint/seqlog_replay.qnt",
    main = "seqlog_replay",
    step = "replayStep",
    max_samples = 300,
    max_steps = 60
)]
fn owner_follows_the_model() -> impl Driver {
    Replay::default()
}
