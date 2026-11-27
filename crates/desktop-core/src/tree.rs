use std::collections::{HashMap, VecDeque};

use desktop_proto::{
    ElementAction, FindMatch, Rect, Snapshot, UiNode, rendered_display_name, sanitize_text,
};

use crate::backend::AnnotateBox;

/// Default number of children per parent explored in the primary BFS pass before deferring the
/// remaining tail of a wide list/sidebar to the overflow queue.
pub const DEFAULT_PRIMARY_FANOUT: usize = 16;

/// Normalized metadata for a single visited accessibility element.
#[derive(Debug, Clone)]
pub struct AxNodeData {
    pub role: String,
    pub subrole: Option<String>,
    pub title: Option<String>,
    pub label: Option<String>,
    pub value: Option<String>,
    pub bounds: Option<Rect>,
    pub enabled: bool,
    pub focused: bool,
    pub selected: bool,
    pub actions: Vec<ElementAction>,
}

/// Traversal budgets controlling bounded BFS snapshot collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalkBudget {
    pub max_depth: usize,
    pub max_nodes: usize,
    pub max_visits: usize,
    pub primary_fanout: usize,
}

impl WalkBudget {
    #[must_use]
    pub fn new(max_depth: usize, max_nodes: usize) -> Self {
        let max_visits = if max_nodes <= 1 {
            1
        } else {
            max_nodes.saturating_mul(3).clamp(256, 1_500)
        };
        Self {
            max_depth,
            max_nodes,
            max_visits,
            primary_fanout: DEFAULT_PRIMARY_FANOUT,
        }
    }
}

/// Returns true for anonymous structural wrappers (e.g. Chromium/Electron `<div>` `AXGroup`s)
/// that carry no semantic label/value/state and should be flattened while keeping descendants.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn is_decorative_container(
    depth: usize,
    role: &str,
    title: Option<&str>,
    label: Option<&str>,
    value: Option<&str>,
    focused: bool,
    selected: bool,
    actions: &[ElementAction],
) -> bool {
    if depth > 0 && matches!(role, "listmarker" | "list_marker") {
        return true;
    }
    let only_generic_container_actions = actions.iter().all(|a| {
        matches!(
            a,
            ElementAction::ShowMenu | ElementAction::Press | ElementAction::Raise
        )
    });
    depth > 0
        && matches!(
            role,
            "group" | "generic" | "scrollarea" | "splitgroup" | "panel" | "filler" | "unknown"
        )
        && title.is_none()
        && label.is_none()
        && value.is_none()
        && !focused
        && !selected
        && (actions.is_empty() || only_generic_container_actions)
}

/// Returns true for canonical interactive control roles.
#[must_use]
pub fn is_primary_interactive_role(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "textfield"
            | "textfield:secure"
            | "textarea"
            | "checkbox"
            | "radio"
            | "tab"
            | "tabbutton"
            | "link"
            | "menuitem"
            | "popup"
            | "combobox"
            | "slider"
            | "switch"
            | "searchfield"
            | "incrementor"
            | "stepper"
            | "disclosuretriangle"
            | "colorwell"
            | "segmentedcontrol"
    )
}

/// Returns true for structural container/landmark roles that should not be treated as leaf
/// interactive controls merely because Chromium exposes generic `AXShowMenu` / `AXPress`.
#[must_use]
pub fn is_structural_container_role(role: &str) -> bool {
    matches!(
        role,
        "window"
            | "application"
            | "webarea"
            | "group"
            | "generic"
            | "scrollarea"
            | "splitgroup"
            | "toolbar"
            | "menubar"
            | "menu"
            | "tabgroup"
            | "list"
            | "table"
            | "tree"
            | "dialog"
            | "panel"
            | "filler"
            | "unknown"
    )
}

fn has_explicit_input_action(actions: &[ElementAction]) -> bool {
    actions.iter().any(|a| {
        matches!(
            a,
            ElementAction::SetValue
                | ElementAction::Confirm
                | ElementAction::Increment
                | ElementAction::Decrement
                | ElementAction::Focus
        )
    })
}

/// Returns true when a node should be prioritized as an interactive control during snapshot
/// budgeting (ahead of static text and non-interactive informative nodes).
#[must_use]
pub fn is_interactive_node(
    role: &str,
    title: Option<&str>,
    label: Option<&str>,
    value: Option<&str>,
    focused: bool,
    selected: bool,
    actions: &[ElementAction],
) -> bool {
    if is_primary_interactive_role(role)
        || focused
        || selected
        || has_explicit_input_action(actions)
    {
        return true;
    }
    if actions.contains(&ElementAction::Press) {
        if !is_structural_container_role(role) && !matches!(role, "text" | "heading") {
            return true;
        }
        if role == "group" && (title.is_some() || label.is_some() || value.is_some()) {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeTier {
    Root,
    Decorative,
    Interactive,
    Informative,
}

/// Metadata linking a popped queue item back to its parent and original sibling position.
#[derive(Debug, Clone, Copy)]
pub struct PendingMeta {
    parent_idx: Option<usize>,
    child_order: usize,
    depth: usize,
}

impl PendingMeta {
    #[must_use]
    pub fn depth(&self) -> usize {
        self.depth
    }
}

/// Next handle scheduled by [`SnapshotWalkBuilder`].
#[derive(Debug)]
pub struct PendingNode<H> {
    handle: H,
    meta: PendingMeta,
}

impl<H> PendingNode<H> {
    #[must_use]
    pub fn into_parts(self) -> (H, PendingMeta) {
        (self.handle, self.meta)
    }
}

/// Action the backend driver should take after recording a visited node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    /// Fetch this element's children and pass them to [`SnapshotWalkBuilder::enqueue_children`].
    Expand(usize),
    /// Do not expand; optionally check whether children exist so `truncated` is accurate.
    CheckChildrenForTruncation,
    /// Budget is already saturated and more queued nodes exist (`truncated` is already set).
    Done,
}

/// Output of a completed bounded BFS snapshot walk.
pub struct WalkOutput<R> {
    pub root: UiNode,
    pub refs: HashMap<String, R>,
    pub truncated: bool,
    pub total_visited: usize,
    pub emitted_nodes: usize,
}

struct ArenaEntry<R> {
    parent_idx: Option<usize>,
    child_order: usize,
    tier: NodeTier,
    data: Option<AxNodeData>,
    stored_ref: Option<R>,
}

/// Sans-IO round-robin breadth-first accessibility tree walker shared by macOS and Linux backends.
///
/// Key properties:
/// - **Round-robin BFS across parents**: sibling panes (left sidebar, main content, right panel)
///   advance together level-by-level instead of letting the first deep subtree starve later panes.
/// - **Primary fanout cap + deferred overflow**: wide lists/sidebars explore their first
///   `primary_fanout` children in pass 1 and defer the remaining tail until shallower panes have
///   been visited, then drain deferred items fairly if budget remains.
/// - **Decorative container flattening**: anonymous wrapper groups (0, 1, or N children) are
///   spliced out so their descendants are lifted cleanly without burning node budget.
/// - **Interactive-first selection**: when a dense tree exceeds `max_nodes`, interactive controls
///   are retained ahead of static text while preserving exact original child ordering and
///   deterministic top-down pre-order `@e1..@eN` refs.
pub struct SnapshotWalkBuilder<H, R> {
    budget: WalkBudget,
    arena: Vec<ArenaEntry<R>>,
    cur_level: VecDeque<VecDeque<PendingNode<H>>>,
    next_level: VecDeque<VecDeque<PendingNode<H>>>,
    deferred: VecDeque<VecDeque<PendingNode<H>>>,
    total_visited: usize,
    priority_count: usize,
    truncated: bool,
}

impl<H, R> SnapshotWalkBuilder<H, R> {
    #[must_use]
    pub fn new(root_handle: H, budget: WalkBudget) -> Self {
        let mut cur_level = VecDeque::new();
        let mut root_bucket = VecDeque::with_capacity(1);
        root_bucket.push_back(PendingNode {
            handle: root_handle,
            meta: PendingMeta {
                parent_idx: None,
                child_order: 0,
                depth: 0,
            },
        });
        cur_level.push_back(root_bucket);
        Self {
            budget,
            arena: Vec::new(),
            cur_level,
            next_level: VecDeque::new(),
            deferred: VecDeque::new(),
            total_visited: 0,
            priority_count: 0,
            truncated: false,
        }
    }

    fn has_queued(&self) -> bool {
        !self.cur_level.is_empty() || !self.next_level.is_empty() || !self.deferred.is_empty()
    }

    /// Pop the next element handle to inspect, or `None` when traversal is complete or bounded.
    pub fn next_pending(&mut self) -> Option<PendingNode<H>> {
        if !self.has_queued() {
            return None;
        }
        if self.total_visited >= self.budget.max_visits
            || self.priority_count >= self.budget.max_nodes
        {
            self.truncated = true;
            return None;
        }

        if let Some(item) = pop_round_robin(&mut self.cur_level) {
            self.total_visited += 1;
            return Some(item);
        }

        if !self.next_level.is_empty() {
            self.cur_level = std::mem::take(&mut self.next_level);
            if let Some(item) = pop_round_robin(&mut self.cur_level) {
                self.total_visited += 1;
                return Some(item);
            }
        }

        if !self.deferred.is_empty() {
            let fanout = self.budget.primary_fanout.max(1);
            let mut remaining_deferred = VecDeque::new();
            while let Some(mut bucket) = self.deferred.pop_front() {
                let mut wave = VecDeque::new();
                for _ in 0..fanout {
                    if let Some(item) = bucket.pop_front() {
                        wave.push_back(item);
                    } else {
                        break;
                    }
                }
                if !wave.is_empty() {
                    self.cur_level.push_back(wave);
                }
                if !bucket.is_empty() {
                    remaining_deferred.push_back(bucket);
                }
            }
            self.deferred = remaining_deferred;
            if let Some(item) = pop_round_robin(&mut self.cur_level) {
                self.total_visited += 1;
                return Some(item);
            }
        }

        None
    }

    /// Returns true once the walk has hit either the interactive node cap or visit cap.
    #[must_use]
    pub fn is_budget_saturated(&self) -> bool {
        self.total_visited >= self.budget.max_visits || self.priority_count >= self.budget.max_nodes
    }

    /// Mark the snapshot as truncated (e.g. wall-clock deadline reached or children past max depth).
    pub fn mark_truncated(&mut self) {
        self.truncated = true;
    }

    /// Record a visited node and return whether the caller should expand its children.
    pub fn record_node(
        &mut self,
        meta: PendingMeta,
        data: AxNodeData,
        stored_ref: Option<R>,
    ) -> RecordOutcome {
        let tier = if meta.parent_idx.is_none() {
            NodeTier::Root
        } else if is_decorative_container(
            meta.depth,
            &data.role,
            data.title.as_deref(),
            data.label.as_deref(),
            data.value.as_deref(),
            data.focused,
            data.selected,
            &data.actions,
        ) {
            NodeTier::Decorative
        } else if is_interactive_node(
            &data.role,
            data.title.as_deref(),
            data.label.as_deref(),
            data.value.as_deref(),
            data.focused,
            data.selected,
            &data.actions,
        ) {
            NodeTier::Interactive
        } else {
            NodeTier::Informative
        };

        if matches!(tier, NodeTier::Root | NodeTier::Interactive) {
            self.priority_count += 1;
        }

        let stored_ref = if tier == NodeTier::Decorative {
            None
        } else {
            stored_ref
        };

        let idx = self.arena.len();
        self.arena.push(ArenaEntry {
            parent_idx: meta.parent_idx,
            child_order: meta.child_order,
            tier,
            data: Some(data),
            stored_ref,
        });

        if meta.depth >= self.budget.max_depth {
            return if self.truncated {
                RecordOutcome::Done
            } else {
                RecordOutcome::CheckChildrenForTruncation
            };
        }
        if self.is_budget_saturated() {
            if self.has_queued() {
                self.truncated = true;
                return RecordOutcome::Done;
            }
            return if self.truncated {
                RecordOutcome::Done
            } else {
                RecordOutcome::CheckChildrenForTruncation
            };
        }
        RecordOutcome::Expand(idx)
    }

    /// Enqueue the direct children of `parent_idx` at `child_depth`.
    ///
    /// The first `primary_fanout` children enter the next BFS level; any overflow children are
    /// deferred so a single wide list/sidebar never starves sibling panes.
    pub fn enqueue_children(&mut self, parent_idx: usize, child_depth: usize, children: Vec<H>) {
        self.enqueue_children_from(parent_idx, child_depth, 0, children);
    }

    /// Enqueue a slice of direct children starting at sibling index `start_order` (used by
    /// backends that paginate child handles lazily, such as AT-SPI2 over D-Bus).
    pub fn enqueue_children_from(
        &mut self,
        parent_idx: usize,
        child_depth: usize,
        start_order: usize,
        children: Vec<H>,
    ) {
        if children.is_empty() {
            return;
        }
        let fanout = self.budget.primary_fanout.max(1);
        let mut primary = VecDeque::new();
        let mut overflow = VecDeque::new();
        for (offset, handle) in children.into_iter().enumerate() {
            let pending = PendingNode {
                handle,
                meta: PendingMeta {
                    parent_idx: Some(parent_idx),
                    child_order: start_order + offset,
                    depth: child_depth,
                },
            };
            if offset < fanout {
                primary.push_back(pending);
            } else {
                overflow.push_back(pending);
            }
        }
        if !primary.is_empty() {
            self.next_level.push_back(primary);
        }
        if !overflow.is_empty() {
            self.deferred.push_back(overflow);
        }
    }

    /// Finalize the snapshot: enforce `max_nodes` prioritizing interactive controls, splice
    /// decorative/dropped wrappers, preserve original sibling order, and assign `@e1..@eN` refs.
    #[must_use]
    pub fn finish(mut self, fallback_root: UiNode) -> WalkOutput<R> {
        let n = self.arena.len();
        if n == 0 || self.budget.max_nodes == 0 {
            return WalkOutput {
                root: fallback_root,
                refs: HashMap::new(),
                truncated: self.truncated,
                total_visited: self.total_visited.max(1),
                emitted_nodes: 1,
            };
        }

        let mut children_of: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (idx, entry) in self.arena.iter().enumerate().skip(1) {
            if let Some(p) = entry.parent_idx {
                children_of[p].push(idx);
            }
        }
        for kids in &mut children_of {
            kids.sort_by_key(|&idx| self.arena[idx].child_order);
        }

        // Pre-pass A: splice redundant same-title non-leaf structural wrappers (e.g. Electron
        // `window "Title" -> group "Title" -> webarea "Title"`) before budget selection so they
        // never consume interactive slots.
        demote_redundant_wrappers(0, None, &mut self.arena, &children_of);

        // Pre-pass B: prune redundant plain leaf `text` children whose text is already rendered
        // in their direct semantic parent's label, freeing budget slots for real controls.
        demote_redundant_child_text(&mut self.arena, &children_of);

        let mut kept = vec![false; n];
        kept[0] = true;
        let mut kept_count = 1usize;
        let mut omitted_non_decorative = false;

        // Pass 1: retain interactive controls first in BFS discovery order (balanced across panes).
        for (idx, entry) in self.arena.iter().enumerate().skip(1) {
            if entry.tier == NodeTier::Interactive {
                if kept_count < self.budget.max_nodes {
                    kept[idx] = true;
                    kept_count += 1;
                } else {
                    omitted_non_decorative = true;
                }
            }
        }

        // Pass 2: fill remaining budget with informative nodes in BFS discovery order.
        for (idx, entry) in self.arena.iter().enumerate().skip(1) {
            if entry.tier == NodeTier::Informative {
                if kept_count < self.budget.max_nodes {
                    kept[idx] = true;
                    kept_count += 1;
                } else {
                    omitted_non_decorative = true;
                }
            }
        }

        let truncated = self.truncated || omitted_non_decorative;

        let mut refs = HashMap::with_capacity(kept_count);
        let mut seq = 1usize;
        let built = build_subtree(0, &mut self.arena, &children_of, &kept, &mut refs, &mut seq);
        let root = built.into_iter().next().unwrap_or(fallback_root);
        let emitted_nodes = (seq - 1).max(1);

        WalkOutput {
            root,
            refs,
            truncated,
            total_visited: self.total_visited.max(emitted_nodes),
            emitted_nodes,
        }
    }
}

/// Returns `Some(lowercased_text)` when a leaf `text` node renders at most one text string
/// (either empty, or only display name, or only value, or identical name & value).
/// Returns `None` when both display name and a distinct non-empty `value` are rendered, so
/// redundancy pruning never drops a distinct `value`.
fn plain_text_leaf_content(data: &AxNodeData) -> Option<String> {
    let name = rendered_display_name(data.title.as_deref(), data.label.as_deref());
    let val = data
        .value
        .as_deref()
        .map(|s| sanitize_text(s, 120))
        .filter(|s| !s.is_empty());
    match (name, val) {
        (Some(n), Some(v)) if n != v => None,
        (Some(n), _) => Some(n.to_lowercase()),
        (None, Some(v)) => Some(v.to_lowercase()),
        (None, None) => Some(String::new()),
    }
}

fn demote_redundant_wrappers<R>(
    idx: usize,
    ancestor_title: Option<&str>,
    arena: &mut [ArenaEntry<R>],
    children_of: &[Vec<usize>],
) {
    let mut next_ancestor_title: Option<String> = ancestor_title.map(str::to_string);

    if arena[idx].tier != NodeTier::Decorative
        && let Some(data) = arena[idx].data.as_ref()
    {
        let own_title = rendered_display_name(data.title.as_deref(), data.label.as_deref())
            .map(|s| s.to_lowercase());
        let is_redundant = idx > 0
            && has_non_decorative_descendant(idx, arena, children_of)
            && matches!(
                data.role.as_str(),
                "group" | "generic" | "webarea" | "scrollarea" | "splitgroup" | "panel"
            )
            && !data.focused
            && !data.selected
            && data.enabled
            && data.value.is_none()
            && data.actions.iter().all(|a| {
                matches!(
                    a,
                    ElementAction::ShowMenu | ElementAction::Press | ElementAction::Raise
                )
            })
            && own_title.as_deref().is_some()
            && own_title.as_deref() == ancestor_title;

        if is_redundant {
            arena[idx].tier = NodeTier::Decorative;
            arena[idx].stored_ref = None;
        } else {
            // Only carry a title to direct non-decorative descendants; unlabeled non-decorative
            // containers (e.g. `list`/`row`) break ancestor-title inheritance.
            next_ancestor_title = own_title;
        }
    }

    for &child in &children_of[idx] {
        demote_redundant_wrappers(child, next_ancestor_title.as_deref(), arena, children_of);
    }
}

fn is_semantic_label_parent_role(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "heading"
            | "tab"
            | "tabbutton"
            | "menuitem"
            | "checkbox"
            | "radio"
            | "switch"
            | "popup"
            | "combobox"
            | "row"
            | "cell"
            | "treeitem"
            | "listitem"
            | "option"
            | "group"
    )
}

fn collect_direct_non_decorative<R>(
    parent: usize,
    arena: &[ArenaEntry<R>],
    children_of: &[Vec<usize>],
    out: &mut Vec<usize>,
) {
    for &child in &children_of[parent] {
        if arena[child].tier == NodeTier::Decorative {
            collect_direct_non_decorative(child, arena, children_of, out);
        } else {
            out.push(child);
        }
    }
}

fn has_non_decorative_descendant<R>(
    idx: usize,
    arena: &[ArenaEntry<R>],
    children_of: &[Vec<usize>],
) -> bool {
    for &child in &children_of[idx] {
        if arena[child].tier != NodeTier::Decorative
            || has_non_decorative_descendant(child, arena, children_of)
        {
            return true;
        }
    }
    false
}

fn strip_whitespace(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn contains_word_bounded(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    for (pos, _) in haystack.match_indices(needle) {
        let left_ok = pos == 0
            || !haystack[..pos]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric);
        let end = pos + needle.len();
        let right_ok = end == haystack.len()
            || !haystack[end..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric);
        if left_ok && right_ok {
            return true;
        }
    }
    false
}

fn demote_redundant_child_text<R>(arena: &mut [ArenaEntry<R>], children_of: &[Vec<usize>]) {
    let n = arena.len();
    let mut to_demote = Vec::new();

    for p in 0..n {
        if arena[p].tier == NodeTier::Decorative {
            continue;
        }
        let Some(parent_data) = arena[p].data.as_ref() else {
            continue;
        };
        if !is_semantic_label_parent_role(&parent_data.role) {
            continue;
        }
        let Some(parent_label) =
            rendered_display_name(parent_data.title.as_deref(), parent_data.label.as_deref())
                .map(|s| s.to_lowercase())
        else {
            continue;
        };

        let mut direct = Vec::new();
        collect_direct_non_decorative(p, arena, children_of, &mut direct);
        if direct.is_empty() {
            continue;
        }

        let mut text_leaves: Vec<(usize, String)> = Vec::new();
        for &c in &direct {
            let Some(cd) = arena[c].data.as_ref() else {
                continue;
            };
            if cd.role == "text"
                && !cd.focused
                && !cd.selected
                && cd.enabled
                && !has_explicit_input_action(&cd.actions)
                && !has_non_decorative_descendant(c, arena, children_of)
                && let Some(content) = plain_text_leaf_content(cd)
            {
                text_leaves.push((c, content));
            }
        }
        if text_leaves.is_empty() {
            continue;
        }

        let non_empty: Vec<&str> = text_leaves
            .iter()
            .map(|(_, t)| t.as_str())
            .filter(|t| !t.is_empty())
            .collect();
        let concat_spaced = non_empty.join(" ");
        let concat_compact = strip_whitespace(&concat_spaced);
        let parent_compact = strip_whitespace(&parent_label);

        let all_match_via_concat = !concat_compact.is_empty()
            && (concat_compact == parent_compact
                || (concat_compact.chars().count() >= 4
                    && (contains_word_bounded(&parent_label, &concat_spaced)
                        || (parent_compact.contains(&concat_compact)
                            && concat_compact.chars().count() * 2
                                >= parent_compact.chars().count()))));

        if all_match_via_concat {
            for (c, _) in text_leaves {
                to_demote.push(c);
            }
        } else {
            for (c, t) in text_leaves {
                if t.is_empty()
                    || t == parent_label
                    || (t.chars().count() >= 3 && contains_word_bounded(&parent_label, &t))
                {
                    to_demote.push(c);
                }
            }
        }
    }

    for idx in to_demote {
        arena[idx].tier = NodeTier::Decorative;
        arena[idx].stored_ref = None;
    }
}

fn pop_round_robin<T>(buckets: &mut VecDeque<VecDeque<T>>) -> Option<T> {
    while let Some(mut bucket) = buckets.pop_front() {
        if let Some(item) = bucket.pop_front() {
            if !bucket.is_empty() {
                buckets.push_back(bucket);
            }
            return Some(item);
        }
    }
    None
}

fn build_subtree<R>(
    idx: usize,
    arena: &mut [ArenaEntry<R>],
    children_of: &[Vec<usize>],
    kept: &[bool],
    refs: &mut HashMap<String, R>,
    seq: &mut usize,
) -> Vec<UiNode> {
    if kept[idx] {
        let Some(data) = arena[idx].data.take() else {
            return Vec::new();
        };
        let ref_id = format!("@e{}", *seq);
        *seq += 1;
        if let Some(stored) = arena[idx].stored_ref.take() {
            refs.insert(ref_id.clone(), stored);
        }
        let mut children = Vec::new();
        for &child_idx in &children_of[idx] {
            children.extend(build_subtree(
                child_idx,
                arena,
                children_of,
                kept,
                refs,
                seq,
            ));
        }
        vec![UiNode {
            ref_id,
            role: data.role,
            subrole: data.subrole,
            title: data.title,
            label: data.label,
            value: data.value,
            bounds: data.bounds,
            enabled: data.enabled,
            focused: data.focused,
            selected: data.selected,
            actions: data.actions,
            children,
        }]
    } else {
        let mut lifted = Vec::new();
        for &child_idx in &children_of[idx] {
            lifted.extend(build_subtree(
                child_idx,
                arena,
                children_of,
                kept,
                refs,
                seq,
            ));
        }
        lifted
    }
}

/// Search a snapshot's UI tree by case-insensitive substring across title, label, value, and role.
#[must_use]
pub fn find_in_snapshot(
    snapshot: &Snapshot,
    query: &str,
    role_filter: Option<&str>,
    limit: usize,
) -> Vec<FindMatch> {
    let q = query.trim().to_ascii_lowercase();
    let role_q = role_filter
        .map(|r| r.trim().to_ascii_lowercase())
        .filter(|r| !r.is_empty());
    let mut matches = Vec::new();
    let mut path = Vec::new();
    visit_find(
        &snapshot.root,
        &q,
        role_q.as_deref(),
        limit,
        &mut path,
        &mut matches,
    );
    matches
}

fn visit_find(
    node: &UiNode,
    query: &str,
    role_filter: Option<&str>,
    limit: usize,
    path: &mut Vec<String>,
    out: &mut Vec<FindMatch>,
) {
    if out.len() >= limit {
        return;
    }
    let role_matches = role_filter.is_none_or(|rf| node.role.to_ascii_lowercase().contains(rf));
    let text_matches = query.is_empty()
        || node
            .title
            .as_deref()
            .is_some_and(|t| t.to_ascii_lowercase().contains(query))
        || node
            .label
            .as_deref()
            .is_some_and(|l| l.to_ascii_lowercase().contains(query))
        || node
            .value
            .as_deref()
            .is_some_and(|v| v.to_ascii_lowercase().contains(query))
        || node.role.to_ascii_lowercase().contains(query);

    if role_matches && text_matches {
        out.push(FindMatch {
            ref_id: node.ref_id.clone(),
            role: node.role.clone(),
            title: node.title.clone(),
            label: node.label.clone(),
            value: node.value.clone(),
            bounds: node.bounds,
            actions: node.actions.clone(),
            path: path.clone(),
        });
    }

    let label = node
        .title
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| node.label.as_deref().filter(|s| !s.is_empty()))
        .map(|s| format!("{}:{s}", node.role))
        .unwrap_or_else(|| node.role.clone());
    path.push(label);
    for child in &node.children {
        visit_find(child, query, role_filter, limit, path, out);
        if out.len() >= limit {
            break;
        }
    }
    path.pop();
}

/// Minimum width/height (in display points) for a Set-of-Marks target box.
const MIN_ANNOTATE_DIM: f64 = 4.0;
/// IoU threshold above which two overlapping candidate boxes are treated as duplicates.
const DUPLICATE_IOU_THRESHOLD: f64 = 0.80;
/// Spatial grid resolution (3 columns x 3 rows) used for pane-balanced capping.
const SPATIAL_GRID_COLS: usize = 3;
const SPATIAL_GRID_ROWS: usize = 3;

#[derive(Debug, Clone)]
struct AnnotateCandidate {
    ref_id: String,
    bounds: Rect,
    tier: u8,
    preorder_idx: usize,
    depth: usize,
}

/// Collect bounded, deduplicated interactive elements from a snapshot for Set-of-Marks annotation.
///
/// Filters out decorative wrappers, static text nodes that merely inherit Chromium `AXShowMenu` /
/// `AXPress`, off-screen elements, and overlapping container boxes. When more eligible controls
/// exist than `max_boxes`, selects round-robin across a 3x3 spatial grid so left, center, and
/// right panes all receive coverage, returning boxes in deterministic pre-order (`@eN`).
#[must_use]
pub fn collect_annotate_boxes(snapshot: &Snapshot, max_boxes: usize) -> Vec<AnnotateBox> {
    if max_boxes == 0 {
        return Vec::new();
    }

    let window_bounds = snapshot
        .window_bounds
        .or(snapshot.root.bounds)
        .filter(|b| is_finite_non_empty(*b));

    let mut candidates = Vec::new();
    let mut seq = 0usize;
    gather_annotate_candidates(&snapshot.root, 0, window_bounds, &mut seq, &mut candidates);

    let deduped = deduplicate_annotate_candidates(candidates);
    let selected = if deduped.len() <= max_boxes {
        deduped
    } else {
        select_spatially_balanced(deduped, window_bounds, max_boxes)
    };

    let mut ordered = selected;
    ordered.sort_by_key(|c| c.preorder_idx);
    ordered
        .into_iter()
        .map(|c| AnnotateBox {
            ref_id: c.ref_id,
            bounds: c.bounds,
        })
        .collect()
}

/// Returns `(has_eligible_descendant, has_text_descendant)`.
fn gather_annotate_candidates(
    node: &UiNode,
    depth: usize,
    window_bounds: Option<Rect>,
    seq: &mut usize,
    out: &mut Vec<AnnotateCandidate>,
) -> (bool, bool) {
    let preorder_idx = *seq;
    *seq += 1;

    let mut has_eligible_descendant = false;
    let mut has_text_descendant = false;
    for child in &node.children {
        let (child_eligible, child_text) =
            gather_annotate_candidates(child, depth + 1, window_bounds, seq, out);
        has_eligible_descendant |= child_eligible;
        has_text_descendant |= child_text;
    }

    let self_has_text = has_non_empty_text(node);
    let mut self_eligible = false;

    if let Some(bounds) = node.bounds
        && is_valid_annotate_rect(bounds)
        && window_bounds.is_none_or(|wb| rects_intersect(bounds, wb))
        && let Some(tier) = classify_annotate_tier(
            node,
            bounds,
            window_bounds,
            has_eligible_descendant,
            self_has_text || has_text_descendant,
        )
    {
        out.push(AnnotateCandidate {
            ref_id: node.ref_id.clone(),
            bounds,
            tier,
            preorder_idx,
            depth,
        });
        self_eligible = true;
    }

    (
        has_eligible_descendant || self_eligible,
        has_text_descendant || self_has_text,
    )
}

fn classify_annotate_tier(
    node: &UiNode,
    bounds: Rect,
    window_bounds: Option<Rect>,
    has_eligible_descendant: bool,
    has_label_or_text: bool,
) -> Option<u8> {
    let role = node.role.as_str();
    let is_text_input = matches!(
        role,
        "textfield" | "textfield:secure" | "textarea" | "searchfield"
    );

    if is_primary_interactive_role(role) {
        if !is_text_input
            && has_eligible_descendant
            && window_bounds.is_some_and(|wb| rect_area(bounds) >= 0.75 * rect_area(wb))
        {
            return None;
        }
        return Some(0);
    }

    // Never annotate static text/headings (Chromium attaches generic ShowMenu/Press/Focus to them),
    // except explicitly editable text nodes exposing SetValue.
    if matches!(role, "text" | "heading") {
        return node.actions.contains(&ElementAction::SetValue).then_some(0);
    }

    // List/table/tree rows/items are annotatable when clickable/selected and labeled.
    // Rows that wrap a full-size inner button are deduplicated geometrically later, while rows
    // with small inline controls (e.g. a trailing archive/delete button or disclosure arrow)
    // remain individually clickable.
    if matches!(role, "row" | "cell" | "treeitem" | "listitem" | "option") {
        let clickable = node.selected
            || node.actions.iter().any(|a| {
                matches!(
                    a,
                    ElementAction::Press
                        | ElementAction::Confirm
                        | ElementAction::Focus
                        | ElementAction::SetValue
                )
            });
        let too_large = window_bounds.is_some_and(|wb| rect_area(bounds) >= 0.50 * rect_area(wb));
        if clickable && has_label_or_text && !too_large {
            return Some(1);
        }
        return None;
    }

    // Explicitly labeled leaf clickable `group` / `image` controls (e.g. custom Electron tiles),
    // excluding ShowMenu/Raise-only containers and large backdrop panes.
    if matches!(role, "group" | "image") {
        let explicit_press = node.actions.iter().any(|a| {
            matches!(
                a,
                ElementAction::Press | ElementAction::Confirm | ElementAction::SetValue
            )
        });
        let too_large = window_bounds.is_some_and(|wb| rect_area(bounds) >= 0.50 * rect_area(wb));
        if explicit_press && !has_eligible_descendant && has_non_empty_text(node) && !too_large {
            return Some(1);
        }
        return None;
    }

    // Never annotate remaining structural containers (`window`, `webarea`, `scrollarea`, etc.).
    if is_structural_container_role(role) {
        return None;
    }

    // Any other custom non-structural role with explicit input actions.
    if has_explicit_input_action(&node.actions) {
        return Some(0);
    }

    let explicit_press = node
        .actions
        .iter()
        .any(|a| matches!(a, ElementAction::Press | ElementAction::Confirm));
    if explicit_press && !has_eligible_descendant && has_label_or_text {
        return Some(1);
    }

    None
}

fn deduplicate_annotate_candidates(
    mut candidates: Vec<AnnotateCandidate>,
) -> Vec<AnnotateCandidate> {
    // Prefer higher-priority tier, deeper (more specific) control, smaller area, then tree order.
    candidates.sort_by(|a, b| {
        a.tier
            .cmp(&b.tier)
            .then_with(|| b.depth.cmp(&a.depth))
            .then_with(|| {
                rect_area(a.bounds)
                    .partial_cmp(&rect_area(b.bounds))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.preorder_idx.cmp(&b.preorder_idx))
    });

    let mut kept: Vec<AnnotateCandidate> = Vec::with_capacity(candidates.len());
    for cand in candidates {
        let redundant = kept.iter().any(|existing| {
            if rect_iou(cand.bounds, existing.bounds) >= DUPLICATE_IOU_THRESHOLD {
                return true;
            }
            // Drop an enclosing wrapper box when an already-kept more specific control fills >= 70%
            // of its area (while keeping e.g. a large textarea that has a small button inside it).
            if cand.tier >= existing.tier
                && covers_with_tolerance(cand.bounds, existing.bounds, 2.0)
                && rect_area(existing.bounds) >= 0.70 * rect_area(cand.bounds)
            {
                return true;
            }
            false
        });
        if !redundant {
            kept.push(cand);
        }
    }
    kept
}

fn select_spatially_balanced(
    candidates: Vec<AnnotateCandidate>,
    window_bounds: Option<Rect>,
    max_boxes: usize,
) -> Vec<AnnotateCandidate> {
    let env = window_bounds.unwrap_or_else(|| union_rect(&candidates));
    let bucket_count = SPATIAL_GRID_COLS * SPATIAL_GRID_ROWS;
    let mut grid: Vec<Vec<AnnotateCandidate>> = vec![Vec::new(); bucket_count];

    for cand in candidates {
        let center = cand.bounds.center();
        let nx = if env.width > 0.0 {
            ((center.x - env.x) / env.width).clamp(0.0, 0.999_999)
        } else {
            0.0
        };
        let ny = if env.height > 0.0 {
            ((center.y - env.y) / env.height).clamp(0.0, 0.999_999)
        } else {
            0.0
        };
        let col = (nx * SPATIAL_GRID_COLS as f64) as usize;
        let row = (ny * SPATIAL_GRID_ROWS as f64) as usize;
        let idx =
            row.min(SPATIAL_GRID_ROWS - 1) * SPATIAL_GRID_COLS + col.min(SPATIAL_GRID_COLS - 1);
        grid[idx].push(cand);
    }

    let mut queues: VecDeque<VecDeque<AnnotateCandidate>> = VecDeque::new();
    for mut cell in grid {
        if cell.is_empty() {
            continue;
        }
        cell.sort_by_key(|c| (c.tier, c.preorder_idx));
        queues.push_back(cell.into());
    }

    let mut selected = Vec::with_capacity(max_boxes);
    while selected.len() < max_boxes {
        let Some(cand) = pop_round_robin(&mut queues) else {
            break;
        };
        selected.push(cand);
    }
    selected
}

fn has_non_empty_text(node: &UiNode) -> bool {
    node.title.as_deref().is_some_and(|s| !s.trim().is_empty())
        || node.label.as_deref().is_some_and(|s| !s.trim().is_empty())
        || node.value.as_deref().is_some_and(|s| !s.trim().is_empty())
}

fn is_finite_non_empty(r: Rect) -> bool {
    r.x.is_finite()
        && r.y.is_finite()
        && r.width.is_finite()
        && r.height.is_finite()
        && r.width > 0.0
        && r.height > 0.0
}

fn is_valid_annotate_rect(r: Rect) -> bool {
    is_finite_non_empty(r) && r.width >= MIN_ANNOTATE_DIM && r.height >= MIN_ANNOTATE_DIM
}

fn rect_area(r: Rect) -> f64 {
    r.width.max(0.0) * r.height.max(0.0)
}

fn rects_intersect(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.width && a.x + a.width > b.x && a.y < b.y + b.height && a.y + a.height > b.y
}

fn covers_with_tolerance(outer: Rect, inner: Rect, tol: f64) -> bool {
    outer.x - tol <= inner.x
        && outer.y - tol <= inner.y
        && outer.x + outer.width + tol >= inner.x + inner.width
        && outer.y + outer.height + tol >= inner.y + inner.height
}

fn rect_iou(a: Rect, b: Rect) -> f64 {
    let ix0 = a.x.max(b.x);
    let iy0 = a.y.max(b.y);
    let ix1 = (a.x + a.width).min(b.x + b.width);
    let iy1 = (a.y + a.height).min(b.y + b.height);
    let iw = (ix1 - ix0).max(0.0);
    let ih = (iy1 - iy0).max(0.0);
    let inter = iw * ih;
    if inter <= 0.0 {
        return 0.0;
    }
    let union = rect_area(a) + rect_area(b) - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

fn union_rect(candidates: &[AnnotateCandidate]) -> Rect {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for c in candidates {
        min_x = min_x.min(c.bounds.x);
        min_y = min_y.min(c.bounds.y);
        max_x = max_x.max(c.bounds.x + c.bounds.width);
        max_y = max_y.max(c.bounds.y + c.bounds.height);
    }
    if !min_x.is_finite() {
        return Rect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        };
    }
    Rect {
        x: min_x,
        y: min_y,
        width: (max_x - min_x).max(1.0),
        height: (max_y - min_y).max(1.0),
    }
}

#[cfg(test)]
mod tests {
    use desktop_proto::AppInfo;

    use super::*;

    #[derive(Clone)]
    struct FakeAxNode {
        id: usize,
        data: AxNodeData,
        children: Vec<FakeAxNode>,
    }

    fn make_ax(
        role: &str,
        title: Option<&str>,
        bounds: Option<Rect>,
        actions: Vec<ElementAction>,
    ) -> AxNodeData {
        AxNodeData {
            role: role.into(),
            subrole: None,
            title: title.map(str::to_string),
            label: None,
            value: None,
            bounds,
            enabled: true,
            focused: false,
            selected: false,
            actions,
        }
    }

    fn walk_fake_tree(root: FakeAxNode, budget: WalkBudget) -> WalkOutput<usize> {
        let mut builder = SnapshotWalkBuilder::new(root, budget);
        while let Some(pending) = builder.next_pending() {
            let (node, meta) = pending.into_parts();
            let depth = meta.depth();
            let FakeAxNode { id, data, children } = node;
            match builder.record_node(meta, data, Some(id)) {
                RecordOutcome::Expand(idx) => {
                    builder.enqueue_children(idx, depth + 1, children);
                }
                RecordOutcome::CheckChildrenForTruncation => {
                    if !children.is_empty() {
                        builder.mark_truncated();
                    }
                }
                RecordOutcome::Done => {}
            }
        }
        builder.finish(UiNode {
            ref_id: "@e1".into(),
            role: "window".into(),
            subrole: None,
            title: None,
            label: None,
            value: None,
            bounds: None,
            enabled: true,
            focused: false,
            selected: false,
            actions: vec![],
            children: vec![],
        })
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn build_dense_electron_tree() -> FakeAxNode {
        let mut next_id = 1usize;
        let mut alloc = || {
            let id = next_id;
            next_id += 1;
            id
        };

        // Left sidebar: 50 rows x (wrapper div -> inner div -> static text + timestamp text) = 200+ non-interactive nodes
        // with Chromium's generic ShowMenu/Press actions on wrappers and static text.
        let mut sidebar_rows = Vec::new();
        for i in 0..50 {
            let y = 60.0 + (i as f64) * 16.0;
            let text_node = FakeAxNode {
                id: alloc(),
                data: make_ax(
                    "text",
                    Some(&format!("Thread {i}")),
                    Some(rect(16.0, y, 180.0, 14.0)),
                    vec![ElementAction::ShowMenu, ElementAction::Press],
                ),
                children: vec![],
            };
            let ts_node = FakeAxNode {
                id: alloc(),
                data: make_ax(
                    "text",
                    Some("2m"),
                    Some(rect(200.0, y, 40.0, 14.0)),
                    vec![ElementAction::ShowMenu],
                ),
                children: vec![],
            };
            let inner_div = FakeAxNode {
                id: alloc(),
                data: make_ax(
                    "group",
                    None,
                    Some(rect(12.0, y, 240.0, 16.0)),
                    vec![ElementAction::ShowMenu, ElementAction::Press],
                ),
                children: vec![text_node, ts_node],
            };
            let outer_div = FakeAxNode {
                id: alloc(),
                data: make_ax(
                    "group",
                    None,
                    Some(rect(8.0, y, 250.0, 16.0)),
                    vec![ElementAction::ShowMenu],
                ),
                children: vec![inner_div],
            };
            sidebar_rows.push(outer_div);
        }

        let sidebar_btn = FakeAxNode {
            id: alloc(),
            data: make_ax(
                "button",
                Some("New Thread"),
                Some(rect(12.0, 20.0, 120.0, 28.0)),
                vec![ElementAction::Press, ElementAction::ShowMenu],
            ),
            children: vec![],
        };
        let mut sidebar_children = vec![sidebar_btn];
        sidebar_children.extend(sidebar_rows);

        let left_sidebar = FakeAxNode {
            id: alloc(),
            data: make_ax(
                "group",
                None,
                Some(rect(0.0, 0.0, 280.0, 900.0)),
                vec![ElementAction::ShowMenu],
            ),
            children: sidebar_children,
        };

        // Center chat pane with nested wrappers around interactive controls.
        let center_pane = FakeAxNode {
            id: alloc(),
            data: make_ax(
                "group",
                None,
                Some(rect(280.0, 0.0, 840.0, 900.0)),
                vec![ElementAction::ShowMenu],
            ),
            children: vec![
                FakeAxNode {
                    id: alloc(),
                    data: make_ax(
                        "group",
                        None,
                        Some(rect(290.0, 10.0, 820.0, 40.0)),
                        vec![ElementAction::ShowMenu],
                    ),
                    children: vec![
                        FakeAxNode {
                            id: alloc(),
                            data: make_ax(
                                "popup",
                                Some("Model Selector"),
                                Some(rect(300.0, 16.0, 180.0, 28.0)),
                                vec![ElementAction::Press],
                            ),
                            children: vec![],
                        },
                        FakeAxNode {
                            id: alloc(),
                            data: make_ax(
                                "checkbox",
                                Some("Auto-approve"),
                                Some(rect(500.0, 16.0, 120.0, 28.0)),
                                vec![ElementAction::Press],
                            ),
                            children: vec![],
                        },
                    ],
                },
                FakeAxNode {
                    id: alloc(),
                    data: make_ax(
                        "group",
                        None,
                        Some(rect(290.0, 740.0, 820.0, 140.0)),
                        vec![ElementAction::ShowMenu],
                    ),
                    children: vec![
                        FakeAxNode {
                            id: alloc(),
                            data: make_ax(
                                "textarea",
                                Some("Message composer"),
                                Some(rect(300.0, 750.0, 700.0, 120.0)),
                                vec![ElementAction::SetValue],
                            ),
                            children: vec![],
                        },
                        FakeAxNode {
                            id: alloc(),
                            data: make_ax(
                                "button",
                                Some("Send"),
                                Some(rect(1010.0, 830.0, 80.0, 32.0)),
                                vec![ElementAction::Press],
                            ),
                            children: vec![],
                        },
                    ],
                },
            ],
        };

        // Right sidebar with tabs and action buttons.
        let right_pane = FakeAxNode {
            id: alloc(),
            data: make_ax(
                "group",
                None,
                Some(rect(1120.0, 0.0, 320.0, 900.0)),
                vec![ElementAction::ShowMenu],
            ),
            children: vec![
                FakeAxNode {
                    id: alloc(),
                    data: make_ax(
                        "tabgroup",
                        Some("Inspector Tabs"),
                        Some(rect(1130.0, 12.0, 300.0, 32.0)),
                        vec![ElementAction::ShowMenu],
                    ),
                    children: vec![
                        FakeAxNode {
                            id: alloc(),
                            data: make_ax(
                                "tab",
                                Some("Diff"),
                                Some(rect(1135.0, 14.0, 90.0, 28.0)),
                                vec![ElementAction::Press],
                            ),
                            children: vec![],
                        },
                        FakeAxNode {
                            id: alloc(),
                            data: make_ax(
                                "tab",
                                Some("Files"),
                                Some(rect(1235.0, 14.0, 90.0, 28.0)),
                                vec![ElementAction::Press],
                            ),
                            children: vec![],
                        },
                    ],
                },
                FakeAxNode {
                    id: alloc(),
                    data: make_ax(
                        "button",
                        Some("Commit Changes"),
                        Some(rect(1140.0, 820.0, 260.0, 36.0)),
                        vec![ElementAction::Press],
                    ),
                    children: vec![],
                },
            ],
        };

        FakeAxNode {
            id: alloc(),
            data: make_ax(
                "window",
                Some("T3 Code"),
                Some(rect(0.0, 0.0, 1440.0, 900.0)),
                vec![ElementAction::Raise, ElementAction::ShowMenu],
            ),
            children: vec![FakeAxNode {
                id: alloc(),
                data: make_ax(
                    "webarea",
                    Some("T3 Code"),
                    Some(rect(0.0, 0.0, 1440.0, 900.0)),
                    vec![ElementAction::ShowMenu],
                ),
                children: vec![left_sidebar, center_pane, right_pane],
            }],
        }
    }

    fn collect_titles(node: &UiNode, out: &mut Vec<(String, String, String)>) {
        out.push((
            node.ref_id.clone(),
            node.role.clone(),
            node.title.clone().unwrap_or_default(),
        ));
        for c in &node.children {
            collect_titles(c, out);
        }
    }

    fn make_snapshot_from_walk(out: WalkOutput<usize>) -> Snapshot {
        Snapshot {
            snapshot_id: "s1".into(),
            app: AppInfo {
                name: "T3 Code".into(),
                bundle_id: Some("com.example.t3".into()),
                pid: Some(123),
                path: None,
                running: true,
            },
            window_id: Some(1),
            window_title: Some("T3 Code".into()),
            window_bounds: Some(rect(0.0, 0.0, 1440.0, 900.0)),
            truncated: out.truncated,
            total_nodes: out.total_visited,
            emitted_nodes: out.emitted_nodes,
            elapsed_ms: 1,
            root: out.root,
        }
    }

    #[test]
    fn dense_sidebar_does_not_starve_center_and_right_panes_in_snapshot_or_som() {
        let tree = build_dense_electron_tree();
        // Even with a tight budget (60 nodes, much smaller than the 150+ sidebar wrapper/text nodes),
        // BFS + decorative flattening + interactive prioritization must discover all panes.
        let out = walk_fake_tree(tree, WalkBudget::new(24, 60));
        assert!(out.emitted_nodes <= 60);
        assert_eq!(out.refs.len(), out.emitted_nodes);
        assert_eq!(out.root.ref_id, "@e1");

        let mut visited = Vec::new();
        collect_titles(&out.root, &mut visited);

        // Verify anonymous decorative wrapper groups were spliced away.
        assert!(
            !visited
                .iter()
                .any(|(_, role, title)| role == "group" && title.is_empty()),
            "anonymous wrapper groups should be flattened: {visited:?}"
        );

        // Verify deterministic sequential pre-order @e1..@eN numbering.
        for (i, (ref_id, _, _)) in visited.iter().enumerate() {
            assert_eq!(ref_id, &format!("@e{}", i + 1));
        }

        // Verify interactive controls from left, center, and right panes are all present.
        for expected in [
            "New Thread",
            "Model Selector",
            "Auto-approve",
            "Message composer",
            "Send",
            "Diff",
            "Files",
            "Commit Changes",
        ] {
            assert!(
                visited.iter().any(|(_, _, t)| t == expected),
                "missing control {expected:?} in snapshot"
            );
        }

        // Verify Set-of-Marks annotation covers all 8 interactive controls across all 3 panes
        // and skips static text, window, webarea, and tabgroup containers!
        let snap = make_snapshot_from_walk(out);
        let boxes = collect_annotate_boxes(&snap, 80);
        let annotated_titles: Vec<String> = boxes
            .iter()
            .filter_map(|b| {
                visited
                    .iter()
                    .find(|(r, _, _)| r == &b.ref_id)
                    .map(|(_, _, t)| t.clone())
            })
            .collect();

        for expected in [
            "New Thread",
            "Model Selector",
            "Auto-approve",
            "Message composer",
            "Send",
            "Diff",
            "Files",
            "Commit Changes",
        ] {
            assert!(
                annotated_titles.iter().any(|t| t == expected),
                "missing SoM box for {expected:?}: {annotated_titles:?}"
            );
        }
        assert_eq!(
            annotated_titles.len(),
            8,
            "should annotate only the 8 interactive controls, not static text or wrappers: {annotated_titles:?}"
        );
    }

    #[test]
    fn wide_interactive_sidebar_still_reaches_center_and_right_panes() {
        let mut next_id = 1usize;
        let mut alloc = || {
            let id = next_id;
            next_id += 1;
            id
        };

        // 120 interactive buttons in the left sidebar directly under a list container.
        let sidebar_buttons: Vec<FakeAxNode> = (0..120)
            .map(|i| FakeAxNode {
                id: alloc(),
                data: make_ax(
                    "button",
                    Some(&format!("Sidebar Btn {i}")),
                    Some(rect(10.0, 10.0 + (i as f64) * 7.0, 220.0, 6.0)),
                    vec![ElementAction::Press],
                ),
                children: vec![],
            })
            .collect();

        let root = FakeAxNode {
            id: alloc(),
            data: make_ax(
                "window",
                Some("App"),
                Some(rect(0.0, 0.0, 1440.0, 900.0)),
                vec![],
            ),
            children: vec![
                FakeAxNode {
                    id: alloc(),
                    data: make_ax("group", None, None, vec![]),
                    children: sidebar_buttons,
                },
                FakeAxNode {
                    id: alloc(),
                    data: make_ax("group", None, None, vec![]),
                    children: vec![FakeAxNode {
                        id: alloc(),
                        data: make_ax("group", None, None, vec![]),
                        children: vec![FakeAxNode {
                            id: alloc(),
                            data: make_ax(
                                "button",
                                Some("Center Action"),
                                Some(rect(600.0, 400.0, 120.0, 36.0)),
                                vec![ElementAction::Press],
                            ),
                            children: vec![],
                        }],
                    }],
                },
                FakeAxNode {
                    id: alloc(),
                    data: make_ax("group", None, None, vec![]),
                    children: vec![FakeAxNode {
                        id: alloc(),
                        data: make_ax(
                            "button",
                            Some("Right Action"),
                            Some(rect(1260.0, 400.0, 120.0, 36.0)),
                            vec![ElementAction::Press],
                        ),
                        children: vec![],
                    }],
                },
            ],
        };

        let out = walk_fake_tree(root, WalkBudget::new(24, 50));
        assert_eq!(out.emitted_nodes, 50);
        assert!(out.truncated);

        let mut visited = Vec::new();
        collect_titles(&out.root, &mut visited);
        assert!(visited.iter().any(|(_, _, t)| t == "Center Action"));
        assert!(visited.iter().any(|(_, _, t)| t == "Right Action"));

        // Even with a tight SoM cap of 20 boxes, spatial balancing must include left, center, right.
        let snap = make_snapshot_from_walk(out);
        let boxes = collect_annotate_boxes(&snap, 20);
        assert_eq!(boxes.len(), 20);
        assert!(boxes.iter().any(|b| b.bounds.x < 280.0));
        assert!(boxes.iter().any(|b| (500.0..800.0).contains(&b.bounds.x)));
        assert!(boxes.iter().any(|b| b.bounds.x > 1120.0));
    }

    #[test]
    fn annotate_deduplicates_overlapping_wrappers_and_keeps_floating_button_in_textarea() {
        let root = UiNode {
            ref_id: "@e1".into(),
            role: "window".into(),
            subrole: None,
            title: Some("Win".into()),
            label: None,
            value: None,
            bounds: Some(rect(0.0, 0.0, 1000.0, 800.0)),
            enabled: true,
            focused: false,
            selected: false,
            actions: vec![ElementAction::Raise, ElementAction::ShowMenu],
            children: vec![
                // Row wrapping an identical-bounds button: only the inner button should be kept.
                UiNode {
                    ref_id: "@e2".into(),
                    role: "row".into(),
                    subrole: None,
                    title: Some("Thread 1".into()),
                    label: None,
                    value: None,
                    bounds: Some(rect(10.0, 20.0, 200.0, 36.0)),
                    enabled: true,
                    focused: false,
                    selected: true,
                    actions: vec![ElementAction::Press],
                    children: vec![UiNode {
                        ref_id: "@e3".into(),
                        role: "button".into(),
                        subrole: None,
                        title: Some("Thread 1".into()),
                        label: None,
                        value: None,
                        bounds: Some(rect(10.0, 20.0, 200.0, 36.0)),
                        enabled: true,
                        focused: false,
                        selected: false,
                        actions: vec![ElementAction::Press],
                        children: vec![],
                    }],
                },
                // Duplicate tab + inner button at virtually identical bounds -> keep inner button.
                UiNode {
                    ref_id: "@e4".into(),
                    role: "tab".into(),
                    subrole: None,
                    title: Some("Diff".into()),
                    label: None,
                    value: None,
                    bounds: Some(rect(800.0, 10.0, 80.0, 28.0)),
                    enabled: true,
                    focused: false,
                    selected: false,
                    actions: vec![ElementAction::Press],
                    children: vec![UiNode {
                        ref_id: "@e5".into(),
                        role: "button".into(),
                        subrole: None,
                        title: Some("Diff".into()),
                        label: None,
                        value: None,
                        bounds: Some(rect(801.0, 11.0, 78.0, 26.0)),
                        enabled: true,
                        focused: false,
                        selected: false,
                        actions: vec![ElementAction::Press],
                        children: vec![],
                    }],
                },
                // Large textarea containing a small Send button -> BOTH must be kept.
                UiNode {
                    ref_id: "@e6".into(),
                    role: "textarea".into(),
                    subrole: None,
                    title: Some("Composer".into()),
                    label: None,
                    value: None,
                    bounds: Some(rect(250.0, 600.0, 600.0, 150.0)),
                    enabled: true,
                    focused: true,
                    selected: false,
                    actions: vec![ElementAction::SetValue],
                    children: vec![UiNode {
                        ref_id: "@e7".into(),
                        role: "button".into(),
                        subrole: None,
                        title: Some("Send".into()),
                        label: None,
                        value: None,
                        bounds: Some(rect(780.0, 700.0, 60.0, 32.0)),
                        enabled: true,
                        focused: false,
                        selected: false,
                        actions: vec![ElementAction::Press],
                        children: vec![],
                    }],
                },
                // Off-screen scrolled button should be skipped.
                UiNode {
                    ref_id: "@e8".into(),
                    role: "button".into(),
                    subrole: None,
                    title: Some("Offscreen".into()),
                    label: None,
                    value: None,
                    bounds: Some(rect(10.0, 1200.0, 100.0, 30.0)),
                    enabled: true,
                    focused: false,
                    selected: false,
                    actions: vec![ElementAction::Press],
                    children: vec![],
                },
                // Clickable row with only static text child (no button child) SHOULD be annotated.
                UiNode {
                    ref_id: "@e9".into(),
                    role: "row".into(),
                    subrole: None,
                    title: None,
                    label: None,
                    value: None,
                    bounds: Some(rect(10.0, 70.0, 200.0, 32.0)),
                    enabled: true,
                    focused: false,
                    selected: false,
                    actions: vec![ElementAction::Press],
                    children: vec![UiNode {
                        ref_id: "@e10".into(),
                        role: "text".into(),
                        subrole: None,
                        title: Some("Thread 2".into()),
                        label: None,
                        value: None,
                        bounds: Some(rect(16.0, 74.0, 150.0, 20.0)),
                        enabled: true,
                        focused: false,
                        selected: false,
                        actions: vec![ElementAction::Press, ElementAction::ShowMenu],
                        children: vec![],
                    }],
                },
                // Clickable sidebar row with a small trailing Delete button: BOTH row and button
                // must be annotated (dedup only drops rows >= 70% covered by an inner control).
                UiNode {
                    ref_id: "@e11".into(),
                    role: "row".into(),
                    subrole: None,
                    title: Some("Thread 3".into()),
                    label: None,
                    value: None,
                    bounds: Some(rect(10.0, 120.0, 240.0, 40.0)),
                    enabled: true,
                    focused: false,
                    selected: false,
                    actions: vec![ElementAction::Press, ElementAction::Focus],
                    children: vec![UiNode {
                        ref_id: "@e12".into(),
                        role: "button".into(),
                        subrole: None,
                        title: Some("Delete".into()),
                        label: None,
                        value: None,
                        bounds: Some(rect(224.0, 130.0, 20.0, 20.0)),
                        enabled: true,
                        focused: false,
                        selected: false,
                        actions: vec![ElementAction::Press],
                        children: vec![],
                    }],
                },
                // Large backdrop image exposing Confirm must still be filtered by the 50% size cap.
                UiNode {
                    ref_id: "@e13".into(),
                    role: "image".into(),
                    subrole: None,
                    title: Some("Backdrop".into()),
                    label: None,
                    value: None,
                    bounds: Some(rect(0.0, 0.0, 900.0, 700.0)),
                    enabled: true,
                    focused: false,
                    selected: false,
                    actions: vec![ElementAction::Confirm],
                    children: vec![],
                },
                // Static text with Focus (not SetValue) must not be annotated.
                UiNode {
                    ref_id: "@e14".into(),
                    role: "text".into(),
                    subrole: None,
                    title: Some("Focusable caption".into()),
                    label: None,
                    value: None,
                    bounds: Some(rect(300.0, 20.0, 180.0, 20.0)),
                    enabled: true,
                    focused: false,
                    selected: false,
                    actions: vec![ElementAction::Focus],
                    children: vec![],
                },
            ],
        };

        let snap = Snapshot {
            snapshot_id: "s2".into(),
            app: AppInfo {
                name: "Test".into(),
                bundle_id: None,
                pid: None,
                path: None,
                running: true,
            },
            window_id: Some(1),
            window_title: Some("Win".into()),
            window_bounds: Some(rect(0.0, 0.0, 1000.0, 800.0)),
            truncated: false,
            total_nodes: 14,
            emitted_nodes: 14,
            elapsed_ms: 1,
            root,
        };

        let boxes = collect_annotate_boxes(&snap, 80);
        let ids: Vec<&str> = boxes.iter().map(|b| b.ref_id.as_str()).collect();
        assert_eq!(ids, vec!["@e3", "@e5", "@e6", "@e7", "@e9", "@e11", "@e12"]);
    }

    #[test]
    fn deferred_overflow_preserves_original_sibling_order_and_truncation_flags() {
        // A list with 25 buttons (> DEFAULT_PRIMARY_FANOUT) under a window with max_nodes = 250
        // should emit all 25 buttons in exact original sibling order with truncated = false.
        let children: Vec<FakeAxNode> = (0..25)
            .map(|i| FakeAxNode {
                id: i + 2,
                data: make_ax(
                    "button",
                    Some(&format!("Item {i}")),
                    Some(rect(10.0, 10.0 + (i as f64) * 24.0, 120.0, 20.0)),
                    vec![ElementAction::Press],
                ),
                children: vec![],
            })
            .collect();

        let root = FakeAxNode {
            id: 1,
            data: make_ax(
                "window",
                Some("List"),
                Some(rect(0.0, 0.0, 400.0, 700.0)),
                vec![],
            ),
            children,
        };

        let out = walk_fake_tree(root.clone(), WalkBudget::new(24, 250));
        assert!(!out.truncated);
        assert_eq!(out.emitted_nodes, 26);
        let titles: Vec<&str> = out
            .root
            .children
            .iter()
            .filter_map(|c| c.title.as_deref())
            .collect();
        let expected: Vec<String> = (0..25).map(|i| format!("Item {i}")).collect();
        assert_eq!(titles, expected);

        // max_nodes = 1 on a tree with children must report truncated = true and emit only @e1.
        let one = walk_fake_tree(root.clone(), WalkBudget::new(24, 1));
        assert!(one.truncated);
        assert_eq!(one.emitted_nodes, 1);
        assert!(one.root.children.is_empty());

        // max_depth = 0 on a tree with children must report truncated = true.
        let shallow = walk_fake_tree(root, WalkBudget::new(0, 250));
        assert!(shallow.truncated);
        assert_eq!(shallow.emitted_nodes, 1);
    }

    #[test]
    fn handles_map_matches_emitted_preorder_refs() {
        let tree = build_dense_electron_tree();
        let out = walk_fake_tree(tree, WalkBudget::new(24, 250));
        let mut nodes = Vec::new();
        collect_titles(&out.root, &mut nodes);
        assert_eq!(out.refs.len(), nodes.len());
        for (ref_id, _, _) in &nodes {
            assert!(
                out.refs.contains_key(ref_id),
                "missing live ref handle for {ref_id}"
            );
        }
    }

    #[test]
    fn prunes_redundant_wrappers_list_markers_and_child_text_with_guards() {
        let make_text = |id: usize, val: &str| FakeAxNode {
            id,
            data: AxNodeData {
                role: "text".into(),
                subrole: None,
                title: None,
                label: None,
                value: Some(val.into()),
                bounds: Some(rect(10.0, 10.0, 40.0, 14.0)),
                enabled: true,
                focused: false,
                selected: false,
                actions: vec![ElementAction::ShowMenu],
            },
            children: vec![],
        };

        let long_card_title = format!("{} trailing-detail-past-eighty-chars", "A".repeat(82));

        let root = FakeAxNode {
            id: 1,
            data: make_ax(
                "window",
                Some("agent-desktop"),
                Some(rect(0.0, 0.0, 1200.0, 800.0)),
                vec![ElementAction::Raise],
            ),
            children: vec![FakeAxNode {
                id: 2,
                // Redundant same-title non-leaf wrapper even with Electron AXPress/AXShowMenu.
                data: make_ax(
                    "group",
                    Some("agent-desktop"),
                    Some(rect(0.0, 0.0, 1200.0, 800.0)),
                    vec![ElementAction::Press, ElementAction::ShowMenu],
                ),
                children: vec![FakeAxNode {
                    id: 3,
                    data: make_ax(
                        "webarea",
                        Some("agent-desktop"),
                        Some(rect(0.0, 0.0, 1200.0, 800.0)),
                        vec![ElementAction::ShowMenu],
                    ),
                    children: vec![
                        // 1. Button whose 6 child text fragments concatenate to its label -> all 6 dropped.
                        FakeAxNode {
                            id: 10,
                            data: make_ax(
                                "button",
                                Some("Changes vs origin/main +0 -0"),
                                Some(rect(10.0, 10.0, 220.0, 30.0)),
                                vec![ElementAction::Press, ElementAction::ShowMenu],
                            ),
                            children: vec![
                                make_text(11, "Changes"),
                                make_text(12, "vs origin/main"),
                                make_text(13, "+"),
                                make_text(14, "0"),
                                make_text(15, "-"),
                                make_text(16, "0"),
                            ],
                        },
                        // 2. Thread card button: duplicate title & repo dropped, unique timestamp + inner button kept.
                        FakeAxNode {
                            id: 20,
                            data: make_ax(
                                "button",
                                Some("Release Checklist Notes, acme/demo"),
                                Some(rect(10.0, 50.0, 240.0, 70.0)),
                                vec![ElementAction::Press, ElementAction::ShowMenu],
                            ),
                            children: vec![
                                make_text(21, "acme/demo"),
                                make_text(22, "14h"),
                                FakeAxNode {
                                    id: 23,
                                    data: make_ax(
                                        "button",
                                        Some("Unpin thread"),
                                        Some(rect(20.0, 55.0, 16.0, 16.0)),
                                        vec![ElementAction::Press],
                                    ),
                                    children: vec![],
                                },
                                make_text(24, "Release Checklist Notes"),
                            ],
                        },
                        // 2b. Leaf text with matching title + distinct value, or whitespace title + value: must be KEPT.
                        FakeAxNode {
                            id: 25,
                            data: make_ax(
                                "group",
                                Some("Status"),
                                Some(rect(10.0, 115.0, 200.0, 20.0)),
                                vec![],
                            ),
                            children: vec![
                                FakeAxNode {
                                    id: 26,
                                    data: AxNodeData {
                                        role: "text".into(),
                                        subrole: None,
                                        title: Some("Status".into()),
                                        label: None,
                                        value: Some("Error: timeout".into()),
                                        bounds: None,
                                        enabled: true,
                                        focused: false,
                                        selected: false,
                                        actions: vec![],
                                    },
                                    children: vec![],
                                },
                                FakeAxNode {
                                    id: 27,
                                    data: AxNodeData {
                                        role: "text".into(),
                                        subrole: None,
                                        title: Some("   ".into()),
                                        label: None,
                                        value: Some("Important".into()),
                                        bounds: None,
                                        enabled: true,
                                        focused: false,
                                        selected: false,
                                        actions: vec![],
                                    },
                                    children: vec![],
                                },
                            ],
                        },
                        // 3. Long card title (>80 chars): text past char 80 is NOT rendered in parent, must stay.
                        FakeAxNode {
                            id: 30,
                            data: make_ax(
                                "button",
                                Some(&long_card_title),
                                Some(rect(10.0, 130.0, 240.0, 40.0)),
                                vec![ElementAction::Press],
                            ),
                            children: vec![make_text(31, "trailing-detail-past-eighty-chars")],
                        },
                        // 4. Short badge "0" inside "Open 10 tabs": must NOT be falsely dropped.
                        FakeAxNode {
                            id: 40,
                            data: make_ax(
                                "button",
                                Some("Open 10 tabs"),
                                Some(rect(10.0, 180.0, 120.0, 24.0)),
                                vec![ElementAction::Press],
                            ),
                            children: vec![make_text(41, "0")],
                        },
                        // 5. Unlabeled row inside window "agent-desktop": window title must NOT bleed into row's text.
                        FakeAxNode {
                            id: 50,
                            data: make_ax(
                                "row",
                                None,
                                Some(rect(10.0, 210.0, 200.0, 24.0)),
                                vec![ElementAction::Press],
                            ),
                            children: vec![make_text(51, "agent-desktop")],
                        },
                        // 6. List markers (macOS + Linux variants) are dropped as decorative.
                        FakeAxNode {
                            id: 60,
                            data: make_ax("listmarker", Some("•"), None, vec![]),
                            children: vec![],
                        },
                        FakeAxNode {
                            id: 61,
                            data: make_ax("list_marker", Some("-"), None, vec![]),
                            children: vec![],
                        },
                        // 7. Leaf clickable group (or group with only decorative children) matching window title must NOT be spliced.
                        FakeAxNode {
                            id: 70,
                            data: make_ax(
                                "group",
                                Some("agent-desktop"),
                                Some(rect(10.0, 250.0, 140.0, 28.0)),
                                vec![ElementAction::Press],
                            ),
                            children: vec![FakeAxNode {
                                id: 71,
                                data: make_ax("listmarker", Some("•"), None, vec![]),
                                children: vec![],
                            }],
                        },
                        // 8. Clickable group inside unlabeled list sharing window title must NOT inherit window title.
                        FakeAxNode {
                            id: 72,
                            data: make_ax(
                                "list",
                                None,
                                Some(rect(10.0, 290.0, 200.0, 40.0)),
                                vec![],
                            ),
                            children: vec![FakeAxNode {
                                id: 73,
                                data: make_ax(
                                    "group",
                                    Some("agent-desktop"),
                                    Some(rect(10.0, 290.0, 200.0, 24.0)),
                                    vec![ElementAction::Press],
                                ),
                                children: vec![make_text(74, "branch: main")],
                            }],
                        },
                    ],
                }],
            }],
        };

        let out = walk_fake_tree(root, WalkBudget::new(24, 250));
        assert!(!out.truncated);

        let snap = Snapshot {
            snapshot_id: "s1".into(),
            app: AppInfo {
                name: "Test".into(),
                bundle_id: None,
                pid: None,
                path: None,
                running: true,
            },
            window_id: Some(1),
            window_title: Some("agent-desktop".into()),
            window_bounds: Some(rect(0.0, 0.0, 1200.0, 800.0)),
            truncated: out.truncated,
            total_nodes: out.total_visited,
            emitted_nodes: out.emitted_nodes,
            elapsed_ms: 1,
            root: out.root,
        };
        let rendered = snap.render_compact();

        // Triple window/group/webarea wrapper collapsed directly to window's children.
        assert!(!rendered.contains("webarea"));
        assert!(!rendered.contains("listmarker"));
        assert!(!rendered.contains("list_marker"));

        // Changes button has zero redundant child text nodes.
        let changes_btn = snap
            .root
            .children
            .iter()
            .find(|n| n.title.as_deref() == Some("Changes vs origin/main +0 -0"))
            .expect("Changes button kept");
        assert!(changes_btn.children.is_empty());

        // Thread card kept "14h" and "Unpin thread", dropped "acme/demo" and duplicate title.
        let card_btn = snap
            .root
            .children
            .iter()
            .find(|n| n.title.as_deref() == Some("Release Checklist Notes, acme/demo"))
            .expect("Card button kept");
        let card_child_texts: Vec<String> = card_btn
            .children
            .iter()
            .map(|c| {
                c.title
                    .clone()
                    .or_else(|| c.value.clone())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(card_child_texts, vec!["14h", "Unpin thread"]);

        // Distinct value and whitespace-title leaf text nodes must be kept.
        assert!(rendered.contains("value=\"Error: timeout\""));
        assert!(rendered.contains("text \"Important\""));

        // Long card kept child text beyond char 80.
        assert!(rendered.contains("trailing-detail-past-eighty-chars"));
        // Short badge "0" kept.
        assert!(rendered.contains("text \"0\""));
        // Sidebar row kept "agent-desktop" text (no window-title bleed) and clickable row keeps actions=[press].
        assert!(rendered.contains("row actions=[press]\n  @e"));
        assert!(rendered.contains("text \"agent-desktop\""));
        // Decorative-only-child clickable group and nested group inside unlabeled list both kept.
        assert_eq!(
            rendered
                .matches("group \"agent-desktop\" actions=[press]")
                .count(),
            2
        );
        assert!(rendered.contains("text \"branch: main\""));
        // Ref map stays 1:1 with emitted nodes.
        let mut nodes = Vec::new();
        collect_titles(&snap.root, &mut nodes);
        assert_eq!(out.refs.len(), nodes.len());
    }
}
