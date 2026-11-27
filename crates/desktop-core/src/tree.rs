use desktop_proto::{FindMatch, Snapshot, UiNode};

use crate::backend::AnnotateBox;

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

/// Collect bounded interactive elements from a snapshot for Set-of-Marks screenshot annotation.
#[must_use]
pub fn collect_annotate_boxes(snapshot: &Snapshot, max_boxes: usize) -> Vec<AnnotateBox> {
    let mut boxes = Vec::new();
    visit_boxes(&snapshot.root, max_boxes, &mut boxes);
    boxes
}

fn visit_boxes(node: &UiNode, max_boxes: usize, out: &mut Vec<AnnotateBox>) {
    if out.len() >= max_boxes {
        return;
    }
    if let Some(bounds) = node.bounds
        && !bounds.is_empty()
        && (!node.actions.is_empty()
            || matches!(
                node.role.as_str(),
                "button"
                    | "textfield"
                    | "textarea"
                    | "checkbox"
                    | "radio"
                    | "tab"
                    | "link"
                    | "menuitem"
                    | "popup"
            ))
    {
        out.push(AnnotateBox {
            ref_id: node.ref_id.clone(),
            bounds,
        });
    }
    for child in &node.children {
        visit_boxes(child, max_boxes, out);
        if out.len() >= max_boxes {
            break;
        }
    }
}
