//! `AnyStore` — the runtime-selected estate backend, as ONE concrete type.
//!
//! The engine (wicked-core) programs against `&dyn GraphStore` / `&dyn GraphRead`, but the actor
//! that OWNS the store must hold a single concrete value it can lend under EVERY store param style
//! the codebase uses — `&dyn GraphRead`, `&impl GraphRead` (anonymous generic), and
//! `&mut dyn GraphStore`. A trait object (`Box<dyn GraphStore>`) satisfies none of the read-generic
//! ones (`Box<dyn GraphStore>` does not implement `GraphRead`, and `&dyn GraphStore → &dyn GraphRead`
//! would need trait-upcasting at every call). `AnyStore` is a CONCRETE enum over the compiled
//! backends that forwards the whole `GraphRead`/`GraphWrite` surface to whichever backend it holds,
//! so `&AnyStore` / `&mut AnyStore` coerce cleanly to every param style, and it satisfies any
//! `S: GraphRead + GraphWrite` bound directly. `GraphStore` comes free via estate's blanket impl.
//!
//! Backends are added here as one enum arm + one `on_backend!` line — the same "one module + one
//! factory arm, zero caller changes" shape estate's own `open_store` factory uses.

#[cfg(feature = "postgres")]
use wicked_estate_store::PostgresStore;
use wicked_estate_store::SqliteStore;

use wicked_estate_core::{
    Annotation, Change, ChangeOp, Direction, Edge, GraphRead, GraphStats, GraphWrite,
    HistoricalEdge, Node, NodeSemantics, RepoInfo, Result, StoreCapabilities, Subgraph, SymbolId,
    SymbolQuery, TraversalSpec, UnresolvedRef,
};

/// The estate graph backend chosen at runtime, as one concrete type. See the module docs.
pub enum AnyStore {
    /// The zero-infra default: estate's embedded SQLite backend.
    Sqlite(SqliteStore),
    /// estate's Postgres backend — compiled ONLY under the `postgres` feature.
    #[cfg(feature = "postgres")]
    Postgres(PostgresStore),
}

impl AnyStore {
    /// Wrap an already-open SQLite backend.
    pub fn sqlite(store: SqliteStore) -> Self {
        AnyStore::Sqlite(store)
    }

    /// Wrap an already-open Postgres backend.
    #[cfg(feature = "postgres")]
    pub fn postgres(store: PostgresStore) -> Self {
        AnyStore::Postgres(store)
    }
}

/// Dispatch a call to whichever backend this `AnyStore` currently holds. The `postgres` arm is
/// compiled only under the feature, so the default build is a trivial single-arm match.
macro_rules! on_backend {
    ($self:ident, $s:ident => $call:expr) => {
        match $self {
            AnyStore::Sqlite($s) => $call,
            #[cfg(feature = "postgres")]
            AnyStore::Postgres($s) => $call,
        }
    };
}

impl GraphRead for AnyStore {
    fn capabilities(&self) -> StoreCapabilities {
        on_backend!(self, s => s.capabilities())
    }
    fn get_node(&self, id: &SymbolId) -> Result<Option<Node>> {
        on_backend!(self, s => s.get_node(id))
    }
    fn find_symbols(&self, query: &SymbolQuery) -> Result<Vec<Node>> {
        on_backend!(self, s => s.find_symbols(query))
    }
    fn neighbors(&self, id: &SymbolId, dir: Direction) -> Result<Vec<Edge>> {
        on_backend!(self, s => s.neighbors(id, dir))
    }
    fn traverse(&self, start: &SymbolId, spec: &TraversalSpec) -> Result<Subgraph> {
        on_backend!(self, s => s.traverse(start, spec))
    }
    fn traverse_multi(&self, starts: &[SymbolId], spec: &TraversalSpec) -> Result<Subgraph> {
        on_backend!(self, s => s.traverse_multi(starts, spec))
    }
    fn all_nodes(&self) -> Result<Vec<Node>> {
        on_backend!(self, s => s.all_nodes())
    }
    fn all_edges(&self) -> Result<Vec<Edge>> {
        on_backend!(self, s => s.all_edges())
    }
    fn unresolved_refs_for_name(&self, name: &str) -> Result<Vec<UnresolvedRef>> {
        on_backend!(self, s => s.unresolved_refs_for_name(name))
    }
    fn file_digest(&self, file: &str) -> Result<Option<String>> {
        on_backend!(self, s => s.file_digest(file))
    }
    fn file_git_sha(&self, file: &str) -> Result<Option<String>> {
        on_backend!(self, s => s.file_git_sha(file))
    }
    fn repo_info(&self) -> Result<Option<RepoInfo>> {
        on_backend!(self, s => s.repo_info())
    }
    fn edge_history(&self, file: &str) -> Result<Vec<HistoricalEdge>> {
        on_backend!(self, s => s.edge_history(file))
    }
    fn file_content(&self, file: &str) -> Result<Option<String>> {
        on_backend!(self, s => s.file_content(file))
    }
    fn symbol_source(&self, node: &Node) -> Result<Option<String>> {
        on_backend!(self, s => s.symbol_source(node))
    }
    fn changes_since(&self, cursor: u64) -> Result<Vec<Change>> {
        on_backend!(self, s => s.changes_since(cursor))
    }
    fn node_semantics(&self, symbol: &SymbolId) -> Result<Option<NodeSemantics>> {
        on_backend!(self, s => s.node_semantics(symbol))
    }
    fn find_by_requirement(&self, requirement: &str) -> Result<Vec<Node>> {
        on_backend!(self, s => s.find_by_requirement(requirement))
    }
    fn annotations(&self, symbol: &SymbolId) -> Result<Vec<Annotation>> {
        on_backend!(self, s => s.annotations(symbol))
    }
    fn annotations_by_type(&self, ty: &str) -> Result<Vec<(SymbolId, Annotation)>> {
        on_backend!(self, s => s.annotations_by_type(ty))
    }
    fn annotations_stale_since(&self, cutoff: i64) -> Result<Vec<(SymbolId, Annotation)>> {
        on_backend!(self, s => s.annotations_stale_since(cutoff))
    }
    fn symbol_epoch(&self, id: &SymbolId) -> Result<Option<u64>> {
        on_backend!(self, s => s.symbol_epoch(id))
    }
    fn stats(&self) -> Result<GraphStats> {
        on_backend!(self, s => s.stats())
    }
}

impl GraphWrite for AnyStore {
    fn begin_batch(&mut self) -> Result<()> {
        on_backend!(self, s => s.begin_batch())
    }
    fn commit_batch(&mut self) -> Result<()> {
        on_backend!(self, s => s.commit_batch())
    }
    fn upsert_nodes(&mut self, nodes: &[Node]) -> Result<()> {
        on_backend!(self, s => s.upsert_nodes(nodes))
    }
    fn upsert_edges(&mut self, edges: &[Edge]) -> Result<()> {
        on_backend!(self, s => s.upsert_edges(edges))
    }
    fn upsert_unresolved_refs(&mut self, refs: &[UnresolvedRef]) -> Result<()> {
        on_backend!(self, s => s.upsert_unresolved_refs(refs))
    }
    fn remove_file(&mut self, file: &str) -> Result<()> {
        on_backend!(self, s => s.remove_file(file))
    }
    fn set_file_digest(&mut self, file: &str, digest: &str) -> Result<()> {
        on_backend!(self, s => s.set_file_digest(file, digest))
    }
    fn set_repo_info(&mut self, info: &RepoInfo) -> Result<()> {
        on_backend!(self, s => s.set_repo_info(info))
    }
    fn set_file_content(&mut self, file: &str, text: &str) -> Result<()> {
        on_backend!(self, s => s.set_file_content(file, text))
    }
    fn prune_dangling_edges(&mut self) -> Result<usize> {
        on_backend!(self, s => s.prune_dangling_edges())
    }
    fn log_change(&mut self, op: ChangeOp, target: &str) -> Result<()> {
        on_backend!(self, s => s.log_change(op, target))
    }
    fn set_node_semantics(
        &mut self,
        symbol: &SymbolId,
        description: Option<&str>,
        requirement: Option<&str>,
        requirement_validated: Option<bool>,
    ) -> Result<()> {
        on_backend!(self, s => s.set_node_semantics(symbol, description, requirement, requirement_validated))
    }
    fn annotate(&mut self, symbol: &SymbolId, annotation: Annotation) -> Result<()> {
        on_backend!(self, s => s.annotate(symbol, annotation))
    }
    fn delete_annotations(
        &mut self,
        symbol: &SymbolId,
        ty: Option<&str>,
        key: &str,
    ) -> Result<usize> {
        on_backend!(self, s => s.delete_annotations(symbol, ty, key))
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        open_store_any, synthetic_symbol, GraphRead, GraphStore, GraphWrite, Language, Location,
        Node, NodeKind, Span, SYMBOL_SCHEME,
    };

    fn seed_via_generic<S: GraphRead + GraphWrite>(store: &mut S) {
        let sym = synthetic_symbol("anystore_test", "n1");
        let node = Node::new(
            sym,
            NodeKind::Other("anystore_test".to_string()),
            "n1".to_string(),
            Language::new(SYMBOL_SCHEME),
            Location::new("anystore_test/n1".to_string(), Span::ZERO),
        );
        store.begin_batch().unwrap();
        store.upsert_nodes(&[node]).unwrap();
        store.commit_batch().unwrap();
    }

    fn reads_back_via_dyn(store: &dyn GraphStore) -> bool {
        store
            .get_node(&synthetic_symbol("anystore_test", "n1"))
            .unwrap()
            .is_some()
    }

    #[test]
    fn any_store_sqlite_bridges_generic_bound_and_dyn_object() {
        // The whole point: ONE concrete `AnyStore` value written through an `S: GraphRead +
        // GraphWrite` bound reads back through a `&dyn GraphStore` object — the two call styles the
        // engine mixes. If AnyStore's forwarding were wrong, the read would miss.
        let mut store = open_store_any(Some(":memory:")).expect("open in-memory AnyStore");
        seed_via_generic(&mut store);
        assert!(
            reads_back_via_dyn(&store),
            "node written via a generic S bound must read back via &dyn GraphStore"
        );
    }

    // §5 backend-parity: AnyStore must bridge the SAME generic→dyn call styles through Postgres.
    // Skips when TEST_POSTGRES_URL is absent (local dev without a running Postgres); the CI
    // `postgres-parity` job always sets it so this is a hard runtime assertion there (core#30).
    #[cfg(feature = "postgres")]
    #[test]
    fn postgres_open_store_any_round_trip() {
        let url = match std::env::var("TEST_POSTGRES_URL") {
            Ok(u) if !u.is_empty() => u,
            _ => {
                eprintln!("TEST_POSTGRES_URL not set — skipping postgres round-trip");
                return;
            }
        };
        let mut store =
            open_store_any(Some(&url)).expect("open postgres AnyStore via TEST_POSTGRES_URL");
        seed_via_generic(&mut store);
        assert!(
            reads_back_via_dyn(&store),
            "node written via generic S bound must read back via &dyn GraphStore on Postgres backend"
        );
    }

    // §5 backend-parity note: the DEFAULT build (no `postgres` feature) must REJECT a postgres spec
    // loudly — never silently open SQLite instead (deny-dominates). Real parity runs only under
    // `--features postgres` against a provisioned Postgres, so this asserts the rejection path.
    #[test]
    #[cfg(not(feature = "postgres"))]
    fn postgres_spec_fails_closed_without_feature() {
        for spec in [
            "postgres://localhost/estate",
            "postgresql://localhost/estate",
        ] {
            let result = open_store_any(Some(spec));
            assert!(
                result.is_err(),
                "a postgres spec must be refused when built without the feature"
            );
            // `.err().unwrap()` (not `expect_err`) avoids requiring `AnyStore: Debug`.
            let err = result.err().unwrap().to_string();
            assert!(
                err.contains("postgres` feature"),
                "expected a loud fail-closed error naming the feature, got: {err}"
            );
        }
    }
}
