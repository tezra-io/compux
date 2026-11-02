//! A walk as the wire publishes it: each listed control's entry, its click point
//! in the reply's pixels, the reference it is named by and the listed control it
//! sits inside. Pure: the walk and the view are read before anything here runs.

use serde_json::{json, Map, Value};

use crate::ax;
use crate::geometry::{self, to_sent, Geometry, Region};
use crate::roles;
use crate::tree;

/// The view a listing's click points are in.
pub struct Viewport<'a> {
    pub geom: &'a Geometry,
    pub region: &'a Region,
}

/// A listed AX node paired with its click point in sent-image space, where the
/// platform places it at all.
pub type ViewNode = (tree::Node, Option<(i64, i64)>);

/// A walk as a listing publishes it, and the entries behind its references: its
/// controls, what the walk cost, why it stopped early if it did, and for a
/// skeleton what its cuts hide that could not be listed. `names`, `first` and
/// `root` as [`listed_nodes`].
pub fn published(
    found: tree::Walk,
    messages: u64,
    shape: &tree::Shape,
    names: bool,
    first: usize,
    root: Option<&str>,
    view: &Viewport<'_>,
) -> (Value, Vec<ax::Entry>) {
    let (placed, outside) = in_view(found.nodes, view.geom, view.region);
    let (items, entries) = listed_nodes(placed, names, first, root, view);
    let mut payload = json!({ "ok": true, "elements": items, "ax_messages": messages });
    if let Some(object) = payload.as_object_mut() {
        if shape.skeleton {
            let hidden = found.hidden + outside;
            object.insert(
                "skeleton".to_string(),
                json!({ "depth": tree::SKELETON_DEPTH, "hidden": hidden }),
            );
        }
        if let Some(truncation) = found.truncated {
            object.insert("truncated".to_string(), json!(truncation));
        }
    }
    (payload, entries)
}

/// The nodes of one walk inside this view, each with its click point in this
/// view's pixels, and how many children the skeleton cuts it dropped hid.
///
/// A node the platform places nowhere is kept with no point, and nothing
/// synthesises one; a node placed outside the view is dropped, which releases its
/// reference. What a kept node sits inside is re-pointed at the nearest kept node
/// it was inside.
pub fn in_view(nodes: Vec<tree::Node>, geom: &Geometry, region: &Region) -> (Vec<ViewNode>, usize) {
    let mut kept: Vec<Option<usize>> = Vec::with_capacity(nodes.len());
    let mut unders: Vec<Option<usize>> = Vec::with_capacity(nodes.len());
    let mut placed = Vec::new();
    let mut hidden = 0;

    for mut node in nodes {
        unders.push(node.under);
        let point = node.frame.map(|frame| {
            let (centre_x, centre_y) = frame.centre();
            to_sent(geom, region, centre_x, centre_y)
        });
        if point == Some(None) {
            kept.push(None);
            hidden += node.hidden.unwrap_or(0);
            continue;
        }
        node.under = nearest_kept(node.under, &kept, &unders);
        kept.push(Some(placed.len()));
        placed.push((node, point.flatten()));
    }
    (placed, hidden)
}

/// The nearest kept node up a chain of `under`s. Each step is to an earlier node,
/// so the chain is never longer than the walk.
fn nearest_kept(
    mut under: Option<usize>,
    kept: &[Option<usize>],
    unders: &[Option<usize>],
) -> Option<usize> {
    for _ in 0..kept.len() {
        let index = under?;
        if let Some(position) = kept.get(index).copied().flatten() {
            return Some(position);
        }
        under = unders.get(index).copied().flatten();
    }
    None
}

/// The listed nodes as the wire publishes them, and the entries behind their
/// references, numbered from `first`: what each sits inside is named by reference,
/// and a node inside no listed node sits inside `root` (a drill's).
pub fn listed_nodes(
    placed: Vec<ViewNode>,
    names: bool,
    first: usize,
    root: Option<&str>,
    view: &Viewport<'_>,
) -> (Vec<Value>, Vec<ax::Entry>) {
    let mut items = Vec::new();
    let mut entries = Vec::new();
    for (index, (node, point)) in placed.into_iter().enumerate() {
        let reference = ax::reference_for(first + index);
        items.push(element_value(
            &node,
            names.then_some(reference.as_str()),
            point,
            view.geom,
            view.region,
        ));
        let under = match node.under {
            Some(position) => Some(ax::reference_for(first + position)),
            None => root.map(str::to_string),
        };
        entries.push(ax::Entry {
            reference,
            role: node.role,
            secure: node.secure,
            under,
            element: node.element,
        });
    }
    (items, entries)
}

/// Whether a listing's entry sits inside `root`: its chain of `under`s reaches
/// it. Each step names a listed node, so the chain is never longer than the
/// listing.
pub fn inside(named: &ax::Elements, entry: &ax::Entry, root: &str) -> bool {
    let mut under = entry.under.as_deref();
    for _ in 0..named.len() {
        let Some(reference) = under else {
            return false;
        };
        if reference == root {
            return true;
        }
        under = named
            .get(reference)
            .and_then(|parent| parent.under.as_deref());
    }
    false
}

/// One element, as the wire publishes it.
///
/// Everything here was read in the ONE traversal that found it: a second round of
/// queries per element would double the cost of a list and could disagree with
/// itself half way through. Nothing is inferred from the role — `verbs` lists only
/// what the control has a route for (protocol 12), and its `role` is the
/// normalised name beside the platform's own.
///
/// `bounds` is in the SAME pixels as the click point beside it, which is the
/// pixels of the image this reply names. The frame the walk read is in global
/// logical points and stays inside this process: one reply, one coordinate space.
/// A control the platform places nowhere has neither, and nothing synthesises one.
pub fn element_value(
    node: &tree::Node,
    reference: Option<&str>,
    point: Option<(i64, i64)>,
    geom: &Geometry,
    region: &Region,
) -> Value {
    let kind = node.kind();
    let mut item = json!({
        "role": kind,
        "native_role": node.role,
        "enabled": node.enabled,
    });

    let Some(object) = item.as_object_mut() else {
        return item;
    };
    insert_place(object, node.frame, point, geom, region);
    // A reference this reply cannot resolve is worse than none: the control is
    // still listed with its click point, but nothing offers a name that would come
    // back `stale_element` the moment it was used. The one place that can happen
    // is an application whose start time would not read, which is what `retained`
    // refuses to build a table from.
    if let Some(reference) = reference {
        object.insert("element_ref".to_string(), json!(reference));
        object.insert("verbs".to_string(), json!(node.verbs()));
    }
    if let Some(label) = &node.label {
        object.insert("label".to_string(), json!(label));
    }
    // Absent for a secure field, which says so: the only way to tell "no value"
    // from "a value nobody may read".
    if let Some(value) = &node.value {
        object.insert("value".to_string(), json!(value));
    }
    if node.secure {
        object.insert("secure".to_string(), json!(true));
    }
    insert_states(object, kind, &node.states);
    if !node.path.is_empty() {
        object.insert("path".to_string(), json!(node.path));
    }
    if let Some(hidden) = node.hidden {
        object.insert("hidden".to_string(), json!(hidden));
    }
    // Only where it is true: some of the control's own actions could not be named,
    // so what it can do may be more than its verbs say.
    if node.actions_partial {
        object.insert("actions_partial".to_string(), json!(true));
    }

    item
}

/// Where a listed control is: its `bounds` and click point `x`, `y` in this
/// reply's pixels. Neither for a control the platform places nowhere.
fn insert_place(
    object: &mut Map<String, Value>,
    frame: Option<ax::Frame>,
    point: Option<(i64, i64)>,
    geom: &Geometry,
    region: &Region,
) {
    let (Some(frame), Some((x, y))) = (frame, point) else {
        return;
    };
    let bounds = geometry::sent_rect(geom, region, frame.x, frame.y, frame.w, frame.h);
    object.insert(
        "bounds".to_string(),
        json!({
            "x": bounds.x.round() as i64,
            "y": bounds.y.round() as i64,
            "w": bounds.w.round().max(1.0) as i64,
            "h": bounds.h.round().max(1.0) as i64
        }),
    );
    // The click point in this reply's own pixels, unchanged since protocol 4: a
    // caller that aims by coordinate keeps working exactly as it did.
    object.insert("x".to_string(), json!(x));
    object.insert("y".to_string(), json!(y));
}

/// A control's states as a listing and `element_after` publish them: `checked`
/// and `expanded` where the control has one, `selected` and `focused` only when
/// true.
pub fn insert_states(object: &mut Map<String, Value>, kind: &str, states: &ax::States) {
    let checked = states.on.filter(|_| roles::has_check_state(kind));
    let expanded = if kind == "disclosure" {
        states.on
    } else {
        states.expanded
    };
    for (key, state) in [("checked", checked), ("expanded", expanded)] {
        if let Some(state) = state {
            object.insert(key.to_string(), json!(state));
        }
    }
    for (key, state) in [("selected", states.selected), ("focused", states.focused)] {
        if state == Some(true) {
            object.insert(key.to_string(), json!(true));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ax::{Ax, Frame, Recorder, Scripted};
    use std::rc::Rc;

    const PID: i32 = 4711;

    // A kept node sits inside the nearest kept node above it: dropped nodes in
    // between are passed over, and a chain with nothing kept above names nothing.
    #[test]
    fn a_kept_node_sits_inside_the_nearest_kept_node_above_it() {
        // Node 0 is kept first; 1 (inside 0) and 2 (inside 1) were dropped.
        let kept = [Some(0), None, None];
        let unders = [None, Some(0), Some(1)];

        assert_eq!(nearest_kept(Some(2), &kept, &unders), Some(0));
        assert_eq!(nearest_kept(Some(1), &kept, &unders), Some(0));
        assert_eq!(nearest_kept(Some(0), &kept, &unders), Some(0));
        assert_eq!(nearest_kept(None, &kept, &unders), None);
        assert_eq!(nearest_kept(Some(1), &[None, None], &[None, Some(0)]), None);
    }

    // The chain is bounded by the walk: even a malformed one that loops ends.
    #[test]
    fn a_chain_of_unders_that_loops_still_ends() {
        let kept = [None, None];
        let unders = [Some(1), Some(0)];

        assert_eq!(nearest_kept(Some(0), &kept, &unders), None);
    }

    /// A listing of buttons whose entries sit inside the references given.
    fn listing(unders: &[Option<&str>]) -> (Rc<Recorder>, ax::Elements) {
        let frame = Frame {
            x: 0.0,
            y: 0.0,
            w: 80.0,
            h: 24.0,
        };
        let buttons = (0..unders.len())
            .map(|index| Scripted::button(&format!("B{index}"), frame))
            .collect();
        let recorder = Recorder::new(PID, buttons);
        let platform: Rc<dyn Ax> = recorder.clone();
        let root = platform
            .application(&platform, PID)
            .expect("the application");
        let page = platform
            .children(&platform, root.handle(), 0, unders.len())
            .expect("its buttons");
        let entries = page
            .into_iter()
            .zip(unders)
            .enumerate()
            .map(|(index, (element, under))| ax::Entry {
                reference: ax::reference_for(index),
                role: "AXButton".to_string(),
                secure: false,
                under: under.map(str::to_string),
                element,
            })
            .collect();
        (recorder, ax::Elements::new(PID, 1, None, entries))
    }

    // An entry is inside a root when its chain of `under`s reaches it, however
    // many steps up; the root itself, and what sits beside it, are not.
    #[test]
    fn an_entry_is_inside_a_root_its_chain_reaches() {
        let (_recorder, named) = listing(&[None, Some("e1"), Some("e2"), None]);
        let entry = |reference: &str| named.get(reference).expect(reference);

        assert!(inside(&named, entry("e2"), "e1"));
        assert!(inside(&named, entry("e3"), "e1"), "two steps up");
        assert!(!inside(&named, entry("e1"), "e1"), "the root itself");
        assert!(!inside(&named, entry("e4"), "e1"), "beside it");
        assert!(!inside(&named, entry("e1"), "e3"), "above it");
    }

    // The chain is bounded by the listing: one that loops ends, outside.
    #[test]
    fn a_chain_of_references_that_loops_is_not_inside() {
        let (_recorder, named) = listing(&[None, Some("e3"), Some("e2")]);

        assert!(!inside(&named, named.get("e2").expect("e2"), "e1"));
    }
}
