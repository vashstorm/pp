use anyhow::{Result, anyhow};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
};

const UNAVAILABLE: &str = "Runtime input queue unavailable; restart pp.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingKind {
    Steering,
    FollowUp,
}

pub(crate) struct PendingInput {
    pub id: u64,
    pub kind: PendingKind,
    pub source: String,
    pub expansion: Option<String>,
    pub echo_source: bool,
}

impl PendingInput {
    pub(crate) fn prompt(&self) -> &str {
        self.expansion.as_deref().unwrap_or(&self.source)
    }

    pub(crate) fn into_display(self) -> String {
        if self.echo_source {
            self.source
        } else {
            self.expansion.unwrap_or(self.source)
        }
    }
}

pub(crate) struct PendingSummary {
    pub id: u64,
    pub kind: PendingKind,
    pub preview: String,
}

pub(crate) struct QueueSnapshot {
    pub revision: u64,
    pub items: Vec<PendingSummary>,
}

impl QueueSnapshot {
    pub(crate) fn steering(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.kind == PendingKind::Steering)
            .count()
    }

    pub(crate) fn follow_up(&self) -> usize {
        self.items.len() - self.steering()
    }
}

#[derive(Default)]
struct QueueState {
    inputs: VecDeque<PendingInput>,
    last_id: u64,
    steering_open: bool,
    revision: u64,
}

impl QueueState {
    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

#[derive(Clone, Default)]
pub(crate) struct InputQueue {
    state: Arc<Mutex<QueueState>>,
}

impl InputQueue {
    fn lock(&self) -> Result<MutexGuard<'_, QueueState>> {
        self.state.lock().map_err(|_| anyhow!(UNAVAILABLE))
    }

    pub(crate) fn enqueue(
        &self,
        kind: PendingKind,
        source: String,
        expansion: Option<String>,
        echo_source: bool,
    ) -> Result<u64> {
        let mut state = self.lock()?;
        let id = state
            .last_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("Runtime input queue ID limit reached."))?;
        state.last_id = id;
        state.inputs.push_back(PendingInput {
            id,
            kind,
            source,
            expansion,
            echo_source,
        });
        state.changed();
        Ok(id)
    }

    pub(crate) fn begin_run(&self) -> Result<()> {
        self.set_open(true)
    }

    pub(crate) fn close(&self) -> Result<()> {
        self.set_open(false)
    }

    fn set_open(&self, open: bool) -> Result<()> {
        let mut state = self.lock()?;
        if state.steering_open != open {
            state.steering_open = open;
            state.changed();
        }
        Ok(())
    }

    pub(crate) fn claim_steering(&self, finish_if_empty: bool) -> Result<VecDeque<PendingInput>> {
        let mut state = self.lock()?;
        let mut batch = VecDeque::new();
        if !state.steering_open {
            return Ok(batch);
        }
        for _ in 0..state.inputs.len() {
            let input = state
                .inputs
                .pop_front()
                .expect("original length is bounded");
            if input.kind == PendingKind::Steering {
                batch.push_back(input);
            } else {
                state.inputs.push_back(input);
            }
        }
        if !batch.is_empty() {
            state.changed();
        } else if finish_if_empty {
            state.steering_open = false;
            state.changed();
        }
        Ok(batch)
    }

    pub(crate) fn claim_next(&self) -> Result<Option<PendingInput>> {
        let mut state = self.lock()?;
        let index = state
            .inputs
            .iter()
            .position(|input| input.kind == PendingKind::Steering)
            .or_else(|| (!state.inputs.is_empty()).then_some(0));
        let input = index.and_then(|index| state.inputs.remove(index));
        if input.is_some() {
            state.changed();
        }
        Ok(input)
    }

    pub(crate) fn restore_front(&self, mut inputs: VecDeque<PendingInput>) -> Result<()> {
        if inputs.is_empty() {
            return Ok(());
        }
        let mut state = self.lock()?;
        inputs.append(&mut state.inputs);
        state.inputs = inputs;
        state.changed();
        Ok(())
    }

    pub(crate) fn recall(&self, id: u64) -> Result<Option<PendingInput>> {
        let mut state = self.lock()?;
        let input = state
            .inputs
            .iter()
            .position(|input| input.id == id)
            .and_then(|index| state.inputs.remove(index));
        if input.is_some() {
            state.changed();
        }
        Ok(input)
    }

    pub(crate) fn snapshot_since(&self, revision: Option<u64>) -> Result<Option<QueueSnapshot>> {
        let state = self.lock()?;
        if revision == Some(state.revision) {
            return Ok(None);
        }
        let items = state
            .inputs
            .iter()
            .map(|input| {
                let mut chars = input.source.lines().next().unwrap_or_default().chars();
                let mut preview: String = chars.by_ref().take(120).collect();
                if chars.next().is_some() {
                    preview.push('…');
                }
                PendingSummary {
                    id: input.id,
                    kind: input.kind,
                    preview,
                }
            })
            .collect();
        Ok(Some(QueueSnapshot {
            revision: state.revision,
            items,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Barrier, mpsc},
        thread,
    };

    fn enqueue(queue: &InputQueue, kind: PendingKind, text: &str) -> u64 {
        queue.enqueue(kind, text.into(), None, false).unwrap()
    }

    #[test]
    fn runtime_input_priority_and_restore_preserve_type_fifo() {
        let queue = InputQueue::default();
        queue.begin_run().unwrap();
        enqueue(&queue, PendingKind::FollowUp, "Q1");
        let s1 = enqueue(&queue, PendingKind::Steering, "S1");
        enqueue(&queue, PendingKind::FollowUp, "Q2");
        let s2 = enqueue(&queue, PendingKind::Steering, "S2");
        let mut batch = queue.claim_steering(false).unwrap();
        assert_eq!(
            batch.iter().map(|input| input.id).collect::<Vec<_>>(),
            [s1, s2]
        );
        assert!(queue.recall(s1).unwrap().is_none());
        batch.pop_front(); // The first item was successfully applied.
        enqueue(&queue, PendingKind::Steering, "S3");
        queue.restore_front(batch).unwrap();
        for expected in ["S2", "S3", "Q1", "Q2"] {
            assert_eq!(queue.claim_next().unwrap().unwrap().source, expected);
        }
        assert!(queue.claim_next().unwrap().is_none());
    }

    #[test]
    fn runtime_input_final_boundary_arbitrates_both_orders() {
        for enqueue_first in [false, true] {
            let queue = InputQueue::default();
            queue.begin_run().unwrap();
            let worker = queue.clone();
            let barrier = Arc::new(Barrier::new(2));
            let worker_barrier = barrier.clone();
            let (enqueued_tx, enqueued_rx) = mpsc::channel();
            let (closed_tx, closed_rx) = mpsc::channel();
            let handle = thread::spawn(move || {
                worker_barrier.wait();
                if enqueue_first {
                    enqueued_rx.recv().unwrap();
                }
                let batch = worker.claim_steering(true).unwrap();
                closed_tx.send(()).unwrap();
                batch
            });
            barrier.wait();
            if !enqueue_first {
                closed_rx.recv().unwrap();
            }
            enqueue(&queue, PendingKind::Steering, "S");
            if enqueue_first {
                enqueued_tx.send(()).unwrap();
            }
            let mut batch = handle.join().unwrap();
            if enqueue_first {
                assert_eq!(batch.pop_front().unwrap().source, "S");
            } else {
                assert_eq!(queue.claim_next().unwrap().unwrap().source, "S");
            }
            assert!(batch.is_empty());
            assert!(queue.claim_next().unwrap().is_none());
        }
    }

    #[test]
    fn runtime_input_recall_wins_and_snapshot_is_bounded() {
        let queue = InputQueue::default();
        queue.begin_run().unwrap();
        let source = format!("{}\nsecond", "界".repeat(121));
        let id = enqueue(&queue, PendingKind::Steering, &source);
        let snapshot = queue.snapshot_since(None).unwrap().unwrap();
        assert_eq!(snapshot.items[0].preview, format!("{}…", "界".repeat(120)));
        assert_eq!((snapshot.steering(), snapshot.follow_up()), (1, 0));
        assert!(
            queue
                .snapshot_since(Some(snapshot.revision))
                .unwrap()
                .is_none()
        );
        assert_eq!(queue.recall(id).unwrap().unwrap().source, source);
        assert!(queue.claim_steering(true).unwrap().is_empty());
    }

    #[test]
    fn runtime_input_expansion_display_and_id_exhaustion() {
        let queue = InputQueue::default();
        let id = queue
            .enqueue(
                PendingKind::Steering,
                "/init".into(),
                Some("expanded".into()),
                true,
            )
            .unwrap();
        let input = queue.recall(id).unwrap().unwrap();
        assert_eq!(input.prompt(), "expanded");
        assert_eq!(input.into_display(), "/init");
        enqueue(&queue, PendingKind::FollowUp, "retained");
        queue.state.lock().unwrap().last_id = u64::MAX;
        assert!(
            queue
                .enqueue(PendingKind::Steering, "lost".into(), None, false)
                .is_err()
        );
        assert_eq!(queue.claim_next().unwrap().unwrap().source, "retained");
    }
}
