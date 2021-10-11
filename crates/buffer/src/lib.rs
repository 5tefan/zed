mod anchor;
mod highlight_map;
mod language;
mod operation_queue;
mod point;
#[cfg(any(test, feature = "test-support"))]
pub mod random_char_iter;
pub mod rope;
mod selection;
#[cfg(test)]
mod tests;

pub use anchor::*;
use anyhow::{anyhow, Result};
use clock::ReplicaId;
use gpui::{AppContext, Entity, ModelContext, MutableAppContext, Task};
pub use highlight_map::{HighlightId, HighlightMap};
use language::Tree;
pub use language::{AutoclosePair, Language, LanguageConfig, LanguageRegistry};
use lazy_static::lazy_static;
use operation_queue::OperationQueue;
use parking_lot::Mutex;
pub use point::*;
#[cfg(any(test, feature = "test-support"))]
pub use random_char_iter::*;
pub use rope::{Chunks, Rope, TextSummary};
use rpc::proto;
use seahash::SeaHasher;
pub use selection::*;
use similar::{ChangeTag, TextDiff};
use smol::future::yield_now;
use std::{
    any::Any,
    cell::RefCell,
    cmp,
    collections::BTreeMap,
    convert::{TryFrom, TryInto},
    ffi::OsString,
    future::Future,
    hash::BuildHasher,
    iter::Iterator,
    ops::{Deref, DerefMut, Range},
    path::{Path, PathBuf},
    str,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use sum_tree::{Bias, FilterCursor, SumTree};
use tree_sitter::{InputEdit, Parser, QueryCursor};

pub trait File {
    fn worktree_id(&self) -> usize;

    fn entry_id(&self) -> Option<usize>;

    fn set_entry_id(&mut self, entry_id: Option<usize>);

    fn mtime(&self) -> SystemTime;

    fn set_mtime(&mut self, mtime: SystemTime);

    fn path(&self) -> &Arc<Path>;

    fn set_path(&mut self, path: Arc<Path>);

    fn full_path(&self, cx: &AppContext) -> PathBuf;

    /// Returns the last component of this handle's absolute path. If this handle refers to the root
    /// of its worktree, then this method will return the name of the worktree itself.
    fn file_name<'a>(&'a self, cx: &'a AppContext) -> Option<OsString>;

    fn is_deleted(&self) -> bool;

    fn save(
        &self,
        buffer_id: u64,
        text: Rope,
        version: clock::Global,
        cx: &mut MutableAppContext,
    ) -> Task<Result<(clock::Global, SystemTime)>>;

    fn buffer_updated(&self, buffer_id: u64, operation: Operation, cx: &mut MutableAppContext);

    fn buffer_removed(&self, buffer_id: u64, cx: &mut MutableAppContext);

    fn boxed_clone(&self) -> Box<dyn File>;

    fn as_any(&self) -> &dyn Any;
}

#[derive(Clone, Default)]
struct DeterministicState;

impl BuildHasher for DeterministicState {
    type Hasher = SeaHasher;

    fn build_hasher(&self) -> Self::Hasher {
        SeaHasher::new()
    }
}

#[cfg(any(test, feature = "test-support"))]
type HashMap<K, V> = std::collections::HashMap<K, V, DeterministicState>;

#[cfg(any(test, feature = "test-support"))]
type HashSet<T> = std::collections::HashSet<T, DeterministicState>;

#[cfg(not(any(test, feature = "test-support")))]
type HashMap<K, V> = std::collections::HashMap<K, V>;

#[cfg(not(any(test, feature = "test-support")))]
type HashSet<T> = std::collections::HashSet<T>;

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(Parser::new());
}

lazy_static! {
    static ref QUERY_CURSORS: Mutex<Vec<QueryCursor>> = Default::default();
}

// TODO - Make this configurable
const INDENT_SIZE: u32 = 4;

struct QueryCursorHandle(Option<QueryCursor>);

impl QueryCursorHandle {
    fn new() -> Self {
        QueryCursorHandle(Some(
            QUERY_CURSORS
                .lock()
                .pop()
                .unwrap_or_else(|| QueryCursor::new()),
        ))
    }
}

impl Deref for QueryCursorHandle {
    type Target = QueryCursor;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}

impl DerefMut for QueryCursorHandle {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().unwrap()
    }
}

impl Drop for QueryCursorHandle {
    fn drop(&mut self) {
        let mut cursor = self.0.take().unwrap();
        cursor.set_byte_range(0..usize::MAX);
        cursor.set_point_range(Point::zero().into()..Point::MAX.into());
        QUERY_CURSORS.lock().push(cursor)
    }
}

pub struct Buffer {
    fragments: SumTree<Fragment>,
    visible_text: Rope,
    deleted_text: Rope,
    pub version: clock::Global,
    saved_version: clock::Global,
    saved_mtime: SystemTime,
    last_edit: clock::Local,
    undo_map: UndoMap,
    history: History,
    file: Option<Box<dyn File>>,
    language: Option<Arc<Language>>,
    autoindent_requests: Vec<Arc<AutoindentRequest>>,
    pending_autoindent: Option<Task<()>>,
    sync_parse_timeout: Duration,
    syntax_tree: Mutex<Option<SyntaxTree>>,
    parsing_in_background: bool,
    parse_count: usize,
    selections: HashMap<SelectionSetId, SelectionSet>,
    deferred_ops: OperationQueue,
    deferred_replicas: HashSet<ReplicaId>,
    replica_id: ReplicaId,
    remote_id: u64,
    local_clock: clock::Local,
    lamport_clock: clock::Lamport,
    #[cfg(test)]
    operations: Vec<Operation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectionSet {
    pub selections: Arc<[Selection]>,
    pub active: bool,
}

#[derive(Clone)]
struct SyntaxTree {
    tree: Tree,
    version: clock::Global,
}

#[derive(Clone)]
struct AutoindentRequest {
    before_edit: Snapshot,
    edited: AnchorSet,
    inserted: Option<AnchorRangeSet>,
}

#[derive(Clone, Debug)]
struct Transaction {
    start: clock::Global,
    end: clock::Global,
    buffer_was_dirty: bool,
    edits: Vec<clock::Local>,
    ranges: Vec<Range<usize>>,
    selections_before: Option<(SelectionSetId, Arc<[Selection]>)>,
    selections_after: Option<(SelectionSetId, Arc<[Selection]>)>,
    first_edit_at: Instant,
    last_edit_at: Instant,
}

impl Transaction {
    fn push_edit(&mut self, edit: &EditOperation) {
        self.edits.push(edit.timestamp.local());
        self.end.observe(edit.timestamp.local());

        let mut other_ranges = edit.ranges.iter().peekable();
        let mut new_ranges: Vec<Range<usize>> = Vec::new();
        let insertion_len = edit.new_text.as_ref().map_or(0, |t| t.len());
        let mut delta = 0;

        for mut self_range in self.ranges.iter().cloned() {
            self_range.start += delta;
            self_range.end += delta;

            while let Some(other_range) = other_ranges.peek() {
                let mut other_range = (*other_range).clone();
                other_range.start += delta;
                other_range.end += delta;

                if other_range.start <= self_range.end {
                    other_ranges.next().unwrap();
                    delta += insertion_len;

                    if other_range.end < self_range.start {
                        new_ranges.push(other_range.start..other_range.end + insertion_len);
                        self_range.start += insertion_len;
                        self_range.end += insertion_len;
                    } else {
                        self_range.start = cmp::min(self_range.start, other_range.start);
                        self_range.end = cmp::max(self_range.end, other_range.end) + insertion_len;
                    }
                } else {
                    break;
                }
            }

            new_ranges.push(self_range);
        }

        for other_range in other_ranges {
            new_ranges.push(other_range.start + delta..other_range.end + delta + insertion_len);
            delta += insertion_len;
        }

        self.ranges = new_ranges;
    }
}

#[derive(Clone)]
pub struct History {
    // TODO: Turn this into a String or Rope, maybe.
    pub base_text: Arc<str>,
    ops: HashMap<clock::Local, EditOperation>,
    undo_stack: Vec<Transaction>,
    redo_stack: Vec<Transaction>,
    transaction_depth: usize,
    group_interval: Duration,
}

impl History {
    pub fn new(base_text: Arc<str>) -> Self {
        Self {
            base_text,
            ops: Default::default(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            transaction_depth: 0,
            group_interval: Duration::from_millis(300),
        }
    }

    fn push(&mut self, op: EditOperation) {
        self.ops.insert(op.timestamp.local(), op);
    }

    fn start_transaction(
        &mut self,
        start: clock::Global,
        buffer_was_dirty: bool,
        selections: Option<(SelectionSetId, Arc<[Selection]>)>,
        now: Instant,
    ) {
        self.transaction_depth += 1;
        if self.transaction_depth == 1 {
            self.undo_stack.push(Transaction {
                start: start.clone(),
                end: start,
                buffer_was_dirty,
                edits: Vec::new(),
                ranges: Vec::new(),
                selections_before: selections,
                selections_after: None,
                first_edit_at: now,
                last_edit_at: now,
            });
        }
    }

    fn end_transaction(
        &mut self,
        selections: Option<(SelectionSetId, Arc<[Selection]>)>,
        now: Instant,
    ) -> Option<&Transaction> {
        assert_ne!(self.transaction_depth, 0);
        self.transaction_depth -= 1;
        if self.transaction_depth == 0 {
            if self.undo_stack.last().unwrap().ranges.is_empty() {
                self.undo_stack.pop();
                None
            } else {
                let transaction = self.undo_stack.last_mut().unwrap();
                transaction.selections_after = selections;
                transaction.last_edit_at = now;
                Some(transaction)
            }
        } else {
            None
        }
    }

    fn group(&mut self) {
        let mut new_len = self.undo_stack.len();
        let mut transactions = self.undo_stack.iter_mut();

        if let Some(mut transaction) = transactions.next_back() {
            while let Some(prev_transaction) = transactions.next_back() {
                if transaction.first_edit_at - prev_transaction.last_edit_at <= self.group_interval
                    && transaction.start == prev_transaction.end
                {
                    transaction = prev_transaction;
                    new_len -= 1;
                } else {
                    break;
                }
            }
        }

        let (transactions_to_keep, transactions_to_merge) = self.undo_stack.split_at_mut(new_len);
        if let Some(last_transaction) = transactions_to_keep.last_mut() {
            for transaction in &*transactions_to_merge {
                for edit_id in &transaction.edits {
                    last_transaction.push_edit(&self.ops[edit_id]);
                }
            }

            if let Some(transaction) = transactions_to_merge.last_mut() {
                last_transaction.last_edit_at = transaction.last_edit_at;
                last_transaction.selections_after = transaction.selections_after.take();
                last_transaction.end = transaction.end.clone();
            }
        }

        self.undo_stack.truncate(new_len);
    }

    fn push_undo(&mut self, edit_id: clock::Local) {
        assert_ne!(self.transaction_depth, 0);
        let last_transaction = self.undo_stack.last_mut().unwrap();
        last_transaction.push_edit(&self.ops[&edit_id]);
    }

    fn pop_undo(&mut self) -> Option<&Transaction> {
        assert_eq!(self.transaction_depth, 0);
        if let Some(transaction) = self.undo_stack.pop() {
            self.redo_stack.push(transaction);
            self.redo_stack.last()
        } else {
            None
        }
    }

    fn pop_redo(&mut self) -> Option<&Transaction> {
        assert_eq!(self.transaction_depth, 0);
        if let Some(transaction) = self.redo_stack.pop() {
            self.undo_stack.push(transaction);
            self.undo_stack.last()
        } else {
            None
        }
    }
}

#[derive(Clone, Default, Debug)]
struct UndoMap(HashMap<clock::Local, Vec<(clock::Local, u32)>>);

impl UndoMap {
    fn insert(&mut self, undo: &UndoOperation) {
        for (edit_id, count) in &undo.counts {
            self.0.entry(*edit_id).or_default().push((undo.id, *count));
        }
    }

    fn is_undone(&self, edit_id: clock::Local) -> bool {
        self.undo_count(edit_id) % 2 == 1
    }

    fn was_undone(&self, edit_id: clock::Local, version: &clock::Global) -> bool {
        let undo_count = self
            .0
            .get(&edit_id)
            .unwrap_or(&Vec::new())
            .iter()
            .filter(|(undo_id, _)| version.observed(*undo_id))
            .map(|(_, undo_count)| *undo_count)
            .max()
            .unwrap_or(0);
        undo_count % 2 == 1
    }

    fn undo_count(&self, edit_id: clock::Local) -> u32 {
        self.0
            .get(&edit_id)
            .unwrap_or(&Vec::new())
            .iter()
            .map(|(_, undo_count)| *undo_count)
            .max()
            .unwrap_or(0)
    }
}

struct Edits<'a, F: Fn(&FragmentSummary) -> bool> {
    visible_text: &'a Rope,
    deleted_text: &'a Rope,
    cursor: Option<FilterCursor<'a, F, Fragment, FragmentTextSummary>>,
    undos: &'a UndoMap,
    since: clock::Global,
    old_offset: usize,
    new_offset: usize,
    old_point: Point,
    new_point: Point,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Edit {
    pub old_bytes: Range<usize>,
    pub new_bytes: Range<usize>,
    pub old_lines: Range<Point>,
}

impl Edit {
    pub fn delta(&self) -> isize {
        self.inserted_bytes() as isize - self.deleted_bytes() as isize
    }

    pub fn deleted_bytes(&self) -> usize {
        self.old_bytes.end - self.old_bytes.start
    }

    pub fn inserted_bytes(&self) -> usize {
        self.new_bytes.end - self.new_bytes.start
    }

    pub fn deleted_lines(&self) -> Point {
        self.old_lines.end - self.old_lines.start
    }
}

struct Diff {
    base_version: clock::Global,
    new_text: Arc<str>,
    changes: Vec<(ChangeTag, usize)>,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
struct InsertionTimestamp {
    replica_id: ReplicaId,
    local: clock::Seq,
    lamport: clock::Seq,
}

impl InsertionTimestamp {
    fn local(&self) -> clock::Local {
        clock::Local {
            replica_id: self.replica_id,
            value: self.local,
        }
    }

    fn lamport(&self) -> clock::Lamport {
        clock::Lamport {
            replica_id: self.replica_id,
            value: self.lamport,
        }
    }
}

#[derive(Eq, PartialEq, Clone, Debug)]
struct Fragment {
    timestamp: InsertionTimestamp,
    len: usize,
    visible: bool,
    deletions: HashSet<clock::Local>,
    max_undos: clock::Global,
}

#[derive(Eq, PartialEq, Clone, Debug)]
pub struct FragmentSummary {
    text: FragmentTextSummary,
    max_version: clock::Global,
    min_insertion_version: clock::Global,
    max_insertion_version: clock::Global,
}

#[derive(Copy, Default, Clone, Debug, PartialEq, Eq)]
struct FragmentTextSummary {
    visible: usize,
    deleted: usize,
}

impl<'a> sum_tree::Dimension<'a, FragmentSummary> for FragmentTextSummary {
    fn add_summary(&mut self, summary: &'a FragmentSummary, _: &Option<clock::Global>) {
        self.visible += summary.text.visible;
        self.deleted += summary.text.deleted;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Operation {
    Edit(EditOperation),
    Undo {
        undo: UndoOperation,
        lamport_timestamp: clock::Lamport,
    },
    UpdateSelections {
        set_id: SelectionSetId,
        selections: Option<Arc<[Selection]>>,
        lamport_timestamp: clock::Lamport,
    },
    SetActiveSelections {
        set_id: Option<SelectionSetId>,
        lamport_timestamp: clock::Lamport,
    },
    #[cfg(test)]
    Test(clock::Lamport),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditOperation {
    timestamp: InsertionTimestamp,
    version: clock::Global,
    ranges: Vec<Range<usize>>,
    new_text: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UndoOperation {
    id: clock::Local,
    counts: HashMap<clock::Local, u32>,
    ranges: Vec<Range<usize>>,
    version: clock::Global,
}

impl Buffer {
    pub fn new<T: Into<Arc<str>>>(
        replica_id: ReplicaId,
        base_text: T,
        cx: &mut ModelContext<Self>,
    ) -> Self {
        Self::build(
            replica_id,
            History::new(base_text.into()),
            None,
            cx.model_id() as u64,
            None,
            cx,
        )
    }

    pub fn from_history(
        replica_id: ReplicaId,
        history: History,
        file: Option<Box<dyn File>>,
        language: Option<Arc<Language>>,
        cx: &mut ModelContext<Self>,
    ) -> Self {
        Self::build(
            replica_id,
            history,
            file,
            cx.model_id() as u64,
            language,
            cx,
        )
    }

    fn build(
        replica_id: ReplicaId,
        history: History,
        file: Option<Box<dyn File>>,
        remote_id: u64,
        language: Option<Arc<Language>>,
        cx: &mut ModelContext<Self>,
    ) -> Self {
        let saved_mtime;
        if let Some(file) = file.as_ref() {
            saved_mtime = file.mtime();
        } else {
            saved_mtime = UNIX_EPOCH;
        }

        let mut fragments = SumTree::new();

        let visible_text = Rope::from(history.base_text.as_ref());
        if visible_text.len() > 0 {
            fragments.push(
                Fragment {
                    timestamp: Default::default(),
                    len: visible_text.len(),
                    visible: true,
                    deletions: Default::default(),
                    max_undos: Default::default(),
                },
                &None,
            );
        }

        let mut result = Self {
            visible_text,
            deleted_text: Rope::new(),
            fragments,
            version: clock::Global::new(),
            saved_version: clock::Global::new(),
            last_edit: clock::Local::default(),
            undo_map: Default::default(),
            history,
            file,
            syntax_tree: Mutex::new(None),
            parsing_in_background: false,
            parse_count: 0,
            sync_parse_timeout: Duration::from_millis(1),
            autoindent_requests: Default::default(),
            pending_autoindent: Default::default(),
            language,
            saved_mtime,
            selections: HashMap::default(),
            deferred_ops: OperationQueue::new(),
            deferred_replicas: HashSet::default(),
            replica_id,
            remote_id,
            local_clock: clock::Local::new(replica_id),
            lamport_clock: clock::Lamport::new(replica_id),

            #[cfg(test)]
            operations: Default::default(),
        };
        result.reparse(cx);
        result
    }

    pub fn replica_id(&self) -> ReplicaId {
        self.local_clock.replica_id
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            visible_text: self.visible_text.clone(),
            fragments: self.fragments.clone(),
            version: self.version.clone(),
            tree: self.syntax_tree(),
            is_parsing: self.parsing_in_background,
            language: self.language.clone(),
            query_cursor: QueryCursorHandle::new(),
        }
    }

    pub fn from_proto(
        replica_id: ReplicaId,
        message: proto::Buffer,
        file: Option<Box<dyn File>>,
        language: Option<Arc<Language>>,
        cx: &mut ModelContext<Self>,
    ) -> Result<Self> {
        let mut buffer = Buffer::build(
            replica_id,
            History::new(message.content.into()),
            file,
            message.id,
            language,
            cx,
        );
        let ops = message
            .history
            .into_iter()
            .map(|op| Operation::Edit(op.into()));
        buffer.apply_ops(ops, cx)?;
        buffer.selections = message
            .selections
            .into_iter()
            .map(|set| {
                let set_id = clock::Lamport {
                    replica_id: set.replica_id as ReplicaId,
                    value: set.local_timestamp,
                };
                let selections: Vec<Selection> = set
                    .selections
                    .into_iter()
                    .map(TryFrom::try_from)
                    .collect::<Result<_, _>>()?;
                let set = SelectionSet {
                    selections: Arc::from(selections),
                    active: set.is_active,
                };
                Result::<_, anyhow::Error>::Ok((set_id, set))
            })
            .collect::<Result<_, _>>()?;
        Ok(buffer)
    }

    pub fn to_proto(&self, cx: &mut ModelContext<Self>) -> proto::Buffer {
        let ops = self.history.ops.values().map(Into::into).collect();
        proto::Buffer {
            id: cx.model_id() as u64,
            content: self.history.base_text.to_string(),
            history: ops,
            selections: self
                .selections
                .iter()
                .map(|(set_id, set)| proto::SelectionSetSnapshot {
                    replica_id: set_id.replica_id as u32,
                    local_timestamp: set_id.value,
                    selections: set.selections.iter().map(Into::into).collect(),
                    is_active: set.active,
                })
                .collect(),
        }
    }

    pub fn file(&self) -> Option<&dyn File> {
        self.file.as_deref()
    }

    pub fn file_mut(&mut self) -> Option<&mut dyn File> {
        self.file.as_mut().map(|f| f.deref_mut() as &mut dyn File)
    }

    pub fn save(
        &mut self,
        cx: &mut ModelContext<Self>,
    ) -> Result<Task<Result<(clock::Global, SystemTime)>>> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| anyhow!("buffer has no file"))?;
        let text = self.visible_text.clone();
        let version = self.version.clone();
        let save = file.save(self.remote_id, text, version, cx.as_mut());
        Ok(cx.spawn(|this, mut cx| async move {
            let (version, mtime) = save.await?;
            this.update(&mut cx, |this, cx| {
                this.did_save(version.clone(), mtime, None, cx);
            });
            Ok((version, mtime))
        }))
    }

    pub fn as_rope(&self) -> &Rope {
        &self.visible_text
    }

    pub fn set_language(&mut self, language: Option<Arc<Language>>, cx: &mut ModelContext<Self>) {
        self.language = language;
        self.reparse(cx);
    }

    pub fn did_save(
        &mut self,
        version: clock::Global,
        mtime: SystemTime,
        new_file: Option<Box<dyn File>>,
        cx: &mut ModelContext<Self>,
    ) {
        self.saved_mtime = mtime;
        self.saved_version = version;
        if let Some(new_file) = new_file {
            self.file = Some(new_file);
        }
        cx.emit(Event::Saved);
    }

    pub fn file_updated(
        &mut self,
        path: Arc<Path>,
        mtime: SystemTime,
        new_text: Option<String>,
        cx: &mut ModelContext<Self>,
    ) {
        let file = self.file.as_mut().unwrap();
        let mut changed = false;
        if path != *file.path() {
            file.set_path(path);
            changed = true;
        }

        if mtime != file.mtime() {
            file.set_mtime(mtime);
            changed = true;
            if let Some(new_text) = new_text {
                if self.version == self.saved_version {
                    cx.spawn(|this, mut cx| async move {
                        let diff = this
                            .read_with(&cx, |this, cx| this.diff(new_text.into(), cx))
                            .await;
                        this.update(&mut cx, |this, cx| {
                            if this.apply_diff(diff, cx) {
                                this.saved_version = this.version.clone();
                                this.saved_mtime = mtime;
                                cx.emit(Event::Reloaded);
                            }
                        });
                    })
                    .detach();
                }
            }
        }

        if changed {
            cx.emit(Event::FileHandleChanged);
        }
    }

    pub fn file_deleted(&mut self, cx: &mut ModelContext<Self>) {
        if self.version == self.saved_version {
            cx.emit(Event::Dirtied);
        }
        cx.emit(Event::FileHandleChanged);
    }

    pub fn close(&mut self, cx: &mut ModelContext<Self>) {
        cx.emit(Event::Closed);
    }

    pub fn language(&self) -> Option<&Arc<Language>> {
        self.language.as_ref()
    }

    pub fn parse_count(&self) -> usize {
        self.parse_count
    }

    fn syntax_tree(&self) -> Option<Tree> {
        if let Some(syntax_tree) = self.syntax_tree.lock().as_mut() {
            self.interpolate_tree(syntax_tree);
            Some(syntax_tree.tree.clone())
        } else {
            None
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn is_parsing(&self) -> bool {
        self.parsing_in_background
    }

    #[cfg(test)]
    pub fn set_sync_parse_timeout(&mut self, timeout: Duration) {
        self.sync_parse_timeout = timeout;
    }

    fn reparse(&mut self, cx: &mut ModelContext<Self>) -> bool {
        if self.parsing_in_background {
            return false;
        }

        if let Some(language) = self.language.clone() {
            let old_tree = self.syntax_tree();
            let text = self.visible_text.clone();
            let parsed_version = self.version();
            let parse_task = cx.background().spawn({
                let language = language.clone();
                async move { Self::parse_text(&text, old_tree, &language) }
            });

            match cx
                .background()
                .block_with_timeout(self.sync_parse_timeout, parse_task)
            {
                Ok(new_tree) => {
                    self.did_finish_parsing(new_tree, parsed_version, cx);
                    return true;
                }
                Err(parse_task) => {
                    self.parsing_in_background = true;
                    cx.spawn(move |this, mut cx| async move {
                        let new_tree = parse_task.await;
                        this.update(&mut cx, move |this, cx| {
                            let language_changed =
                                this.language.as_ref().map_or(true, |curr_language| {
                                    !Arc::ptr_eq(curr_language, &language)
                                });
                            let parse_again = this.version > parsed_version || language_changed;
                            this.parsing_in_background = false;
                            this.did_finish_parsing(new_tree, parsed_version, cx);

                            if parse_again && this.reparse(cx) {
                                return;
                            }
                        });
                    })
                    .detach();
                }
            }
        }
        false
    }

    fn parse_text(text: &Rope, old_tree: Option<Tree>, language: &Language) -> Tree {
        PARSER.with(|parser| {
            let mut parser = parser.borrow_mut();
            parser
                .set_language(language.grammar)
                .expect("incompatible grammar");
            let mut chunks = text.chunks_in_range(0..text.len());
            let tree = parser
                .parse_with(
                    &mut move |offset, _| {
                        chunks.seek(offset);
                        chunks.next().unwrap_or("").as_bytes()
                    },
                    old_tree.as_ref(),
                )
                .unwrap();
            tree
        })
    }

    fn interpolate_tree(&self, tree: &mut SyntaxTree) {
        let mut delta = 0_isize;
        for edit in self.edits_since(tree.version.clone()) {
            let start_offset = (edit.old_bytes.start as isize + delta) as usize;
            let start_point = self.visible_text.to_point(start_offset);
            tree.tree.edit(&InputEdit {
                start_byte: start_offset,
                old_end_byte: start_offset + edit.deleted_bytes(),
                new_end_byte: start_offset + edit.inserted_bytes(),
                start_position: start_point.into(),
                old_end_position: (start_point + edit.deleted_lines()).into(),
                new_end_position: self
                    .visible_text
                    .to_point(start_offset + edit.inserted_bytes())
                    .into(),
            });
            delta += edit.inserted_bytes() as isize - edit.deleted_bytes() as isize;
        }
        tree.version = self.version();
    }

    fn did_finish_parsing(
        &mut self,
        tree: Tree,
        version: clock::Global,
        cx: &mut ModelContext<Self>,
    ) {
        self.parse_count += 1;
        *self.syntax_tree.lock() = Some(SyntaxTree { tree, version });
        self.request_autoindent(cx);
        cx.emit(Event::Reparsed);
        cx.notify();
    }

    fn request_autoindent(&mut self, cx: &mut ModelContext<Self>) {
        if let Some(indent_columns) = self.compute_autoindents() {
            let indent_columns = cx.background().spawn(indent_columns);
            match cx
                .background()
                .block_with_timeout(Duration::from_micros(500), indent_columns)
            {
                Ok(indent_columns) => {
                    log::info!("finished synchronously {:?}", indent_columns);
                    self.autoindent_requests.clear();
                    self.start_transaction(None).unwrap();
                    for (row, indent_column) in indent_columns {
                        self.set_indent_column_for_line(row, indent_column, cx);
                    }
                    self.end_transaction(None, cx).unwrap();
                }
                Err(indent_columns) => {
                    self.pending_autoindent = Some(cx.spawn(|this, mut cx| async move {
                        let indent_columns = indent_columns.await;
                        log::info!("finished ASYNC, {:?}", indent_columns);
                        this.update(&mut cx, |this, cx| {
                            this.autoindent_requests.clear();
                            this.start_transaction(None).unwrap();
                            for (row, indent_column) in indent_columns {
                                this.set_indent_column_for_line(row, indent_column, cx);
                            }
                            this.end_transaction(None, cx).unwrap();
                        });
                    }));
                }
            }
        }
    }

    fn compute_autoindents(&self) -> Option<impl Future<Output = BTreeMap<u32, u32>>> {
        let max_rows_between_yields = 100;
        let snapshot = self.snapshot();
        if snapshot.language.is_none()
            || snapshot.tree.is_none()
            || self.autoindent_requests.is_empty()
        {
            return None;
        }

        let autoindent_requests = self.autoindent_requests.clone();
        Some(async move {
            let mut indent_columns = BTreeMap::new();
            for request in autoindent_requests {
                let old_to_new_rows = request
                    .edited
                    .to_points(&request.before_edit)
                    .map(|point| point.row)
                    .zip(request.edited.to_points(&snapshot).map(|point| point.row))
                    .collect::<BTreeMap<u32, u32>>();

                let mut old_suggestions = HashMap::default();
                let old_edited_ranges =
                    contiguous_ranges(old_to_new_rows.keys().copied(), max_rows_between_yields);
                for old_edited_range in old_edited_ranges {
                    let suggestions = request
                        .before_edit
                        .suggest_autoindents(old_edited_range.clone())
                        .into_iter()
                        .flatten();
                    for (old_row, suggestion) in old_edited_range.zip(suggestions) {
                        let indentation_basis = old_to_new_rows
                            .get(&suggestion.basis_row)
                            .and_then(|from_row| old_suggestions.get(from_row).copied())
                            .unwrap_or_else(|| {
                                request
                                    .before_edit
                                    .indent_column_for_line(suggestion.basis_row)
                            });
                        let delta = if suggestion.indent { INDENT_SIZE } else { 0 };
                        old_suggestions.insert(
                            *old_to_new_rows.get(&old_row).unwrap(),
                            indentation_basis + delta,
                        );
                    }
                    yield_now().await;
                }

                // At this point, old_suggestions contains the suggested indentation for all edited lines with respect to the state of the
                // buffer before the edit, but keyed by the row for these lines after the edits were applied.
                let new_edited_row_ranges =
                    contiguous_ranges(old_to_new_rows.values().copied(), max_rows_between_yields);
                for new_edited_row_range in new_edited_row_ranges {
                    let suggestions = snapshot
                        .suggest_autoindents(new_edited_row_range.clone())
                        .into_iter()
                        .flatten();
                    for (new_row, suggestion) in new_edited_row_range.zip(suggestions) {
                        let delta = if suggestion.indent { INDENT_SIZE } else { 0 };
                        let new_indentation = indent_columns
                            .get(&suggestion.basis_row)
                            .copied()
                            .unwrap_or_else(|| {
                                snapshot.indent_column_for_line(suggestion.basis_row)
                            })
                            + delta;
                        if old_suggestions
                            .get(&new_row)
                            .map_or(true, |old_indentation| new_indentation != *old_indentation)
                        {
                            indent_columns.insert(new_row, new_indentation);
                        }
                    }
                    yield_now().await;
                }

                if let Some(inserted) = request.inserted.as_ref() {
                    let inserted_row_ranges = contiguous_ranges(
                        inserted
                            .to_point_ranges(&snapshot)
                            .flat_map(|range| range.start.row..range.end.row + 1),
                        max_rows_between_yields,
                    );
                    for inserted_row_range in inserted_row_ranges {
                        let suggestions = snapshot
                            .suggest_autoindents(inserted_row_range.clone())
                            .into_iter()
                            .flatten();
                        for (row, suggestion) in inserted_row_range.zip(suggestions) {
                            let delta = if suggestion.indent { INDENT_SIZE } else { 0 };
                            let new_indentation = indent_columns
                                .get(&suggestion.basis_row)
                                .copied()
                                .unwrap_or_else(|| {
                                    snapshot.indent_column_for_line(suggestion.basis_row)
                                })
                                + delta;
                            indent_columns.insert(row, new_indentation);
                        }
                        yield_now().await;
                    }
                }
            }
            indent_columns
        })
    }

    pub fn indent_column_for_line(&self, row: u32) -> u32 {
        self.content().indent_column_for_line(row)
    }

    fn set_indent_column_for_line(&mut self, row: u32, column: u32, cx: &mut ModelContext<Self>) {
        let current_column = self.indent_column_for_line(row);
        if column > current_column {
            let offset = self.visible_text.to_offset(Point::new(row, 0));

            // TODO: do this differently. By replacing the preceding newline,
            // we force the new indentation to come before any left-biased anchors
            // on the line.
            let delta = (column - current_column) as usize;
            if offset > 0 {
                let mut prefix = String::with_capacity(1 + delta);
                prefix.push('\n');
                prefix.extend(std::iter::repeat(' ').take(delta));
                self.edit([(offset - 1)..offset], prefix, cx);
            } else {
                self.edit(
                    [offset..offset],
                    std::iter::repeat(' ').take(delta).collect::<String>(),
                    cx,
                );
            }
        } else if column < current_column {
            self.edit(
                [Point::new(row, 0)..Point::new(row, current_column - column)],
                "",
                cx,
            );
        }
    }

    pub fn range_for_syntax_ancestor<T: ToOffset>(&self, range: Range<T>) -> Option<Range<usize>> {
        if let Some(tree) = self.syntax_tree() {
            let root = tree.root_node();
            let range = range.start.to_offset(self)..range.end.to_offset(self);
            let mut node = root.descendant_for_byte_range(range.start, range.end);
            while node.map_or(false, |n| n.byte_range() == range) {
                node = node.unwrap().parent();
            }
            node.map(|n| n.byte_range())
        } else {
            None
        }
    }

    pub fn enclosing_bracket_ranges<T: ToOffset>(
        &self,
        range: Range<T>,
    ) -> Option<(Range<usize>, Range<usize>)> {
        let (lang, tree) = self.language.as_ref().zip(self.syntax_tree())?;
        let open_capture_ix = lang.brackets_query.capture_index_for_name("open")?;
        let close_capture_ix = lang.brackets_query.capture_index_for_name("close")?;

        // Find bracket pairs that *inclusively* contain the given range.
        let range = range.start.to_offset(self).saturating_sub(1)..range.end.to_offset(self) + 1;
        let mut cursor = QueryCursorHandle::new();
        let matches = cursor.set_byte_range(range).matches(
            &lang.brackets_query,
            tree.root_node(),
            TextProvider(&self.visible_text),
        );

        // Get the ranges of the innermost pair of brackets.
        matches
            .filter_map(|mat| {
                let open = mat.nodes_for_capture_index(open_capture_ix).next()?;
                let close = mat.nodes_for_capture_index(close_capture_ix).next()?;
                Some((open.byte_range(), close.byte_range()))
            })
            .min_by_key(|(open_range, close_range)| close_range.end - open_range.start)
    }

    fn diff(&self, new_text: Arc<str>, cx: &AppContext) -> Task<Diff> {
        // TODO: it would be nice to not allocate here.
        let old_text = self.text();
        let base_version = self.version();
        cx.background().spawn(async move {
            let changes = TextDiff::from_lines(old_text.as_str(), new_text.as_ref())
                .iter_all_changes()
                .map(|c| (c.tag(), c.value().len()))
                .collect::<Vec<_>>();
            Diff {
                base_version,
                new_text,
                changes,
            }
        })
    }

    pub fn set_text_from_disk(&self, new_text: Arc<str>, cx: &mut ModelContext<Self>) -> Task<()> {
        cx.spawn(|this, mut cx| async move {
            let diff = this
                .read_with(&cx, |this, cx| this.diff(new_text, cx))
                .await;

            this.update(&mut cx, |this, cx| {
                if this.apply_diff(diff, cx) {
                    this.saved_version = this.version.clone();
                }
            });
        })
    }

    fn apply_diff(&mut self, diff: Diff, cx: &mut ModelContext<Self>) -> bool {
        if self.version == diff.base_version {
            self.start_transaction(None).unwrap();
            let mut offset = 0;
            for (tag, len) in diff.changes {
                let range = offset..(offset + len);
                match tag {
                    ChangeTag::Equal => offset += len,
                    ChangeTag::Delete => self.edit(Some(range), "", cx),
                    ChangeTag::Insert => {
                        self.edit(Some(offset..offset), &diff.new_text[range], cx);
                        offset += len;
                    }
                }
            }
            self.end_transaction(None, cx).unwrap();
            true
        } else {
            false
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.version > self.saved_version
            || self.file.as_ref().map_or(false, |file| file.is_deleted())
    }

    pub fn has_conflict(&self) -> bool {
        self.version > self.saved_version
            && self
                .file
                .as_ref()
                .map_or(false, |file| file.mtime() > self.saved_mtime)
    }

    pub fn remote_id(&self) -> u64 {
        self.remote_id
    }

    pub fn version(&self) -> clock::Global {
        self.version.clone()
    }

    pub fn text_summary(&self) -> TextSummary {
        self.visible_text.summary()
    }

    pub fn len(&self) -> usize {
        self.content().len()
    }

    pub fn line_len(&self, row: u32) -> u32 {
        self.content().line_len(row)
    }

    pub fn max_point(&self) -> Point {
        self.visible_text.max_point()
    }

    pub fn row_count(&self) -> u32 {
        self.max_point().row + 1
    }

    pub fn text(&self) -> String {
        self.text_for_range(0..self.len()).collect()
    }

    pub fn text_for_range<'a, T: ToOffset>(&'a self, range: Range<T>) -> Chunks<'a> {
        self.content().text_for_range(range)
    }

    pub fn chars(&self) -> impl Iterator<Item = char> + '_ {
        self.chars_at(0)
    }

    pub fn chars_at<'a, T: 'a + ToOffset>(
        &'a self,
        position: T,
    ) -> impl Iterator<Item = char> + 'a {
        self.content().chars_at(position)
    }

    pub fn chars_for_range<T: ToOffset>(&self, range: Range<T>) -> impl Iterator<Item = char> + '_ {
        self.text_for_range(range).flat_map(str::chars)
    }

    pub fn bytes_at<T: ToOffset>(&self, position: T) -> impl Iterator<Item = u8> + '_ {
        let offset = position.to_offset(self);
        self.visible_text.bytes_at(offset)
    }

    pub fn contains_str_at<T>(&self, position: T, needle: &str) -> bool
    where
        T: ToOffset,
    {
        let position = position.to_offset(self);
        position == self.clip_offset(position, Bias::Left)
            && self
                .bytes_at(position)
                .take(needle.len())
                .eq(needle.bytes())
    }

    pub fn edits_since<'a>(&'a self, since: clock::Global) -> impl 'a + Iterator<Item = Edit> {
        let since_2 = since.clone();
        let cursor = if since == self.version {
            None
        } else {
            Some(self.fragments.filter(
                move |summary| summary.max_version.changed_since(&since_2),
                &None,
            ))
        };

        Edits {
            visible_text: &self.visible_text,
            deleted_text: &self.deleted_text,
            cursor,
            undos: &self.undo_map,
            since,
            old_offset: 0,
            new_offset: 0,
            old_point: Point::zero(),
            new_point: Point::zero(),
        }
    }

    pub fn deferred_ops_len(&self) -> usize {
        self.deferred_ops.len()
    }

    pub fn start_transaction(&mut self, set_id: Option<SelectionSetId>) -> Result<()> {
        self.start_transaction_at(set_id, Instant::now())
    }

    fn start_transaction_at(&mut self, set_id: Option<SelectionSetId>, now: Instant) -> Result<()> {
        let selections = if let Some(set_id) = set_id {
            let set = self
                .selections
                .get(&set_id)
                .ok_or_else(|| anyhow!("invalid selection set {:?}", set_id))?;
            Some((set_id, set.selections.clone()))
        } else {
            None
        };
        self.history
            .start_transaction(self.version.clone(), self.is_dirty(), selections, now);
        Ok(())
    }

    pub fn end_transaction(
        &mut self,
        set_id: Option<SelectionSetId>,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        self.end_transaction_at(set_id, Instant::now(), cx)
    }

    fn end_transaction_at(
        &mut self,
        set_id: Option<SelectionSetId>,
        now: Instant,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        let selections = if let Some(set_id) = set_id {
            let set = self
                .selections
                .get(&set_id)
                .ok_or_else(|| anyhow!("invalid selection set {:?}", set_id))?;
            Some((set_id, set.selections.clone()))
        } else {
            None
        };

        if let Some(transaction) = self.history.end_transaction(selections, now) {
            let since = transaction.start.clone();
            let was_dirty = transaction.buffer_was_dirty;
            self.history.group();

            cx.notify();
            if self.edits_since(since).next().is_some() {
                self.did_edit(was_dirty, cx);
                self.reparse(cx);
            }
        }

        Ok(())
    }

    pub fn edit<I, S, T>(&mut self, ranges_iter: I, new_text: T, cx: &mut ModelContext<Self>)
    where
        I: IntoIterator<Item = Range<S>>,
        S: ToOffset,
        T: Into<String>,
    {
        self.edit_internal(ranges_iter, new_text, false, cx)
    }

    pub fn edit_with_autoindent<I, S, T>(
        &mut self,
        ranges_iter: I,
        new_text: T,
        cx: &mut ModelContext<Self>,
    ) where
        I: IntoIterator<Item = Range<S>>,
        S: ToOffset,
        T: Into<String>,
    {
        self.edit_internal(ranges_iter, new_text, true, cx)
    }

    pub fn edit_internal<I, S, T>(
        &mut self,
        ranges_iter: I,
        new_text: T,
        autoindent: bool,
        cx: &mut ModelContext<Self>,
    ) where
        I: IntoIterator<Item = Range<S>>,
        S: ToOffset,
        T: Into<String>,
    {
        let new_text = new_text.into();

        // Skip invalid ranges and coalesce contiguous ones.
        let mut ranges: Vec<Range<usize>> = Vec::new();
        for range in ranges_iter {
            let range = range.start.to_offset(&*self)..range.end.to_offset(&*self);
            if !new_text.is_empty() || !range.is_empty() {
                if let Some(prev_range) = ranges.last_mut() {
                    if prev_range.end >= range.start {
                        prev_range.end = cmp::max(prev_range.end, range.end);
                    } else {
                        ranges.push(range);
                    }
                } else {
                    ranges.push(range);
                }
            }
        }
        if ranges.is_empty() {
            return;
        }

        self.pending_autoindent.take();
        let autoindent_request = if autoindent && self.language.is_some() {
            let before_edit = self.snapshot();
            let edited = self.content().anchor_set(ranges.iter().filter_map(|range| {
                let start = range.start.to_point(&*self);
                if new_text.starts_with('\n') && start.column == self.line_len(start.row) {
                    None
                } else {
                    Some((range.start, Bias::Left))
                }
            }));
            Some((before_edit, edited))
        } else {
            None
        };

        let first_newline_ix = new_text.find('\n');
        let new_text_len = new_text.len();
        let new_text = if new_text_len > 0 {
            Some(new_text)
        } else {
            None
        };

        self.start_transaction(None).unwrap();
        let timestamp = InsertionTimestamp {
            replica_id: self.replica_id,
            local: self.local_clock.tick().value,
            lamport: self.lamport_clock.tick().value,
        };
        let edit = self.apply_local_edit(&ranges, new_text, timestamp);

        self.history.push(edit.clone());
        self.history.push_undo(edit.timestamp.local());
        self.last_edit = edit.timestamp.local();
        self.version.observe(edit.timestamp.local());

        if let Some((before_edit, edited)) = autoindent_request {
            let mut inserted = None;
            if let Some(first_newline_ix) = first_newline_ix {
                let mut delta = 0isize;
                inserted = Some(self.content().anchor_range_set(ranges.iter().map(|range| {
                    let start = (delta + range.start as isize) as usize + first_newline_ix + 1;
                    let end = (delta + range.start as isize) as usize + new_text_len;
                    delta += (range.end as isize - range.start as isize) + new_text_len as isize;
                    (start, Bias::Left)..(end, Bias::Right)
                })));
            }

            self.autoindent_requests.push(Arc::new(AutoindentRequest {
                before_edit,
                edited,
                inserted,
            }));
        }

        self.end_transaction(None, cx).unwrap();
        self.send_operation(Operation::Edit(edit), cx);
    }

    fn did_edit(&self, was_dirty: bool, cx: &mut ModelContext<Self>) {
        cx.emit(Event::Edited);
        if !was_dirty {
            cx.emit(Event::Dirtied);
        }
    }

    pub fn add_selection_set(
        &mut self,
        selections: impl Into<Arc<[Selection]>>,
        cx: &mut ModelContext<Self>,
    ) -> SelectionSetId {
        let selections = selections.into();
        let lamport_timestamp = self.lamport_clock.tick();
        self.selections.insert(
            lamport_timestamp,
            SelectionSet {
                selections: selections.clone(),
                active: false,
            },
        );
        cx.notify();

        self.send_operation(
            Operation::UpdateSelections {
                set_id: lamport_timestamp,
                selections: Some(selections),
                lamport_timestamp,
            },
            cx,
        );

        lamport_timestamp
    }

    pub fn update_selection_set(
        &mut self,
        set_id: SelectionSetId,
        selections: impl Into<Arc<[Selection]>>,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        let selections = selections.into();
        let set = self
            .selections
            .get_mut(&set_id)
            .ok_or_else(|| anyhow!("invalid selection set id {:?}", set_id))?;
        set.selections = selections.clone();
        let lamport_timestamp = self.lamport_clock.tick();
        cx.notify();
        self.send_operation(
            Operation::UpdateSelections {
                set_id,
                selections: Some(selections),
                lamport_timestamp,
            },
            cx,
        );
        Ok(())
    }

    pub fn set_active_selection_set(
        &mut self,
        set_id: Option<SelectionSetId>,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        if let Some(set_id) = set_id {
            assert_eq!(set_id.replica_id, self.replica_id());
        }

        for (id, set) in &mut self.selections {
            if id.replica_id == self.local_clock.replica_id {
                if Some(*id) == set_id {
                    set.active = true;
                } else {
                    set.active = false;
                }
            }
        }

        let lamport_timestamp = self.lamport_clock.tick();
        self.send_operation(
            Operation::SetActiveSelections {
                set_id,
                lamport_timestamp,
            },
            cx,
        );
        Ok(())
    }

    pub fn remove_selection_set(
        &mut self,
        set_id: SelectionSetId,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        self.selections
            .remove(&set_id)
            .ok_or_else(|| anyhow!("invalid selection set id {:?}", set_id))?;
        let lamport_timestamp = self.lamport_clock.tick();
        cx.notify();
        self.send_operation(
            Operation::UpdateSelections {
                set_id,
                selections: None,
                lamport_timestamp,
            },
            cx,
        );
        Ok(())
    }

    pub fn selection_set(&self, set_id: SelectionSetId) -> Result<&SelectionSet> {
        self.selections
            .get(&set_id)
            .ok_or_else(|| anyhow!("invalid selection set id {:?}", set_id))
    }

    pub fn selection_sets(&self) -> impl Iterator<Item = (&SelectionSetId, &SelectionSet)> {
        self.selections.iter()
    }

    pub fn apply_ops<I: IntoIterator<Item = Operation>>(
        &mut self,
        ops: I,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        self.pending_autoindent.take();

        let was_dirty = self.is_dirty();
        let old_version = self.version.clone();

        let mut deferred_ops = Vec::new();
        for op in ops {
            if self.can_apply_op(&op) {
                self.apply_op(op)?;
            } else {
                self.deferred_replicas.insert(op.replica_id());
                deferred_ops.push(op);
            }
        }
        self.deferred_ops.insert(deferred_ops);
        self.flush_deferred_ops()?;

        cx.notify();
        if self.edits_since(old_version).next().is_some() {
            self.did_edit(was_dirty, cx);
            self.reparse(cx);
        }

        Ok(())
    }

    fn apply_op(&mut self, op: Operation) -> Result<()> {
        match op {
            Operation::Edit(edit) => {
                if !self.version.observed(edit.timestamp.local()) {
                    self.apply_remote_edit(
                        &edit.version,
                        &edit.ranges,
                        edit.new_text.as_deref(),
                        edit.timestamp,
                    );
                    self.version.observe(edit.timestamp.local());
                    self.history.push(edit);
                }
            }
            Operation::Undo {
                undo,
                lamport_timestamp,
            } => {
                if !self.version.observed(undo.id) {
                    self.apply_undo(&undo)?;
                    self.version.observe(undo.id);
                    self.lamport_clock.observe(lamport_timestamp);
                }
            }
            Operation::UpdateSelections {
                set_id,
                selections,
                lamport_timestamp,
            } => {
                if let Some(selections) = selections {
                    if let Some(set) = self.selections.get_mut(&set_id) {
                        set.selections = selections;
                    } else {
                        self.selections.insert(
                            set_id,
                            SelectionSet {
                                selections,
                                active: false,
                            },
                        );
                    }
                } else {
                    self.selections.remove(&set_id);
                }
                self.lamport_clock.observe(lamport_timestamp);
            }
            Operation::SetActiveSelections {
                set_id,
                lamport_timestamp,
            } => {
                for (id, set) in &mut self.selections {
                    if id.replica_id == lamport_timestamp.replica_id {
                        if Some(*id) == set_id {
                            set.active = true;
                        } else {
                            set.active = false;
                        }
                    }
                }
                self.lamport_clock.observe(lamport_timestamp);
            }
            #[cfg(test)]
            Operation::Test(_) => {}
        }
        Ok(())
    }

    fn apply_remote_edit(
        &mut self,
        version: &clock::Global,
        ranges: &[Range<usize>],
        new_text: Option<&str>,
        timestamp: InsertionTimestamp,
    ) {
        if ranges.is_empty() {
            return;
        }

        let cx = Some(version.clone());
        let mut new_ropes =
            RopeBuilder::new(self.visible_text.cursor(0), self.deleted_text.cursor(0));
        let mut old_fragments = self.fragments.cursor::<VersionedOffset>();
        let mut new_fragments =
            old_fragments.slice(&VersionedOffset::Offset(ranges[0].start), Bias::Left, &cx);
        new_ropes.push_tree(new_fragments.summary().text);

        let mut fragment_start = old_fragments.start().offset();
        for range in ranges {
            let fragment_end = old_fragments.end(&cx).offset();

            // If the current fragment ends before this range, then jump ahead to the first fragment
            // that extends past the start of this range, reusing any intervening fragments.
            if fragment_end < range.start {
                // If the current fragment has been partially consumed, then consume the rest of it
                // and advance to the next fragment before slicing.
                if fragment_start > old_fragments.start().offset() {
                    if fragment_end > fragment_start {
                        let mut suffix = old_fragments.item().unwrap().clone();
                        suffix.len = fragment_end - fragment_start;
                        new_ropes.push_fragment(&suffix, suffix.visible);
                        new_fragments.push(suffix, &None);
                    }
                    old_fragments.next(&cx);
                }

                let slice =
                    old_fragments.slice(&VersionedOffset::Offset(range.start), Bias::Left, &cx);
                new_ropes.push_tree(slice.summary().text);
                new_fragments.push_tree(slice, &None);
                fragment_start = old_fragments.start().offset();
            }

            // If we are at the end of a non-concurrent fragment, advance to the next one.
            let fragment_end = old_fragments.end(&cx).offset();
            if fragment_end == range.start && fragment_end > fragment_start {
                let mut fragment = old_fragments.item().unwrap().clone();
                fragment.len = fragment_end - fragment_start;
                new_ropes.push_fragment(&fragment, fragment.visible);
                new_fragments.push(fragment, &None);
                old_fragments.next(&cx);
                fragment_start = old_fragments.start().offset();
            }

            // Skip over insertions that are concurrent to this edit, but have a lower lamport
            // timestamp.
            while let Some(fragment) = old_fragments.item() {
                if fragment_start == range.start
                    && fragment.timestamp.lamport() > timestamp.lamport()
                {
                    new_ropes.push_fragment(fragment, fragment.visible);
                    new_fragments.push(fragment.clone(), &None);
                    old_fragments.next(&cx);
                    debug_assert_eq!(fragment_start, range.start);
                } else {
                    break;
                }
            }
            debug_assert!(fragment_start <= range.start);

            // Preserve any portion of the current fragment that precedes this range.
            if fragment_start < range.start {
                let mut prefix = old_fragments.item().unwrap().clone();
                prefix.len = range.start - fragment_start;
                fragment_start = range.start;
                new_ropes.push_fragment(&prefix, prefix.visible);
                new_fragments.push(prefix, &None);
            }

            // Insert the new text before any existing fragments within the range.
            if let Some(new_text) = new_text {
                new_ropes.push_str(new_text);
                new_fragments.push(
                    Fragment {
                        timestamp,
                        len: new_text.len(),
                        deletions: Default::default(),
                        max_undos: Default::default(),
                        visible: true,
                    },
                    &None,
                );
            }

            // Advance through every fragment that intersects this range, marking the intersecting
            // portions as deleted.
            while fragment_start < range.end {
                let fragment = old_fragments.item().unwrap();
                let fragment_end = old_fragments.end(&cx).offset();
                let mut intersection = fragment.clone();
                let intersection_end = cmp::min(range.end, fragment_end);
                if fragment.was_visible(version, &self.undo_map) {
                    intersection.len = intersection_end - fragment_start;
                    intersection.deletions.insert(timestamp.local());
                    intersection.visible = false;
                }
                if intersection.len > 0 {
                    new_ropes.push_fragment(&intersection, fragment.visible);
                    new_fragments.push(intersection, &None);
                    fragment_start = intersection_end;
                }
                if fragment_end <= range.end {
                    old_fragments.next(&cx);
                }
            }
        }

        // If the current fragment has been partially consumed, then consume the rest of it
        // and advance to the next fragment before slicing.
        if fragment_start > old_fragments.start().offset() {
            let fragment_end = old_fragments.end(&cx).offset();
            if fragment_end > fragment_start {
                let mut suffix = old_fragments.item().unwrap().clone();
                suffix.len = fragment_end - fragment_start;
                new_ropes.push_fragment(&suffix, suffix.visible);
                new_fragments.push(suffix, &None);
            }
            old_fragments.next(&cx);
        }

        let suffix = old_fragments.suffix(&cx);
        new_ropes.push_tree(suffix.summary().text);
        new_fragments.push_tree(suffix, &None);
        let (visible_text, deleted_text) = new_ropes.finish();
        drop(old_fragments);

        self.fragments = new_fragments;
        self.visible_text = visible_text;
        self.deleted_text = deleted_text;
        self.local_clock.observe(timestamp.local());
        self.lamport_clock.observe(timestamp.lamport());
    }

    #[cfg(not(test))]
    pub fn send_operation(&mut self, operation: Operation, cx: &mut ModelContext<Self>) {
        if let Some(file) = &self.file {
            file.buffer_updated(self.remote_id, operation, cx.as_mut());
        }
    }

    #[cfg(test)]
    pub fn send_operation(&mut self, operation: Operation, _: &mut ModelContext<Self>) {
        self.operations.push(operation);
    }

    pub fn remove_peer(&mut self, replica_id: ReplicaId, cx: &mut ModelContext<Self>) {
        self.selections
            .retain(|set_id, _| set_id.replica_id != replica_id);
        cx.notify();
    }

    pub fn undo(&mut self, cx: &mut ModelContext<Self>) {
        let was_dirty = self.is_dirty();
        let old_version = self.version.clone();

        if let Some(transaction) = self.history.pop_undo().cloned() {
            let selections = transaction.selections_before.clone();
            self.undo_or_redo(transaction, cx).unwrap();
            if let Some((set_id, selections)) = selections {
                let _ = self.update_selection_set(set_id, selections, cx);
            }
        }

        cx.notify();
        if self.edits_since(old_version).next().is_some() {
            self.did_edit(was_dirty, cx);
            self.reparse(cx);
        }
    }

    pub fn redo(&mut self, cx: &mut ModelContext<Self>) {
        let was_dirty = self.is_dirty();
        let old_version = self.version.clone();

        if let Some(transaction) = self.history.pop_redo().cloned() {
            let selections = transaction.selections_after.clone();
            self.undo_or_redo(transaction, cx).unwrap();
            if let Some((set_id, selections)) = selections {
                let _ = self.update_selection_set(set_id, selections, cx);
            }
        }

        cx.notify();
        if self.edits_since(old_version).next().is_some() {
            self.did_edit(was_dirty, cx);
            self.reparse(cx);
        }
    }

    fn undo_or_redo(
        &mut self,
        transaction: Transaction,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        let mut counts = HashMap::default();
        for edit_id in transaction.edits {
            counts.insert(edit_id, self.undo_map.undo_count(edit_id) + 1);
        }

        let undo = UndoOperation {
            id: self.local_clock.tick(),
            counts,
            ranges: transaction.ranges,
            version: transaction.start.clone(),
        };
        self.apply_undo(&undo)?;
        self.version.observe(undo.id);

        let operation = Operation::Undo {
            undo,
            lamport_timestamp: self.lamport_clock.tick(),
        };
        self.send_operation(operation, cx);

        Ok(())
    }

    fn apply_undo(&mut self, undo: &UndoOperation) -> Result<()> {
        self.undo_map.insert(undo);

        let mut cx = undo.version.clone();
        for edit_id in undo.counts.keys().copied() {
            cx.observe(edit_id);
        }
        let cx = Some(cx);

        let mut old_fragments = self.fragments.cursor::<VersionedOffset>();
        let mut new_fragments = old_fragments.slice(
            &VersionedOffset::Offset(undo.ranges[0].start),
            Bias::Right,
            &cx,
        );
        let mut new_ropes =
            RopeBuilder::new(self.visible_text.cursor(0), self.deleted_text.cursor(0));
        new_ropes.push_tree(new_fragments.summary().text);

        for range in &undo.ranges {
            let mut end_offset = old_fragments.end(&cx).offset();

            if end_offset < range.start {
                let preceding_fragments =
                    old_fragments.slice(&VersionedOffset::Offset(range.start), Bias::Right, &cx);
                new_ropes.push_tree(preceding_fragments.summary().text);
                new_fragments.push_tree(preceding_fragments, &None);
            }

            while end_offset <= range.end {
                if let Some(fragment) = old_fragments.item() {
                    let mut fragment = fragment.clone();
                    let fragment_was_visible = fragment.visible;

                    if fragment.was_visible(&undo.version, &self.undo_map)
                        || undo.counts.contains_key(&fragment.timestamp.local())
                    {
                        fragment.visible = fragment.is_visible(&self.undo_map);
                        fragment.max_undos.observe(undo.id);
                    }
                    new_ropes.push_fragment(&fragment, fragment_was_visible);
                    new_fragments.push(fragment, &None);

                    old_fragments.next(&cx);
                    if end_offset == old_fragments.end(&cx).offset() {
                        let unseen_fragments = old_fragments.slice(
                            &VersionedOffset::Offset(end_offset),
                            Bias::Right,
                            &cx,
                        );
                        new_ropes.push_tree(unseen_fragments.summary().text);
                        new_fragments.push_tree(unseen_fragments, &None);
                    }
                    end_offset = old_fragments.end(&cx).offset();
                } else {
                    break;
                }
            }
        }

        let suffix = old_fragments.suffix(&cx);
        new_ropes.push_tree(suffix.summary().text);
        new_fragments.push_tree(suffix, &None);

        drop(old_fragments);
        let (visible_text, deleted_text) = new_ropes.finish();
        self.fragments = new_fragments;
        self.visible_text = visible_text;
        self.deleted_text = deleted_text;
        Ok(())
    }

    fn flush_deferred_ops(&mut self) -> Result<()> {
        self.deferred_replicas.clear();
        let mut deferred_ops = Vec::new();
        for op in self.deferred_ops.drain().cursor().cloned() {
            if self.can_apply_op(&op) {
                self.apply_op(op)?;
            } else {
                self.deferred_replicas.insert(op.replica_id());
                deferred_ops.push(op);
            }
        }
        self.deferred_ops.insert(deferred_ops);
        Ok(())
    }

    fn can_apply_op(&self, op: &Operation) -> bool {
        if self.deferred_replicas.contains(&op.replica_id()) {
            false
        } else {
            match op {
                Operation::Edit(edit) => self.version >= edit.version,
                Operation::Undo { undo, .. } => self.version >= undo.version,
                Operation::UpdateSelections { selections, .. } => {
                    if let Some(selections) = selections {
                        selections.iter().all(|selection| {
                            let contains_start = self.version >= selection.start.version;
                            let contains_end = self.version >= selection.end.version;
                            contains_start && contains_end
                        })
                    } else {
                        true
                    }
                }
                Operation::SetActiveSelections { set_id, .. } => {
                    set_id.map_or(true, |set_id| self.selections.contains_key(&set_id))
                }
                #[cfg(test)]
                Operation::Test(_) => true,
            }
        }
    }

    fn apply_local_edit(
        &mut self,
        ranges: &[Range<usize>],
        new_text: Option<String>,
        timestamp: InsertionTimestamp,
    ) -> EditOperation {
        let mut edit = EditOperation {
            timestamp,
            version: self.version(),
            ranges: Vec::with_capacity(ranges.len()),
            new_text: None,
        };

        let mut new_ropes =
            RopeBuilder::new(self.visible_text.cursor(0), self.deleted_text.cursor(0));
        let mut old_fragments = self.fragments.cursor::<FragmentTextSummary>();
        let mut new_fragments = old_fragments.slice(&ranges[0].start, Bias::Right, &None);
        new_ropes.push_tree(new_fragments.summary().text);

        let mut fragment_start = old_fragments.start().visible;
        for range in ranges {
            let fragment_end = old_fragments.end(&None).visible;

            // If the current fragment ends before this range, then jump ahead to the first fragment
            // that extends past the start of this range, reusing any intervening fragments.
            if fragment_end < range.start {
                // If the current fragment has been partially consumed, then consume the rest of it
                // and advance to the next fragment before slicing.
                if fragment_start > old_fragments.start().visible {
                    if fragment_end > fragment_start {
                        let mut suffix = old_fragments.item().unwrap().clone();
                        suffix.len = fragment_end - fragment_start;
                        new_ropes.push_fragment(&suffix, suffix.visible);
                        new_fragments.push(suffix, &None);
                    }
                    old_fragments.next(&None);
                }

                let slice = old_fragments.slice(&range.start, Bias::Right, &None);
                new_ropes.push_tree(slice.summary().text);
                new_fragments.push_tree(slice, &None);
                fragment_start = old_fragments.start().visible;
            }

            let full_range_start = range.start + old_fragments.start().deleted;

            // Preserve any portion of the current fragment that precedes this range.
            if fragment_start < range.start {
                let mut prefix = old_fragments.item().unwrap().clone();
                prefix.len = range.start - fragment_start;
                new_ropes.push_fragment(&prefix, prefix.visible);
                new_fragments.push(prefix, &None);
                fragment_start = range.start;
            }

            // Insert the new text before any existing fragments within the range.
            if let Some(new_text) = new_text.as_deref() {
                new_ropes.push_str(new_text);
                new_fragments.push(
                    Fragment {
                        timestamp,
                        len: new_text.len(),
                        deletions: Default::default(),
                        max_undos: Default::default(),
                        visible: true,
                    },
                    &None,
                );
            }

            // Advance through every fragment that intersects this range, marking the intersecting
            // portions as deleted.
            while fragment_start < range.end {
                let fragment = old_fragments.item().unwrap();
                let fragment_end = old_fragments.end(&None).visible;
                let mut intersection = fragment.clone();
                let intersection_end = cmp::min(range.end, fragment_end);
                if fragment.visible {
                    intersection.len = intersection_end - fragment_start;
                    intersection.deletions.insert(timestamp.local());
                    intersection.visible = false;
                }
                if intersection.len > 0 {
                    new_ropes.push_fragment(&intersection, fragment.visible);
                    new_fragments.push(intersection, &None);
                    fragment_start = intersection_end;
                }
                if fragment_end <= range.end {
                    old_fragments.next(&None);
                }
            }

            let full_range_end = range.end + old_fragments.start().deleted;
            edit.ranges.push(full_range_start..full_range_end);
        }

        // If the current fragment has been partially consumed, then consume the rest of it
        // and advance to the next fragment before slicing.
        if fragment_start > old_fragments.start().visible {
            let fragment_end = old_fragments.end(&None).visible;
            if fragment_end > fragment_start {
                let mut suffix = old_fragments.item().unwrap().clone();
                suffix.len = fragment_end - fragment_start;
                new_ropes.push_fragment(&suffix, suffix.visible);
                new_fragments.push(suffix, &None);
            }
            old_fragments.next(&None);
        }

        let suffix = old_fragments.suffix(&None);
        new_ropes.push_tree(suffix.summary().text);
        new_fragments.push_tree(suffix, &None);
        let (visible_text, deleted_text) = new_ropes.finish();
        drop(old_fragments);

        self.fragments = new_fragments;
        self.visible_text = visible_text;
        self.deleted_text = deleted_text;
        edit.new_text = new_text;
        edit
    }

    fn content<'a>(&'a self) -> Content<'a> {
        self.into()
    }

    pub fn text_summary_for_range(&self, range: Range<usize>) -> TextSummary {
        self.content().text_summary_for_range(range)
    }

    pub fn anchor_before<T: ToOffset>(&self, position: T) -> Anchor {
        self.anchor_at(position, Bias::Left)
    }

    pub fn anchor_after<T: ToOffset>(&self, position: T) -> Anchor {
        self.anchor_at(position, Bias::Right)
    }

    pub fn anchor_at<T: ToOffset>(&self, position: T, bias: Bias) -> Anchor {
        self.content().anchor_at(position, bias)
    }

    pub fn point_for_offset(&self, offset: usize) -> Result<Point> {
        self.content().point_for_offset(offset)
    }

    pub fn clip_point(&self, point: Point, bias: Bias) -> Point {
        self.visible_text.clip_point(point, bias)
    }

    pub fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        self.visible_text.clip_offset(offset, bias)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Buffer {
    fn random_byte_range(&mut self, start_offset: usize, rng: &mut impl rand::Rng) -> Range<usize> {
        let end = self.clip_offset(rng.gen_range(start_offset..=self.len()), Bias::Right);
        let start = self.clip_offset(rng.gen_range(start_offset..=end), Bias::Right);
        start..end
    }

    pub fn randomly_edit<T>(
        &mut self,
        rng: &mut T,
        old_range_count: usize,
        cx: &mut ModelContext<Self>,
    ) -> (Vec<Range<usize>>, String)
    where
        T: rand::Rng,
    {
        let mut old_ranges: Vec<Range<usize>> = Vec::new();
        for _ in 0..old_range_count {
            let last_end = old_ranges.last().map_or(0, |last_range| last_range.end + 1);
            if last_end > self.len() {
                break;
            }
            old_ranges.push(self.random_byte_range(last_end, rng));
        }
        let new_text_len = rng.gen_range(0..10);
        let new_text: String = crate::random_char_iter::RandomCharIter::new(&mut *rng)
            .take(new_text_len)
            .collect();
        log::info!(
            "mutating buffer {} at {:?}: {:?}",
            self.replica_id,
            old_ranges,
            new_text
        );
        self.edit(old_ranges.iter().cloned(), new_text.as_str(), cx);
        (old_ranges, new_text)
    }

    pub fn randomly_mutate<T>(
        &mut self,
        rng: &mut T,
        cx: &mut ModelContext<Self>,
    ) -> (Vec<Range<usize>>, String)
    where
        T: rand::Rng,
    {
        use rand::prelude::*;

        let (old_ranges, new_text) = self.randomly_edit(rng, 5, cx);

        // Randomly add, remove or mutate selection sets.
        let replica_selection_sets = &self
            .selection_sets()
            .map(|(set_id, _)| *set_id)
            .filter(|set_id| self.replica_id == set_id.replica_id)
            .collect::<Vec<_>>();
        let set_id = replica_selection_sets.choose(rng);
        if set_id.is_some() && rng.gen_bool(1.0 / 6.0) {
            self.remove_selection_set(*set_id.unwrap(), cx).unwrap();
        } else {
            let mut ranges = Vec::new();
            for _ in 0..5 {
                ranges.push(self.random_byte_range(0, rng));
            }
            let new_selections = self.selections_from_ranges(ranges).unwrap();

            if set_id.is_none() || rng.gen_bool(1.0 / 5.0) {
                self.add_selection_set(new_selections, cx);
            } else {
                self.update_selection_set(*set_id.unwrap(), new_selections, cx)
                    .unwrap();
            }
        }

        (old_ranges, new_text)
    }

    pub fn randomly_undo_redo(&mut self, rng: &mut impl rand::Rng, cx: &mut ModelContext<Self>) {
        use rand::prelude::*;

        for _ in 0..rng.gen_range(1..=5) {
            if let Some(transaction) = self.history.undo_stack.choose(rng).cloned() {
                log::info!(
                    "undoing buffer {} transaction {:?}",
                    self.replica_id,
                    transaction
                );
                self.undo_or_redo(transaction, cx).unwrap();
            }
        }
    }

    fn selections_from_ranges<I>(&self, ranges: I) -> Result<Vec<Selection>>
    where
        I: IntoIterator<Item = Range<usize>>,
    {
        use std::sync::atomic::{self, AtomicUsize};

        static NEXT_SELECTION_ID: AtomicUsize = AtomicUsize::new(0);

        let mut ranges = ranges.into_iter().collect::<Vec<_>>();
        ranges.sort_unstable_by_key(|range| range.start);

        let mut selections = Vec::with_capacity(ranges.len());
        for range in ranges {
            if range.start > range.end {
                selections.push(Selection {
                    id: NEXT_SELECTION_ID.fetch_add(1, atomic::Ordering::SeqCst),
                    start: self.anchor_before(range.end),
                    end: self.anchor_before(range.start),
                    reversed: true,
                    goal: SelectionGoal::None,
                });
            } else {
                selections.push(Selection {
                    id: NEXT_SELECTION_ID.fetch_add(1, atomic::Ordering::SeqCst),
                    start: self.anchor_after(range.start),
                    end: self.anchor_before(range.end),
                    reversed: false,
                    goal: SelectionGoal::None,
                });
            }
        }
        Ok(selections)
    }

    pub fn selection_ranges<'a>(&'a self, set_id: SelectionSetId) -> Result<Vec<Range<usize>>> {
        Ok(self
            .selection_set(set_id)?
            .selections
            .iter()
            .map(move |selection| {
                let start = selection.start.to_offset(self);
                let end = selection.end.to_offset(self);
                if selection.reversed {
                    end..start
                } else {
                    start..end
                }
            })
            .collect())
    }

    pub fn all_selection_ranges<'a>(
        &'a self,
    ) -> impl 'a + Iterator<Item = (SelectionSetId, Vec<Range<usize>>)> {
        self.selections
            .keys()
            .map(move |set_id| (*set_id, self.selection_ranges(*set_id).unwrap()))
    }

    pub fn enclosing_bracket_point_ranges<T: ToOffset>(
        &self,
        range: Range<T>,
    ) -> Option<(Range<Point>, Range<Point>)> {
        self.enclosing_bracket_ranges(range).map(|(start, end)| {
            let point_start = start.start.to_point(self)..start.end.to_point(self);
            let point_end = end.start.to_point(self)..end.end.to_point(self);
            (point_start, point_end)
        })
    }
}

impl Clone for Buffer {
    fn clone(&self) -> Self {
        Self {
            fragments: self.fragments.clone(),
            visible_text: self.visible_text.clone(),
            deleted_text: self.deleted_text.clone(),
            version: self.version.clone(),
            saved_version: self.saved_version.clone(),
            saved_mtime: self.saved_mtime,
            last_edit: self.last_edit.clone(),
            undo_map: self.undo_map.clone(),
            history: self.history.clone(),
            selections: self.selections.clone(),
            deferred_ops: self.deferred_ops.clone(),
            file: self.file.as_ref().map(|f| f.boxed_clone()),
            language: self.language.clone(),
            syntax_tree: Mutex::new(self.syntax_tree.lock().clone()),
            parsing_in_background: false,
            sync_parse_timeout: self.sync_parse_timeout,
            parse_count: self.parse_count,
            autoindent_requests: Default::default(),
            pending_autoindent: Default::default(),
            deferred_replicas: self.deferred_replicas.clone(),
            replica_id: self.replica_id,
            remote_id: self.remote_id.clone(),
            local_clock: self.local_clock.clone(),
            lamport_clock: self.lamport_clock.clone(),

            #[cfg(test)]
            operations: self.operations.clone(),
        }
    }
}

pub struct Snapshot {
    visible_text: Rope,
    fragments: SumTree<Fragment>,
    version: clock::Global,
    tree: Option<Tree>,
    is_parsing: bool,
    language: Option<Arc<Language>>,
    query_cursor: QueryCursorHandle,
}

impl Clone for Snapshot {
    fn clone(&self) -> Self {
        Self {
            visible_text: self.visible_text.clone(),
            fragments: self.fragments.clone(),
            version: self.version.clone(),
            tree: self.tree.clone(),
            is_parsing: self.is_parsing,
            language: self.language.clone(),
            query_cursor: QueryCursorHandle::new(),
        }
    }
}

impl Snapshot {
    pub fn len(&self) -> usize {
        self.visible_text.len()
    }

    pub fn line_len(&self, row: u32) -> u32 {
        self.content().line_len(row)
    }

    pub fn indent_column_for_line(&self, row: u32) -> u32 {
        self.content().indent_column_for_line(row)
    }

    fn suggest_autoindents<'a>(
        &'a self,
        row_range: Range<u32>,
    ) -> Option<impl Iterator<Item = IndentSuggestion> + 'a> {
        let mut query_cursor = QueryCursorHandle::new();
        if let Some((language, tree)) = self.language.as_ref().zip(self.tree.as_ref()) {
            let prev_non_blank_row = self.prev_non_blank_row(row_range.start);

            // Get the "indentation ranges" that intersect this row range.
            let indent_capture_ix = language.indents_query.capture_index_for_name("indent");
            let end_capture_ix = language.indents_query.capture_index_for_name("end");
            query_cursor.set_point_range(
                Point::new(prev_non_blank_row.unwrap_or(row_range.start), 0).into()
                    ..Point::new(row_range.end, 0).into(),
            );
            let mut indentation_ranges = Vec::<(Range<Point>, &'static str)>::new();
            for mat in query_cursor.matches(
                &language.indents_query,
                tree.root_node(),
                TextProvider(&self.visible_text),
            ) {
                let mut node_kind = "";
                let mut start: Option<Point> = None;
                let mut end: Option<Point> = None;
                for capture in mat.captures {
                    if Some(capture.index) == indent_capture_ix {
                        node_kind = capture.node.kind();
                        start.get_or_insert(capture.node.start_position().into());
                        end.get_or_insert(capture.node.end_position().into());
                    } else if Some(capture.index) == end_capture_ix {
                        end = Some(capture.node.start_position().into());
                    }
                }

                if let Some((start, end)) = start.zip(end) {
                    if start.row == end.row {
                        continue;
                    }

                    let range = start..end;
                    match indentation_ranges.binary_search_by_key(&range.start, |r| r.0.start) {
                        Err(ix) => indentation_ranges.insert(ix, (range, node_kind)),
                        Ok(ix) => {
                            let prev_range = &mut indentation_ranges[ix];
                            prev_range.0.end = prev_range.0.end.max(range.end);
                        }
                    }
                }
            }

            let mut prev_row = prev_non_blank_row.unwrap_or(0);
            Some(row_range.map(move |row| {
                let row_start = Point::new(row, self.indent_column_for_line(row));

                let mut indent_from_prev_row = false;
                let mut outdent_to_row = u32::MAX;
                for (range, _node_kind) in &indentation_ranges {
                    if range.start.row >= row {
                        break;
                    }

                    if range.start.row == prev_row && range.end > row_start {
                        indent_from_prev_row = true;
                    }
                    if range.end.row >= prev_row && range.end <= row_start {
                        outdent_to_row = outdent_to_row.min(range.start.row);
                    }
                }

                let suggestion = if outdent_to_row == prev_row {
                    IndentSuggestion {
                        basis_row: prev_row,
                        indent: false,
                    }
                } else if indent_from_prev_row {
                    IndentSuggestion {
                        basis_row: prev_row,
                        indent: true,
                    }
                } else if outdent_to_row < prev_row {
                    IndentSuggestion {
                        basis_row: outdent_to_row,
                        indent: false,
                    }
                } else {
                    IndentSuggestion {
                        basis_row: prev_row,
                        indent: false,
                    }
                };

                prev_row = row;
                suggestion
            }))
        } else {
            None
        }
    }

    fn prev_non_blank_row(&self, mut row: u32) -> Option<u32> {
        while row > 0 {
            row -= 1;
            if !self.is_line_blank(row) {
                return Some(row);
            }
        }
        None
    }

    fn is_line_blank(&self, row: u32) -> bool {
        self.text_for_range(Point::new(row, 0)..Point::new(row, self.line_len(row)))
            .all(|chunk| chunk.matches(|c: char| !c.is_whitespace()).next().is_none())
    }

    pub fn text(&self) -> Rope {
        self.visible_text.clone()
    }

    pub fn text_summary(&self) -> TextSummary {
        self.visible_text.summary()
    }

    pub fn max_point(&self) -> Point {
        self.visible_text.max_point()
    }

    pub fn text_for_range<T: ToOffset>(&self, range: Range<T>) -> Chunks {
        let range = range.start.to_offset(self)..range.end.to_offset(self);
        self.visible_text.chunks_in_range(range)
    }

    pub fn highlighted_text_for_range<T: ToOffset>(
        &mut self,
        range: Range<T>,
    ) -> HighlightedChunks {
        let range = range.start.to_offset(&*self)..range.end.to_offset(&*self);
        let chunks = self.visible_text.chunks_in_range(range.clone());
        if let Some((language, tree)) = self.language.as_ref().zip(self.tree.as_ref()) {
            let captures = self.query_cursor.set_byte_range(range.clone()).captures(
                &language.highlights_query,
                tree.root_node(),
                TextProvider(&self.visible_text),
            );

            HighlightedChunks {
                range,
                chunks,
                highlights: Some(Highlights {
                    captures,
                    next_capture: None,
                    stack: Default::default(),
                    highlight_map: language.highlight_map(),
                }),
            }
        } else {
            HighlightedChunks {
                range,
                chunks,
                highlights: None,
            }
        }
    }

    pub fn text_summary_for_range<T>(&self, range: Range<T>) -> TextSummary
    where
        T: ToOffset,
    {
        let range = range.start.to_offset(self.content())..range.end.to_offset(self.content());
        self.content().text_summary_for_range(range)
    }

    pub fn point_for_offset(&self, offset: usize) -> Result<Point> {
        self.content().point_for_offset(offset)
    }

    pub fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        self.visible_text.clip_offset(offset, bias)
    }

    pub fn clip_point(&self, point: Point, bias: Bias) -> Point {
        self.visible_text.clip_point(point, bias)
    }

    pub fn to_offset(&self, point: Point) -> usize {
        self.visible_text.to_offset(point)
    }

    pub fn to_point(&self, offset: usize) -> Point {
        self.visible_text.to_point(offset)
    }

    pub fn anchor_before<T: ToOffset>(&self, position: T) -> Anchor {
        self.content().anchor_at(position, Bias::Left)
    }

    pub fn anchor_after<T: ToOffset>(&self, position: T) -> Anchor {
        self.content().anchor_at(position, Bias::Right)
    }

    fn content(&self) -> Content {
        self.into()
    }
}

pub struct Content<'a> {
    visible_text: &'a Rope,
    fragments: &'a SumTree<Fragment>,
    version: &'a clock::Global,
}

impl<'a> From<&'a Snapshot> for Content<'a> {
    fn from(snapshot: &'a Snapshot) -> Self {
        Self {
            visible_text: &snapshot.visible_text,
            fragments: &snapshot.fragments,
            version: &snapshot.version,
        }
    }
}

impl<'a> From<&'a Buffer> for Content<'a> {
    fn from(buffer: &'a Buffer) -> Self {
        Self {
            visible_text: &buffer.visible_text,
            fragments: &buffer.fragments,
            version: &buffer.version,
        }
    }
}

impl<'a> From<&'a mut Buffer> for Content<'a> {
    fn from(buffer: &'a mut Buffer) -> Self {
        Self {
            visible_text: &buffer.visible_text,
            fragments: &buffer.fragments,
            version: &buffer.version,
        }
    }
}

impl<'a> From<&'a Content<'a>> for Content<'a> {
    fn from(content: &'a Content) -> Self {
        Self {
            visible_text: &content.visible_text,
            fragments: &content.fragments,
            version: &content.version,
        }
    }
}

impl<'a> Content<'a> {
    fn max_point(&self) -> Point {
        self.visible_text.max_point()
    }

    fn len(&self) -> usize {
        self.fragments.extent::<usize>(&None)
    }

    pub fn chars_at<T: ToOffset>(&self, position: T) -> impl Iterator<Item = char> + 'a {
        let offset = position.to_offset(self);
        self.visible_text.chars_at(offset)
    }

    pub fn text_for_range<T: ToOffset>(&self, range: Range<T>) -> Chunks<'a> {
        let start = range.start.to_offset(self);
        let end = range.end.to_offset(self);
        self.visible_text.chunks_in_range(start..end)
    }

    fn line_len(&self, row: u32) -> u32 {
        let row_start_offset = Point::new(row, 0).to_offset(self);
        let row_end_offset = if row >= self.max_point().row {
            self.len()
        } else {
            Point::new(row + 1, 0).to_offset(self) - 1
        };
        (row_end_offset - row_start_offset) as u32
    }

    pub fn indent_column_for_line(&self, row: u32) -> u32 {
        let mut result = 0;
        for c in self.chars_at(Point::new(row, 0)) {
            if c == ' ' {
                result += 1;
            } else {
                break;
            }
        }
        result
    }

    fn summary_for_anchor(&self, anchor: &Anchor) -> TextSummary {
        let cx = Some(anchor.version.clone());
        let mut cursor = self.fragments.cursor::<(VersionedOffset, usize)>();
        cursor.seek(&VersionedOffset::Offset(anchor.offset), anchor.bias, &cx);
        let overshoot = if cursor.item().map_or(false, |fragment| fragment.visible) {
            anchor.offset - cursor.start().0.offset()
        } else {
            0
        };
        self.text_summary_for_range(0..cursor.start().1 + overshoot)
    }

    fn text_summary_for_range(&self, range: Range<usize>) -> TextSummary {
        self.visible_text.cursor(range.start).summary(range.end)
    }

    fn summaries_for_anchors<T>(
        &self,
        map: &'a AnchorMap<T>,
    ) -> impl Iterator<Item = (TextSummary, &'a T)> {
        let cx = Some(map.version.clone());
        let mut summary = TextSummary::default();
        let mut rope_cursor = self.visible_text.cursor(0);
        let mut cursor = self.fragments.cursor::<(VersionedOffset, usize)>();
        map.entries.iter().map(move |((offset, bias), value)| {
            cursor.seek_forward(&VersionedOffset::Offset(*offset), *bias, &cx);
            let overshoot = if cursor.item().map_or(false, |fragment| fragment.visible) {
                offset - cursor.start().0.offset()
            } else {
                0
            };
            summary += rope_cursor.summary(cursor.start().1 + overshoot);
            (summary.clone(), value)
        })
    }

    fn summaries_for_anchor_ranges<T>(
        &self,
        map: &'a AnchorRangeMap<T>,
    ) -> impl Iterator<Item = (Range<TextSummary>, &'a T)> {
        let cx = Some(map.version.clone());
        let mut summary = TextSummary::default();
        let mut rope_cursor = self.visible_text.cursor(0);
        let mut cursor = self.fragments.cursor::<(VersionedOffset, usize)>();
        map.entries.iter().map(move |(range, value)| {
            let Range {
                start: (start_offset, start_bias),
                end: (end_offset, end_bias),
            } = range;

            cursor.seek_forward(&VersionedOffset::Offset(*start_offset), *start_bias, &cx);
            let overshoot = if cursor.item().map_or(false, |fragment| fragment.visible) {
                start_offset - cursor.start().0.offset()
            } else {
                0
            };
            summary += rope_cursor.summary(cursor.start().1 + overshoot);
            let start_summary = summary.clone();

            cursor.seek_forward(&VersionedOffset::Offset(*end_offset), *end_bias, &cx);
            let overshoot = if cursor.item().map_or(false, |fragment| fragment.visible) {
                end_offset - cursor.start().0.offset()
            } else {
                0
            };
            summary += rope_cursor.summary(cursor.start().1 + overshoot);
            let end_summary = summary.clone();

            (start_summary..end_summary, value)
        })
    }

    fn anchor_at<T: ToOffset>(&self, position: T, bias: Bias) -> Anchor {
        let offset = position.to_offset(self);
        let max_offset = self.len();
        assert!(offset <= max_offset, "offset is out of range");
        let mut cursor = self.fragments.cursor::<FragmentTextSummary>();
        cursor.seek(&offset, bias, &None);
        Anchor {
            offset: offset + cursor.start().deleted,
            bias,
            version: self.version.clone(),
        }
    }

    pub fn anchor_map<T, E>(&self, entries: E) -> AnchorMap<T>
    where
        E: IntoIterator<Item = ((usize, Bias), T)>,
    {
        let version = self.version.clone();
        let mut cursor = self.fragments.cursor::<FragmentTextSummary>();
        let entries = entries
            .into_iter()
            .map(|((offset, bias), value)| {
                cursor.seek_forward(&offset, bias, &None);
                let full_offset = cursor.start().deleted + offset;
                ((full_offset, bias), value)
            })
            .collect();

        AnchorMap { version, entries }
    }

    pub fn anchor_range_map<T, E>(&self, entries: E) -> AnchorRangeMap<T>
    where
        E: IntoIterator<Item = (Range<(usize, Bias)>, T)>,
    {
        let version = self.version.clone();
        let mut cursor = self.fragments.cursor::<FragmentTextSummary>();
        let entries = entries
            .into_iter()
            .map(|(range, value)| {
                let Range {
                    start: (start_offset, start_bias),
                    end: (end_offset, end_bias),
                } = range;
                cursor.seek_forward(&start_offset, start_bias, &None);
                let full_start_offset = cursor.start().deleted + start_offset;
                cursor.seek_forward(&end_offset, end_bias, &None);
                let full_end_offset = cursor.start().deleted + end_offset;
                (
                    (full_start_offset, start_bias)..(full_end_offset, end_bias),
                    value,
                )
            })
            .collect();

        AnchorRangeMap { version, entries }
    }

    pub fn anchor_set<E>(&self, entries: E) -> AnchorSet
    where
        E: IntoIterator<Item = (usize, Bias)>,
    {
        AnchorSet(self.anchor_map(entries.into_iter().map(|range| (range, ()))))
    }

    pub fn anchor_range_set<E>(&self, entries: E) -> AnchorRangeSet
    where
        E: IntoIterator<Item = Range<(usize, Bias)>>,
    {
        AnchorRangeSet(self.anchor_range_map(entries.into_iter().map(|range| (range, ()))))
    }

    fn full_offset_for_anchor(&self, anchor: &Anchor) -> usize {
        let cx = Some(anchor.version.clone());
        let mut cursor = self
            .fragments
            .cursor::<(VersionedOffset, FragmentTextSummary)>();
        cursor.seek(&VersionedOffset::Offset(anchor.offset), anchor.bias, &cx);
        let overshoot = if cursor.item().is_some() {
            anchor.offset - cursor.start().0.offset()
        } else {
            0
        };
        let summary = cursor.start().1;
        summary.visible + summary.deleted + overshoot
    }

    fn point_for_offset(&self, offset: usize) -> Result<Point> {
        if offset <= self.len() {
            Ok(self.text_summary_for_range(0..offset).lines)
        } else {
            Err(anyhow!("offset out of bounds"))
        }
    }
}

#[derive(Debug)]
struct IndentSuggestion {
    basis_row: u32,
    indent: bool,
}

struct RopeBuilder<'a> {
    old_visible_cursor: rope::Cursor<'a>,
    old_deleted_cursor: rope::Cursor<'a>,
    new_visible: Rope,
    new_deleted: Rope,
}

impl<'a> RopeBuilder<'a> {
    fn new(old_visible_cursor: rope::Cursor<'a>, old_deleted_cursor: rope::Cursor<'a>) -> Self {
        Self {
            old_visible_cursor,
            old_deleted_cursor,
            new_visible: Rope::new(),
            new_deleted: Rope::new(),
        }
    }

    fn push_tree(&mut self, len: FragmentTextSummary) {
        self.push(len.visible, true, true);
        self.push(len.deleted, false, false);
    }

    fn push_fragment(&mut self, fragment: &Fragment, was_visible: bool) {
        debug_assert!(fragment.len > 0);
        self.push(fragment.len, was_visible, fragment.visible)
    }

    fn push(&mut self, len: usize, was_visible: bool, is_visible: bool) {
        let text = if was_visible {
            self.old_visible_cursor
                .slice(self.old_visible_cursor.offset() + len)
        } else {
            self.old_deleted_cursor
                .slice(self.old_deleted_cursor.offset() + len)
        };
        if is_visible {
            self.new_visible.append(text);
        } else {
            self.new_deleted.append(text);
        }
    }

    fn push_str(&mut self, text: &str) {
        self.new_visible.push(text);
    }

    fn finish(mut self) -> (Rope, Rope) {
        self.new_visible.append(self.old_visible_cursor.suffix());
        self.new_deleted.append(self.old_deleted_cursor.suffix());
        (self.new_visible, self.new_deleted)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    Edited,
    Dirtied,
    Saved,
    FileHandleChanged,
    Reloaded,
    Reparsed,
    Closed,
}

impl Entity for Buffer {
    type Event = Event;

    fn release(&mut self, cx: &mut gpui::MutableAppContext) {
        if let Some(file) = self.file.as_ref() {
            file.buffer_removed(self.remote_id, cx);
        }
    }
}

impl<'a, F: Fn(&FragmentSummary) -> bool> Iterator for Edits<'a, F> {
    type Item = Edit;

    fn next(&mut self) -> Option<Self::Item> {
        let mut change: Option<Edit> = None;
        let cursor = self.cursor.as_mut()?;

        while let Some(fragment) = cursor.item() {
            let bytes = cursor.start().visible - self.new_offset;
            let lines = self.visible_text.to_point(cursor.start().visible) - self.new_point;
            self.old_offset += bytes;
            self.old_point += &lines;
            self.new_offset += bytes;
            self.new_point += &lines;

            if !fragment.was_visible(&self.since, &self.undos) && fragment.visible {
                let fragment_lines =
                    self.visible_text.to_point(self.new_offset + fragment.len) - self.new_point;
                if let Some(ref mut change) = change {
                    if change.new_bytes.end == self.new_offset {
                        change.new_bytes.end += fragment.len;
                    } else {
                        break;
                    }
                } else {
                    change = Some(Edit {
                        old_bytes: self.old_offset..self.old_offset,
                        new_bytes: self.new_offset..self.new_offset + fragment.len,
                        old_lines: self.old_point..self.old_point,
                    });
                }

                self.new_offset += fragment.len;
                self.new_point += &fragment_lines;
            } else if fragment.was_visible(&self.since, &self.undos) && !fragment.visible {
                let deleted_start = cursor.start().deleted;
                let fragment_lines = self.deleted_text.to_point(deleted_start + fragment.len)
                    - self.deleted_text.to_point(deleted_start);
                if let Some(ref mut change) = change {
                    if change.new_bytes.end == self.new_offset {
                        change.old_bytes.end += fragment.len;
                        change.old_lines.end += &fragment_lines;
                    } else {
                        break;
                    }
                } else {
                    change = Some(Edit {
                        old_bytes: self.old_offset..self.old_offset + fragment.len,
                        new_bytes: self.new_offset..self.new_offset,
                        old_lines: self.old_point..self.old_point + &fragment_lines,
                    });
                }

                self.old_offset += fragment.len;
                self.old_point += &fragment_lines;
            }

            cursor.next(&None);
        }

        change
    }
}

struct ByteChunks<'a>(rope::Chunks<'a>);

impl<'a> Iterator for ByteChunks<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(str::as_bytes)
    }
}

struct TextProvider<'a>(&'a Rope);

impl<'a> tree_sitter::TextProvider<'a> for TextProvider<'a> {
    type I = ByteChunks<'a>;

    fn text(&mut self, node: tree_sitter::Node) -> Self::I {
        ByteChunks(self.0.chunks_in_range(node.byte_range()))
    }
}

struct Highlights<'a> {
    captures: tree_sitter::QueryCaptures<'a, 'a, TextProvider<'a>>,
    next_capture: Option<(tree_sitter::QueryMatch<'a, 'a>, usize)>,
    stack: Vec<(usize, HighlightId)>,
    highlight_map: HighlightMap,
}

pub struct HighlightedChunks<'a> {
    range: Range<usize>,
    chunks: Chunks<'a>,
    highlights: Option<Highlights<'a>>,
}

impl<'a> HighlightedChunks<'a> {
    pub fn seek(&mut self, offset: usize) {
        self.range.start = offset;
        self.chunks.seek(self.range.start);
        if let Some(highlights) = self.highlights.as_mut() {
            highlights
                .stack
                .retain(|(end_offset, _)| *end_offset > offset);
            if let Some((mat, capture_ix)) = &highlights.next_capture {
                let capture = mat.captures[*capture_ix as usize];
                if offset >= capture.node.start_byte() {
                    let next_capture_end = capture.node.end_byte();
                    if offset < next_capture_end {
                        highlights.stack.push((
                            next_capture_end,
                            highlights.highlight_map.get(capture.index),
                        ));
                    }
                    highlights.next_capture.take();
                }
            }
            highlights.captures.set_byte_range(self.range.clone());
        }
    }

    pub fn offset(&self) -> usize {
        self.range.start
    }
}

impl<'a> Iterator for HighlightedChunks<'a> {
    type Item = (&'a str, HighlightId);

    fn next(&mut self) -> Option<Self::Item> {
        let mut next_capture_start = usize::MAX;

        if let Some(highlights) = self.highlights.as_mut() {
            while let Some((parent_capture_end, _)) = highlights.stack.last() {
                if *parent_capture_end <= self.range.start {
                    highlights.stack.pop();
                } else {
                    break;
                }
            }

            if highlights.next_capture.is_none() {
                highlights.next_capture = highlights.captures.next();
            }

            while let Some((mat, capture_ix)) = highlights.next_capture.as_ref() {
                let capture = mat.captures[*capture_ix as usize];
                if self.range.start < capture.node.start_byte() {
                    next_capture_start = capture.node.start_byte();
                    break;
                } else {
                    let style_id = highlights.highlight_map.get(capture.index);
                    highlights.stack.push((capture.node.end_byte(), style_id));
                    highlights.next_capture = highlights.captures.next();
                }
            }
        }

        if let Some(chunk) = self.chunks.peek() {
            let chunk_start = self.range.start;
            let mut chunk_end = (self.chunks.offset() + chunk.len()).min(next_capture_start);
            let mut style_id = HighlightId::default();
            if let Some((parent_capture_end, parent_style_id)) =
                self.highlights.as_ref().and_then(|h| h.stack.last())
            {
                chunk_end = chunk_end.min(*parent_capture_end);
                style_id = *parent_style_id;
            }

            let slice =
                &chunk[chunk_start - self.chunks.offset()..chunk_end - self.chunks.offset()];
            self.range.start = chunk_end;
            if self.range.start == self.chunks.offset() + chunk.len() {
                self.chunks.next().unwrap();
            }

            Some((slice, style_id))
        } else {
            None
        }
    }
}

impl Fragment {
    fn is_visible(&self, undos: &UndoMap) -> bool {
        !undos.is_undone(self.timestamp.local())
            && self.deletions.iter().all(|d| undos.is_undone(*d))
    }

    fn was_visible(&self, version: &clock::Global, undos: &UndoMap) -> bool {
        (version.observed(self.timestamp.local())
            && !undos.was_undone(self.timestamp.local(), version))
            && self
                .deletions
                .iter()
                .all(|d| !version.observed(*d) || undos.was_undone(*d, version))
    }
}

impl sum_tree::Item for Fragment {
    type Summary = FragmentSummary;

    fn summary(&self) -> Self::Summary {
        let mut max_version = clock::Global::new();
        max_version.observe(self.timestamp.local());
        for deletion in &self.deletions {
            max_version.observe(*deletion);
        }
        max_version.join(&self.max_undos);

        let mut min_insertion_version = clock::Global::new();
        min_insertion_version.observe(self.timestamp.local());
        let max_insertion_version = min_insertion_version.clone();
        if self.visible {
            FragmentSummary {
                text: FragmentTextSummary {
                    visible: self.len,
                    deleted: 0,
                },
                max_version,
                min_insertion_version,
                max_insertion_version,
            }
        } else {
            FragmentSummary {
                text: FragmentTextSummary {
                    visible: 0,
                    deleted: self.len,
                },
                max_version,
                min_insertion_version,
                max_insertion_version,
            }
        }
    }
}

impl sum_tree::Summary for FragmentSummary {
    type Context = Option<clock::Global>;

    fn add_summary(&mut self, other: &Self, _: &Self::Context) {
        self.text.visible += &other.text.visible;
        self.text.deleted += &other.text.deleted;
        self.max_version.join(&other.max_version);
        self.min_insertion_version
            .meet(&other.min_insertion_version);
        self.max_insertion_version
            .join(&other.max_insertion_version);
    }
}

impl Default for FragmentSummary {
    fn default() -> Self {
        FragmentSummary {
            text: FragmentTextSummary::default(),
            max_version: clock::Global::new(),
            min_insertion_version: clock::Global::new(),
            max_insertion_version: clock::Global::new(),
        }
    }
}

impl<'a> sum_tree::Dimension<'a, FragmentSummary> for usize {
    fn add_summary(&mut self, summary: &FragmentSummary, _: &Option<clock::Global>) {
        *self += summary.text.visible;
    }
}

impl<'a> sum_tree::SeekTarget<'a, FragmentSummary, FragmentTextSummary> for usize {
    fn cmp(
        &self,
        cursor_location: &FragmentTextSummary,
        _: &Option<clock::Global>,
    ) -> cmp::Ordering {
        Ord::cmp(self, &cursor_location.visible)
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum VersionedOffset {
    Offset(usize),
    InvalidVersion,
}

impl VersionedOffset {
    fn offset(&self) -> usize {
        if let Self::Offset(offset) = self {
            *offset
        } else {
            panic!("invalid version")
        }
    }
}

impl Default for VersionedOffset {
    fn default() -> Self {
        Self::Offset(0)
    }
}

impl<'a> sum_tree::Dimension<'a, FragmentSummary> for VersionedOffset {
    fn add_summary(&mut self, summary: &'a FragmentSummary, cx: &Option<clock::Global>) {
        if let Self::Offset(offset) = self {
            let version = cx.as_ref().unwrap();
            if *version >= summary.max_insertion_version {
                *offset += summary.text.visible + summary.text.deleted;
            } else if !summary
                .min_insertion_version
                .iter()
                .all(|t| !version.observed(*t))
            {
                *self = Self::InvalidVersion;
            }
        }
    }
}

impl<'a> sum_tree::SeekTarget<'a, FragmentSummary, Self> for VersionedOffset {
    fn cmp(&self, other: &Self, _: &Option<clock::Global>) -> cmp::Ordering {
        match (self, other) {
            (Self::Offset(a), Self::Offset(b)) => Ord::cmp(a, b),
            (Self::Offset(_), Self::InvalidVersion) => cmp::Ordering::Less,
            (Self::InvalidVersion, _) => unreachable!(),
        }
    }
}

impl Operation {
    fn replica_id(&self) -> ReplicaId {
        self.lamport_timestamp().replica_id
    }

    fn lamport_timestamp(&self) -> clock::Lamport {
        match self {
            Operation::Edit(edit) => edit.timestamp.lamport(),
            Operation::Undo {
                lamport_timestamp, ..
            } => *lamport_timestamp,
            Operation::UpdateSelections {
                lamport_timestamp, ..
            } => *lamport_timestamp,
            Operation::SetActiveSelections {
                lamport_timestamp, ..
            } => *lamport_timestamp,
            #[cfg(test)]
            Operation::Test(lamport_timestamp) => *lamport_timestamp,
        }
    }

    pub fn is_edit(&self) -> bool {
        match self {
            Operation::Edit { .. } => true,
            _ => false,
        }
    }
}

impl<'a> Into<proto::Operation> for &'a Operation {
    fn into(self) -> proto::Operation {
        proto::Operation {
            variant: Some(match self {
                Operation::Edit(edit) => proto::operation::Variant::Edit(edit.into()),
                Operation::Undo {
                    undo,
                    lamport_timestamp,
                } => proto::operation::Variant::Undo(proto::operation::Undo {
                    replica_id: undo.id.replica_id as u32,
                    local_timestamp: undo.id.value,
                    lamport_timestamp: lamport_timestamp.value,
                    ranges: undo
                        .ranges
                        .iter()
                        .map(|r| proto::Range {
                            start: r.start as u64,
                            end: r.end as u64,
                        })
                        .collect(),
                    counts: undo
                        .counts
                        .iter()
                        .map(|(edit_id, count)| proto::operation::UndoCount {
                            replica_id: edit_id.replica_id as u32,
                            local_timestamp: edit_id.value,
                            count: *count,
                        })
                        .collect(),
                    version: From::from(&undo.version),
                }),
                Operation::UpdateSelections {
                    set_id,
                    selections,
                    lamport_timestamp,
                } => proto::operation::Variant::UpdateSelections(
                    proto::operation::UpdateSelections {
                        replica_id: set_id.replica_id as u32,
                        local_timestamp: set_id.value,
                        lamport_timestamp: lamport_timestamp.value,
                        set: selections.as_ref().map(|selections| proto::SelectionSet {
                            selections: selections.iter().map(Into::into).collect(),
                        }),
                    },
                ),
                Operation::SetActiveSelections {
                    set_id,
                    lamport_timestamp,
                } => proto::operation::Variant::SetActiveSelections(
                    proto::operation::SetActiveSelections {
                        replica_id: lamport_timestamp.replica_id as u32,
                        local_timestamp: set_id.map(|set_id| set_id.value),
                        lamport_timestamp: lamport_timestamp.value,
                    },
                ),
                #[cfg(test)]
                Operation::Test(_) => unimplemented!(),
            }),
        }
    }
}

impl<'a> Into<proto::operation::Edit> for &'a EditOperation {
    fn into(self) -> proto::operation::Edit {
        let ranges = self
            .ranges
            .iter()
            .map(|range| proto::Range {
                start: range.start as u64,
                end: range.end as u64,
            })
            .collect();
        proto::operation::Edit {
            replica_id: self.timestamp.replica_id as u32,
            local_timestamp: self.timestamp.local,
            lamport_timestamp: self.timestamp.lamport,
            version: From::from(&self.version),
            ranges,
            new_text: self.new_text.clone(),
        }
    }
}

impl<'a> Into<proto::Anchor> for &'a Anchor {
    fn into(self) -> proto::Anchor {
        proto::Anchor {
            version: (&self.version).into(),
            offset: self.offset as u64,
            bias: match self.bias {
                Bias::Left => proto::anchor::Bias::Left as i32,
                Bias::Right => proto::anchor::Bias::Right as i32,
            },
        }
    }
}

impl<'a> Into<proto::Selection> for &'a Selection {
    fn into(self) -> proto::Selection {
        proto::Selection {
            id: self.id as u64,
            start: Some((&self.start).into()),
            end: Some((&self.end).into()),
            reversed: self.reversed,
        }
    }
}

impl TryFrom<proto::Operation> for Operation {
    type Error = anyhow::Error;

    fn try_from(message: proto::Operation) -> Result<Self, Self::Error> {
        Ok(
            match message
                .variant
                .ok_or_else(|| anyhow!("missing operation variant"))?
            {
                proto::operation::Variant::Edit(edit) => Operation::Edit(edit.into()),
                proto::operation::Variant::Undo(undo) => Operation::Undo {
                    lamport_timestamp: clock::Lamport {
                        replica_id: undo.replica_id as ReplicaId,
                        value: undo.lamport_timestamp,
                    },
                    undo: UndoOperation {
                        id: clock::Local {
                            replica_id: undo.replica_id as ReplicaId,
                            value: undo.local_timestamp,
                        },
                        counts: undo
                            .counts
                            .into_iter()
                            .map(|c| {
                                (
                                    clock::Local {
                                        replica_id: c.replica_id as ReplicaId,
                                        value: c.local_timestamp,
                                    },
                                    c.count,
                                )
                            })
                            .collect(),
                        ranges: undo
                            .ranges
                            .into_iter()
                            .map(|r| r.start as usize..r.end as usize)
                            .collect(),
                        version: undo.version.into(),
                    },
                },
                proto::operation::Variant::UpdateSelections(message) => {
                    let selections: Option<Vec<Selection>> = if let Some(set) = message.set {
                        Some(
                            set.selections
                                .into_iter()
                                .map(TryFrom::try_from)
                                .collect::<Result<_, _>>()?,
                        )
                    } else {
                        None
                    };
                    Operation::UpdateSelections {
                        set_id: clock::Lamport {
                            replica_id: message.replica_id as ReplicaId,
                            value: message.local_timestamp,
                        },
                        lamport_timestamp: clock::Lamport {
                            replica_id: message.replica_id as ReplicaId,
                            value: message.lamport_timestamp,
                        },
                        selections: selections.map(Arc::from),
                    }
                }
                proto::operation::Variant::SetActiveSelections(message) => {
                    Operation::SetActiveSelections {
                        set_id: message.local_timestamp.map(|value| clock::Lamport {
                            replica_id: message.replica_id as ReplicaId,
                            value,
                        }),
                        lamport_timestamp: clock::Lamport {
                            replica_id: message.replica_id as ReplicaId,
                            value: message.lamport_timestamp,
                        },
                    }
                }
            },
        )
    }
}

impl From<proto::operation::Edit> for EditOperation {
    fn from(edit: proto::operation::Edit) -> Self {
        let ranges = edit
            .ranges
            .into_iter()
            .map(|range| range.start as usize..range.end as usize)
            .collect();
        EditOperation {
            timestamp: InsertionTimestamp {
                replica_id: edit.replica_id as ReplicaId,
                local: edit.local_timestamp,
                lamport: edit.lamport_timestamp,
            },
            version: edit.version.into(),
            ranges,
            new_text: edit.new_text,
        }
    }
}

impl TryFrom<proto::Anchor> for Anchor {
    type Error = anyhow::Error;

    fn try_from(message: proto::Anchor) -> Result<Self, Self::Error> {
        let mut version = clock::Global::new();
        for entry in message.version {
            version.observe(clock::Local {
                replica_id: entry.replica_id as ReplicaId,
                value: entry.timestamp,
            });
        }

        Ok(Self {
            offset: message.offset as usize,
            bias: if message.bias == proto::anchor::Bias::Left as i32 {
                Bias::Left
            } else if message.bias == proto::anchor::Bias::Right as i32 {
                Bias::Right
            } else {
                Err(anyhow!("invalid anchor bias {}", message.bias))?
            },
            version,
        })
    }
}

impl TryFrom<proto::Selection> for Selection {
    type Error = anyhow::Error;

    fn try_from(selection: proto::Selection) -> Result<Self, Self::Error> {
        Ok(Selection {
            id: selection.id as usize,
            start: selection
                .start
                .ok_or_else(|| anyhow!("missing selection start"))?
                .try_into()?,
            end: selection
                .end
                .ok_or_else(|| anyhow!("missing selection end"))?
                .try_into()?,
            reversed: selection.reversed,
            goal: SelectionGoal::None,
        })
    }
}

pub trait ToOffset {
    fn to_offset<'a>(&self, content: impl Into<Content<'a>>) -> usize;
}

impl ToOffset for Point {
    fn to_offset<'a>(&self, content: impl Into<Content<'a>>) -> usize {
        content.into().visible_text.to_offset(*self)
    }
}

impl ToOffset for usize {
    fn to_offset<'a>(&self, _: impl Into<Content<'a>>) -> usize {
        *self
    }
}

impl ToOffset for Anchor {
    fn to_offset<'a>(&self, content: impl Into<Content<'a>>) -> usize {
        content.into().summary_for_anchor(self).bytes
    }
}

impl<'a> ToOffset for &'a Anchor {
    fn to_offset<'b>(&self, content: impl Into<Content<'b>>) -> usize {
        content.into().summary_for_anchor(self).bytes
    }
}

pub trait ToPoint {
    fn to_point<'a>(&self, content: impl Into<Content<'a>>) -> Point;
}

impl ToPoint for Anchor {
    fn to_point<'a>(&self, content: impl Into<Content<'a>>) -> Point {
        content.into().summary_for_anchor(self).lines
    }
}

impl ToPoint for usize {
    fn to_point<'a>(&self, content: impl Into<Content<'a>>) -> Point {
        content.into().visible_text.to_point(*self)
    }
}

fn contiguous_ranges(
    values: impl IntoIterator<Item = u32>,
    max_len: usize,
) -> impl Iterator<Item = Range<u32>> {
    let mut values = values.into_iter();
    let mut current_range: Option<Range<u32>> = None;
    std::iter::from_fn(move || loop {
        if let Some(value) = values.next() {
            if let Some(range) = &mut current_range {
                if value == range.end && range.len() < max_len {
                    range.end += 1;
                    continue;
                }
            }

            let prev_range = current_range.clone();
            current_range = Some(value..(value + 1));
            if prev_range.is_some() {
                return prev_range;
            }
        } else {
            return current_range.take();
        }
    })
}
