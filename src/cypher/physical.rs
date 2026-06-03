//! Physical execution engine — iterator-based operators that consume a
//! [`LogicalPlan`] and produce result rows.
//!
//! Each operator implements the [`PhysicalOperator`] trait and yields rows
//! lazily.  Eager operators (e.g. `Sort`) buffer their input in memory but
//! respect a configurable row budget.

use crate::cypher::ast::{Expression, Projection, RemoveItem, SetItem};
use crate::cypher::executor::{ExecError, QueryResult};
use crate::cypher::interpreter::{evaluate, eval_projections, EvalContext};
use crate::cypher::plan::{LogicalOperator, LogicalPlan};
use crate::cypher::value::Value;
use crate::graph::engine::{GraphStorageEngine, StorageEngine};
use crate::graph::record::{NodeRecord, SlotRef};
use crate::io::FileSystem;
use std::collections::HashMap;
use std::hash::Hasher;

// ------------------------------------------------------------------
// Row representation
// ------------------------------------------------------------------

/// One row produced by a physical operator.
///
/// Maps variable names to runtime [`Value`]s.  The order of bindings is
/// not significant — the `Project` operator re-orders them to match the
/// query's RETURN clause.
pub type Row = HashMap<String, Value>;

fn empty_row() -> Row {
    HashMap::new()
}

// ------------------------------------------------------------------
// Physical operator trait
// ------------------------------------------------------------------

/// Iterator-based physical operator.
///
/// Implementations are responsible for managing their own state (cursors,
/// buffers, counters).  `next_row` returns `Ok(None)` when the operator
/// has no more rows to yield.
pub trait PhysicalOperator {
    /// Yield the next result row, or `None` if exhausted.
    fn next_row(
        &mut self,
        ctx: &ExecutionContext,
    ) -> Result<Option<Row>, ExecError>;

    /// Reset the operator to its initial state (for sub-query re-execution).
    fn reset(&mut self);
}

// ------------------------------------------------------------------
// Execution context
// ------------------------------------------------------------------

/// Runtime state shared across all operators in a query.
pub struct ExecutionContext<'a> {
    /// Reference to the graph storage engine.
    pub engine: &'a GraphStorageEngine,
    /// Reference to the file-system abstraction (for I/O).
    pub fs: &'a dyn FileSystem,
}

// ------------------------------------------------------------------
// Operator implementations
// ------------------------------------------------------------------

/// Scan every node in the graph.
pub struct AllNodesScanOp {
    /// Current position in the node index range scan.
    cursor: Vec<(crate::index::key::CompositeKey, Vec<u8>)>,
    /// Next index into `cursor`.
    idx: usize,
}

impl AllNodesScanOp {
    pub fn new() -> Self {
        Self {
            cursor: Vec::new(),
            idx: 0,
        }
    }
}

impl PhysicalOperator for AllNodesScanOp {
    fn next_row(
        &mut self,
        ctx: &ExecutionContext,
    ) -> Result<Option<Row>, ExecError> {
        if self.cursor.is_empty() {
            // Perform a full range scan over the node index.
            let start = crate::index::key::node_id_key(0);
            let end = crate::index::key::node_id_key(u128::MAX);
            self.cursor = ctx.engine.node_index.range_search(&start, &end);
        }
        if self.idx >= self.cursor.len() {
            return Ok(None);
        }
        let (key, _value_bytes) = &self.cursor[self.idx];
        self.idx += 1;
        // Extract node_id from the first 16 bytes of the key.
        let node_id = u128::from_be_bytes([
            key.bytes[0], key.bytes[1], key.bytes[2], key.bytes[3],
            key.bytes[4], key.bytes[5], key.bytes[6], key.bytes[7],
            key.bytes[8], key.bytes[9], key.bytes[10], key.bytes[11],
            key.bytes[12], key.bytes[13], key.bytes[14], key.bytes[15],
        ]);
        match ctx.engine.get_node(node_id as u64, ctx.fs) {
            Ok(Some(record)) => {
                let mut row = empty_row();
                row.insert("_node".to_string(), node_to_value(&record));
                Ok(Some(row))
            }
            Ok(None) => self.next_row(ctx), // deleted — skip
            Err(e) => Err(ExecError::Eval(e.to_string())),
        }
    }

    fn reset(&mut self) {
        self.idx = 0;
        self.cursor.clear();
    }
}

/// Scan nodes that have a specific label.
pub struct NodeByLabelScanOp {
    label: String,
    /// Internal label id resolved at plan time (stub: use string hash).
    label_id: u64,
    cursor: Vec<NodeRecord>,
    idx: usize,
}

impl NodeByLabelScanOp {
    pub fn new(label: impl Into<String>) -> Self {
        let label = label.into();
        let mut hasher = twox_hash::XxHash64::with_seed(0);
        hasher.write(label.as_bytes());
        let label_id = hasher.finish();
        Self {
            label,
            label_id,
            cursor: Vec::new(),
            idx: 0,
        }
    }
}

impl PhysicalOperator for NodeByLabelScanOp {
    fn next_row(
        &mut self,
        ctx: &ExecutionContext,
    ) -> Result<Option<Row>, ExecError> {
        if self.cursor.is_empty() {
            self.cursor = ctx
                .engine
                .scan_nodes_by_label(self.label_id, ctx.fs)
                .map_err(|e| ExecError::Eval(e.to_string()))?;
        }
        if self.idx >= self.cursor.len() {
            return Ok(None);
        }
        let record = self.cursor[self.idx].clone();
        self.idx += 1;
        let mut row = empty_row();
        row.insert("_node".to_string(), node_to_value(&record));
        Ok(Some(row))
    }

    fn reset(&mut self) {
        self.idx = 0;
        self.cursor.clear();
    }
}

/// Expand relationships from the bound start node.
pub struct ExpandOp {
    direction: crate::cypher::ast::Direction,
    rel_types: Vec<String>,
    from_variable: String,
    /// Resolved label/type ids (stub: hashed at plan time).
    type_ids: Vec<u64>,
    /// Buffer of pending edges for the current input row.
    pending: Vec<(crate::graph::record::EdgeRecord, u64)>,
    /// Index into `pending`.
    pending_idx: usize,
}

impl ExpandOp {
    pub fn new(
        direction: crate::cypher::ast::Direction,
        rel_types: Vec<String>,
        from_variable: impl Into<String>,
    ) -> Self {
        let type_ids: Vec<u64> = rel_types
            .iter()
            .map(|t| {
                let mut h = twox_hash::XxHash64::with_seed(0);
                h.write(t.as_bytes());
                h.finish()
            })
            .collect();
        Self {
            direction,
            rel_types,
            from_variable: from_variable.into(),
            type_ids,
            pending: Vec::new(),
            pending_idx: 0,
        }
    }
}

impl PhysicalOperator for ExpandOp {
    fn next_row(
        &mut self,
        ctx: &ExecutionContext,
    ) -> Result<Option<Row>, ExecError> {
        // Yield from pending buffer first.
        if self.pending_idx < self.pending.len() {
            let (edge, end_node_id) = self.pending[self.pending_idx].clone();
            self.pending_idx += 1;
            let mut row = empty_row();
            row.insert("_edge".to_string(), edge_to_value(&edge));
            row.insert("_end_node".to_string(), Value::Integer(end_node_id as i64));
            return Ok(Some(row));
        }
        Ok(None)
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.pending_idx = 0;
    }
}

/// Apply a predicate to each input row.
pub struct FilterOp {
    predicate: Expression,
    input: Box<dyn PhysicalOperator>,
}

impl FilterOp {
    pub fn new(predicate: Expression, input: Box<dyn PhysicalOperator>) -> Self {
        Self { predicate, input }
    }
}

impl PhysicalOperator for FilterOp {
    fn next_row(
        &mut self,
        ctx: &ExecutionContext,
    ) -> Result<Option<Row>, ExecError> {
        loop {
            match self.input.next_row(ctx)? {
                None => return Ok(None),
                Some(row) => {
                    let eval_ctx = row_to_eval_context(&row);
                    match evaluate(&self.predicate, &eval_ctx) {
                        Ok(Value::Boolean(true)) => return Ok(Some(row)),
                        Ok(Value::Boolean(false)) => continue,
                        Ok(Value::Null) => continue, // Cypher: NULL is not TRUE
                        Ok(other) => {
                            return Err(ExecError::Eval(format!(
                                "FILTER predicate returned '{}', expected Boolean",
                                other.type_name()
                            )))
                        }
                        Err(e) => return Err(ExecError::Eval(e.to_string())),
                    }
                }
            }
        }
    }

    fn reset(&mut self) {
        self.input.reset();
    }
}

/// Compute projections.
pub struct ProjectOp {
    projections: Vec<Projection>,
    input: Box<dyn PhysicalOperator>,
}

impl ProjectOp {
    pub fn new(projections: Vec<Projection>, input: Box<dyn PhysicalOperator>) -> Self {
        Self {
            projections,
            input,
        }
    }
}

impl PhysicalOperator for ProjectOp {
    fn next_row(
        &mut self,
        ctx: &ExecutionContext,
    ) -> Result<Option<Row>, ExecError> {
        match self.input.next_row(ctx)? {
            None => Ok(None),
            Some(row) => {
                let eval_ctx = row_to_eval_context(&row);
                let pairs = eval_projections(&self.projections, &eval_ctx)
                    .map_err(|e| ExecError::Eval(e.to_string()))?;
                let mut out = empty_row();
                for (name, value) in pairs {
                    out.insert(name, value);
                }
                Ok(Some(out))
            }
        }
    }

    fn reset(&mut self) {
        self.input.reset();
    }
}

/// Sort rows in memory (eager materialisation).
pub struct SortOp {
    order_by: Vec<crate::cypher::ast::OrderItem>,
    input: Box<dyn PhysicalOperator>,
    /// Buffered rows after first pull.
    buffer: Option<Vec<Row>>,
    idx: usize,
}

impl SortOp {
    pub fn new(
        order_by: Vec<crate::cypher::ast::OrderItem>,
        input: Box<dyn PhysicalOperator>,
    ) -> Self {
        Self {
            order_by,
            input,
            buffer: None,
            idx: 0,
        }
    }
}

impl PhysicalOperator for SortOp {
    fn next_row(
        &mut self,
        ctx: &ExecutionContext,
    ) -> Result<Option<Row>, ExecError> {
        if self.buffer.is_none() {
            let mut rows = Vec::new();
            while let Some(row) = self.input.next_row(ctx)? {
                rows.push(row);
            }
            // Sort using the first order item (stub: multi-key sorting).
            if let Some(first) = self.order_by.first() {
                rows.sort_by(|a, b| {
                    let ctx_a = row_to_eval_context(a);
                    let ctx_b = row_to_eval_context(b);
                    let va = evaluate(&first.expression, &ctx_a).unwrap_or(Value::Null);
                    let vb = evaluate(&first.expression, &ctx_b).unwrap_or(Value::Null);
                    compare_values(&va, &vb, first.ascending)
                });
            }
            self.buffer = Some(rows);
        }
        let buf = self.buffer.as_ref().unwrap();
        if self.idx >= buf.len() {
            return Ok(None);
        }
        let row = buf[self.idx].clone();
        self.idx += 1;
        Ok(Some(row))
    }

    fn reset(&mut self) {
        self.buffer = None;
        self.idx = 0;
        self.input.reset();
    }
}

/// Skip the first N rows.
pub struct SkipOp {
    expression: Expression,
    input: Box<dyn PhysicalOperator>,
    skip_count: Option<u64>,
    skipped: u64,
}

impl SkipOp {
    pub fn new(expression: Expression, input: Box<dyn PhysicalOperator>) -> Self {
        Self {
            expression,
            input,
            skip_count: None,
            skipped: 0,
        }
    }
}

impl PhysicalOperator for SkipOp {
    fn next_row(
        &mut self,
        ctx: &ExecutionContext,
    ) -> Result<Option<Row>, ExecError> {
        if self.skip_count.is_none() {
            let eval_ctx = EvalContext::new();
            let val = evaluate(&self.expression, &eval_ctx)
                .map_err(|e| ExecError::Eval(e.to_string()))?;
            self.skip_count = val.as_integer().map(|v| v as u64);
        }
        let target = self.skip_count.unwrap_or(0);
        while self.skipped < target {
            if self.input.next_row(ctx)?.is_none() {
                return Ok(None);
            }
            self.skipped += 1;
        }
        self.input.next_row(ctx)
    }

    fn reset(&mut self) {
        self.skip_count = None;
        self.skipped = 0;
        self.input.reset();
    }
}

/// Limit to the first N rows.
pub struct LimitOp {
    expression: Expression,
    input: Box<dyn PhysicalOperator>,
    limit_count: Option<u64>,
    emitted: u64,
}

impl LimitOp {
    pub fn new(expression: Expression, input: Box<dyn PhysicalOperator>) -> Self {
        Self {
            expression,
            input,
            limit_count: None,
            emitted: 0,
        }
    }
}

impl PhysicalOperator for LimitOp {
    fn next_row(
        &mut self,
        ctx: &ExecutionContext,
    ) -> Result<Option<Row>, ExecError> {
        if self.limit_count.is_none() {
            let eval_ctx = EvalContext::new();
            let val = evaluate(&self.expression, &eval_ctx)
                .map_err(|e| ExecError::Eval(e.to_string()))?;
            self.limit_count = val.as_integer().map(|v| v as u64);
        }
        let target = self.limit_count.unwrap_or(0);
        if self.emitted >= target {
            return Ok(None);
        }
        match self.input.next_row(ctx)? {
            None => Ok(None),
            Some(row) => {
                self.emitted += 1;
                Ok(Some(row))
            }
        }
    }

    fn reset(&mut self) {
        self.limit_count = None;
        self.emitted = 0;
        self.input.reset();
    }
}

/// Create new nodes / relationships.
///
/// When driven by a read sub-tree (`MATCH ... CREATE ...`) the operator runs
/// once per incoming row, binding the created variables on top of that row.
/// A standalone `CREATE` (no `input`) emits exactly one row.
pub struct CreateOp {
    pattern: crate::cypher::ast::Pattern,
    input: Option<Box<dyn PhysicalOperator>>,
    /// For the standalone form: whether the single row has been emitted.
    emitted: bool,
}

impl CreateOp {
    pub fn new(
        pattern: crate::cypher::ast::Pattern,
        input: Option<Box<dyn PhysicalOperator>>,
    ) -> Self {
        Self {
            pattern,
            input,
            emitted: false,
        }
    }

    /// Bind the variables introduced by the CREATE pattern onto `row`.
    fn bind_created_variables(&self, row: &mut Row) {
        for elem in &self.pattern.elements {
            match elem {
                crate::cypher::ast::PatternElement::Node(n) => {
                    if let Some(v) = &n.variable {
                        // TODO: integrate with GraphStorageEngine to create the
                        // record and bind the real node value.
                        row.entry(v.clone()).or_insert(Value::Null);
                    }
                }
                crate::cypher::ast::PatternElement::Relationship(r) => {
                    if let Some(v) = &r.variable {
                        row.entry(v.clone()).or_insert(Value::Null);
                    }
                }
            }
        }
    }
}

impl PhysicalOperator for CreateOp {
    fn next_row(
        &mut self,
        ctx: &ExecutionContext,
    ) -> Result<Option<Row>, ExecError> {
        match &mut self.input {
            Some(input) => match input.next_row(ctx)? {
                Some(mut row) => {
                    self.bind_created_variables(&mut row);
                    Ok(Some(row))
                }
                None => Ok(None),
            },
            None => {
                if self.emitted {
                    return Ok(None);
                }
                self.emitted = true;
                let mut row = empty_row();
                self.bind_created_variables(&mut row);
                Ok(Some(row))
            }
        }
    }

    fn reset(&mut self) {
        self.emitted = false;
        if let Some(input) = &mut self.input {
            input.reset();
        }
    }
}

/// Eager barrier — fully materialise the input before yielding any row.
///
/// On the first `next_row`, the operator drains its entire input into an
/// in-memory buffer; subsequent calls yield the buffered rows one at a time.
/// This freezes the read set so a downstream write cannot feed newly created
/// entities back into the scan that drives it (the Halloween problem).
pub struct EagerOp {
    input: Box<dyn PhysicalOperator>,
    /// Buffered rows after the first pull (`None` until materialised).
    buffer: Option<Vec<Row>>,
    idx: usize,
}

impl EagerOp {
    pub fn new(input: Box<dyn PhysicalOperator>) -> Self {
        Self {
            input,
            buffer: None,
            idx: 0,
        }
    }
}

impl PhysicalOperator for EagerOp {
    fn next_row(
        &mut self,
        ctx: &ExecutionContext,
    ) -> Result<Option<Row>, ExecError> {
        if self.buffer.is_none() {
            // Fully consume the input before yielding the first row.
            let mut rows = Vec::new();
            while let Some(row) = self.input.next_row(ctx)? {
                rows.push(row);
            }
            self.buffer = Some(rows);
        }
        let buf = self.buffer.as_ref().unwrap();
        if self.idx >= buf.len() {
            return Ok(None);
        }
        let row = buf[self.idx].clone();
        self.idx += 1;
        Ok(Some(row))
    }

    fn reset(&mut self) {
        self.buffer = None;
        self.idx = 0;
        self.input.reset();
    }
}

/// Delete nodes / relationships.
pub struct DeleteOp {
    expressions: Vec<Expression>,
    detach: bool,
    input: Box<dyn PhysicalOperator>,
}

impl DeleteOp {
    pub fn new(expressions: Vec<Expression>, detach: bool, input: Box<dyn PhysicalOperator>) -> Self {
        Self { expressions, detach, input }
    }
}

impl PhysicalOperator for DeleteOp {
    fn next_row(
        &mut self,
        _ctx: &ExecutionContext,
    ) -> Result<Option<Row>, ExecError> {
        // Stub: consume all input rows and return a single summary row.
        while self.input.next_row(_ctx)?.is_some() {}
        let mut row = empty_row();
        row.insert("_deleted".to_string(), Value::Integer(self.expressions.len() as i64));
        Ok(Some(row))
    }

    fn reset(&mut self) {
        self.input.reset();
    }
}

/// Set properties or labels on existing entities.
pub struct SetOp {
    items: Vec<SetItem>,
    input: Box<dyn PhysicalOperator>,
}

impl SetOp {
    pub fn new(items: Vec<SetItem>, input: Box<dyn PhysicalOperator>) -> Self {
        Self { items, input }
    }
}

impl PhysicalOperator for SetOp {
    fn next_row(
        &mut self,
        _ctx: &ExecutionContext,
    ) -> Result<Option<Row>, ExecError> {
        self.input.next_row(_ctx)
    }

    fn reset(&mut self) {
        self.input.reset();
    }
}

/// Remove properties or labels from existing entities.
pub struct RemoveOp {
    items: Vec<RemoveItem>,
    input: Box<dyn PhysicalOperator>,
}

impl RemoveOp {
    pub fn new(items: Vec<RemoveItem>, input: Box<dyn PhysicalOperator>) -> Self {
        Self { items, input }
    }
}

impl PhysicalOperator for RemoveOp {
    fn next_row(
        &mut self,
        _ctx: &ExecutionContext,
    ) -> Result<Option<Row>, ExecError> {
        self.input.next_row(_ctx)
    }

    fn reset(&mut self) {
        self.input.reset();
    }
}

/// MERGE pattern with ON CREATE / ON MATCH actions.
pub struct MergeOp {
    pattern: crate::cypher::ast::Pattern,
    on_create: Vec<SetItem>,
    on_match: Vec<SetItem>,
    input: Box<dyn PhysicalOperator>,
}

impl MergeOp {
    pub fn new(
        pattern: crate::cypher::ast::Pattern,
        on_create: Vec<SetItem>,
        on_match: Vec<SetItem>,
        input: Box<dyn PhysicalOperator>,
    ) -> Self {
        Self { pattern, on_create, on_match, input }
    }
}

impl PhysicalOperator for MergeOp {
    fn next_row(
        &mut self,
        _ctx: &ExecutionContext,
    ) -> Result<Option<Row>, ExecError> {
        // Stub: consume input and return a placeholder row.
        while self.input.next_row(_ctx)?.is_some() {}
        let mut row = empty_row();
        for elem in &self.pattern.elements {
            if let crate::cypher::ast::PatternElement::Node(n) = elem {
                if let Some(v) = &n.variable {
                    row.insert(v.clone(), Value::Null);
                }
            }
        }
        Ok(Some(row))
    }

    fn reset(&mut self) {
        self.input.reset();
    }
}

/// Aggregate operator with implicit grouping.
pub struct AggregateOp {
    grouping_keys: Vec<Expression>,
    aggregations: Vec<crate::cypher::plan::Aggregation>,
    input: Box<dyn PhysicalOperator>,
    /// Buffered after first pull (eager materialisation).
    buffer: Option<Vec<Row>>,
    idx: usize,
}

impl AggregateOp {
    pub fn new(
        grouping_keys: Vec<Expression>,
        aggregations: Vec<crate::cypher::plan::Aggregation>,
        input: Box<dyn PhysicalOperator>,
    ) -> Self {
        Self {
            grouping_keys,
            aggregations,
            input,
            buffer: None,
            idx: 0,
        }
    }
}

impl PhysicalOperator for AggregateOp {
    fn next_row(
        &mut self,
        ctx: &ExecutionContext,
    ) -> Result<Option<Row>, ExecError> {
        if self.buffer.is_none() {
            let mut groups: Vec<(Vec<Value>, Vec<Row>)> = Vec::new();

            // Consume all input rows and partition by grouping keys.
            while let Some(row) = self.input.next_row(ctx)? {
                let eval_ctx = row_to_eval_context(&row);
                let key: Vec<Value> = self
                    .grouping_keys
                    .iter()
                    .map(|gk| evaluate(gk, &eval_ctx).unwrap_or(Value::Null))
                    .collect();
                if let Some((_k, rows)) = groups.iter_mut().find(|(k, _)| k == &key) {
                    rows.push(row);
                } else {
                    groups.push((key, vec![row]));
                }
            }

            // Compute one output row per group.
            let mut out = Vec::new();
            for (key_values, rows) in groups {
                let mut result_row = empty_row();
                // Bind grouping keys (using their string repr as variable names).
                for (i, gk) in self.grouping_keys.iter().enumerate() {
                    result_row.insert(gk.to_string(), key_values[i].clone());
                }
                // Compute each aggregate.
                for agg in &self.aggregations {
                    let val = compute_aggregate(&agg.function, &agg.argument, &rows, agg.distinct)?;
                    result_row.insert(agg.alias.clone(), val);
                }
                out.push(result_row);
            }
            self.buffer = Some(out);
        }
        let buf = self.buffer.as_ref().unwrap();
        if self.idx >= buf.len() {
            return Ok(None);
        }
        let row = buf[self.idx].clone();
        self.idx += 1;
        Ok(Some(row))
    }

    fn reset(&mut self) {
        self.buffer = None;
        self.idx = 0;
        self.input.reset();
    }
}

fn compute_aggregate(
    func: &crate::cypher::plan::AggregateFunction,
    arg: &Expression,
    rows: &[Row],
    distinct: bool,
) -> Result<Value, ExecError> {
    use crate::cypher::plan::AggregateFunction;

    // Wildcard `*` means count rows directly without evaluating an expression.
    let is_wildcard = matches!(arg, Expression::Wildcard);

    let mut values: Vec<Value> = Vec::new();
    if is_wildcard {
        // For count(*), each row contributes one value.
        for _ in rows {
            values.push(Value::Integer(1));
        }
    } else {
        for row in rows {
            let eval_ctx = row_to_eval_context(row);
            match evaluate(arg, &eval_ctx) {
                Ok(v) => values.push(v),
                Err(_) => {} // skip rows where argument evaluates to error
            }
        }
    }

    if distinct {
        values.sort_by(|a, b| compare_values(a, b, true));
        values.dedup();
    }

    match func {
        AggregateFunction::Count => Ok(Value::Integer(values.len() as i64)),
        AggregateFunction::Collect => {
            // collect(NULL) returns [NULL], not []
            if values.is_empty() {
                return Ok(Value::List(vec![]));
            }
            Ok(Value::List(values))
        }
        AggregateFunction::Sum => {
            let mut sum = Value::Integer(0);
            for v in values {
                sum = sum.add(&v).ok_or_else(|| ExecError::Eval(
                    "type mismatch in sum".to_string()
                ))?;
            }
            Ok(sum)
        }
        AggregateFunction::Avg => {
            if values.is_empty() {
                return Ok(Value::Null);
            }
            let mut sum = Value::Float(crate::graph::property::OrderedF64(0.0));
            for v in &values {
                sum = sum.add(v).ok_or_else(|| ExecError::Eval(
                    "type mismatch in avg".to_string()
                ))?;
            }
            let count = Value::Integer(values.len() as i64);
            sum.div(&count).ok_or_else(|| ExecError::Eval(
                "type mismatch in avg".to_string()
            ))
        }
        AggregateFunction::Min => {
            if values.is_empty() {
                return Ok(Value::Null);
            }
            let mut min = values[0].clone();
            for v in values.into_iter().skip(1) {
                if compare_values(&v, &min, true) == std::cmp::Ordering::Less {
                    min = v;
                }
            }
            Ok(min)
        }
        AggregateFunction::Max => {
            if values.is_empty() {
                return Ok(Value::Null);
            }
            let mut max = values[0].clone();
            for v in values.into_iter().skip(1) {
                if compare_values(&v, &max, true) == std::cmp::Ordering::Greater {
                    max = v;
                }
            }
            Ok(max)
        }
    }
}

// ------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------

/// Convert a [`NodeRecord`] into a runtime [`Value`].
fn node_to_value(node: &NodeRecord) -> Value {
    let mut map = HashMap::new();
    map.insert("node_id".to_string(), Value::Integer(node.node_id as i64));
    map.insert("label_id".to_string(), Value::Integer(node.label_id as i64));
    Value::Map(map)
}

/// Convert an [`EdgeRecord`] into a runtime [`Value`].
fn edge_to_value(edge: &crate::graph::record::EdgeRecord) -> Value {
    let mut map = HashMap::new();
    map.insert("edge_id".to_string(), Value::Integer(edge.edge_id as i64));
    map.insert("type_id".to_string(), Value::Integer(edge.type_id as i64));
    Value::Map(map)
}

/// Build an [`EvalContext`] from a physical row.
fn row_to_eval_context(row: &Row) -> EvalContext {
    let mut ctx = EvalContext::new();
    for (k, v) in row {
        ctx = ctx.bind(k.clone(), v.clone());
    }
    ctx
}

fn compare_values(a: &Value, b: &Value, ascending: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let ord = match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater, // NULL sorts last
        (_, Value::Null) => Ordering::Less,
        (Value::Integer(x), Value::Integer(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Boolean(x), Value::Boolean(y)) => x.cmp(y),
        _ => Ordering::Equal, // incomparable types
    };
    if ascending { ord } else { ord.reverse() }
}

/// Decode a [`SlotRef`] from a 4-byte big-endian value.
fn decode_slot_ref(bytes: &[u8]) -> Option<SlotRef> {
    if bytes.len() < 4 {
        return None;
    }
    let raw = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    Some(SlotRef { raw })
}

// ------------------------------------------------------------------
// Plan-to-physical translation
// ------------------------------------------------------------------

/// Translate a [`LogicalPlan`] into a root [`PhysicalOperator`].
pub fn build_physical_plan(plan: &LogicalPlan) -> Box<dyn PhysicalOperator> {
    build_physical_operator(&plan.root)
}

fn build_physical_operator(op: &LogicalOperator) -> Box<dyn PhysicalOperator> {
    match op {
        LogicalOperator::AllNodesScan => Box::new(AllNodesScanOp::new()),
        LogicalOperator::NodeByLabelScan { label } => Box::new(NodeByLabelScanOp::new(label)),
        LogicalOperator::NodeByIdScan { node_id } => {
            // Point lookup is modelled as a label scan with a filter (stub).
            Box::new(AllNodesScanOp::new())
        }
        LogicalOperator::Expand {
            input,
            direction,
            rel_types,
            from_variable,
            ..
        } => Box::new(ExpandOp::new(*direction, rel_types.clone(), from_variable)),
        LogicalOperator::Filter { input, predicate } => Box::new(FilterOp::new(
            predicate.clone(),
            build_physical_operator(input),
        )),
        LogicalOperator::Project { input, projections } => Box::new(ProjectOp::new(
            projections.clone(),
            build_physical_operator(input),
        )),
        LogicalOperator::Sort { input, order_by } => Box::new(SortOp::new(
            order_by.clone(),
            build_physical_operator(input),
        )),
        LogicalOperator::Skip { input, expression } => Box::new(SkipOp::new(
            expression.clone(),
            build_physical_operator(input),
        )),
        LogicalOperator::Limit { input, expression } => Box::new(LimitOp::new(
            expression.clone(),
            build_physical_operator(input),
        )),
        LogicalOperator::Create { input, pattern } => {
            let input_op = input.as_ref().map(|i| build_physical_operator(i));
            Box::new(CreateOp::new(pattern.clone(), input_op))
        }
        LogicalOperator::Eager { input } => {
            Box::new(EagerOp::new(build_physical_operator(input)))
        }
        LogicalOperator::Delete { input, expressions, detach } => Box::new(DeleteOp::new(
            expressions.clone(),
            *detach,
            build_physical_operator(input),
        )),
        LogicalOperator::Set { input, items } => Box::new(SetOp::new(
            items.clone(),
            build_physical_operator(input),
        )),
        LogicalOperator::Remove { input, items } => Box::new(RemoveOp::new(
            items.clone(),
            build_physical_operator(input),
        )),
        LogicalOperator::Apply { left, right } => {
            // Apply is a nested-loop join.  For Sprint 21 we return the
            // Cartesian product of left and right (simplified stub).
            let _l = build_physical_operator(left);
            let _r = build_physical_operator(right);
            Box::new(AllNodesScanOp::new()) // stub
        }
        LogicalOperator::Aggregate {
            input,
            grouping_keys,
            aggregations,
        } => Box::new(AggregateOp::new(
            grouping_keys.clone(),
            aggregations.clone(),
            build_physical_operator(input),
        )),
        LogicalOperator::HashJoin { left, right, .. } => {
            let _l = build_physical_operator(left);
            let _r = build_physical_operator(right);
            Box::new(AllNodesScanOp::new()) // stub
        }
        LogicalOperator::Merge { pattern, on_create, on_match, .. } => {
            Box::new(MergeOp::new(
                pattern.clone(),
                on_create.clone(),
                on_match.clone(),
                Box::new(AllNodesScanOp::new()),
            ))
        }
    }
}

// ------------------------------------------------------------------
// High-level execution entry point
// ------------------------------------------------------------------

/// Execute a [`LogicalPlan`] against the storage engine and return a
/// [`QueryResult`].
///
/// This is the bridge between the planner and the naive executor.  It
/// materialises all rows into memory (suitable for the Sprint 21 subset).
/// Future sprints will add streaming result sets.
pub fn execute_plan(
    plan: &LogicalPlan,
    ctx: &ExecutionContext,
) -> Result<QueryResult, ExecError> {
    let mut physical = build_physical_plan(plan);
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut columns: Vec<String> = Vec::new();

    // Pull the first row to discover column names from Project.
    if let Some(first_row) = physical.next_row(ctx)? {
        columns = first_row.keys().cloned().collect();
        columns.sort(); // deterministic order
        let values: Vec<Value> = columns.iter().map(|c| {
            first_row.get(c).cloned().unwrap_or(Value::Null)
        }).collect();
        rows.push(values);
    }

    // Pull remaining rows.
    while let Some(row) = physical.next_row(ctx)? {
        let values: Vec<Value> = columns.iter().map(|c| {
            row.get(c).cloned().unwrap_or(Value::Null)
        }).collect();
        rows.push(values);
    }

    Ok(QueryResult { columns, rows })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::ast::{Expression, Literal, Projection};
    use crate::cypher::plan::LogicalPlan;

    #[test]
    fn filter_op_keeps_true_rows() {
        let input = Box::new(MockOp::new(vec![
            vec![("x".to_string(), Value::Integer(5))].into_iter().collect(),
            vec![("x".to_string(), Value::Integer(15))].into_iter().collect(),
        ]));
        let mut filter = FilterOp::new(
            Expression::Comparison { span: None,
                op: crate::cypher::ast::ComparisonOperator::Gt,
                left: Box::new(Expression::Variable("x".to_string())),
                right: Box::new(Expression::Literal(Literal::Integer(10))),
            },
            input,
        );
        // Mock context — FilterOp only needs EvalContext, not real storage.
        // We use a dummy ctx that will panic if storage is touched.
        let ctx = mock_ctx();
        let row1 = filter.next_row(&ctx).unwrap().unwrap();
        assert_eq!(row1.get("x"), Some(&Value::Integer(15)));
        assert!(filter.next_row(&ctx).unwrap().is_none());
    }

    #[test]
    fn project_op_renames_columns() {
        let input = Box::new(MockOp::new(vec![
            vec![("a".to_string(), Value::Integer(1))].into_iter().collect(),
        ]));
        let mut proj = ProjectOp::new(
            vec![Projection { span: None,
                    expression: Expression::Variable("a".to_string()),
                alias: Some("b".to_string()),
            }],
            input,
        );
        let ctx = mock_ctx();
        let row = proj.next_row(&ctx).unwrap().unwrap();
        assert_eq!(row.get("b"), Some(&Value::Integer(1)));
    }

    #[test]
    fn limit_op_stops_after_n() {
        let input = Box::new(MockOp::new(vec![
            vec![("i".to_string(), Value::Integer(1))].into_iter().collect(),
            vec![("i".to_string(), Value::Integer(2))].into_iter().collect(),
            vec![("i".to_string(), Value::Integer(3))].into_iter().collect(),
        ]));
        let mut limit = LimitOp::new(
            Expression::Literal(Literal::Integer(2)),
            input,
        );
        let ctx = mock_ctx();
        assert!(limit.next_row(&ctx).unwrap().is_some());
        assert!(limit.next_row(&ctx).unwrap().is_some());
        assert!(limit.next_row(&ctx).unwrap().is_none());
    }

    #[test]
    fn skip_op_drops_first_n() {
        let input = Box::new(MockOp::new(vec![
            vec![("i".to_string(), Value::Integer(1))].into_iter().collect(),
            vec![("i".to_string(), Value::Integer(2))].into_iter().collect(),
            vec![("i".to_string(), Value::Integer(3))].into_iter().collect(),
        ]));
        let mut skip = SkipOp::new(
            Expression::Literal(Literal::Integer(1)),
            input,
        );
        let ctx = mock_ctx();
        let row = skip.next_row(&ctx).unwrap().unwrap();
        assert_eq!(row.get("i"), Some(&Value::Integer(2)));
        assert!(skip.next_row(&ctx).unwrap().is_some());
        assert!(skip.next_row(&ctx).unwrap().is_none());
    }

    #[test]
    fn aggregate_op_count_grouped() {
        let input = Box::new(MockOp::new(vec![
            vec![("dept".to_string(), Value::String("a".to_string())), ("salary".to_string(), Value::Integer(100))].into_iter().collect(),
            vec![("dept".to_string(), Value::String("a".to_string())), ("salary".to_string(), Value::Integer(200))].into_iter().collect(),
            vec![("dept".to_string(), Value::String("b".to_string())), ("salary".to_string(), Value::Integer(300))].into_iter().collect(),
        ]));
        let mut agg = AggregateOp::new(
            vec![Expression::Variable("dept".to_string())],
            vec![crate::cypher::plan::Aggregation {
                alias: "c".to_string(),
                function: crate::cypher::plan::AggregateFunction::Count,
                argument: Expression::Wildcard,
                distinct: false,
            }],
            input,
        );
        let ctx = mock_ctx();
        let mut results = Vec::new();
        while let Some(row) = agg.next_row(&ctx).unwrap() {
            results.push(row);
        }
        assert_eq!(results.len(), 2);
        // Find group 'a'
        let group_a = results.iter().find(|r| r.get("dept") == Some(&Value::String("a".to_string()))).unwrap();
        assert_eq!(group_a.get("c"), Some(&Value::Integer(2)));
        let group_b = results.iter().find(|r| r.get("dept") == Some(&Value::String("b".to_string()))).unwrap();
        assert_eq!(group_b.get("c"), Some(&Value::Integer(1)));
    }

    #[test]
    fn aggregate_op_collect_returns_null_in_list() {
        let input = Box::new(MockOp::new(vec![
            vec![("v".to_string(), Value::Null)].into_iter().collect(),
        ]));
        let mut agg = AggregateOp::new(
            vec![],
            vec![crate::cypher::plan::Aggregation {
                alias: "items".to_string(),
                function: crate::cypher::plan::AggregateFunction::Collect,
                argument: Expression::Variable("v".to_string()),
                distinct: false,
            }],
            input,
        );
        let ctx = mock_ctx();
        let row = agg.next_row(&ctx).unwrap().unwrap();
        assert_eq!(row.get("items"), Some(&Value::List(vec![Value::Null])));
    }

    #[test]
    fn eager_op_fully_consumes_input_before_first_yield() {
        use std::cell::Cell;
        use std::rc::Rc;

        // A mock operator that records how many times it was polled, so we can
        // prove the Eager barrier drains it completely before yielding.
        struct CountingOp {
            n: usize,
            idx: usize,
            polls: Rc<Cell<usize>>,
        }
        impl PhysicalOperator for CountingOp {
            fn next_row(
                &mut self,
                _ctx: &ExecutionContext,
            ) -> Result<Option<Row>, ExecError> {
                self.polls.set(self.polls.get() + 1);
                if self.idx >= self.n {
                    return Ok(None);
                }
                let mut row = empty_row();
                row.insert("i".to_string(), Value::Integer(self.idx as i64));
                self.idx += 1;
                Ok(Some(row))
            }
            fn reset(&mut self) {
                self.idx = 0;
            }
        }

        let polls = Rc::new(Cell::new(0));
        let input = Box::new(CountingOp {
            n: 3,
            idx: 0,
            polls: Rc::clone(&polls),
        });
        let mut eager = EagerOp::new(input);
        let ctx = mock_ctx();

        // The very first pull must have drained the entire input: 3 data rows
        // plus the terminating `None` poll = 4 polls.
        let first = eager.next_row(&ctx).unwrap();
        assert!(first.is_some());
        assert_eq!(
            polls.get(),
            4,
            "EagerOp must fully consume its input before yielding the first row"
        );

        // The remaining rows are served from the buffer — no further input polls.
        let mut count = 1;
        while eager.next_row(&ctx).unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 3, "EagerOp must yield exactly the buffered rows");
        assert_eq!(
            polls.get(),
            4,
            "EagerOp must not poll its input again after materialisation"
        );
    }

    #[test]
    fn create_op_runs_once_per_input_row() {
        // CREATE driven by a 2-row input binds the created variable on each row.
        let input = Box::new(MockOp::new(vec![
            vec![("n".to_string(), Value::Integer(1))]
                .into_iter()
                .collect(),
            vec![("n".to_string(), Value::Integer(2))]
                .into_iter()
                .collect(),
        ]));
        let pattern = crate::cypher::ast::Pattern {
            span: None,
            elements: vec![crate::cypher::ast::PatternElement::Node(
                crate::cypher::ast::NodePattern {
                    span: None,
                    variable: Some("m".to_string()),
                    labels: vec![],
                    properties: std::collections::HashMap::new(),
                },
            )],
        };
        let mut create = CreateOp::new(pattern, Some(input));
        let ctx = mock_ctx();
        let mut rows = 0;
        while let Some(row) = create.next_row(&ctx).unwrap() {
            assert!(row.contains_key("m"), "created variable must be bound");
            assert!(row.contains_key("n"), "input variable must be preserved");
            rows += 1;
        }
        assert_eq!(rows, 2, "CREATE must run once per incoming row");
    }

    #[test]
    fn create_op_standalone_emits_single_row() {
        let pattern = crate::cypher::ast::Pattern {
            span: None,
            elements: vec![crate::cypher::ast::PatternElement::Node(
                crate::cypher::ast::NodePattern {
                    span: None,
                    variable: Some("n".to_string()),
                    labels: vec!["Person".to_string()],
                    properties: std::collections::HashMap::new(),
                },
            )],
        };
        let mut create = CreateOp::new(pattern, None);
        let ctx = mock_ctx();
        assert!(create.next_row(&ctx).unwrap().is_some());
        assert!(create.next_row(&ctx).unwrap().is_none());
    }

    // ------------------------------------------------------------------
    // Mock infrastructure
    // ------------------------------------------------------------------

    struct MockOp {
        rows: Vec<Row>,
        idx: usize,
    }

    impl MockOp {
        fn new(rows: Vec<Row>) -> Self {
            Self { rows, idx: 0 }
        }
    }

    impl PhysicalOperator for MockOp {
        fn next_row(
            &mut self,
            _ctx: &ExecutionContext,
        ) -> Result<Option<Row>, ExecError> {
            if self.idx >= self.rows.len() {
                return Ok(None);
            }
            let row = self.rows[self.idx].clone();
            self.idx += 1;
            Ok(Some(row))
        }
        fn reset(&mut self) {
            self.idx = 0;
        }
    }

    fn mock_ctx() -> ExecutionContext<'static> {
        // A dummy context for tests that do not touch storage.
        // We create an empty engine on a temp path; it will never be used
        // by the mock operators.
        let dir = std::env::temp_dir().join(format!("rgraph-mock-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let fs = crate::io::posix::PosixFileSystem::new(false);
        let engine = GraphStorageEngine::init(dir.join("data.db"), &fs).unwrap();
        // Leak both to keep them alive for 'static.
        let fs_ref: &'static dyn FileSystem = Box::leak(Box::new(fs));
        let engine_ref: &'static GraphStorageEngine = Box::leak(Box::new(engine));
        ExecutionContext {
            engine: engine_ref,
            fs: fs_ref,
        }
    }
}
