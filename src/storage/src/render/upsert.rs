// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::any::Any;
use std::cell::RefCell;
use std::cmp::Reverse;
use std::convert::AsRef;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use differential_dataflow::consolidation;
use differential_dataflow::hashable::Hashable;
use differential_dataflow::{AsCollection, Collection};
use itertools::Itertools;
use mz_ore::cast::CastFrom;
use mz_ore::collections::CollectionExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use timely::dataflow::channels::pact::Exchange;
use timely::dataflow::Scope;
use timely::order::{PartialOrder, TotalOrder};
use timely::progress::{Antichain, Timestamp};

use crate::source::types::UpsertMetrics;
use mz_repr::{Datum, DatumVec, Diff, Row};
use mz_storage_client::types::errors::{DataflowError, EnvelopeError, UpsertError};
use mz_storage_client::types::instances::StorageInstanceContext;
use mz_storage_client::types::sources::UpsertEnvelope;
use mz_timely_util::builder_async::{Event as AsyncEvent, OperatorBuilder as AsyncOperatorBuilder};

use self::types::{InMemoryHashMap, StatsState, UpsertState};

mod rocksdb;
mod types;

pub type UpsertValue = Result<Row, UpsertError>;

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct UpsertKey([u8; 32]);

impl AsRef<[u8]> for UpsertKey {
    #[inline(always)]
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

thread_local! {
    /// A thread-local datum cache used to calculate hashes
    pub static KEY_DATUMS: RefCell<DatumVec> = RefCell::new(DatumVec::new());
}

/// The hash function used to map upsert keys. It is important that this hash is a cryptographic
/// hash so that there is no risk of collisions. Collisions on SHA256 have a probability of 2^128
/// which is many orders of magnitude smaller than many other events that we don't even think about
/// (e.g bit flips). In short, we can safely assume that sha256(a) == sha256(b) iff a == b.
type KeyHash = Sha256;

impl UpsertKey {
    pub fn from_key(key: Result<&Row, &UpsertError>) -> Self {
        Self::from_iter(key.map(|r| r.iter()))
    }

    pub fn from_value(value: Result<&Row, &UpsertError>, mut key_indices: &[usize]) -> Self {
        Self::from_iter(value.map(|value| {
            value.iter().enumerate().flat_map(move |(idx, datum)| {
                let key_idx = key_indices.get(0)?;
                if idx == *key_idx {
                    key_indices = &key_indices[1..];
                    Some(datum)
                } else {
                    None
                }
            })
        }))
    }

    pub fn from_iter<'a, 'b>(
        key: Result<impl Iterator<Item = Datum<'a>> + 'b, &UpsertError>,
    ) -> Self {
        KEY_DATUMS.with(|key_datums| {
            let mut key_datums = key_datums.borrow_mut();
            // Borrowing the DatumVec gives us a temporary buffer to store datums in that will be
            // automatically cleared on Drop. See the DatumVec docs for more details.
            let mut key_datums = key_datums.borrow();
            let key: Result<&[Datum], Datum> = match key {
                Ok(key) => {
                    for datum in key {
                        key_datums.push(datum);
                    }
                    Ok(&*key_datums)
                }
                Err(UpsertError::Value(err)) => {
                    key_datums.extend(err.for_key.iter());
                    Ok(&*key_datums)
                }
                Err(UpsertError::KeyDecode(err)) => Err(Datum::Bytes(&err.raw)),
                Err(UpsertError::NullKey(_)) => Err(Datum::Null),
            };
            let mut hasher = DigestHasher(KeyHash::new());
            key.hash(&mut hasher);
            Self(hasher.0.finalize().into())
        })
    }
}

struct DigestHasher<H: Digest>(H);

impl<H: Digest> Hasher for DigestHasher<H> {
    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    fn finish(&self) -> u64 {
        panic!("digest wrapper used to produce a hash");
    }
}

/// Resumes an upsert computation at `resume_upper` given as inputs a collection of upsert commands
/// and the collection of the previous output of this operator.
pub(crate) fn upsert<G: Scope, O: timely::ExchangeData + Ord>(
    input: &Collection<G, (UpsertKey, Option<UpsertValue>, O), Diff>,
    upsert_envelope: UpsertEnvelope,
    resume_upper: Antichain<G::Timestamp>,
    previous: Collection<G, Result<Row, DataflowError>, Diff>,
    previous_token: Option<Rc<dyn Any>>,
    source_config: crate::source::RawSourceCreationConfig,
    instance_context: &StorageInstanceContext,
) -> Collection<G, Result<Row, DataflowError>, Diff>
where
    G::Timestamp: TotalOrder,
{
    let upsert_metrics = UpsertMetrics::new(
        &source_config.base_metrics,
        source_config.id,
        source_config.worker_id,
    );

    if upsert_envelope.disk {
        tracing::info!(
            "timely-{} rendering {} with rocksdb-backed upsert state",
            source_config.worker_id,
            source_config.id
        );
        let rocksdb_metrics = Arc::clone(&upsert_metrics.rocksdb);
        let rocksdb_dir = instance_context
            .scratch_directory
            .as_ref()
            .expect("instance directory to be there if rendering an ON DISK source")
            .join(source_config.id.to_string())
            .join(source_config.worker_id.to_string());
        upsert_inner(
            input,
            upsert_envelope.key_indices,
            resume_upper,
            previous,
            previous_token,
            upsert_metrics,
            move || async move {
                rocksdb::RocksDB::new(
                    mz_rocksdb::RocksDBInstance::new(
                        &rocksdb_dir,
                        mz_rocksdb::Options::new_with_defaults().unwrap(),
                        rocksdb_metrics,
                    )
                    .await
                    .unwrap(),
                )
            },
        )
    } else {
        tracing::info!(
            "timely-{} rendering {} with memory-backed upsert state",
            source_config.worker_id,
            source_config.id
        );
        upsert_inner(
            input,
            upsert_envelope.key_indices,
            resume_upper,
            previous,
            previous_token,
            upsert_metrics,
            || async { InMemoryHashMap::default() },
        )
    }
}

fn upsert_inner<G: Scope, O: timely::ExchangeData + Ord, F, Fut, US>(
    input: &Collection<G, (UpsertKey, Option<UpsertValue>, O), Diff>,
    mut key_indices: Vec<usize>,
    resume_upper: Antichain<G::Timestamp>,
    previous: Collection<G, Result<Row, DataflowError>, Diff>,
    previous_token: Option<Rc<dyn Any>>,
    upsert_metrics: UpsertMetrics,
    state: F,
) -> Collection<G, Result<Row, DataflowError>, Diff>
where
    G::Timestamp: TotalOrder,
    F: FnOnce() -> Fut + 'static,
    Fut: std::future::Future<Output = US>,
    US: UpsertState,
{
    // Sort key indices to ensure we can construct the key by iterating over the datums of the row
    key_indices.sort_unstable();

    let mut builder = AsyncOperatorBuilder::new("Upsert".to_string(), input.scope());

    let mut input = builder.new_input(
        &input.inner,
        Exchange::new(move |((key, _, _), _, _)| UpsertKey::hashed(key)),
    );

    // We only care about UpsertValueError since this is the only error that we can retract
    let previous = previous.flat_map(move |result| {
        let value = match result {
            Ok(ok) => Ok(ok),
            Err(DataflowError::EnvelopeError(err)) => match *err {
                EnvelopeError::Upsert(err) => Err(err),
                _ => return None,
            },
            Err(_) => return None,
        };
        Some((UpsertKey::from_value(value.as_ref(), &key_indices), value))
    });
    let mut previous = builder.new_input(
        &previous.inner,
        Exchange::new(|((key, _), _, _)| UpsertKey::hashed(key)),
    );
    let (mut output_handle, output) = builder.new_output();

    let upsert_shared_metrics = Arc::clone(&upsert_metrics.shared);
    builder.build(move |caps| async move {
        let mut output_cap = caps.into_element();

        let mut snapshot = vec![];

        // In the first phase we will collect all the updates from our output that are not beyond
        // resume_upper. This will be the seed state for the command processing below.
        while let Some(event) = previous.next_mut().await {
            match event {
                AsyncEvent::Data(_cap, data) => {
                    snapshot.extend(
                        data.drain(..)
                            .filter(|(_row, ts, _diff)| !resume_upper.less_equal(ts))
                            .map(|(row, _ts, diff)| (row, diff)),
                    );
                }
                AsyncEvent::Progress(upper) => {
                    if PartialOrder::less_equal(&resume_upper, &upper) {
                        break;
                    }
                }
            }
        }
        drop(previous_token);
        while let Some(_event) = previous.next().await {
            // Exchaust the previous input. It is expected to immediately reach the empty
            // antichain since we have dropped its token.
        }

        consolidation::consolidate(&mut snapshot);

        // The main key->value used to store previous values.
        let mut state = StatsState::new(state().await, upsert_shared_metrics);

        // A re-usable buffer of changes, per key. This is
        // an `IndexMap` because it has to be `drain`-able
        // and have a consistent iteration order.
        let mut commands_state = indexmap::IndexMap::new();
        let mut multi_get_scratch = Vec::new();

        // Rehydrate the upsert state (and bump some stats), even if the snapshot is empty.
        let snapshot = snapshot.into_iter().map(|((key, value), diff)| {
            assert_eq!(diff, 1, "invalid upsert state");
            (key, Some(value))
        });

        let now = Instant::now();
        let snapshot_size = snapshot.len();
        state
            .multi_put(snapshot)
            .await
            .expect("hashmap impl to not error");
        upsert_metrics
            .rehydration_latency
            .set(now.elapsed().as_secs_f64());
        upsert_metrics
            .rehydration_total
            .set(u64::cast_from(snapshot_size));

        // Now can can resume consuming the collection
        let mut stash = vec![];
        let mut output_updates = vec![];
        let mut input_upper = Antichain::from_elem(Timestamp::minimum());
        while let Some(event) = input.next_mut().await {
            match event {
                AsyncEvent::Data(_cap, data) => {
                    if PartialOrder::less_equal(&input_upper, &resume_upper) {
                        data.retain(|(_, ts, _)| resume_upper.less_equal(ts));
                    }

                    stash.extend(data.drain(..).map(|((key, value, order), time, diff)| {
                        assert!(diff > 0, "invalid upsert input");
                        (time, key, Reverse(order), value)
                    }));
                }
                AsyncEvent::Progress(upper) => {
                    stash.sort_unstable();

                    // Find the prefix that we can emit
                    let idx = stash.partition_point(|(ts, _, _, _)| !upper.less_equal(ts));

                    // Read the previous values _per key_ out of `state`, recording it
                    // along with the value with the _latest timestamp for that key_.
                    commands_state.clear();
                    for (_, key, _, _) in stash.iter().take(idx) {
                        commands_state.entry(*key).or_insert(None);
                    }

                    // These iterators iterate in the same order because `commands_state`
                    // is an `IndexMap`.
                    multi_get_scratch.clear();
                    multi_get_scratch.extend(commands_state.iter().map(|(k, _)| *k));
                    state
                        .multi_get(
                            multi_get_scratch.drain(..),
                            commands_state.iter_mut().map(|(_, v)| v),
                        )
                        .await
                        .expect("hashmap impl to not fail");

                    // From the prefix that can be emitted we can deduplicate based on (ts, key) in
                    // order to only process the command with the maximum order within the (ts,
                    // key) group. This is achieved by wrapping order in `Reverse(order)` above.
                    let mut commands = stash.drain(..idx).dedup_by(|a, b| {
                        let ((a_ts, a_key, _, _), (b_ts, b_key, _, _)) = (a, b);
                        a_ts == b_ts && a_key == b_key
                    });

                    // Upsert the values into `commands_state`, by recording the latest
                    // value (or deletion). These will be synced at the end to the `state`.
                    while let Some((ts, key, _, value)) = commands.next() {
                        let command_state = commands_state
                            .get_mut(&key)
                            .expect("key missing from commands_state");
                        match value {
                            Some(value) => {
                                if let Some(old_value) = command_state.replace(value.clone()) {
                                    output_updates.push((old_value, ts.clone(), -1));
                                }
                                output_updates.push((value, ts, 1));
                            }
                            None => {
                                if let Some(old_value) = command_state.take() {
                                    output_updates.push((old_value, ts, -1));
                                }
                            }
                        }
                    }

                    // Record the changes in `state`.
                    state
                        .multi_put(commands_state.drain(..))
                        .await
                        .expect("hashmap impl to not fail");

                    // Emit the _consolidated_ changes to the output.
                    output_handle
                        .give_container(&output_cap, &mut output_updates)
                        .await;
                    if let Some(ts) = upper.as_option() {
                        output_cap.downgrade(ts);
                    }
                    input_upper = upper;
                }
            }
        }
    });

    output.as_collection().map(|result| match result {
        Ok(ok) => Ok(ok),
        Err(err) => Err(DataflowError::from(EnvelopeError::Upsert(err))),
    })
}
