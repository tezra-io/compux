//! The Linux reader's writes: one AT-SPI call per verb, each the node's own route
//! ([`super::routes`]), each answered as the seam's `Refusal` when it fails.
//!
//! A reply means the call was delivered, never that it took effect: every verb is
//! read back above the seam (`semantic::judge`, `main.rs`'s `read_back`). And a
//! refused verb is never retried another way: a node with no route is refused
//! before anything here runs.

use std::time::{Duration, Instant};

use zbus::zvariant::{OwnedObjectPath, Value};
use zbus::Connection;

use super::atspi::{
    area, read_timeout, Address, Failed, Known, Real, ACCESSIBLE, ACTION, COMPONENT, EDITABLE_TEXT,
    PROPERTIES, SELECTION, TEXT, VALUE,
};
use super::routes::{self, Toolkit};
use super::vocabulary::{self, SCROLL_BAR, SCROLL_PANE, VERTICAL};
use crate::ax::{Expander, Frame, Handle, Refusal, Write, PRESS, SCROLL_TO_VISIBLE};

/// `ATSPI_SCROLL_ANYWHERE`: bring the node into view however it fits.
const SCROLL_ANYWHERE: u32 = 6;

/// `ATSPI_COORD_TYPE_WINDOW`: the coordinates GTK 4 gets right (measured on GTK
/// 4.14 under Xvfb in a container), and GTK 3 as well.
const WINDOW: u32 = 1;

/// How far up a node's ancestors the scroll pane it sits in is looked for.
const MAX_SCROLL_HOPS: usize = 8;

/// How many of a scroll pane's children are looked at for its vertical bar.
const MAX_PANE_CHILDREN: usize = 8;

/// What a call that did not do what it was sent for means for the receipt: only a
/// call that may have arrived — no answer in time, or a connection that failed
/// under it — says it went out.
fn refused(why: Failed, what: &str) -> Refusal {
    match why {
        Failed::Late => Refusal::new(
            true,
            format!("{what}: no answer in time, so it may have landed"),
        ),
        Failed::Lost => Refusal::new(
            true,
            format!("{what}: the accessibility bus failed under it, so it may have landed"),
        ),
        Failed::Quit => Refusal::new(false, format!("{what}: the application is gone")),
        Failed::Suspect => Refusal::new(
            false,
            format!("{what}: the application has no such object or call now"),
        ),
        Failed::Other => Refusal::new(false, format!("{what}: the application refused it")),
    }
}

/// A toolkit call that answers whether it did it.
fn answered(done: Result<bool, Failed>, what: &str) -> Result<(), Refusal> {
    match done {
        Ok(true) => Ok(()),
        Ok(false) => Err(Refusal::new(
            false,
            format!("{what}: the application declined it"),
        )),
        Err(why) => Err(refused(why, what)),
    }
}

/// A read a verb needs before its write, that did not answer: nothing that
/// changes anything went out, however the read failed. The verb could not read
/// what it acts from, which the contract calls `state_unreadable`.
fn unread(why: Failed, what: &str) -> Refusal {
    let because = match why {
        Failed::Late => "no answer in time",
        Failed::Lost => "the accessibility bus failed under it",
        Failed::Quit => "the application is gone",
        Failed::Suspect => "the application has no such object or call now",
        Failed::Other => "the application refused it",
    };
    Refusal::unsent(
        "state_unreadable",
        format!("{what}: {because}, so nothing was sent"),
    )
}

/// Nothing was sent: there is no route for this on the node.
fn no_route(what: &str) -> Refusal {
    Refusal::new(false, format!("this control has no route for {what}"))
}

fn gone() -> Refusal {
    Refusal::new(false, "that control is no longer held")
}

fn no_bus() -> Refusal {
    Refusal::new(false, "the accessibility bus is not open")
}

/// What is left of a deadline, for the next call.
fn left(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// A read before a write: a read's timeout, cut to what is left of the verb's
/// time. The reads are charged to the verb, so they can never leave its write
/// none.
fn reading(deadline: Instant) -> Duration {
    read_timeout().min(left(deadline))
}

/// Nothing is sent once the reads before a write used the verb's time up: a write
/// sent then would only be given up on, and say it may have landed.
fn in_time(deadline: Instant, what: &str) -> Result<(), Refusal> {
    if !left(deadline).is_zero() {
        return Ok(());
    }
    Err(Refusal::unsent(
        "state_unreadable",
        format!("{what} took all of this verb's time, so nothing was sent"),
    ))
}

impl Real {
    /// The node's last read, where it is, and the bus: what every write starts
    /// from.
    fn target(&self, handle: Handle) -> Result<(Known, Address, Connection), Refusal> {
        let known = self.known(handle).ok_or_else(gone)?;
        let at = self.address(handle).ok_or_else(gone)?;
        let bus = self.bus().ok_or_else(no_bus)?;
        Ok((known, at, bus))
    }

    /// `press` and `scroll_to` as the seam names them: the node's press action, or
    /// the scroll its toolkit has.
    pub(super) fn act(&self, handle: Handle, action: &str, limit: Duration) -> Result<(), Refusal> {
        let (known, at, bus) = self.target(handle)?;
        let read = known.read();
        match action {
            PRESS => {
                let index = routes::press(&read).ok_or_else(|| no_route("press"))?;
                self.do_action(&bus, &at, index, limit)
            }
            SCROLL_TO_VISIBLE if routes::scrolls(&read) && known.toolkit == Toolkit::Chromium => {
                let done = async_io::block_on(self.call_within(
                    &bus,
                    &at,
                    (COMPONENT, "ScrollTo"),
                    &(SCROLL_ANYWHERE,),
                    limit,
                ));
                answered(done, "scroll_to")
            }
            SCROLL_TO_VISIBLE if routes::scrolls(&read) => self.scroll_bar(&bus, &at, limit),
            other => Err(no_route(other)),
        }
    }

    /// `Action.DoAction` on the action at `index`.
    fn do_action(
        &self,
        bus: &Connection,
        at: &Address,
        index: i32,
        limit: Duration,
    ) -> Result<(), Refusal> {
        let done =
            async_io::block_on(self.call_within(bus, at, (ACTION, "DoAction"), &(index,), limit));
        answered(done, "the action")
    }

    /// `set_value`: a field's text whole, through `EditableText`, or a number
    /// through `Value`. Text that is no finite number is a wrong value, refused
    /// before a slider is sent anything: "NaN" and "inf" parse, and no control
    /// takes either.
    pub(super) fn set(&self, handle: Handle, value: &str, limit: Duration) -> Result<(), Refusal> {
        let (known, at, bus) = self.target(handle)?;
        if !routes::settable(&known.read()) {
            return Err(no_route("set_value"));
        }
        if known.interfaces.editable_text {
            let done = async_io::block_on(self.call_within(
                &bus,
                &at,
                (EDITABLE_TEXT, "SetTextContents"),
                &(value,),
                limit,
            ));
            return answered(done, "set_value");
        }
        let number = value
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite())
            .ok_or_else(|| {
                Refusal::unsent(
                    "invalid_argument",
                    format!(
                        "this control takes a finite number, and {value:?} is none: nothing was \
                         sent"
                    ),
                )
            })?;
        let written: Result<(), Failed> = async_io::block_on(self.call_within(
            &bus,
            &at,
            (PROPERTIES, "Set"),
            &(VALUE, "CurrentValue", Value::from(number)),
            limit,
        ));
        written.map_err(|why| refused(why, "set_value"))
    }

    /// One write a verb sends: focus, an expander's action, text at the caret, or
    /// an item selected through its container. An item never selects itself on
    /// AT-SPI.
    pub(super) fn written(
        &self,
        handle: Handle,
        write: Write<'_>,
        limit: Duration,
    ) -> Result<(), Refusal> {
        let (known, at, bus) = self.target(handle)?;
        match write {
            Write::Focused => {
                let done = async_io::block_on(self.call_within(
                    &bus,
                    &at,
                    (COMPONENT, "GrabFocus"),
                    &(),
                    limit,
                ));
                answered(done, "focus")
            }
            Write::Expand(Expander::Action, _) => {
                let index = routes::expander(&known.read()).ok_or_else(|| no_route("expand"))?;
                self.do_action(&bus, &at, index, limit)
            }
            Write::SelectedText(text) => self.type_text(&bus, &at, text, limit),
            Write::SelectedChildren(item) => self.select_child(&bus, &at, item, limit),
            // Qt's item `Toggle`; `semantic` sends it only while the item reads
            // not selected.
            Write::Selected => {
                let index = routes::selector(&known.read()).ok_or_else(|| no_route("select"))?;
                self.do_action(&bus, &at, index, limit)
            }
            Write::Expand(..) => Err(no_route("that write")),
        }
    }

    /// `type` by reference: delete the selection where there is one, then insert
    /// the text where it began, or at the caret. Once a deletion went out, a
    /// failure after it says so: the field has already changed.
    fn type_text(
        &self,
        bus: &Connection,
        at: &Address,
        text: &str,
        limit: Duration,
    ) -> Result<(), Refusal> {
        let deadline = Instant::now() + limit;
        let selections: i32 = async_io::block_on(self.call_within(
            bus,
            at,
            (TEXT, "GetNSelections"),
            &(),
            reading(deadline),
        ))
        .map_err(|why| unread(why, "reading the selection"))?;
        let insert_at = if selections > 0 {
            self.delete_selection(bus, at, deadline)?
        } else {
            let caret = async_io::block_on(self.property_within::<i32>(
                bus,
                at,
                (TEXT, "CaretOffset"),
                reading(deadline),
            ))
            .map_err(|why| unread(why, "reading the caret"))?;
            in_time(deadline, "reading the caret")?;
            caret
        };
        let length = i32::try_from(text.chars().count()).unwrap_or(i32::MAX);
        let done = async_io::block_on(self.call_within(
            bus,
            at,
            (EDITABLE_TEXT, "InsertText"),
            &(insert_at, text, length),
            left(deadline),
        ));
        match (answered(done, "type"), selections > 0) {
            (Err(refusal), true) => Err(Refusal::new(
                true,
                format!("the selection was deleted, and then {}", refusal.detail()),
            )),
            (outcome, _) => outcome,
        }
    }

    /// Delete the first selection, and answer where it began.
    fn delete_selection(
        &self,
        bus: &Connection,
        at: &Address,
        deadline: Instant,
    ) -> Result<i32, Refusal> {
        let (start, end): (i32, i32) = async_io::block_on(self.call_within(
            bus,
            at,
            (TEXT, "GetSelection"),
            &(0i32,),
            reading(deadline),
        ))
        .map_err(|why| unread(why, "reading the selection"))?;
        in_time(deadline, "reading the selection")?;
        let done = async_io::block_on(self.call_within(
            bus,
            at,
            (EDITABLE_TEXT, "DeleteText"),
            &(start, end),
            left(deadline),
        ));
        answered(done, "deleting the selection")?;
        Ok(start)
    }

    /// `Selection.SelectChild` on the container, at the item's own index in it.
    fn select_child(
        &self,
        bus: &Connection,
        container: &Address,
        item: Handle,
        limit: Duration,
    ) -> Result<(), Refusal> {
        let item = self.address(item).ok_or_else(gone)?;
        let index: i32 = async_io::block_on(self.accessible(bus, &item, "GetIndexInParent"))
            .map_err(|why| unread(why, "reading where the item is"))?;
        if index < 0 {
            return Err(Refusal::new(false, "the item is in no container now"));
        }
        let done = async_io::block_on(self.call_within(
            bus,
            container,
            (SELECTION, "SelectChild"),
            &(index,),
            limit,
        ));
        answered(done, "select")
    }

    /// `scroll_to` on GTK, which has no ScrollTo of its own: the vertical scroll
    /// bar of the scroll pane the node sits in moves by how far the node lies
    /// outside the pane, in window coordinates. A node already inside it moves
    /// nothing, and nothing is sent.
    fn scroll_bar(&self, bus: &Connection, at: &Address, limit: Duration) -> Result<(), Refusal> {
        let deadline = Instant::now() + limit;
        let Some((bar, target)) = async_io::block_on(self.scroll_target(bus, at, deadline))? else {
            return Ok(());
        };
        in_time(deadline, "reading where to scroll")?;
        let written: Result<(), Failed> = async_io::block_on(self.call_within(
            bus,
            &bar,
            (PROPERTIES, "Set"),
            &(VALUE, "CurrentValue", Value::from(target)),
            left(deadline),
        ));
        written.map_err(|why| refused(why, "scroll_to"))
    }

    /// The scroll bar to move and where to move it; `None` for a node already
    /// inside its pane. Every read is charged to `deadline`.
    async fn scroll_target(
        &self,
        bus: &Connection,
        at: &Address,
        deadline: Instant,
    ) -> Result<Option<(Address, f64)>, Refusal> {
        let pane = self
            .scroll_pane_of(bus, at, deadline)
            .await
            .map_err(|why| unread(why, "finding its scroll pane"))?
            .ok_or_else(|| Refusal::new(false, "no scroll pane holds this control now"))?;
        let bar = self
            .vertical_bar(bus, &pane, deadline)
            .await
            .map_err(|why| unread(why, "finding its scroll bar"))?
            .ok_or_else(|| Refusal::new(false, "its scroll pane has no vertical scroll bar"))?;
        let node = self.window_frame(bus, at, deadline).await;
        let view = self.window_frame(bus, &pane, deadline).await;
        let (Some(node), Some(view)) = (
            node.map_err(|why| unread(why, "reading where it is"))?,
            view.map_err(|why| unread(why, "reading where its scroll pane is"))?,
        ) else {
            return Err(Refusal::new(
                false,
                "it or its scroll pane would not say where it is",
            ));
        };
        let delta = outside(&node, &view);
        if delta == 0.0 {
            return Ok(None);
        }
        let (now, low, high) = self
            .range(bus, &bar, deadline)
            .await
            .map_err(|why| unread(why, "reading the scroll bar"))?;
        let target = moved(now, delta, (low, high)).ok_or_else(|| {
            Refusal::unsent(
                "state_unreadable",
                format!(
                    "its scroll bar reads {now} in {low} to {high}, which is no range to move \
                     in, so how far to scroll could not be decided: nothing was sent"
                ),
            )
        })?;
        Ok(Some((bar, target)))
    }

    /// The nearest scroll pane above a node, [`MAX_SCROLL_HOPS`] parents up at most.
    async fn scroll_pane_of(
        &self,
        bus: &Connection,
        at: &Address,
        deadline: Instant,
    ) -> Result<Option<Address>, Failed> {
        let mut node = at.clone();
        for _ in 0..MAX_SCROLL_HOPS {
            let parent: (String, OwnedObjectPath) = self
                .property_within(bus, &node, (ACCESSIBLE, "Parent"), reading(deadline))
                .await?;
            let Some(parent) = Address::of_child(parent) else {
                return Ok(None);
            };
            node = parent;
            if self.role_within(bus, &node, deadline).await? == SCROLL_PANE {
                return Ok(Some(node));
            }
        }
        Ok(None)
    }

    /// A scroll pane's vertical scroll bar, among its first [`MAX_PANE_CHILDREN`]
    /// children.
    async fn vertical_bar(
        &self,
        bus: &Connection,
        pane: &Address,
        deadline: Instant,
    ) -> Result<Option<Address>, Failed> {
        let listed: Vec<(String, OwnedObjectPath)> = self
            .call_within(
                bus,
                pane,
                (ACCESSIBLE, "GetChildren"),
                &(),
                reading(deadline),
            )
            .await?;
        let children = listed.into_iter().filter_map(Address::of_child);
        for child in children.take(MAX_PANE_CHILDREN) {
            if self.role_within(bus, &child, deadline).await? != SCROLL_BAR {
                continue;
            }
            let states: Vec<u32> = self
                .call_within(
                    bus,
                    &child,
                    (ACCESSIBLE, "GetState"),
                    &(),
                    reading(deadline),
                )
                .await?;
            if vocabulary::has(&states, VERTICAL) {
                return Ok(Some(child));
            }
        }
        Ok(None)
    }

    async fn role_within(
        &self,
        bus: &Connection,
        at: &Address,
        deadline: Instant,
    ) -> Result<u32, Failed> {
        self.call_within(bus, at, (ACCESSIBLE, "GetRole"), &(), reading(deadline))
            .await
    }

    async fn window_frame(
        &self,
        bus: &Connection,
        at: &Address,
        deadline: Instant,
    ) -> Result<Option<Frame>, Failed> {
        let extents: (i32, i32, i32, i32) = self
            .call_within(
                bus,
                at,
                (COMPONENT, "GetExtents"),
                &(WINDOW,),
                reading(deadline),
            )
            .await?;
        Ok(area(extents))
    }

    /// A Value's current, minimum and maximum.
    async fn range(
        &self,
        bus: &Connection,
        at: &Address,
        deadline: Instant,
    ) -> Result<(f64, f64, f64), Failed> {
        let value = |name| self.property_within(bus, at, (VALUE, name), reading(deadline));
        let now: f64 = value("CurrentValue").await?;
        let low: f64 = value("MinimumValue").await?;
        let high: f64 = value("MaximumValue").await?;
        Ok((now, low, high))
    }
}

/// Where a scroll bar at `now` ends after moving by `delta`, held to its range.
/// `None` for a range no application could mean, which it may still publish: a
/// number that is not finite, or a minimum above the maximum (`f64::clamp`
/// panics on either).
fn moved(now: f64, delta: f64, (low, high): (f64, f64)) -> Option<f64> {
    let finite = [now, delta, low, high]
        .iter()
        .all(|number| number.is_finite());
    (finite && low <= high).then(|| (now + delta).clamp(low, high))
}

/// How far a node lies outside the view it scrolls in, downward positive: 0 when
/// it is inside.
fn outside(node: &Frame, view: &Frame) -> f64 {
    let (top, bottom) = (view.y, view.y + view.h);
    if node.y < top {
        node.y - top
    } else if node.y + node.h > bottom {
        node.y + node.h - bottom
    } else {
        0.0
    }
}

#[cfg(test)]
mod scrolling {
    use super::*;

    fn frame(y: f64, h: f64) -> Frame {
        Frame {
            x: 0.0,
            y,
            w: 80.0,
            h,
        }
    }

    #[test]
    fn a_node_outside_its_view_is_that_far_outside_and_one_inside_is_not() {
        let view = frame(100.0, 100.0);
        assert_eq!(outside(&frame(1000.0, 20.0), &view), 820.0);
        assert_eq!(outside(&frame(40.0, 20.0), &view), -60.0);
        assert_eq!(outside(&frame(120.0, 20.0), &view), 0.0);
        assert_eq!(outside(&frame(180.0, 20.0), &view), 0.0);
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use crate::ax::{Ax, Handle, Write, PRESS, SCROLL_TO_VISIBLE};
    use crate::gate::SystemClock;
    use crate::linux::atspi::Real;
    use std::sync::Arc;

    use crate::gate::Gate;
    use crate::linux::fake::{
        self, Desktop, Fake, Life, Node, ACTIVE, APP, CHECKED, EDITABLE, EXPANDABLE, EXPANDED,
        FOCUSABLE, FOCUSED, PID, SELECTABLE, SELECTED, VERTICAL,
    };
    use crate::semantic::{self, Op, Verb};
    use crate::tree::{self, Walk};
    use crate::wire::Effect;

    const TIMEOUT: Duration = Duration::from_millis(200);
    const WRITE_MS: u64 = 500;
    const OTHER: &str = ":1.11";
    const OTHER_PID: u32 = 4712;

    fn reader(desktop: Desktop) -> (Fake, Rc<dyn Ax>) {
        let (fake, bus) = Fake::start(desktop, TIMEOUT);
        let ax: Rc<dyn Ax> = Rc::new(Real::over(bus));
        (fake, ax)
    }

    fn walk(ax: &Rc<dyn Ax>) -> Walk {
        tree::walk(ax, PID, &SystemClock::new(), u64::MAX)
    }

    /// The listed control with this label.
    fn listed(found: &Walk, label: &str) -> Handle {
        found
            .nodes
            .iter()
            .find(|node| node.label.as_deref() == Some(label))
            .unwrap_or_else(|| panic!("{label} is listed"))
            .element
            .handle()
    }

    fn plan(ax: &Rc<dyn Ax>, handle: Handle, verb: Verb) -> Result<semantic::Plan, String> {
        let facts = ax.facts(handle).map_err(|why| format!("{why:?}"))?;
        semantic::plan(ax, handle, PID, &facts, false, &verb).map_err(|failure| failure.code)
    }

    fn slider() -> Node {
        Node {
            value: 50.0,
            ..Node::of(51)
                .named("Volume")
                .at(10, 40, 200, 24)
                .listing(&["Value"])
        }
    }

    fn field(name: &str, text: &str) -> Node {
        Node {
            text: text.to_string(),
            ..Node::of(61)
                .named(name)
                .at(10, 10, 200, 24)
                .reading(&[EDITABLE, FOCUSABLE])
                .listing(&["Text", "EditableText"])
        }
    }

    // Press is DoAction on the index its press name was read at by
    // GetName(i). GetActions is never sent, and `doDefault` is never done.
    #[test]
    fn press_does_the_action_at_the_index_its_name_was_read_at() {
        let (fake, ax) = reader(fake::one_window(
            &["/w/new_tab"],
            vec![(
                "/w/new_tab",
                Node::of(43).named("New Tab").at(10, 10, 80, 24).acting(&[
                    Ok("doDefault"),
                    Ok("showContextMenu"),
                    Ok("press"),
                ]),
            )],
        ));
        let found = walk(&ax);
        let tab = listed(&found, "New Tab");

        ax.perform(tab, PRESS, WRITE_MS).expect("pressed");

        assert_eq!(fake.sent("DoAction"), vec!["/w/new_tab 2"]);
        assert!(fake.sent("GetActions").is_empty());
    }

    // A field's text is written whole, a slider's number through Value, and each
    // is read back by what it now holds. Text that is no number is refused before
    // anything is sent to a slider.
    #[test]
    fn set_value_writes_a_fields_text_and_a_sliders_number() {
        let (fake, ax) = reader(fake::one_window(
            &["/w/name", "/w/volume"],
            vec![("/w/name", field("Name", "ada")), ("/w/volume", slider())],
        ));
        let found = walk(&ax);
        let (name, volume) = (listed(&found, "Name"), listed(&found, "Volume"));

        ax.set_value(name, "grace", WRITE_MS).expect("written");
        ax.set_value(volume, "75", WRITE_MS).expect("written");
        let refused = ax
            .set_value(volume, "loud", WRITE_MS)
            .expect_err("no number");

        assert_eq!(fake.sent("SetTextContents"), vec!["/w/name grace"]);
        assert_eq!(ax.value(name).as_deref(), Some("grace"));
        assert_eq!(fake.node(APP, "/w/volume").value, 75.0);
        assert_eq!(ax.value(volume).as_deref(), Some("75"));
        assert!(!refused.sent());
        assert_eq!(fake.sent("Set").len(), 1, "the refused one sent nothing");
    }

    // "NaN", "inf" and "infinity" parse as numbers, but no control takes one. Like
    // text that is no number, each is a wrong value, refused before anything is
    // sent to the slider.
    #[test]
    fn set_value_sends_a_slider_only_a_finite_number() {
        let (fake, ax) = reader(fake::one_window(
            &["/w/volume"],
            vec![("/w/volume", slider())],
        ));
        let found = walk(&ax);
        let volume = listed(&found, "Volume");

        for text in ["NaN", "inf", "-inf", "infinity", "loud"] {
            let refused = ax.set_value(volume, text, WRITE_MS).expect_err(text);

            assert_eq!(refused.code(), Some("invalid_argument"), "{text}");
            assert!(!refused.sent(), "{text}");
        }
        assert!(fake.sent("Set").is_empty());
        assert_eq!(fake.node(APP, "/w/volume").value, 50.0);
    }

    // `type` by reference: the selection is deleted, then the text inserted at the
    // caret; with nothing selected, only inserted at the caret.
    #[test]
    fn type_replaces_the_selection_then_inserts_at_the_caret() {
        let selected = Node {
            selection: Some((6, 11)),
            caret: 11,
            ..field("Greeting", "hello world")
        };
        let at_caret = Node {
            caret: 2,
            ..field("Code", "abef")
        };
        let (fake, ax) = reader(fake::one_window(
            &["/w/greeting", "/w/code"],
            vec![("/w/greeting", selected), ("/w/code", at_caret)],
        ));
        let found = walk(&ax);

        ax.write(
            listed(&found, "Greeting"),
            Write::SelectedText("there"),
            WRITE_MS,
        )
        .expect("typed");
        ax.write(listed(&found, "Code"), Write::SelectedText("cd"), WRITE_MS)
            .expect("typed");

        assert_eq!(fake.node(APP, "/w/greeting").text, "hello there");
        assert_eq!(fake.node(APP, "/w/code").text, "abcdef");
        assert_eq!(fake.sent("DeleteText"), vec!["/w/greeting 6"]);
    }

    // `select` asks the container's Selection for the item's own index, and the
    // item is read back as selected.
    #[test]
    fn select_asks_the_container_for_the_items_index() {
        let item = |name: &str, y: i32| {
            Node::of(32)
                .named(name)
                .at(10, y, 80, 20)
                .reading(&[SELECTABLE])
        };
        let (fake, ax) = reader(fake::one_window(
            &["/w/list"],
            vec![
                (
                    "/w/list",
                    Node::of(98)
                        .named("Fruit")
                        .listing(&["Selection"])
                        .holding(APP, &["/w/apple", "/w/banana"]),
                ),
                ("/w/apple", item("Apple", 10)),
                ("/w/banana", item("Banana", 30)),
            ],
        ));
        let found = walk(&ax);
        let banana = listed(&found, "Banana");

        let planned = plan(&ax, banana, Verb::Select).expect("a route");
        semantic::send(&ax, banana, &planned, WRITE_MS).expect("selected");

        assert_eq!(fake.sent("SelectChild"), vec!["/w/list 1"]);
        assert!(fake.node(APP, "/w/banana").states.contains(&SELECTED));
        assert_eq!(
            ax.states(banana, false).expect("reads").selected,
            Some(true)
        );
    }

    // toggle reads the check state first and presses only when it differs.
    #[test]
    fn toggle_presses_only_when_the_state_differs() {
        let check = Node {
            flips: Some(CHECKED),
            ..Node::of(7)
                .named("Bold")
                .at(10, 10, 80, 24)
                .acting(&[Ok("click")])
        };
        let (fake, ax) = reader(fake::one_window(&["/w/bold"], vec![("/w/bold", check)]));
        let found = walk(&ax);
        let bold = listed(&found, "Bold");

        assert!(plan(&ax, bold, Verb::Toggle(false))
            .expect("a route")
            .already());
        let on = plan(&ax, bold, Verb::Toggle(true)).expect("a route");
        assert!(!on.already());
        semantic::send(&ax, bold, &on, WRITE_MS).expect("pressed");

        assert_eq!(fake.sent("DoAction"), vec!["/w/bold 0"]);
        assert_eq!(ax.states(bold, true).expect("reads").on, Some(true));
    }

    /// A window of a Qt application holding these nodes.
    fn qt_window(top: &[&str], nodes: Vec<(&str, Node)>) -> Desktop {
        let mut desktop = fake::one_window(top, nodes);
        desktop.put(
            APP,
            fake::ROOT,
            Node::of(75)
                .named("qt")
                .holding(APP, &["/w"])
                .made_with("Qt", "6.4.2"),
        );
        desktop
    }

    // Qt's views have no Selection. `select` sends a selectable item's own
    // `Toggle`, only while the item reads not selected, and reads SELECTED back:
    // an item that turned selected changed, one that did not is not observed, and
    // one already selected is sent nothing. `toggle` never takes that action.
    #[test]
    fn select_sends_a_qt_items_toggle_only_while_it_reads_not_selected() {
        let item = |name: &str, flips: Option<u32>, bits: &[u32]| Node {
            flips,
            ..Node::of(56)
                .named(name)
                .at(10, 10, 80, 20)
                .reading(bits)
                .acting(&[Ok("Toggle")])
        };
        let (fake, ax) = reader(qt_window(
            &["/w/banana", "/w/cherry", "/w/stuck"],
            vec![
                ("/w/banana", item("Banana", Some(SELECTED), &[SELECTABLE])),
                ("/w/cherry", item("Cherry", None, &[SELECTABLE, SELECTED])),
                ("/w/stuck", item("Stuck", None, &[SELECTABLE])),
            ],
        ));
        let found = walk(&ax);
        let (banana, cherry, stuck) = (
            listed(&found, "Banana"),
            listed(&found, "Cherry"),
            listed(&found, "Stuck"),
        );
        let gate = Gate::new("boot-1".to_string(), Arc::new(SystemClock::new()));

        let selecting = plan(&ax, banana, Verb::Select).expect("a route");
        assert!(!selecting.already());
        semantic::send(&ax, banana, &selecting, WRITE_MS).expect("sent");
        let selected = semantic::judge(&gate, &ax, banana, &selecting);
        assert!(plan(&ax, cherry, Verb::Select).expect("a route").already());
        let unmoved = plan(&ax, stuck, Verb::Select).expect("a route");
        semantic::send(&ax, stuck, &unmoved, WRITE_MS).expect("sent");
        let unseen = semantic::judge(&gate, &ax, stuck, &unmoved);

        assert_eq!(selected.effect, Effect::Changed);
        assert_eq!(unseen.effect, Effect::NotObserved);
        assert_eq!(fake.sent("DoAction"), vec!["/w/banana 0", "/w/stuck 0"]);
        assert!(fake.sent("SelectChild").is_empty());
        assert_eq!(
            plan(&ax, cherry, Verb::Toggle(false)).err().as_deref(),
            Some("ax_action_unsupported")
        );
    }

    // A Qt menu title opens with `ShowMenu`, which is its press: it holds its menu.
    #[test]
    fn a_qt_menu_title_is_pressed_by_show_menu() {
        let (fake, ax) = reader(qt_window(
            &["/w/bar"],
            vec![
                (
                    "/w/bar",
                    Node::of(34).at(0, 0, 300, 24).holding(APP, &["/w/actions"]),
                ),
                (
                    "/w/actions",
                    Node::of(35)
                        .named("Actions")
                        .at(0, 0, 60, 24)
                        .acting(&[Ok("ShowMenu")])
                        .holding(APP, &["/w/menu"]),
                ),
                ("/w/menu", Node::of(41)),
            ],
        ));
        let found = walk(&ax);
        let actions = listed(&found, "Actions");

        ax.perform(actions, PRESS, WRITE_MS).expect("opened");

        assert_eq!(fake.sent("DoAction"), vec!["/w/actions 0"]);
    }

    // A GTK 3 expander's one action is `activate`. Expand sends it only while
    // the expander reads collapsed, and reads EXPANDED back.
    #[test]
    fn expand_sends_the_one_action_only_while_collapsed() {
        let expander = Node {
            flips: Some(EXPANDED),
            ..Node::of(62)
                .named("Details")
                .at(10, 10, 80, 24)
                .reading(&[EXPANDABLE])
                .acting(&[Ok("activate")])
        };
        let (fake, ax) = reader(fake::one_window(
            &["/w/details"],
            vec![("/w/details", expander)],
        ));
        let found = walk(&ax);
        let details = listed(&found, "Details");

        assert!(plan(&ax, details, Verb::Collapse)
            .expect("a route")
            .already());
        let open = plan(&ax, details, Verb::Expand).expect("a route");
        assert!(matches!(open.op, Op::Expand(_, true)));
        semantic::send(&ax, details, &open, WRITE_MS).expect("expanded");

        assert_eq!(fake.sent("DoAction"), vec!["/w/details 0"]);
        assert_eq!(
            ax.states(details, false).expect("reads").expanded,
            Some(true)
        );
        assert!(plan(&ax, details, Verb::Expand).expect("a route").already());
    }

    /// The window `/w` reading ACTIVE or not, with a focusable field in it.
    fn focus_desktop(active: bool) -> Desktop {
        let mut desktop = fake::one_window(&["/w/name"], vec![("/w/name", field("Name", ""))]);
        if !active {
            desktop
                .nodes
                .get_mut(&(APP.to_string(), "/w".to_string()))
                .expect("the window")
                .states
                .retain(|bit| *bit != ACTIVE);
        }
        desktop
    }

    // GrabFocus can activate an inactive window. focus is refused unless the
    // control's frame reads ACTIVE, and nothing is sent.
    #[test]
    fn focus_never_activates_a_window() {
        let (fake, ax) = reader(focus_desktop(false));
        let found = walk(&ax);

        let refused = plan(&ax, listed(&found, "Name"), Verb::Focus);

        assert_eq!(refused.err().as_deref(), Some("focus_refused"));
        assert!(fake.sent("GrabFocus").is_empty());
    }

    // Another application's popup holds the grab, so the target frame does
    // not read ACTIVE — whatever the window manager says is active. focus is
    // refused and nothing is sent.
    #[test]
    fn focus_is_refused_while_another_application_holds_the_grab() {
        let mut desktop = focus_desktop(false);
        desktop.pids.insert(OTHER.to_string(), OTHER_PID);
        desktop.put(
            OTHER,
            fake::ROOT,
            Node::of(75)
                .named("menu-holder")
                .holding(OTHER, &["/popup"]),
        );
        desktop.put(OTHER, "/popup", Node::of(69).named("").reading(&[ACTIVE]));
        desktop
            .nodes
            .get_mut(&(fake::REGISTRY.to_string(), fake::ROOT.to_string()))
            .expect("the registry")
            .children
            .push((OTHER.to_string(), fake::ROOT.to_string()));
        let (fake, ax) = reader(desktop);
        let found = walk(&ax);

        let refused = plan(&ax, listed(&found, "Name"), Verb::Focus);

        assert_eq!(refused.err().as_deref(), Some("focus_refused"));
        assert!(fake.sent("GrabFocus").is_empty());
    }

    // Inside the active window, focus is GrabFocus, and FOCUSED reads back.
    #[test]
    fn focus_grabs_the_focus_inside_the_active_window() {
        let (fake, ax) = reader(focus_desktop(true));
        let found = walk(&ax);
        let name = listed(&found, "Name");

        let planned = plan(&ax, name, Verb::Focus).expect("a route");
        semantic::send(&ax, name, &planned, WRITE_MS).expect("focused");

        assert_eq!(fake.sent("GrabFocus"), vec!["/w/name "]);
        assert!(fake.node(APP, "/w/name").states.contains(&FOCUSED));
    }

    // scroll_to: Chromium scrolls the node itself into view.
    #[test]
    fn scroll_to_asks_chromium_to_scroll_the_node_into_view() {
        let mut desktop = fake::one_window(
            &["/w/far"],
            vec![(
                "/w/far",
                Node {
                    scrolls_to: Some((10, 300, 80, 24)),
                    ..Node::of(43)
                        .named("Far")
                        .at(10, 900, 80, 24)
                        .acting(&[Ok("press")])
                },
            )],
        );
        desktop.put(
            APP,
            fake::ROOT,
            Node::of(75)
                .named("chromium")
                .holding(APP, &["/w"])
                .made_with("Chromium", "120.0"),
        );
        let (fake, ax) = reader(desktop);
        let found = walk(&ax);

        ax.perform(listed(&found, "Far"), SCROLL_TO_VISIBLE, WRITE_MS)
            .expect("scrolled");

        assert_eq!(fake.sent("ScrollTo"), vec!["/w/far 6"], "ANYWHERE");
    }

    /// A vertical scroll bar at `value` in `range`.
    fn scroll_bar(value: f64, range: (f64, f64)) -> Node {
        Node {
            value,
            range,
            ..Node::of(48)
                .at(0, 0, 0, 0)
                .reading(&[VERTICAL])
                .listing(&["Value"])
        }
    }

    /// A GTK 4 window whose scroll pane, from 100 to 200 in the window, holds a
    /// row inside it and one far below, beside this scroll bar.
    fn scrolling(bar: Node) -> Desktop {
        let mut desktop = fake::one_window(
            &["/w/pane"],
            vec![
                (
                    "/w/pane",
                    Node {
                        window_extents: Some((0, 100, 200, 100)),
                        ..Node::of(49)
                            .at(0, 0, 0, 0)
                            .holding(APP, &["/w/view", "/w/bar"])
                    },
                ),
                ("/w/view", Node::of(68).holding(APP, &["/w/near", "/w/far"])),
                (
                    "/w/near",
                    Node {
                        window_extents: Some((0, 120, 80, 20)),
                        ..Node::of(32).named("Near").at(0, 0, 0, 0)
                    },
                ),
                (
                    "/w/far",
                    Node {
                        window_extents: Some((0, 1000, 80, 20)),
                        ..Node::of(32).named("Far").at(0, 0, 0, 0)
                    },
                ),
                ("/w/bar", bar),
            ],
        );
        desktop.put(
            APP,
            fake::ROOT,
            Node::of(75)
                .named("gtk4")
                .holding(APP, &["/w"])
                .made_with("GTK", "4.14.2"),
        );
        desktop
    }

    // scroll_to on GTK moves the vertical scroll bar of the scroll pane the node
    // sits in, by how far the node lies outside the pane, in window coordinates.
    #[test]
    fn scroll_to_moves_the_gtk_scroll_bar_by_how_far_the_node_lies_outside() {
        let (fake, ax) = reader(scrolling(scroll_bar(0.0, (0.0, 3900.0))));
        let found = walk(&ax);

        ax.perform(listed(&found, "Far"), SCROLL_TO_VISIBLE, WRITE_MS)
            .expect("scrolled");

        // The pane ends at 200; the row ends at 1020.
        assert_eq!(fake.node(APP, "/w/bar").value, 820.0);
        ax.perform(listed(&found, "Near"), SCROLL_TO_VISIBLE, WRITE_MS)
            .expect("already in view");
        assert_eq!(fake.sent("Set").len(), 1, "a row in view moves nothing");
    }

    // An application publishes whatever range it likes. A scroll bar whose
    // minimum is above its maximum, or any of whose numbers is not finite, gives
    // no distance to move: scroll_to could not read the state it needs, and
    // sends nothing.
    #[test]
    fn scroll_to_sends_nothing_to_a_scroll_bar_whose_range_is_no_range() {
        let bars = [
            (0.0, (3900.0, 0.0)),
            (0.0, (f64::NAN, 3900.0)),
            (0.0, (0.0, f64::NAN)),
            (f64::NAN, (0.0, 3900.0)),
            (0.0, (0.0, f64::INFINITY)),
            (f64::NEG_INFINITY, (0.0, 3900.0)),
        ];
        for (value, range) in bars {
            let (fake, ax) = reader(scrolling(scroll_bar(value, range)));
            let found = walk(&ax);

            let refused = ax
                .perform(listed(&found, "Far"), SCROLL_TO_VISIBLE, WRITE_MS)
                .expect_err("no range to move in");

            let case = format!("{value} in {range:?}");
            assert_eq!(refused.code(), Some("state_unreadable"), "{case}");
            assert!(!refused.sent(), "{case}");
            assert!(fake.sent("Set").is_empty(), "{case}");
        }
    }

    // The reads a verb makes before it writes are charged to the verb's own time,
    // never a read's timeout each: a scroll pane chain or a field that does not
    // answer ends the verb within its time, as unreadable, with nothing sent. A
    // write sent after its time had gone would only be given up on, and say it may
    // have landed.
    #[test]
    fn the_reads_before_a_write_end_within_the_verbs_time_and_send_nothing() {
        const BUDGET_MS: u64 = 40;
        // Well under a read's own timeout here (the connection's, `TIMEOUT`), so a
        // read held to that fails this.
        let within = Duration::from_millis(150);
        let (fake, ax) = reader(scrolling(scroll_bar(0.0, (0.0, 3900.0))));
        let found = walk(&ax);
        fake.change(APP, "/w/view", |view| view.stalls = vec!["GetRole"]);
        let (fake_too, ax_too) = reader(fake::one_window(
            &["/w/code"],
            vec![("/w/code", field("Code", "abef"))],
        ));
        let found_too = walk(&ax_too);
        fake_too.change(APP, "/w/code", |code| code.stalls = vec!["GetNSelections"]);

        let started = Instant::now();
        let scrolled = ax
            .perform(listed(&found, "Far"), SCROLL_TO_VISIBLE, BUDGET_MS)
            .expect_err("its scroll pane did not answer");
        let scrolling_took = started.elapsed();
        let started = Instant::now();
        let typed = ax_too
            .write(
                listed(&found_too, "Code"),
                Write::SelectedText("cd"),
                BUDGET_MS,
            )
            .expect_err("its selection did not answer");
        let typing_took = started.elapsed();

        assert!(scrolling_took < within, "scroll_to took {scrolling_took:?}");
        assert!(typing_took < within, "type took {typing_took:?}");
        for refused in [&scrolled, &typed] {
            assert!(!refused.sent(), "{}", refused.detail());
            assert_eq!(
                refused.code(),
                Some("state_unreadable"),
                "{}",
                refused.detail()
            );
        }
        assert!(fake.sent("Set").is_empty());
        assert!(fake_too.sent("InsertText").is_empty());
    }

    // A read a verb needs before it writes, a scroll bar's range or a field's
    // selection, that gets no answer in time sent nothing that changes anything:
    // the verb could not read what it acts from, and says nothing went out.
    #[test]
    fn a_read_before_the_write_that_gets_no_answer_sent_nothing() {
        let (fake, ax) = reader(scrolling(scroll_bar(0.0, (0.0, 3900.0))));
        let found = walk(&ax);
        let far = listed(&found, "Far");
        fake.change(APP, "/w/bar", |bar| bar.stalls = vec!["CurrentValue"]);

        let scrolled = ax
            .perform(far, SCROLL_TO_VISIBLE, 100)
            .expect_err("no range");

        let (fake_too, ax_too) = reader(fake::one_window(
            &["/w/code"],
            vec![("/w/code", field("Code", "abef"))],
        ));
        let found_too = walk(&ax_too);
        let code = listed(&found_too, "Code");
        fake_too.change(APP, "/w/code", |code| code.stalls = vec!["GetNSelections"]);

        let typed = ax_too
            .write(code, Write::SelectedText("cd"), 100)
            .expect_err("no selection");

        for refused in [&scrolled, &typed] {
            assert!(!refused.sent(), "{}", refused.detail());
            assert_eq!(
                refused.code(),
                Some("state_unreadable"),
                "{}",
                refused.detail()
            );
        }
        assert!(fake.sent("Set").is_empty());
        assert!(fake_too.sent("InsertText").is_empty());
        assert_eq!(fake_too.node(APP, "/w/code").text, "abef");
    }

    // A write to an application that quit never went out; one that got no answer
    // in time did, and says so.
    #[test]
    fn a_write_to_a_gone_application_was_not_sent_and_one_that_timed_out_was() {
        let (fake, ax) = reader(fake::one_window(
            &["/w/a", "/w/b"],
            vec![
                (
                    "/w/a",
                    Node::of(43)
                        .named("A")
                        .at(10, 10, 80, 24)
                        .acting(&[Ok("click")]),
                ),
                (
                    "/w/b",
                    Node::of(43)
                        .named("B")
                        .at(10, 40, 80, 24)
                        .acting(&[Ok("click")]),
                ),
            ],
        ));
        let found = walk(&ax);
        let (a, b) = (listed(&found, "A"), listed(&found, "B"));

        fake.change(APP, "/w/b", |b| b.life = Life::Silent);
        let late = ax.perform(b, PRESS, 100).expect_err("no answer");
        fake.quits(APP);
        let gone = ax.perform(a, PRESS, WRITE_MS).expect_err("gone");

        assert!(late.sent(), "{}", late.detail());
        assert!(!gone.sent(), "{}", gone.detail());
    }

    // A toolkit that took the call and did nothing answers false: refused, and not
    // read as a press that may have landed.
    #[test]
    fn a_call_answered_false_is_a_refusal() {
        let declining = Node {
            declines: vec!["DoAction"],
            ..Node::of(43)
                .named("Stuck")
                .at(10, 10, 80, 24)
                .acting(&[Ok("click")])
        };
        let (_fake, ax) = reader(fake::one_window(&["/w/s"], vec![("/w/s", declining)]));
        let found = walk(&ax);

        let refused = ax
            .perform(listed(&found, "Stuck"), PRESS, WRITE_MS)
            .expect_err("declined");

        assert!(!refused.sent());
    }
}
