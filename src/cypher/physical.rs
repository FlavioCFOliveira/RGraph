//! Physical execution engine — iterator-based operators that consume a
//! [`LogicalPlan`] and produce result rows.
//!
//! Each operator implements the [`PhysicalOperator`] trait and yields rows
//! lazily.  Eager operators (e.g. `Sort`) buffer their input in memory but
//! respect a configurable row budget.

use crate::cypher::ast::{Expression, Projection, RemoveItem, SetItem};
use crate::cypher::executor::{ExecError, QueryResult};
use crate::cypher::interpreter::{EvalContext, eval_projections, evaluate};
use crate::cypher::plan::{LogicalOperator, LogicalPlan};
use crate::cypher::value::Value;
use crate::graph::engine::{GraphStorageEngine, StorageEngine};
use crate::io::FileSystem;
use std::collections::HashMap;

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
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError>;

    /// Reset the operator to its initial state (for sub-query re-execution).
    fn reset(&mut self);
}

// ------------------------------------------------------------------
// Execution context
// ------------------------------------------------------------------

/// Runtime state shared across all operators in a query.
///
/// `engine_cell` provides interior-mutable access to the engine for write
/// operators.  The engine pointer stored in the `UnsafeCell` comes from the
/// caller of [`ExecutionContext::new_with_write`] which must guarantee
/// exclusive access for the duration of the execution.
pub struct ExecutionContext<'a> {
    /// Read-only reference to the graph storage engine (used by read operators
    /// and for the public API where mutability is not needed).
    pub engine: &'a GraphStorageEngine,
    /// Interior-mutable engine pointer for write operators.
    /// See [`engine_ptr_mut`](ExecutionContext::engine_ptr_mut) for the invariants.
    engine_ptr: std::cell::UnsafeCell<*mut GraphStorageEngine>,
    /// Reference to the file-system abstraction (for I/O).
    pub fs: &'a dyn FileSystem,
    /// Optional parameter bindings for this execution (e.g. `$name → "Alice"`).
    pub parameters: std::collections::HashMap<String, Value>,
}

// SAFETY: `ExecutionContext` is only used within a single thread during query
// execution.  The `UnsafeCell<*mut GraphStorageEngine>` is not shared across
// threads.
unsafe impl<'a> Send for ExecutionContext<'a> {}

impl<'a> ExecutionContext<'a> {
    /// Create a context backed by a shared (read-only) engine reference.
    ///
    /// Write operators will fail at runtime if used with this constructor.
    /// Use [`new_with_write`] when write operators are present in the plan.
    pub fn new(engine: &'a GraphStorageEngine, fs: &'a dyn FileSystem) -> Self {
        Self {
            engine,
            engine_ptr: std::cell::UnsafeCell::new(engine as *const _ as *mut _),
            fs,
            parameters: std::collections::HashMap::new(),
        }
    }

    /// Create a context backed by a mutable engine reference (for write queries).
    pub fn new_with_write(engine: &'a mut GraphStorageEngine, fs: &'a dyn FileSystem) -> Self {
        let ptr = engine as *mut _;
        Self {
            engine,
            engine_ptr: std::cell::UnsafeCell::new(ptr),
            fs,
            parameters: std::collections::HashMap::new(),
        }
    }

    /// Obtain the raw, mutable engine pointer.
    ///
    /// A raw pointer (rather than `&mut`) is returned deliberately: producing a
    /// `&mut` from `&self` would launder a mutable borrow out of a shared one,
    /// which clippy denies (`mut_from_ref`) because it is unsound in the general
    /// case.  Returning the pointer keeps the aliasing obligation explicit at
    /// every call site, where it must be dereferenced inside an `unsafe` block.
    ///
    /// # Safety
    ///
    /// Dereferencing the returned pointer as `&mut GraphStorageEngine` is sound
    /// only when no other live reference to the engine exists for the duration
    /// of the borrow.  That invariant holds when:
    ///
    /// 1. The context was created with [`new_with_write`] from a `&mut` borrow,
    ///    giving the context exclusive ownership of the engine pointer.
    /// 2. Query execution is single-threaded — at most one write operator runs
    ///    at any point, and no concurrent thread aliases the engine.
    /// 3. The caller does not retain the derived `&mut` across any `.await` or
    ///    other re-entrant call that could produce a second mutable reference.
    ///
    /// A context built with [`new`] (read-only) holds a `&`-derived pointer, so
    /// callers must never derive a `&mut` from it.
    pub(crate) fn engine_ptr_mut(&self) -> *mut GraphStorageEngine {
        // The pointer is returned as-is; the `unsafe` obligation lives at the
        // dereference site, documented above.
        unsafe { *self.engine_ptr.get() }
    }
}

// ------------------------------------------------------------------
// Operator implementations
// ------------------------------------------------------------------

/// Yields exactly one empty row — used as the implicit input for RETURN-only queries.
pub struct SingleRowOp {
    emitted: bool,
}

impl Default for SingleRowOp {
    fn default() -> Self {
        Self::new()
    }
}

impl SingleRowOp {
    pub fn new() -> Self {
        Self { emitted: false }
    }
}

impl PhysicalOperator for SingleRowOp {
    fn next_row(&mut self, _ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        Ok(Some(empty_row()))
    }
    fn reset(&mut self) {
        self.emitted = false;
    }
}

/// Scan every node in the graph, binding each to `node_variable`.
pub struct AllNodesScanOp {
    /// Pattern variable to bind the matched node under.
    node_variable: String,
    /// Cursor over the node index (populated on first call).
    cursor: Vec<(crate::index::key::CompositeKey, Vec<u8>)>,
    /// Next index into `cursor`.
    idx: usize,
}

impl AllNodesScanOp {
    /// `node_variable`: the Cypher variable name to bind matched nodes under.
    pub fn new(node_variable: impl Into<String>) -> Self {
        Self {
            node_variable: node_variable.into(),
            cursor: Vec::new(),
            idx: 0,
        }
    }
}

impl PhysicalOperator for AllNodesScanOp {
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        if self.cursor.is_empty() {
            let start = crate::index::key::node_id_key(0);
            let end = crate::index::key::node_id_key(u128::MAX);
            self.cursor = ctx.engine.node_index.range_search(&start, &end);
        }
        loop {
            if self.idx >= self.cursor.len() {
                return Ok(None);
            }
            let (key, _value_bytes) = &self.cursor[self.idx];
            self.idx += 1;
            let node_id =
                u128::from_be_bytes(key.as_slice()[..16].try_into().unwrap_or([0u8; 16])) as u64;
            match load_node_value(ctx.engine, node_id, ctx.fs)? {
                Some(node_val) => {
                    let mut row = empty_row();
                    row.insert(self.node_variable.clone(), node_val);
                    return Ok(Some(row));
                }
                None => continue, // deleted or not found — skip
            }
        }
    }

    fn reset(&mut self) {
        self.idx = 0;
        self.cursor.clear();
    }
}

/// Scan nodes that have a specific label, binding each to `node_variable`.
pub struct NodeByLabelScanOp {
    /// Pattern variable to bind matched nodes under.
    node_variable: String,
    /// Label string (used to resolve catalog id at runtime).
    label: String,
    /// Cached node ids from the label index (populated on first call).
    cursor: Vec<u64>,
    idx: usize,
}

impl NodeByLabelScanOp {
    /// `node_variable`: Cypher variable; `label`: label string (not an id).
    pub fn new(node_variable: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            node_variable: node_variable.into(),
            label: label.into(),
            cursor: Vec::new(),
            idx: 0,
        }
    }
}

impl PhysicalOperator for NodeByLabelScanOp {
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        if self.cursor.is_empty() {
            // Resolve label name → catalog id → storage u64 label id.
            let label_id = ctx.engine.storage_label_id_for(&self.label).unwrap_or(0u64);
            let records = ctx
                .engine
                .scan_nodes_by_label(label_id, ctx.fs)
                .map_err(|e| ExecError::Eval(e.to_string()))?;
            self.cursor = records
                .iter()
                .filter(|r| r.flags & crate::graph::record::node_flags::DELETED == 0)
                .map(|r| r.node_id)
                .collect();
        }
        loop {
            if self.idx >= self.cursor.len() {
                return Ok(None);
            }
            let node_id = self.cursor[self.idx];
            self.idx += 1;
            match load_node_value(ctx.engine, node_id, ctx.fs)? {
                Some(node_val) => {
                    let mut row = empty_row();
                    row.insert(self.node_variable.clone(), node_val);
                    return Ok(Some(row));
                }
                None => continue,
            }
        }
    }

    fn reset(&mut self) {
        self.idx = 0;
        self.cursor.clear();
    }
}

/// Expand relationships from a bound start node, driving an input operator.
pub struct ExpandOp {
    input: Box<dyn PhysicalOperator>,
    direction: crate::cypher::ast::Direction,
    rel_types: Vec<String>,
    from_variable: String,
    rel_variable: Option<String>,
    end_node_variable: Option<String>,
    /// Buffer of pending (edge, end_node_id, input_row) triples.
    pending: Vec<(crate::graph::record::EdgeRecord, u64, Row)>,
    /// Index into `pending`.
    pending_idx: usize,
}

impl ExpandOp {
    pub fn new(
        input: Box<dyn PhysicalOperator>,
        direction: crate::cypher::ast::Direction,
        rel_types: Vec<String>,
        from_variable: impl Into<String>,
        rel_variable: Option<String>,
        end_node_variable: Option<String>,
    ) -> Self {
        Self {
            input,
            direction,
            rel_types,
            from_variable: from_variable.into(),
            rel_variable,
            end_node_variable,
            pending: Vec::new(),
            pending_idx: 0,
        }
    }
}

impl PhysicalOperator for ExpandOp {
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        loop {
            // Yield from pending buffer first.
            if self.pending_idx < self.pending.len() {
                let (ref edge, end_node_id, ref input_row) = self.pending[self.pending_idx].clone();
                self.pending_idx += 1;

                let mut row = input_row.clone();

                // Bind relationship variable.
                if let Some(rv) = &self.rel_variable {
                    row.insert(rv.clone(), edge_record_to_value(edge, ctx.engine, ctx.fs));
                }

                // Bind end-node variable.
                let end_var = self.end_node_variable.as_deref().unwrap_or("_end_node");
                match load_node_value(ctx.engine, end_node_id, ctx.fs)? {
                    Some(nv) => {
                        row.insert(end_var.to_string(), nv);
                    }
                    None => continue,
                }
                return Ok(Some(row));
            }

            // Need more input.
            let Some(input_row) = self.input.next_row(ctx)? else {
                return Ok(None);
            };

            // Resolve the start node id from the from_variable.
            let start_node_id = match input_row.get(&self.from_variable) {
                Some(Value::Node(n)) => n.id,
                Some(Value::Integer(id)) => *id as u64,
                _ => continue,
            };

            // Resolve type ids via catalog.
            let type_ids: Vec<u64> = self
                .rel_types
                .iter()
                .filter_map(|t| {
                    ctx.engine.storage_label_id_for(t).or_else(|| {
                        // Try rel-type catalog (label and rel-type share namespace in storage).
                        ctx.engine
                            .catalog()
                            .read()
                            .ok()
                            .and_then(|c| c.rel_type_id(t))
                            .map(|id| id as u64)
                    })
                })
                .collect();

            // Traverse adjacency lists.
            let edges = match self.direction {
                crate::cypher::ast::Direction::Outgoing => ctx
                    .engine
                    .scan_outgoing_edges(start_node_id, &type_ids, ctx.fs)
                    .map_err(|e| ExecError::Eval(e.to_string()))?,
                crate::cypher::ast::Direction::Incoming => ctx
                    .engine
                    .scan_incoming_edges(start_node_id, &type_ids, ctx.fs)
                    .map_err(|e| ExecError::Eval(e.to_string()))?,
                crate::cypher::ast::Direction::Both => {
                    let mut out = ctx
                        .engine
                        .scan_outgoing_edges(start_node_id, &type_ids, ctx.fs)
                        .map_err(|e| ExecError::Eval(e.to_string()))?;
                    let inc = ctx
                        .engine
                        .scan_incoming_edges(start_node_id, &type_ids, ctx.fs)
                        .map_err(|e| ExecError::Eval(e.to_string()))?;
                    out.extend(inc);
                    out
                }
            };

            self.pending.clear();
            self.pending_idx = 0;
            for (edge, end_node_id) in edges {
                self.pending.push((edge, end_node_id, input_row.clone()));
            }
        }
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.pending_idx = 0;
        self.input.reset();
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
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        loop {
            match self.input.next_row(ctx)? {
                None => return Ok(None),
                Some(row) => {
                    let eval_ctx = row_to_eval_context(&row, ctx);
                    match evaluate(&self.predicate, &eval_ctx) {
                        Ok(Value::Boolean(true)) => return Ok(Some(row)),
                        Ok(Value::Boolean(false)) => continue,
                        Ok(Value::Null) => continue, // Cypher: NULL is not TRUE
                        Ok(other) => {
                            return Err(ExecError::Eval(format!(
                                "FILTER predicate returned '{}', expected Boolean",
                                other.type_name()
                            )));
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
        Self { projections, input }
    }
}

impl PhysicalOperator for ProjectOp {
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        match self.input.next_row(ctx)? {
            None => Ok(None),
            Some(row) => {
                let eval_ctx = row_to_eval_context(&row, ctx);
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
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        if self.buffer.is_none() {
            let mut rows = Vec::new();
            while let Some(row) = self.input.next_row(ctx)? {
                rows.push(row);
            }
            // Stable multi-key sort: iterate order items from last to first
            // (stable sort of later keys, then sort by earlier keys on top).
            for order_item in self.order_by.iter().rev() {
                let asc = order_item.ascending;
                let expr = order_item.expression.clone();
                rows.sort_by(|a, b| {
                    let ctx_a = row_to_eval_context(a, ctx);
                    let ctx_b = row_to_eval_context(b, ctx);
                    let va = evaluate(&expr, &ctx_a).unwrap_or(Value::Null);
                    let vb = evaluate(&expr, &ctx_b).unwrap_or(Value::Null);
                    compare_values(&va, &vb, asc)
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
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        if self.skip_count.is_none() {
            let eval_ctx = eval_context_with_params(ctx);
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
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        if self.limit_count.is_none() {
            let eval_ctx = eval_context_with_params(ctx);
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

    /// Execute the CREATE pattern against storage, binding created entities in `row`.
    fn execute_create(&self, row: &mut Row, ctx: &ExecutionContext) -> Result<(), ExecError> {
        use crate::cypher::ast::PatternElement;
        use crate::cypher::interpreter::evaluate;
        use crate::graph::builder::{NodeBuilder, RelationshipBuilder};
        use crate::graph::graph::Graph;

        // SAFETY: we are the only caller deriving a `&mut` from the engine
        // pointer during this operator's next_row invocation, and execution is
        // single-threaded. See ExecutionContext::engine_ptr_mut for invariants.
        let engine = unsafe { &mut *ctx.engine_ptr_mut() };

        // We need a row eval context for property expression evaluation.
        let eval_ctx = row_to_eval_context(row, ctx);

        // First pass: create the nodes named in the pattern and bind them into
        // the row. Relationships are created in a second pass below, which
        // resolves their source/target node ids from those row bindings.
        for elem in &self.pattern.elements {
            match elem {
                PatternElement::Node(n) => {
                    // Resolve label → catalog id.
                    let label_id = n
                        .labels
                        .first()
                        .map(|l| engine.catalog_label_id(l))
                        .unwrap_or(0u32);

                    let mut builder = NodeBuilder::new().label(label_id);
                    // Evaluate and add properties.
                    for (k, v_expr) in &n.properties {
                        match evaluate(v_expr, &eval_ctx) {
                            Ok(val) => {
                                if let Some(prop) = value_to_property(&val) {
                                    builder = builder.property(k.clone(), prop);
                                }
                            }
                            Err(e) => return Err(ExecError::Eval(e.to_string())),
                        }
                    }

                    let mut g = Graph::new_ref(engine);
                    let (_, node_id) = g
                        .create_node(builder, ctx.fs)
                        .map_err(|e| ExecError::Eval(e.to_string()))?;

                    // Bind the created node.
                    if let Some(var) = &n.variable
                        && let Some(nv) = load_node_value(engine, node_id, ctx.fs)?
                    {
                        row.insert(var.clone(), nv);
                    }
                }
                PatternElement::Relationship(_) => {
                    // Relationships are created in the second pass below, which
                    // pairs each relationship with its already-bound source and
                    // target nodes from the row.
                }
            }
        }

        // Second pass: create relationships between sequential node pairs.
        let elems = &self.pattern.elements;
        let mut i = 0;
        while i + 2 < elems.len() {
            if let (
                PatternElement::Node(src_node),
                PatternElement::Relationship(rel),
                PatternElement::Node(tgt_node),
            ) = (&elems[i], &elems[i + 1], &elems[i + 2])
            {
                // Get source and target node ids from the row.
                let src_id = src_node
                    .variable
                    .as_ref()
                    .and_then(|v| row.get(v))
                    .and_then(|val| match val {
                        Value::Node(n) => Some(n.id),
                        Value::Integer(id) => Some(*id as u64),
                        _ => None,
                    });
                let tgt_id = tgt_node
                    .variable
                    .as_ref()
                    .and_then(|v| row.get(v))
                    .and_then(|val| match val {
                        Value::Node(n) => Some(n.id),
                        Value::Integer(id) => Some(*id as u64),
                        _ => None,
                    });

                if let (Some(src_id), Some(tgt_id)) = (src_id, tgt_id) {
                    let type_id = rel
                        .types
                        .first()
                        .map(|t| {
                            engine
                                .catalog()
                                .write()
                                .expect("catalog write lock poisoned")
                                .get_or_create_rel_type(t)
                        })
                        .unwrap_or(0u32);

                    let mut builder = RelationshipBuilder::new()
                        .from(src_id)
                        .to(tgt_id)
                        .rel_type(type_id);

                    let eval_ctx2 = row_to_eval_context(row, ctx);
                    for (k, v_expr) in &rel.properties {
                        match evaluate(v_expr, &eval_ctx2) {
                            Ok(val) => {
                                if let Some(prop) = value_to_property(&val) {
                                    builder = builder.property(k.clone(), prop);
                                }
                            }
                            Err(e) => return Err(ExecError::Eval(e.to_string())),
                        }
                    }

                    let mut g = Graph::new_ref(engine);
                    let (_, edge_id) = g
                        .create_relationship(builder, ctx.fs)
                        .map_err(|e| ExecError::Eval(e.to_string()))?;

                    if let Some(rv) = &rel.variable {
                        row.insert(
                            rv.clone(),
                            edge_record_id_to_value(edge_id, type_id, src_id, tgt_id),
                        );
                    }
                }
                i += 2;
            } else {
                i += 1;
            }
        }

        Ok(())
    }
}

impl PhysicalOperator for CreateOp {
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        match &mut self.input {
            Some(input) => {
                // Take input reference temporarily to avoid borrow conflict.
                match input.next_row(ctx)? {
                    Some(mut row) => {
                        self.execute_create(&mut row, ctx)?;
                        Ok(Some(row))
                    }
                    None => Ok(None),
                }
            }
            None => {
                if self.emitted {
                    return Ok(None);
                }
                self.emitted = true;
                let mut row = empty_row();
                self.execute_create(&mut row, ctx)?;
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
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
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

/// Variable-length path expansion using BFS.
///
/// For each input row, expands from the bound start node through `min_hops`
/// to `max_hops` relationships.  No repeated relationship edges (openCypher
/// semantics for `[*m..n]`).
pub struct VarLenExpandOp {
    input: Box<dyn PhysicalOperator>,
    direction: crate::cypher::ast::Direction,
    rel_types: Vec<String>,
    from_variable: String,
    rel_variable: Option<String>,
    end_node_variable: Option<String>,
    min_hops: u32,
    max_hops: Option<u32>,
    /// Pending result rows.
    pending: Vec<(u64, Vec<crate::graph::record::EdgeRecord>, Row)>,
    pending_idx: usize,
}

impl VarLenExpandOp {
    // Each parameter maps directly to a distinct, independent operator field
    // (input, direction, types, three variable bindings, and the hop bounds);
    // grouping them into a config struct would only add an indirection without
    // reducing the genuine arity, so the lint is suppressed for this constructor.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        input: Box<dyn PhysicalOperator>,
        direction: crate::cypher::ast::Direction,
        rel_types: Vec<String>,
        from_variable: impl Into<String>,
        rel_variable: Option<String>,
        end_node_variable: Option<String>,
        min_hops: u32,
        max_hops: Option<u32>,
    ) -> Self {
        Self {
            input,
            direction,
            rel_types,
            from_variable: from_variable.into(),
            rel_variable,
            end_node_variable,
            min_hops,
            max_hops,
            pending: Vec::new(),
            pending_idx: 0,
        }
    }

    /// BFS from `start_id`, collecting all reachable (end_node_id, path_edges)
    /// pairs within the hop bounds.
    fn bfs(
        &self,
        start_id: u64,
        type_ids: &[u64],
        ctx: &ExecutionContext,
        input_row: &Row,
    ) -> Result<Vec<(u64, Vec<crate::graph::record::EdgeRecord>, Row)>, ExecError> {
        use std::collections::VecDeque;
        let max = self.max_hops.unwrap_or(u32::MAX);

        // Queue entries: (current_node_id, path_edges, visited_edge_ids).
        let mut queue: VecDeque<(
            u64,
            Vec<crate::graph::record::EdgeRecord>,
            std::collections::HashSet<u64>,
        )> = VecDeque::new();
        queue.push_back((start_id, Vec::new(), std::collections::HashSet::new()));

        let mut results = Vec::new();

        while let Some((node_id, path, visited)) = queue.pop_front() {
            let hops = path.len() as u32;
            if hops >= max {
                if hops >= self.min_hops {
                    results.push((node_id, path, input_row.clone()));
                }
                continue;
            }

            if hops >= self.min_hops {
                results.push((node_id, path.clone(), input_row.clone()));
            }

            // Expand edges.
            let edges = match self.direction {
                crate::cypher::ast::Direction::Outgoing => ctx
                    .engine
                    .scan_outgoing_edges(node_id, type_ids, ctx.fs)
                    .map_err(|e| ExecError::Eval(e.to_string()))?,
                crate::cypher::ast::Direction::Incoming => ctx
                    .engine
                    .scan_incoming_edges(node_id, type_ids, ctx.fs)
                    .map_err(|e| ExecError::Eval(e.to_string()))?,
                crate::cypher::ast::Direction::Both => {
                    let mut o = ctx
                        .engine
                        .scan_outgoing_edges(node_id, type_ids, ctx.fs)
                        .map_err(|e| ExecError::Eval(e.to_string()))?;
                    let i = ctx
                        .engine
                        .scan_incoming_edges(node_id, type_ids, ctx.fs)
                        .map_err(|e| ExecError::Eval(e.to_string()))?;
                    o.extend(i);
                    o
                }
            };

            for (edge, end_id) in edges {
                if visited.contains(&edge.edge_id) {
                    continue; // no repeated relationships
                }
                let mut new_path = path.clone();
                new_path.push(edge);
                let mut new_visited = visited.clone();
                new_visited.insert(edge.edge_id);
                queue.push_back((end_id, new_path, new_visited));
            }
        }

        Ok(results)
    }
}

impl PhysicalOperator for VarLenExpandOp {
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        loop {
            // Yield from pending first.
            if self.pending_idx < self.pending.len() {
                let (end_node_id, ref path, ref base_row) = self.pending[self.pending_idx].clone();
                self.pending_idx += 1;
                let mut row = base_row.clone();

                if let Some(ev) = &self.end_node_variable
                    && let Ok(Some(nv)) = load_node_value(ctx.engine, end_node_id, ctx.fs)
                {
                    row.insert(ev.clone(), nv);
                }

                if let Some(rv) = &self.rel_variable {
                    // Bind the path as a list of relationship values.
                    let rels: Vec<Value> = path
                        .iter()
                        .map(|e| edge_record_to_value(e, ctx.engine, ctx.fs))
                        .collect();
                    row.insert(rv.clone(), Value::List(rels));
                }
                return Ok(Some(row));
            }

            // Need more input.
            let Some(input_row) = self.input.next_row(ctx)? else {
                return Ok(None);
            };

            let start_id = match input_row.get(&self.from_variable) {
                Some(Value::Node(n)) => n.id,
                Some(Value::Integer(id)) => *id as u64,
                _ => continue,
            };

            let type_ids: Vec<u64> = self
                .rel_types
                .iter()
                .filter_map(|t| {
                    ctx.engine.storage_label_id_for(t).or_else(|| {
                        ctx.engine
                            .catalog()
                            .read()
                            .ok()
                            .and_then(|c| c.rel_type_id(t))
                            .map(|id| id as u64)
                    })
                })
                .collect();

            self.pending = self.bfs(start_id, &type_ids, ctx, &input_row)?;
            self.pending_idx = 0;
        }
    }
    fn reset(&mut self) {
        self.pending.clear();
        self.pending_idx = 0;
        self.input.reset();
    }
}

/// Scan a single node by its internal id.
pub struct NodeByIdScanOp {
    node_id: u64,
    emitted: bool,
}

impl NodeByIdScanOp {
    pub fn new(node_id: u64) -> Self {
        Self {
            node_id,
            emitted: false,
        }
    }
}

impl PhysicalOperator for NodeByIdScanOp {
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        match load_node_value(ctx.engine, self.node_id, ctx.fs)? {
            Some(nv) => {
                let mut row = empty_row();
                row.insert("_node".to_string(), nv);
                Ok(Some(row))
            }
            None => Ok(None),
        }
    }
    fn reset(&mut self) {
        self.emitted = false;
    }
}

/// Nested-loop Apply: for each outer row, iterate the inner plan (re-seeded).
pub struct ApplyOp {
    outer: Box<dyn PhysicalOperator>,
    inner: Box<dyn PhysicalOperator>,
    /// Current outer row we are scanning inner for.
    current_outer: Option<Row>,
}

impl ApplyOp {
    pub fn new(outer: Box<dyn PhysicalOperator>, inner: Box<dyn PhysicalOperator>) -> Self {
        Self {
            outer,
            inner,
            current_outer: None,
        }
    }
}

impl PhysicalOperator for ApplyOp {
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        loop {
            if self.current_outer.is_none() {
                self.current_outer = self.outer.next_row(ctx)?;
                if self.current_outer.is_none() {
                    return Ok(None);
                }
                self.inner.reset();
            }
            match self.inner.next_row(ctx)? {
                Some(inner_row) => {
                    let mut merged = self.current_outer.as_ref().unwrap().clone();
                    merged.extend(inner_row);
                    return Ok(Some(merged));
                }
                None => {
                    self.current_outer = None;
                }
            }
        }
    }
    fn reset(&mut self) {
        self.outer.reset();
        self.inner.reset();
        self.current_outer = None;
    }
}

/// Hash join: hash left side on join keys, probe right side.
pub struct HashJoinOp {
    left: Box<dyn PhysicalOperator>,
    right: Box<dyn PhysicalOperator>,
    join_keys: Vec<String>,
    /// Materialised hash table: key_values → list of left rows.
    hash_table: Option<HashMap<Vec<String>, Vec<Row>>>,
    /// Right-side cursor.
    right_rows: Vec<Row>,
    right_idx: usize,
    /// Current right row + matching left rows.
    current_matches: Vec<Row>,
    current_match_idx: usize,
}

impl HashJoinOp {
    pub fn new(
        left: Box<dyn PhysicalOperator>,
        right: Box<dyn PhysicalOperator>,
        join_keys: Vec<String>,
    ) -> Self {
        Self {
            left,
            right,
            join_keys,
            hash_table: None,
            right_rows: Vec::new(),
            right_idx: 0,
            current_matches: Vec::new(),
            current_match_idx: 0,
        }
    }
}

impl PhysicalOperator for HashJoinOp {
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        // Build hash table from left side on first call.
        if self.hash_table.is_none() {
            let mut ht: HashMap<Vec<String>, Vec<Row>> = HashMap::new();
            while let Some(row) = self.left.next_row(ctx)? {
                let key: Vec<String> = self
                    .join_keys
                    .iter()
                    .map(|k| row.get(k).map(|v| v.to_cypher_string()).unwrap_or_default())
                    .collect();
                ht.entry(key).or_default().push(row);
            }
            // Materialize right side.
            while let Some(row) = self.right.next_row(ctx)? {
                self.right_rows.push(row);
            }
            self.hash_table = Some(ht);
        }
        // Drain current_matches first.
        loop {
            if self.current_match_idx < self.current_matches.len() {
                let row = self.current_matches[self.current_match_idx].clone();
                self.current_match_idx += 1;
                return Ok(Some(row));
            }
            if self.right_idx >= self.right_rows.len() {
                return Ok(None);
            }
            let right_row = &self.right_rows[self.right_idx];
            self.right_idx += 1;
            let key: Vec<String> = self
                .join_keys
                .iter()
                .map(|k| {
                    right_row
                        .get(k)
                        .map(|v| v.to_cypher_string())
                        .unwrap_or_default()
                })
                .collect();
            let ht = self.hash_table.as_ref().unwrap();
            if let Some(left_rows) = ht.get(&key) {
                self.current_matches = left_rows
                    .iter()
                    .map(|lr| {
                        let mut merged = lr.clone();
                        merged.extend(right_row.clone());
                        merged
                    })
                    .collect();
                self.current_match_idx = 0;
            } else {
                self.current_matches.clear();
                self.current_match_idx = 0;
            }
        }
    }
    fn reset(&mut self) {
        self.hash_table = None;
        self.right_rows.clear();
        self.right_idx = 0;
        self.current_matches.clear();
        self.current_match_idx = 0;
        self.left.reset();
        self.right.reset();
    }
}

/// UNWIND list to individual rows.
pub struct UnwindOp {
    expression: Expression,
    variable: String,
    input: Box<dyn PhysicalOperator>,
    /// Pending (element, input_row) pairs.
    pending: Vec<(Value, Row)>,
    pending_idx: usize,
}

impl UnwindOp {
    pub fn new(expression: Expression, variable: String, input: Box<dyn PhysicalOperator>) -> Self {
        Self {
            expression,
            variable,
            input,
            pending: Vec::new(),
            pending_idx: 0,
        }
    }
}

impl PhysicalOperator for UnwindOp {
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        use crate::cypher::interpreter::evaluate;
        loop {
            if self.pending_idx < self.pending.len() {
                let (ref elem, ref base_row) = self.pending[self.pending_idx].clone();
                self.pending_idx += 1;
                let mut row = base_row.clone();
                row.insert(self.variable.clone(), elem.clone());
                return Ok(Some(row));
            }
            let Some(input_row) = self.input.next_row(ctx)? else {
                return Ok(None);
            };
            let eval_ctx = row_to_eval_context(&input_row, ctx);
            let list_val = evaluate(&self.expression, &eval_ctx)
                .map_err(|e| ExecError::Eval(e.to_string()))?;
            self.pending.clear();
            self.pending_idx = 0;
            match list_val {
                Value::List(items) => {
                    for item in items {
                        self.pending.push((item, input_row.clone()));
                    }
                }
                Value::Null => {
                    // UNWIND NULL yields no rows for this input row.
                }
                other => {
                    // Non-list: yield as single element.
                    self.pending.push((other, input_row));
                }
            }
        }
    }
    fn reset(&mut self) {
        self.pending.clear();
        self.pending_idx = 0;
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
    pub fn new(
        expressions: Vec<Expression>,
        detach: bool,
        input: Box<dyn PhysicalOperator>,
    ) -> Self {
        Self {
            expressions,
            detach,
            input,
        }
    }
}

impl PhysicalOperator for DeleteOp {
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        use crate::cypher::interpreter::evaluate;
        // Process each input row and delete the targeted entities.
        while let Some(row) = self.input.next_row(ctx)? {
            let eval_ctx = row_to_eval_context(&row, ctx);
            // SAFETY: single-threaded execution; exclusive engine access.
            let engine = unsafe { &mut *ctx.engine_ptr_mut() };
            for expr in &self.expressions {
                let val = evaluate(expr, &eval_ctx).map_err(|e| ExecError::Eval(e.to_string()))?;
                match val {
                    Value::Node(n) => {
                        if self.detach {
                            // DETACH: delete all outgoing + incoming edges first.
                            let out = engine
                                .scan_outgoing_edges(n.id, &[], ctx.fs)
                                .map_err(|e| ExecError::Eval(e.to_string()))?;
                            for (edge, _) in out {
                                let _ = engine.delete_edge(edge.edge_id, ctx.fs);
                            }
                            let inc = engine
                                .scan_incoming_edges(n.id, &[], ctx.fs)
                                .map_err(|e| ExecError::Eval(e.to_string()))?;
                            for (edge, _) in inc {
                                let _ = engine.delete_edge(edge.edge_id, ctx.fs);
                            }
                        }
                        engine
                            .delete_node(n.id, ctx.fs)
                            .map_err(|e| ExecError::Eval(e.to_string()))?;
                    }
                    Value::Relationship(r) => {
                        engine
                            .delete_edge(r.id, ctx.fs)
                            .map_err(|e| ExecError::Eval(e.to_string()))?;
                    }
                    Value::Null => {} // silently skip NULL
                    other => {
                        return Err(ExecError::Eval(format!(
                            "DELETE requires a node or relationship, got '{}'",
                            other.type_name()
                        )));
                    }
                }
            }
        }
        Ok(None)
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
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        match self.input.next_row(ctx)? {
            None => Ok(None),
            Some(mut row) => {
                apply_set_items_durable(&mut row, &self.items, ctx)?;
                Ok(Some(row))
            }
        }
    }

    fn reset(&mut self) {
        self.input.reset();
    }
}

/// Apply a list of `SET` items to `row`, mutating the in-row entity values and
/// flushing every touched node/relationship's full property map to durable
/// storage within the current statement (rmp Task 191).
///
/// Shared by [`SetOp`] and [`MergeOp`]'s ON CREATE / ON MATCH branches.
///
/// Property values that evaluate to `NULL` remove the property (openCypher
/// semantics).  `SET :Label` updates the in-row labels only — durable
/// multi-label storage is not yet modelled (the node record carries a single
/// `label_id`).  Map forms `n += {map}` (merge) and `n = {map}` (replace) are
/// supported.
fn apply_set_items_durable(
    row: &mut Row,
    items: &[SetItem],
    ctx: &ExecutionContext,
) -> Result<(), ExecError> {
    use crate::cypher::ast::Expression;
    use crate::cypher::interpreter::evaluate;

    if items.is_empty() {
        return Ok(());
    }

    // Track which row variables had their properties mutated so each entity's
    // full property map is flushed exactly once, after all items are applied.
    let mut mutated: Vec<String> = Vec::new();
    let mark_mutated = |var: &str, mutated: &mut Vec<String>| {
        if !mutated.iter().any(|v| v == var) {
            mutated.push(var.to_string());
        }
    };

    for item in items {
        let eval_ctx = row_to_eval_context(row, ctx);
        match item {
            SetItem::Property { target, value } => {
                let new_val =
                    evaluate(value, &eval_ctx).map_err(|e| ExecError::Eval(e.to_string()))?;
                if let Expression::PropertyAccess { base, property, .. } = target.as_ref()
                    && let Expression::Variable(var) = base.as_ref()
                    && let Some(val) = row.get_mut(var)
                {
                    match val {
                        Value::Node(n) => {
                            if matches!(new_val, Value::Null) {
                                n.properties.remove(property);
                            } else {
                                n.properties.insert(property.clone(), new_val);
                            }
                            mark_mutated(var, &mut mutated);
                        }
                        Value::Relationship(r) => {
                            if matches!(new_val, Value::Null) {
                                r.properties.remove(property);
                            } else {
                                r.properties.insert(property.clone(), new_val);
                            }
                            mark_mutated(var, &mut mutated);
                        }
                        _ => {}
                    }
                }
            }
            SetItem::Label { variable, labels } => {
                if let Some(Value::Node(n)) = row.get_mut(variable) {
                    for label in labels {
                        if !n.labels.contains(label) {
                            n.labels.push(label.clone());
                        }
                    }
                }
            }
            SetItem::Merge { variable, value } => {
                let new_val =
                    evaluate(value, &eval_ctx).map_err(|e| ExecError::Eval(e.to_string()))?;
                if let Value::Map(props) = new_val
                    && let Some(Value::Node(n)) = row.get_mut(variable)
                {
                    for (k, v) in props {
                        if matches!(v, Value::Null) {
                            n.properties.remove(&k);
                        } else {
                            n.properties.insert(k, v);
                        }
                    }
                    mark_mutated(variable, &mut mutated);
                }
            }
            SetItem::Replace { variable, value } => {
                let new_val =
                    evaluate(value, &eval_ctx).map_err(|e| ExecError::Eval(e.to_string()))?;
                if let Value::Map(props) = new_val
                    && let Some(Value::Node(n)) = row.get_mut(variable)
                {
                    n.properties.clear();
                    for (k, v) in props {
                        if !matches!(v, Value::Null) {
                            n.properties.insert(k, v);
                        }
                    }
                    mark_mutated(variable, &mut mutated);
                }
            }
        }
    }

    if mutated.is_empty() {
        return Ok(());
    }

    // SAFETY: single-threaded execution; the context holds the sole mutable
    // engine borrow. See ExecutionContext::engine_ptr_mut for the invariants.
    let engine = unsafe { &mut *ctx.engine_ptr_mut() };
    for var in &mutated {
        match row.get(var) {
            Some(Value::Node(n)) => {
                let props = node_properties_to_storage(&n.properties);
                crate::graph::graph::rewrite_node_properties_engine(engine, n.id, props, ctx.fs)
                    .map_err(|e| ExecError::Eval(e.to_string()))?;
            }
            Some(Value::Relationship(r)) => {
                let props = node_properties_to_storage(&r.properties);
                crate::graph::graph::rewrite_edge_properties_engine(engine, r.id, props, ctx.fs)
                    .map_err(|e| ExecError::Eval(e.to_string()))?;
            }
            _ => {}
        }
    }
    Ok(())
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
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        use crate::cypher::ast::RemoveItem;
        match self.input.next_row(ctx)? {
            None => Ok(None),
            Some(mut row) => {
                for item in &self.items {
                    match item {
                        RemoveItem::Property { target } => {
                            if let crate::cypher::ast::Expression::PropertyAccess {
                                base,
                                property,
                                ..
                            } = target.as_ref()
                                && let crate::cypher::ast::Expression::Variable(var) = base.as_ref()
                                && let Some(val) = row.get_mut(var)
                            {
                                if let Value::Node(n) = val {
                                    n.properties.remove(property);
                                } else if let Value::Relationship(r) = val {
                                    r.properties.remove(property);
                                }
                            }
                        }
                        RemoveItem::Label { variable, labels } => {
                            if let Some(val) = row.get_mut(variable)
                                && let Value::Node(n) = val
                            {
                                n.labels.retain(|l| !labels.contains(l));
                            }
                        }
                    }
                }
                Ok(Some(row))
            }
        }
    }

    fn reset(&mut self) {
        self.input.reset();
    }
}

/// MERGE pattern with ON CREATE / ON MATCH actions.
///
/// Implements the single-node MERGE: match an existing node that equals the
/// pattern on its label *and every inline property*, otherwise create one with
/// those properties (persisted).  ON CREATE / ON MATCH SET items are then
/// applied durably via the same write path as `SET` (rmp Task 191).
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
        Self {
            pattern,
            on_create,
            on_match,
            input,
        }
    }

    /// Evaluate the inline property map of a MERGE node pattern against the
    /// surrounding row, returning `(key, value)` pairs (skipping entries that
    /// evaluate to an error).
    fn eval_inline_properties(
        node: &crate::cypher::ast::NodePattern,
        row: &Row,
        ctx: &ExecutionContext,
    ) -> Result<Vec<(String, Value)>, ExecError> {
        let eval_ctx = row_to_eval_context(row, ctx);
        let mut out = Vec::with_capacity(node.properties.len());
        for (k, expr) in &node.properties {
            let v = evaluate(expr, &eval_ctx).map_err(|e| ExecError::Eval(e.to_string()))?;
            out.push((k.clone(), v));
        }
        Ok(out)
    }

    /// Find an existing live node matching `label` (if any) and equal on *every*
    /// inline property in `wanted`.  Returns the bound node value, or `None`.
    fn find_full_match(
        node: &crate::cypher::ast::NodePattern,
        wanted: &[(String, Value)],
        ctx: &ExecutionContext,
    ) -> Result<Option<Value>, ExecError> {
        use crate::graph::record::node_flags;

        // Candidate set: nodes carrying the requested label, or every node when
        // the pattern is unlabelled.
        let records = match node.labels.first() {
            Some(label) => {
                let label_id = ctx.engine.storage_label_id_for(label).unwrap_or(0u64);
                ctx.engine
                    .scan_nodes_by_label(label_id, ctx.fs)
                    .map_err(|e| ExecError::Eval(e.to_string()))?
            }
            None => ctx
                .engine
                .scan_all_nodes(ctx.fs)
                .map_err(|e| ExecError::Eval(e.to_string()))?,
        };

        for record in records {
            if record.flags & node_flags::DELETED != 0 {
                continue;
            }
            let Some(Value::Node(candidate)) = load_node_value(ctx.engine, record.node_id, ctx.fs)?
            else {
                continue;
            };
            // Every inline property must be present and equal.
            let all_match = wanted.iter().all(|(k, want)| {
                candidate
                    .properties
                    .get(k)
                    .is_some_and(|have| values_equal_for_merge(have, want))
            });
            if all_match {
                return Ok(Some(Value::Node(candidate)));
            }
        }
        Ok(None)
    }
}

impl PhysicalOperator for MergeOp {
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        use crate::cypher::ast::PatternElement;
        use crate::graph::builder::NodeBuilder;
        use crate::graph::graph::Graph;

        let Some(input_row) = self.input.next_row(ctx)? else {
            return Ok(None);
        };

        // Locate the single node element of the MERGE pattern.  Relationship
        // MERGE is not yet modelled; a pattern with no node element passes the
        // row through unchanged.
        let Some(PatternElement::Node(node)) = self
            .pattern
            .elements
            .iter()
            .find(|e| matches!(e, PatternElement::Node(_)))
        else {
            return Ok(Some(input_row));
        };

        let mut row = input_row;

        // If the node variable is already bound upstream, treat it as a match
        // and keep the binding (MERGE over an already-bound variable).
        if let Some(var) = &node.variable
            && row.contains_key(var)
        {
            apply_set_items_durable(&mut row, &self.on_match, ctx)?;
            return Ok(Some(row));
        }

        // Evaluate the inline property predicate and look for a full match.
        let wanted = Self::eval_inline_properties(node, &row, ctx)?;
        let matched = Self::find_full_match(node, &wanted, ctx)?;

        if let Some(node_value) = matched {
            // MATCH branch: bind the existing node, apply ON MATCH.
            if let Some(var) = &node.variable {
                row.insert(var.clone(), node_value);
            }
            apply_set_items_durable(&mut row, &self.on_match, ctx)?;
            Ok(Some(row))
        } else {
            // CREATE branch: build the node with its inline properties so they
            // are persisted, bind it, then apply ON CREATE.
            // SAFETY: single-threaded execution; the context holds the sole
            // mutable engine borrow. See ExecutionContext::engine_ptr_mut.
            let engine = unsafe { &mut *ctx.engine_ptr_mut() };
            let label_id = node
                .labels
                .first()
                .map(|l| engine.catalog_label_id(l))
                .unwrap_or(0u32);
            let mut builder = NodeBuilder::new().label(label_id);
            for (k, v) in &wanted {
                if let Some(prop) = value_to_property(v) {
                    builder = builder.property(k.clone(), prop);
                }
            }
            let (_, node_id) = Graph::new_ref(engine)
                .create_node(builder, ctx.fs)
                .map_err(|e| ExecError::Eval(e.to_string()))?;
            if let Some(var) = &node.variable
                && let Some(nv) = load_node_value(engine, node_id, ctx.fs)?
            {
                row.insert(var.clone(), nv);
            }
            apply_set_items_durable(&mut row, &self.on_create, ctx)?;
            Ok(Some(row))
        }
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
    fn next_row(&mut self, ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
        if self.buffer.is_none() {
            let mut groups: Vec<(Vec<Value>, Vec<Row>)> = Vec::new();

            // Consume all input rows and partition by grouping keys.
            while let Some(row) = self.input.next_row(ctx)? {
                let eval_ctx = row_to_eval_context(&row, ctx);
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
                    let val =
                        compute_aggregate(&agg.function, &agg.argument, &rows, agg.distinct, ctx)?;
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
    exec_ctx: &ExecutionContext,
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
            let eval_ctx = row_to_eval_context(row, exec_ctx);
            // Skip rows where the argument evaluates to an error.
            if let Ok(v) = evaluate(arg, &eval_ctx) {
                values.push(v);
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
                sum = sum
                    .add(&v)
                    .ok_or_else(|| ExecError::Eval("type mismatch in sum".to_string()))?;
            }
            Ok(sum)
        }
        AggregateFunction::Avg => {
            if values.is_empty() {
                return Ok(Value::Null);
            }
            let mut sum = Value::Float(crate::graph::property::OrderedF64(0.0));
            for v in &values {
                sum = sum
                    .add(v)
                    .ok_or_else(|| ExecError::Eval("type mismatch in avg".to_string()))?;
            }
            let count = Value::Integer(values.len() as i64);
            sum.div(&count)
                .ok_or_else(|| ExecError::Eval("type mismatch in avg".to_string()))
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

/// Load a node from storage and return it as a `Value::Node`, with properties.
pub(crate) fn load_node_value(
    engine: &GraphStorageEngine,
    node_id: u64,
    fs: &dyn FileSystem,
) -> Result<Option<Value>, ExecError> {
    use crate::graph::graph::read_property_chain_engine;
    use crate::graph::record::node_flags;
    let record = match engine
        .get_node(node_id, fs)
        .map_err(|e| ExecError::Eval(e.to_string()))?
    {
        Some(r) if r.flags & node_flags::DELETED == 0 => r,
        _ => return Ok(None),
    };
    let raw_props = read_property_chain_engine(engine, record.first_property, fs)
        .map_err(|e| ExecError::Eval(e.to_string()))?;
    // Convert label_id to label name via catalog.
    let label_name = engine
        .catalog()
        .read()
        .expect("catalog read lock poisoned")
        .label_name(record.label_id)
        .map(|s| s.to_string());
    let labels = label_name.map(|l| vec![l]).unwrap_or_default();
    let mut properties = HashMap::new();
    for (k, prop) in raw_props {
        properties.insert(k, crate::cypher::value::Value::from_property(prop));
    }
    Ok(Some(Value::Node(crate::cypher::value::NodeValue {
        id: record.node_id,
        labels,
        properties,
    })))
}

/// Convert an [`EdgeRecord`] to `Value::Relationship`, resolving type name.
fn edge_record_to_value(
    edge: &crate::graph::record::EdgeRecord,
    engine: &GraphStorageEngine,
    fs: &dyn FileSystem,
) -> Value {
    use crate::graph::graph::read_property_chain_engine;
    let type_name = engine
        .catalog()
        .read()
        .expect("catalog read lock")
        .rel_type_name(edge.type_id)
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("TYPE_{}", edge.type_id));
    let raw_props = read_property_chain_engine(engine, edge.first_property, fs).unwrap_or_default();
    let mut properties = HashMap::new();
    for (k, prop) in raw_props {
        properties.insert(k, crate::cypher::value::Value::from_property(prop));
    }
    Value::Relationship(crate::cypher::value::RelationshipValue {
        id: edge.edge_id,
        rel_type: type_name,
        source_id: edge.source_id,
        target_id: edge.target_id,
        properties,
    })
}

/// Build a `Value::Relationship` from known ids (no property load).
fn edge_record_id_to_value(edge_id: u64, type_id: u32, source_id: u64, target_id: u64) -> Value {
    Value::Relationship(crate::cypher::value::RelationshipValue {
        id: edge_id,
        rel_type: format!("TYPE_{}", type_id),
        source_id,
        target_id,
        properties: HashMap::new(),
    })
}

/// Convert a row entity's runtime property map into the storage [`Property`]
/// map used by the durable property-chain writer.
///
/// Values that have no storable scalar representation (`List`, `Map`, `Node`,
/// etc.) are dropped — they are not yet supported by the on-disk property
/// codec, matching the CREATE path's behaviour.
fn node_properties_to_storage(
    props: &HashMap<String, Value>,
) -> HashMap<String, crate::graph::property::Property> {
    let mut out = HashMap::with_capacity(props.len());
    for (k, v) in props {
        if let Some(p) = value_to_property(v) {
            out.insert(k.clone(), p);
        }
    }
    out
}

/// Convert a runtime `Value` to a storage `Property`.
fn value_to_property(val: &Value) -> Option<crate::graph::property::Property> {
    match val {
        Value::Null => Some(crate::graph::property::Property::Null),
        Value::Boolean(b) => Some(crate::graph::property::Property::Boolean(*b)),
        Value::Integer(i) => Some(crate::graph::property::Property::Integer(*i)),
        Value::Float(f) => Some(crate::graph::property::Property::Float(*f)),
        Value::String(s) => Some(crate::graph::property::Property::String(s.clone())),
        _ => None,
    }
}

/// Build an [`EvalContext`] from a physical row and the surrounding execution
/// context.
///
/// The query-level parameter bindings carried by [`ExecutionContext`] are
/// copied into the [`EvalContext`] so that `$param` references resolve during
/// expression evaluation.
pub(crate) fn row_to_eval_context(row: &Row, exec_ctx: &ExecutionContext) -> EvalContext {
    let mut ctx = eval_context_with_params(exec_ctx);
    for (k, v) in row {
        ctx = ctx.bind(k.clone(), v.clone());
    }
    ctx
}

/// Build an empty [`EvalContext`] pre-seeded with the execution context's
/// query parameters.
pub(crate) fn eval_context_with_params(exec_ctx: &ExecutionContext) -> EvalContext {
    let mut ctx = EvalContext::new();
    for (k, v) in &exec_ctx.parameters {
        ctx = ctx.bind_parameter(k.clone(), v.clone());
    }
    ctx
}

/// Cypher value equality used to match a stored property against a MERGE inline
/// property predicate.  Returns `true` only when both sides are non-null and
/// compare equal under Cypher semantics (so `1 = 1.0`), matching how a MERGE
/// pattern's inline map filters candidate nodes.
fn values_equal_for_merge(have: &Value, want: &Value) -> bool {
    matches!(have.cypher_eq(want), Some(Value::Boolean(true)))
}

pub(crate) fn compare_values(a: &Value, b: &Value, ascending: bool) -> std::cmp::Ordering {
    let ord = a.cypher_compare(b).unwrap_or(std::cmp::Ordering::Equal);
    if ascending { ord } else { ord.reverse() }
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
        LogicalOperator::SingleRow => Box::new(SingleRowOp::new()),
        LogicalOperator::AllNodesScan => Box::new(AllNodesScanOp::new("_node")),
        LogicalOperator::NodeByLabelScan { label } => {
            Box::new(NodeByLabelScanOp::new("_node", label.clone()))
        }
        LogicalOperator::NodeByIdScan { node_id } => {
            // Point lookup: wrap in a filter over all-nodes-scan (TODO: real index seek).
            Box::new(NodeByIdScanOp::new(*node_id))
        }
        LogicalOperator::Expand {
            input,
            direction,
            rel_types,
            from_variable,
            rel_variable,
            end_node_variable,
        } => Box::new(ExpandOp::new(
            build_physical_operator(input),
            *direction,
            rel_types.clone(),
            from_variable.clone(),
            rel_variable.clone(),
            end_node_variable.clone(),
        )),
        LogicalOperator::VarLenExpand {
            input,
            direction,
            rel_types,
            from_variable,
            rel_variable,
            end_node_variable,
            min_hops,
            max_hops,
        } => Box::new(VarLenExpandOp::new(
            build_physical_operator(input),
            *direction,
            rel_types.clone(),
            from_variable.clone(),
            rel_variable.clone(),
            end_node_variable.clone(),
            *min_hops,
            *max_hops,
        )),
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
        LogicalOperator::Eager { input } => Box::new(EagerOp::new(build_physical_operator(input))),
        LogicalOperator::Delete {
            input,
            expressions,
            detach,
        } => Box::new(DeleteOp::new(
            expressions.clone(),
            *detach,
            build_physical_operator(input),
        )),
        LogicalOperator::Set { input, items } => {
            Box::new(SetOp::new(items.clone(), build_physical_operator(input)))
        }
        LogicalOperator::Remove { input, items } => {
            Box::new(RemoveOp::new(items.clone(), build_physical_operator(input)))
        }
        LogicalOperator::Apply { left, right } => Box::new(ApplyOp::new(
            build_physical_operator(left),
            build_physical_operator(right),
        )),
        LogicalOperator::Aggregate {
            input,
            grouping_keys,
            aggregations,
        } => Box::new(AggregateOp::new(
            grouping_keys.clone(),
            aggregations.clone(),
            build_physical_operator(input),
        )),
        LogicalOperator::HashJoin {
            left,
            right,
            join_keys,
        } => Box::new(HashJoinOp::new(
            build_physical_operator(left),
            build_physical_operator(right),
            join_keys.clone(),
        )),
        LogicalOperator::Merge {
            input,
            pattern,
            on_create,
            on_match,
        } => Box::new(MergeOp::new(
            pattern.clone(),
            on_create.clone(),
            on_match.clone(),
            build_physical_operator(input),
        )),
    }
}

// ------------------------------------------------------------------
// High-level execution entry point
// ------------------------------------------------------------------

/// Execute a [`LogicalPlan`] against the storage engine and return a
/// [`QueryResult`].
///
/// Column order follows the RETURN projection order as declared in the query.
/// The `plan` must have been built with [`crate::cypher::planner::plan`].
pub fn execute_plan(plan: &LogicalPlan, ctx: &ExecutionContext) -> Result<QueryResult, ExecError> {
    // Derive the expected column order from the root Project operator if present.
    let projection_columns = extract_projection_columns(&plan.root);

    let mut physical = build_physical_plan(plan);
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut columns: Vec<String> = Vec::new();

    // Pull the first row to set the column list.
    if let Some(first_row) = physical.next_row(ctx)? {
        if !projection_columns.is_empty() {
            // Use declared RETURN order.
            columns = projection_columns.clone();
        } else {
            // Fall back to sorted keys for operator sub-trees with no Project root.
            columns = first_row.keys().cloned().collect();
            columns.sort();
        }
        let values: Vec<Value> = columns
            .iter()
            .map(|c| first_row.get(c).cloned().unwrap_or(Value::Null))
            .collect();
        rows.push(values);
    } else if !projection_columns.is_empty() {
        // Empty result — still expose the columns from the RETURN clause.
        columns = projection_columns;
    }

    // Pull remaining rows.
    while let Some(row) = physical.next_row(ctx)? {
        let values: Vec<Value> = columns
            .iter()
            .map(|c| row.get(c).cloned().unwrap_or(Value::Null))
            .collect();
        rows.push(values);
    }

    Ok(QueryResult { columns, rows })
}

/// Walk the logical plan tree to find the projection column names in RETURN order.
fn extract_projection_columns(op: &LogicalOperator) -> Vec<String> {
    match op {
        LogicalOperator::Project { projections, .. } => projections
            .iter()
            .map(|p| p.alias.clone().unwrap_or_else(|| p.expression.to_string()))
            .collect(),
        // Descend through transparent wrappers.
        LogicalOperator::Aggregate {
            grouping_keys,
            aggregations,
            ..
        } => {
            let mut cols: Vec<String> = grouping_keys.iter().map(|e| e.to_string()).collect();
            cols.extend(aggregations.iter().map(|a| a.alias.clone()));
            cols
        }
        LogicalOperator::Limit { input, .. }
        | LogicalOperator::Skip { input, .. }
        | LogicalOperator::Sort { input, .. }
        | LogicalOperator::Filter { input, .. }
        | LogicalOperator::Eager { input } => extract_projection_columns(input),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::ast::{Expression, Literal, Projection};

    #[test]
    fn filter_op_keeps_true_rows() {
        let input = Box::new(MockOp::new(vec![
            vec![("x".to_string(), Value::Integer(5))]
                .into_iter()
                .collect(),
            vec![("x".to_string(), Value::Integer(15))]
                .into_iter()
                .collect(),
        ]));
        let mut filter = FilterOp::new(
            Expression::Comparison {
                span: None,
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
            vec![("a".to_string(), Value::Integer(1))]
                .into_iter()
                .collect(),
        ]));
        let mut proj = ProjectOp::new(
            vec![Projection {
                span: None,
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
            vec![("i".to_string(), Value::Integer(1))]
                .into_iter()
                .collect(),
            vec![("i".to_string(), Value::Integer(2))]
                .into_iter()
                .collect(),
            vec![("i".to_string(), Value::Integer(3))]
                .into_iter()
                .collect(),
        ]));
        let mut limit = LimitOp::new(Expression::Literal(Literal::Integer(2)), input);
        let ctx = mock_ctx();
        assert!(limit.next_row(&ctx).unwrap().is_some());
        assert!(limit.next_row(&ctx).unwrap().is_some());
        assert!(limit.next_row(&ctx).unwrap().is_none());
    }

    #[test]
    fn skip_op_drops_first_n() {
        let input = Box::new(MockOp::new(vec![
            vec![("i".to_string(), Value::Integer(1))]
                .into_iter()
                .collect(),
            vec![("i".to_string(), Value::Integer(2))]
                .into_iter()
                .collect(),
            vec![("i".to_string(), Value::Integer(3))]
                .into_iter()
                .collect(),
        ]));
        let mut skip = SkipOp::new(Expression::Literal(Literal::Integer(1)), input);
        let ctx = mock_ctx();
        let row = skip.next_row(&ctx).unwrap().unwrap();
        assert_eq!(row.get("i"), Some(&Value::Integer(2)));
        assert!(skip.next_row(&ctx).unwrap().is_some());
        assert!(skip.next_row(&ctx).unwrap().is_none());
    }

    #[test]
    fn aggregate_op_count_grouped() {
        let input = Box::new(MockOp::new(vec![
            vec![
                ("dept".to_string(), Value::String("a".to_string())),
                ("salary".to_string(), Value::Integer(100)),
            ]
            .into_iter()
            .collect(),
            vec![
                ("dept".to_string(), Value::String("a".to_string())),
                ("salary".to_string(), Value::Integer(200)),
            ]
            .into_iter()
            .collect(),
            vec![
                ("dept".to_string(), Value::String("b".to_string())),
                ("salary".to_string(), Value::Integer(300)),
            ]
            .into_iter()
            .collect(),
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
        let group_a = results
            .iter()
            .find(|r| r.get("dept") == Some(&Value::String("a".to_string())))
            .unwrap();
        assert_eq!(group_a.get("c"), Some(&Value::Integer(2)));
        let group_b = results
            .iter()
            .find(|r| r.get("dept") == Some(&Value::String("b".to_string())))
            .unwrap();
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
            fn next_row(&mut self, _ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
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
        fn next_row(&mut self, _ctx: &ExecutionContext) -> Result<Option<Row>, ExecError> {
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
        ExecutionContext::new(engine_ref, fs_ref)
    }
}
