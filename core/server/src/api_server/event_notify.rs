use super::PriorityOpStatus;
use actix::FinishStream;
use futures::{sync::oneshot, Future, Stream};
use models::{node::block::ExecutedOperations, Action, ActionType, Operation};
use std::collections::BTreeMap;
use storage::{ConnectionPool, TxReceiptResponse};

const MAX_LISTENERS_PER_ENTITY: usize = 4096;

pub enum EventSubscribe {
    Transaction {
        hash: Box<[u8; 32]>,
        commit: bool, // commit of verify
        notify: oneshot::Sender<TxReceiptResponse>,
    },
    PriorityOp {
        serial_id: u64,
        commit: bool,
        notify: oneshot::Sender<PriorityOpStatus>,
    },
}

enum BlockNotifierInput {
    NewOperationCommited(Operation),
    EventSubscription(EventSubscribe),
}

struct OperationNotifier {
    db_pool: ConnectionPool,

    tx_commit_subs: BTreeMap<[u8; 32], Vec<oneshot::Sender<TxReceiptResponse>>>,
    prior_op_commit_subs: BTreeMap<u64, Vec<oneshot::Sender<PriorityOpStatus>>>,

    tx_verify_subs: BTreeMap<[u8; 32], Vec<oneshot::Sender<TxReceiptResponse>>>,
    prior_op_verify_subs: BTreeMap<u64, Vec<oneshot::Sender<PriorityOpStatus>>>,
}

impl OperationNotifier {
    fn run<S: Stream<Item = BlockNotifierInput, Error = ()>>(
        mut self,
        input_stream: S,
    ) -> impl Future<Item = (), Error = ()> {
        input_stream
            .map(move |input| match input {
                BlockNotifierInput::EventSubscription(sub) => self.handle_subscription(sub),
                BlockNotifierInput::NewOperationCommited(op) => self.handle_new_block(op),
            })
            .finish()
    }

    // TODO: remove sub after timeout.
    fn handle_subscription(&mut self, new_sub: EventSubscribe) {
        match new_sub {
            EventSubscribe::Transaction {
                hash,
                commit,
                notify,
            } => {
                // Maybe tx was executed already.
                if let Some(receipt) = self
                    .db_pool
                    .access_storage()
                    .ok()
                    .and_then(|s| s.tx_receipt(hash.as_ref()).ok().unwrap_or(None))
                {
                    if commit {
                        notify.send(receipt).unwrap_or_default();
                        return;
                    } else {
                        if receipt.verified {
                            notify.send(receipt).unwrap_or_default();
                            return;
                        }
                    }
                }

                if commit {
                    let mut listeners = self
                        .tx_commit_subs
                        .remove(hash.as_ref())
                        .unwrap_or_default();
                    if listeners.len() < MAX_LISTENERS_PER_ENTITY {
                        listeners.push(notify);
                    }
                    self.tx_commit_subs.insert(*hash, listeners);
                } else {
                    let mut listeners = self
                        .tx_verify_subs
                        .remove(hash.as_ref())
                        .unwrap_or_default();
                    if listeners.len() < MAX_LISTENERS_PER_ENTITY {
                        listeners.push(notify);
                    }
                    self.tx_verify_subs.insert(*hash, listeners);
                }
            }
            EventSubscribe::PriorityOp {
                serial_id,
                commit,
                notify,
            } => {
                let executed_op = self.db_pool.access_storage().ok().and_then(|s| {
                    s.get_executed_priority_op(serial_id as u32)
                        .ok()
                        .unwrap_or(None)
                });
                if let Some(executed_op) = executed_op {
                    let prior_op_status = PriorityOpStatus {
                        executed: true,
                        block: Some(executed_op.block_number),
                    };
                    if commit {
                        notify.send(prior_op_status).unwrap_or_default();
                        return;
                    } else {
                        if let Some(block_verify) =
                            self.db_pool.access_storage().ok().and_then(|s| {
                                s.load_stored_op_with_block_number(
                                    executed_op.block_number as u32,
                                    ActionType::VERIFY,
                                )
                            })
                        {
                            if block_verify.confirmed {
                                notify.send(prior_op_status).unwrap_or_default();
                                return;
                            }
                        }
                    }
                }

                if commit {
                    let mut listeners = self
                        .prior_op_commit_subs
                        .remove(&serial_id)
                        .unwrap_or_default();
                    if listeners.len() < MAX_LISTENERS_PER_ENTITY {
                        listeners.push(notify);
                    }
                    self.prior_op_commit_subs.insert(serial_id, listeners);
                } else {
                    let mut listeners = self
                        .prior_op_verify_subs
                        .remove(&serial_id)
                        .unwrap_or_default();
                    if listeners.len() < MAX_LISTENERS_PER_ENTITY {
                        listeners.push(notify);
                    }
                    self.prior_op_verify_subs.insert(serial_id, listeners);
                }
            }
        }
    }
    fn handle_new_block(&mut self, op: Operation) {
        let commit = match &op.action {
            Action::Commit => true,
            Action::Verify { .. } => false,
        };

        for tx in op.block.block_transactions {
            match tx {
                ExecutedOperations::Tx(tx) => {
                    let hash = tx.tx.hash();
                    let subs = if commit {
                        self.tx_commit_subs.remove(hash.as_ref())
                    } else {
                        self.tx_verify_subs.remove(hash.as_ref())
                    };
                    if let Some(channels) = subs {
                        let receipt = TxReceiptResponse {
                            tx_hash: hex::encode(hash.as_ref()),
                            block_number: op.block.block_number as i64,
                            success: tx.success,
                            fail_reason: tx.fail_reason,
                            verified: op.action.get_type() == ActionType::VERIFY,
                            prover_run: None,
                        };
                        for ch in channels {
                            ch.send(receipt.clone()).unwrap_or_default();
                        }
                    }
                }
                ExecutedOperations::PriorityOp(prior_op) => {
                    let id = prior_op.priority_op.serial_id;
                    let subs = if commit {
                        self.prior_op_commit_subs.remove(&id)
                    } else {
                        self.prior_op_verify_subs.remove(&id)
                    };

                    if let Some(channels) = subs {
                        let prior_op_status = PriorityOpStatus {
                            executed: true,
                            block: Some(op.block.block_number as i64),
                        };

                        for ch in channels {
                            ch.send(prior_op_status.clone()).unwrap_or_default();
                        }
                    }
                }
            }
        }
    }
}

pub fn start_sub_notifier<BStream, SStream>(
    db_pool: ConnectionPool,
    new_block_stream: BStream,
    subscription_stream: SStream,
) where
    BStream: Stream<Item = Operation, Error = ()> + 'static,
    SStream: Stream<Item = EventSubscribe, Error = ()> + 'static,
{
    let notifier = OperationNotifier {
        db_pool,
        tx_verify_subs: BTreeMap::new(),
        tx_commit_subs: BTreeMap::new(),
        prior_op_commit_subs: BTreeMap::new(),
        prior_op_verify_subs: BTreeMap::new(),
    };
    let input_stream = new_block_stream
        .map(BlockNotifierInput::NewOperationCommited)
        .select(subscription_stream.map(BlockNotifierInput::EventSubscription));
    actix::System::with_current(move |_| actix::spawn(notifier.run(input_stream)));
}