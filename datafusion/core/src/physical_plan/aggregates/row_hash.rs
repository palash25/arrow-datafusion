// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Hash aggregation

use datafusion_physical_expr::{
    AggregateExpr, EmitTo, GroupsAccumulator, GroupsAccumulatorAdapter,
};
use log::debug;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::vec;

use ahash::RandomState;
use arrow::row::{RowConverter, Rows, SortField};
use datafusion_physical_expr::hash_utils::create_hashes;
use futures::ready;
use futures::stream::{Stream, StreamExt};

use crate::physical_plan::aggregates::{
    evaluate_group_by, evaluate_many, evaluate_optional, group_schema, AggregateMode,
    PhysicalGroupBy,
};
use crate::physical_plan::metrics::{BaselineMetrics, RecordOutput};
use crate::physical_plan::{aggregates, PhysicalExpr};
use crate::physical_plan::{RecordBatchStream, SendableRecordBatchStream};
use arrow::array::*;
use arrow::{datatypes::SchemaRef, record_batch::RecordBatch};
use datafusion_common::Result;
use datafusion_execution::memory_pool::proxy::{RawTableAllocExt, VecAllocExt};
use datafusion_execution::memory_pool::{MemoryConsumer, MemoryReservation};
use datafusion_execution::TaskContext;
use hashbrown::raw::RawTable;

#[derive(Debug, Clone)]
/// This object tracks the aggregation phase (input/output)
pub(crate) enum ExecutionState {
    ReadingInput,
    /// When producing output, the remaining rows to output are stored
    /// here and are sliced off as needed in batch_size chunks
    ProducingOutput(RecordBatch),
    Done,
}

use super::order::GroupOrdering;
use super::AggregateExec;

/// An interning store for group keys
trait GroupValues: Send {
    /// Calculates the `groups` for each input row of `cols`
    fn intern(&mut self, cols: &[ArrayRef], groups: &mut Vec<usize>) -> Result<()>;

    /// Returns the number of bytes used by this [`GroupValues`]
    fn size(&self) -> usize;

    /// Returns true if this [`GroupValues`] is empty
    fn is_empty(&self) -> bool;

    /// The number of values stored in this [`GroupValues`]
    fn len(&self) -> usize;

    /// Emits the group values
    fn emit(&mut self, emit_to: EmitTo) -> Result<Vec<ArrayRef>>;
}

/// A [`GroupValues`] making use of [`Rows`]
struct GroupValuesRows {
    /// Converter for the group values
    row_converter: RowConverter,

    /// Logically maps group values to a group_index in
    /// [`Self::group_values`] and in each accumulator
    ///
    /// Uses the raw API of hashbrown to avoid actually storing the
    /// keys (group values) in the table
    ///
    /// keys: u64 hashes of the GroupValue
    /// values: (hash, group_index)
    map: RawTable<(u64, usize)>,

    /// The size of `map` in bytes
    map_size: usize,

    /// The actual group by values, stored in arrow [`Row`] format.
    /// `group_values[i]` holds the group value for group_index `i`.
    ///
    /// The row format is used to compare group keys quickly and store
    /// them efficiently in memory. Quick comparison is especially
    /// important for multi-column group keys.
    ///
    /// [`Row`]: arrow::row::Row
    group_values: Rows,

    // buffer to be reused to store hashes
    hashes_buffer: Vec<u64>,

    /// Random state for creating hashes
    random_state: RandomState,
}

impl GroupValuesRows {
    fn try_new(schema: SchemaRef) -> Result<Self> {
        let row_converter = RowConverter::new(
            schema
                .fields()
                .iter()
                .map(|f| SortField::new(f.data_type().clone()))
                .collect(),
        )?;

        let map = RawTable::with_capacity(0);
        let group_values = row_converter.empty_rows(0, 0);

        Ok(Self {
            row_converter,
            map,
            map_size: 0,
            group_values,
            hashes_buffer: Default::default(),
            random_state: Default::default(),
        })
    }
}

impl GroupValues for GroupValuesRows {
    fn intern(&mut self, cols: &[ArrayRef], groups: &mut Vec<usize>) -> Result<()> {
        // Convert the group keys into the row format
        // Avoid reallocation when https://github.com/apache/arrow-rs/issues/4479 is available
        let group_rows = self.row_converter.convert_columns(cols)?;
        let n_rows = group_rows.num_rows();

        // tracks to which group each of the input rows belongs
        groups.clear();

        // 1.1 Calculate the group keys for the group values
        let batch_hashes = &mut self.hashes_buffer;
        batch_hashes.clear();
        batch_hashes.resize(n_rows, 0);
        create_hashes(cols, &self.random_state, batch_hashes)?;

        for (row, &hash) in batch_hashes.iter().enumerate() {
            let entry = self.map.get_mut(hash, |(_hash, group_idx)| {
                // verify that a group that we are inserting with hash is
                // actually the same key value as the group in
                // existing_idx  (aka group_values @ row)
                group_rows.row(row) == self.group_values.row(*group_idx)
            });

            let group_idx = match entry {
                // Existing group_index for this group value
                Some((_hash, group_idx)) => *group_idx,
                //  1.2 Need to create new entry for the group
                None => {
                    // Add new entry to aggr_state and save newly created index
                    let group_idx = self.group_values.num_rows();
                    self.group_values.push(group_rows.row(row));

                    // for hasher function, use precomputed hash value
                    self.map.insert_accounted(
                        (hash, group_idx),
                        |(hash, _group_index)| *hash,
                        &mut self.map_size,
                    );
                    group_idx
                }
            };
            groups.push(group_idx);
        }

        Ok(())
    }

    fn size(&self) -> usize {
        self.row_converter.size()
            + self.group_values.size()
            + self.map_size
            + self.hashes_buffer.allocated_size()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn len(&self) -> usize {
        self.group_values.num_rows()
    }

    fn emit(&mut self, emit_to: EmitTo) -> Result<Vec<ArrayRef>> {
        Ok(match emit_to {
            EmitTo::All => {
                // Eventually we may also want to clear the hash table here
                self.row_converter.convert_rows(&self.group_values)?
            }
            EmitTo::First(n) => {
                let groups_rows = self.group_values.iter().take(n);
                let output = self.row_converter.convert_rows(groups_rows)?;
                // Clear out first n group keys by copying them to a new Rows.
                // TODO file some ticket in arrow-rs to make this more efficent?
                let mut new_group_values = self.row_converter.empty_rows(0, 0);
                for row in self.group_values.iter().skip(n) {
                    new_group_values.push(row);
                }
                std::mem::swap(&mut new_group_values, &mut self.group_values);

                // SAFETY: self.map outlives iterator and is not modified concurrently
                unsafe {
                    for bucket in self.map.iter() {
                        // Decrement group index by n
                        match bucket.as_ref().1.checked_sub(n) {
                            // Group index was >= n, shift value down
                            Some(sub) => bucket.as_mut().1 = sub,
                            // Group index was < n, so remove from table
                            None => self.map.erase(bucket),
                        }
                    }
                }
                output
            }
        })
    }
}

/// Hash based Grouping Aggregator
///
/// # Design Goals
///
/// This structure is designed so that updating the aggregates can be
/// vectorized (done in a tight loop) without allocations. The
/// accumulator state is *not* managed by this operator (e.g in the
/// hash table) and instead is delegated to the individual
/// accumulators which have type specialized inner loops that perform
/// the aggregation.
///
/// # Architecture
///
/// ```text
///
///     Assigns a consecutive group           internally stores aggregate values
///     index for each unique set                     for all groups
///         of group values
///
///         ┌────────────┐              ┌──────────────┐       ┌──────────────┐
///         │ ┌────────┐ │              │┌────────────┐│       │┌────────────┐│
///         │ │  "A"   │ │              ││accumulator ││       ││accumulator ││
///         │ ├────────┤ │              ││     0      ││       ││     N      ││
///         │ │  "Z"   │ │              ││ ┌────────┐ ││       ││ ┌────────┐ ││
///         │ └────────┘ │              ││ │ state  │ ││       ││ │ state  │ ││
///         │            │              ││ │┌─────┐ │ ││  ...  ││ │┌─────┐ │ ││
///         │    ...     │              ││ │├─────┤ │ ││       ││ │├─────┤ │ ││
///         │            │              ││ │└─────┘ │ ││       ││ │└─────┘ │ ││
///         │            │              ││ │        │ ││       ││ │        │ ││
///         │ ┌────────┐ │              ││ │  ...   │ ││       ││ │  ...   │ ││
///         │ │  "Q"   │ │              ││ │        │ ││       ││ │        │ ││
///         │ └────────┘ │              ││ │┌─────┐ │ ││       ││ │┌─────┐ │ ││
///         │            │              ││ │└─────┘ │ ││       ││ │└─────┘ │ ││
///         └────────────┘              ││ └────────┘ ││       ││ └────────┘ ││
///                                     │└────────────┘│       │└────────────┘│
///                                     └──────────────┘       └──────────────┘
///
///         group_values                             accumulators
///
///  ```
///
/// For example, given a query like `COUNT(x), SUM(y) ... GROUP BY z`,
/// [`group_values`] will store the distinct values of `z`. There will
/// be one accumulator for `COUNT(x)`, specialized for the data type
/// of `x` and one accumulator for `SUM(y)`, specialized for the data
/// type of `y`.
///
/// # Description
///
/// [`group_values`] does not store any aggregate state inline. It only
/// assigns "group indices", one for each (distinct) group value. The
/// accumulators manage the in-progress aggregate state for each
/// group, with the group values themselves are stored in
/// [`group_values`] at the corresponding group index.
///
/// The accumulator state (e.g partial sums) is managed by and stored
/// by a [`GroupsAccumulator`] accumulator. There is one accumulator
/// per aggregate expression (COUNT, AVG, etc) in the
/// stream. Internally, each `GroupsAccumulator` manages the state for
/// multiple groups, and is passed `group_indexes` during update. Note
/// The accumulator state is not managed by this operator (e.g in the
/// hash table).
///
/// [`group_values`]: Self::group_values
pub(crate) struct GroupedHashAggregateStream {
    schema: SchemaRef,
    input: SendableRecordBatchStream,
    mode: AggregateMode,

    /// Accumulators, one for each `AggregateExpr` in the query
    ///
    /// For example, if the query has aggregates, `SUM(x)`,
    /// `COUNT(y)`, there will be two accumulators, each one
    /// specialized for that particular aggregate and its input types
    accumulators: Vec<Box<dyn GroupsAccumulator>>,

    /// Arguments to pass to each accumulator.
    ///
    /// The arguments in `accumulator[i]` is passed `aggregate_arguments[i]`
    ///
    /// The argument to each accumulator is itself a `Vec` because
    /// some aggregates such as `CORR` can accept more than one
    /// argument.
    aggregate_arguments: Vec<Vec<Arc<dyn PhysicalExpr>>>,

    /// Optional filter expression to evaluate, one for each for
    /// accumulator. If present, only those rows for which the filter
    /// evaluate to true should be included in the aggregate results.
    ///
    /// For example, for an aggregate like `SUM(x FILTER x > 100)`,
    /// the filter expression is  `x > 100`.
    filter_expressions: Vec<Option<Arc<dyn PhysicalExpr>>>,

    /// GROUP BY expressions
    group_by: PhysicalGroupBy,

    /// The memory reservation for this grouping
    reservation: MemoryReservation,

    /// An interning store of group keys
    group_values: Box<dyn GroupValues>,

    /// scratch space for the current input [`RecordBatch`] being
    /// processed. Reused across batches here to avoid reallocations
    current_group_indices: Vec<usize>,

    /// Tracks if this stream is generating input or output
    exec_state: ExecutionState,

    /// Execution metrics
    baseline_metrics: BaselineMetrics,

    /// max rows in output RecordBatches
    batch_size: usize,

    /// Optional ordering information, that might allow groups to be
    /// emitted from the hash table prior to seeing the end of the
    /// input
    group_ordering: GroupOrdering,

    /// Have we seen the end of the input
    input_done: bool,
}

impl GroupedHashAggregateStream {
    /// Create a new GroupedHashAggregateStream
    pub fn new(
        agg: &AggregateExec,
        context: Arc<TaskContext>,
        partition: usize,
    ) -> Result<Self> {
        debug!("Creating GroupedHashAggregateStream");
        let agg_schema = Arc::clone(&agg.schema);
        let agg_group_by = agg.group_by.clone();
        let agg_filter_expr = agg.filter_expr.clone();

        let batch_size = context.session_config().batch_size();
        let input = agg.input.execute(partition, Arc::clone(&context))?;
        let baseline_metrics = BaselineMetrics::new(&agg.metrics, partition);

        let timer = baseline_metrics.elapsed_compute().timer();

        let aggregate_exprs = agg.aggr_expr.clone();

        // arguments for each aggregate, one vec of expressions per
        // aggregate
        let aggregate_arguments = aggregates::aggregate_expressions(
            &agg.aggr_expr,
            &agg.mode,
            agg_group_by.expr.len(),
        )?;

        let filter_expressions = match agg.mode {
            AggregateMode::Partial
            | AggregateMode::Single
            | AggregateMode::SinglePartitioned => agg_filter_expr,
            AggregateMode::Final | AggregateMode::FinalPartitioned => {
                vec![None; agg.aggr_expr.len()]
            }
        };

        // Instantiate the accumulators
        let accumulators: Vec<_> = aggregate_exprs
            .iter()
            .map(create_group_accumulator)
            .collect::<Result<_>>()?;

        let group_schema = group_schema(&agg_schema, agg_group_by.expr.len());

        let name = format!("GroupedHashAggregateStream[{partition}]");
        let reservation = MemoryConsumer::new(name).register(context.memory_pool());

        let group_ordering = agg
            .aggregation_ordering
            .as_ref()
            .map(|aggregation_ordering| {
                GroupOrdering::try_new(&group_schema, aggregation_ordering)
            })
            // return error if any
            .transpose()?
            .unwrap_or(GroupOrdering::None);

        let group = Box::new(GroupValuesRows::try_new(group_schema)?);

        timer.done();

        let exec_state = ExecutionState::ReadingInput;

        Ok(GroupedHashAggregateStream {
            schema: agg_schema,
            input,
            mode: agg.mode,
            accumulators,
            aggregate_arguments,
            filter_expressions,
            group_by: agg_group_by,
            reservation,
            group_values: group,
            current_group_indices: Default::default(),
            exec_state,
            baseline_metrics,
            batch_size,
            group_ordering,
            input_done: false,
        })
    }
}

/// Create an accumulator for `agg_expr` -- a [`GroupsAccumulator`] if
/// that is supported by the aggregate, or a
/// [`GroupsAccumulatorAdapter`] if not.
fn create_group_accumulator(
    agg_expr: &Arc<dyn AggregateExpr>,
) -> Result<Box<dyn GroupsAccumulator>> {
    if agg_expr.groups_accumulator_supported() {
        agg_expr.create_groups_accumulator()
    } else {
        // Note in the log when the slow path is used
        debug!(
            "Creating GroupsAccumulatorAdapter for {}: {agg_expr:?}",
            agg_expr.name()
        );
        let agg_expr_captured = agg_expr.clone();
        let factory = move || agg_expr_captured.create_accumulator();
        Ok(Box::new(GroupsAccumulatorAdapter::new(factory)))
    }
}

/// Extracts a successful Ok(_) or returns Poll::Ready(Some(Err(e))) with errors
macro_rules! extract_ok {
    ($RES: expr) => {{
        match $RES {
            Ok(v) => v,
            Err(e) => return Poll::Ready(Some(Err(e))),
        }
    }};
}

impl Stream for GroupedHashAggregateStream {
    type Item = Result<RecordBatch>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let elapsed_compute = self.baseline_metrics.elapsed_compute().clone();

        loop {
            let exec_state = self.exec_state.clone();
            match exec_state {
                ExecutionState::ReadingInput => {
                    match ready!(self.input.poll_next_unpin(cx)) {
                        // new batch to aggregate
                        Some(Ok(batch)) => {
                            let timer = elapsed_compute.timer();
                            // Do the grouping
                            extract_ok!(self.group_aggregate_batch(batch));

                            // If we can begin emitting rows, do so,
                            // otherwise keep consuming input
                            assert!(!self.input_done);

                            if let Some(to_emit) = self.group_ordering.emit_to() {
                                let batch = extract_ok!(self.emit(to_emit));
                                self.exec_state = ExecutionState::ProducingOutput(batch);
                            }
                            timer.done();
                        }
                        Some(Err(e)) => {
                            // inner had error, return to caller
                            return Poll::Ready(Some(Err(e)));
                        }
                        None => {
                            // inner is done, emit all rows and switch to producing output
                            self.input_done = true;
                            self.group_ordering.input_done();
                            let timer = elapsed_compute.timer();
                            let batch = extract_ok!(self.emit(EmitTo::All));
                            self.exec_state = ExecutionState::ProducingOutput(batch);
                            timer.done();
                        }
                    }
                }

                ExecutionState::ProducingOutput(batch) => {
                    // slice off a part of the batch, if needed
                    let output_batch = if batch.num_rows() <= self.batch_size {
                        if self.input_done {
                            self.exec_state = ExecutionState::Done;
                        } else {
                            self.exec_state = ExecutionState::ReadingInput
                        }
                        batch
                    } else {
                        // output first batch_size rows
                        let num_remaining = batch.num_rows() - self.batch_size;
                        let remaining = batch.slice(self.batch_size, num_remaining);
                        self.exec_state = ExecutionState::ProducingOutput(remaining);
                        batch.slice(0, self.batch_size)
                    };
                    return Poll::Ready(Some(Ok(
                        output_batch.record_output(&self.baseline_metrics)
                    )));
                }

                ExecutionState::Done => return Poll::Ready(None),
            }
        }
    }
}

impl RecordBatchStream for GroupedHashAggregateStream {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

impl GroupedHashAggregateStream {
    /// Perform group-by aggregation for the given [`RecordBatch`].
    fn group_aggregate_batch(&mut self, batch: RecordBatch) -> Result<()> {
        // Evaluate the grouping expressions
        let group_by_values = evaluate_group_by(&self.group_by, &batch)?;

        // Evaluate the aggregation expressions.
        let input_values = evaluate_many(&self.aggregate_arguments, &batch)?;

        // Evaluate the filter expressions, if any, against the inputs
        let filter_values = evaluate_optional(&self.filter_expressions, &batch)?;

        for group_values in &group_by_values {
            // calculate the group indices for each input row
            let starting_num_groups = self.group_values.len();
            self.group_values
                .intern(group_values, &mut self.current_group_indices)?;
            let group_indices = &self.current_group_indices;

            // Update ordering information if necessary
            let total_num_groups = self.group_values.len();
            if total_num_groups > starting_num_groups {
                self.group_ordering.new_groups(
                    group_values,
                    group_indices,
                    total_num_groups,
                )?;
            }

            // Gather the inputs to call the actual accumulator
            let t = self
                .accumulators
                .iter_mut()
                .zip(input_values.iter())
                .zip(filter_values.iter());

            for ((acc, values), opt_filter) in t {
                let opt_filter = opt_filter.as_ref().map(|filter| filter.as_boolean());

                // Call the appropriate method on each aggregator with
                // the entire input row and the relevant group indexes
                match self.mode {
                    AggregateMode::Partial
                    | AggregateMode::Single
                    | AggregateMode::SinglePartitioned => {
                        acc.update_batch(
                            values,
                            group_indices,
                            opt_filter,
                            total_num_groups,
                        )?;
                    }
                    AggregateMode::FinalPartitioned | AggregateMode::Final => {
                        // if aggregation is over intermediate states,
                        // use merge
                        acc.merge_batch(
                            values,
                            group_indices,
                            opt_filter,
                            total_num_groups,
                        )?;
                    }
                }
            }
        }

        self.update_memory_reservation()
    }

    fn update_memory_reservation(&mut self) -> Result<()> {
        let acc = self.accumulators.iter().map(|x| x.size()).sum::<usize>();
        self.reservation.try_resize(
            acc + self.group_values.size()
                + self.group_ordering.size()
                + self.current_group_indices.allocated_size(),
        )
    }

    /// Create an output RecordBatch with the group keys and
    /// accumulator states/values specified in emit_to
    fn emit(&mut self, emit_to: EmitTo) -> Result<RecordBatch> {
        if self.group_values.is_empty() {
            return Ok(RecordBatch::new_empty(self.schema()));
        }

        let mut output = self.group_values.emit(emit_to)?;
        if let EmitTo::First(n) = emit_to {
            self.group_ordering.remove_groups(n);
        }

        // Next output each aggregate value
        for acc in self.accumulators.iter_mut() {
            match self.mode {
                AggregateMode::Partial => output.extend(acc.state(emit_to)?),
                AggregateMode::Final
                | AggregateMode::FinalPartitioned
                | AggregateMode::Single
                | AggregateMode::SinglePartitioned => output.push(acc.evaluate(emit_to)?),
            }
        }

        self.update_memory_reservation()?;
        let batch = RecordBatch::try_new(self.schema(), output)?;
        Ok(batch)
    }
}
