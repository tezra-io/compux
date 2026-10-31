//! Finding a reference again before an action.
//!
//! A reference is the object a walk read, at the place the walk reached it: its
//! bus name and object path, and its child-index path from the application root.
//! Neither alone proves the control is still the one the caller was shown. A
//! destroyed GTK 3 or Chromium object keeps answering for about 16 s (L17-6, L17-8),
//! so an object that answers is not proof it is still in the tree; and a list that
//! changed puts another object at the same place. So both are read again:
//!
//! - the object at the place, reached through each parent's child list;
//! - the object itself, present only where its own parent still lists it.
//!
//! The object is at its place too when it says so: its parent is the node at its
//! place's parent, and its index there is the one its place ends in. Qt answers
//! every `GetChildAtIndex` of a table with a new object for the same cell, so the
//! object at a place is never the one a walk read there.
//!
//! Each is a candidate when it answers the role the walk read. One candidate is the
//! control. Two — it moved, and another of its role took its place — is
//! `ambiguous_element`. None is `stale_element`. A candidate that does not answer
//! in time is asked again, at most twice, then `identity_unknown`.

use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

use super::atspi::{Address, Failed, Real, ACCESSIBLE};
use crate::ax::{Facts, Handle, Identity, Unread};

/// How many times a candidate that does not answer in time is read: once, and
/// again at most twice. Then its identity is unknown.
const READS: usize = 3;

/// What one look for a reference found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Look {
    One,
    Two,
    None,
    /// A candidate did not answer in time.
    Unreadable,
    /// The application quit.
    Quit,
}

impl Real {
    /// The reference found again, and what it answers now; or why it was not.
    /// A node whose place was never known is read again as it is.
    pub(super) fn found_again(&self, handle: Handle) -> Result<Facts, Identity> {
        let place = self.place(handle).and_then(|place| place.path);
        let (Some(at), Some(path), Some(known)) = (self.address(handle), place, self.known(handle))
        else {
            return self.reread(handle).map_err(Identity::Unread);
        };
        let bus = self.bus().ok_or(Identity::Unread(Unread::NoAnswer))?;
        for _ in 0..READS {
            match async_io::block_on(self.look(&bus, &at, &path, known.role)) {
                Look::One => return self.reread(handle).map_err(Identity::Unread),
                Look::Two => return Err(Identity::Ambiguous),
                Look::None | Look::Quit => return Err(Identity::Unread(Unread::Gone)),
                Look::Unreadable => {}
            }
        }
        Err(Identity::Unknown)
    }

    /// One look: the object at the reference's place, and the object itself
    /// wherever its parent lists it, each a candidate when it answers `role`.
    async fn look(&self, bus: &Connection, at: &Address, path: &[usize], role: u32) -> Look {
        let placed = match self.at_path(bus, &at.root(), path).await {
            Ok(placed) => placed,
            Err(Failed::Quit) => return Look::Quit,
            Err(Failed::Late | Failed::Lost) => return Look::Unreadable,
            Err(Failed::Suspect | Failed::Other) => None,
        };
        let here = if placed.as_ref() == Some(at) {
            Ok(true)
        } else {
            self.says_it_is_at(bus, at, path).await
        };
        match here {
            Ok(true) => return self.answers(bus, at, role).await,
            Err(Failed::Quit) => return Look::Quit,
            Err(Failed::Late | Failed::Lost) => return Look::Unreadable,
            // It does not say where it is, so its place does not prove it.
            Ok(false) | Err(Failed::Suspect | Failed::Other) => {}
        }
        let there = match &placed {
            Some(other) => self.role_of(bus, other).await.map(|now| now == role),
            None => Ok(false),
        };
        let moved = self.listed_elsewhere(bus, at, role).await;
        verdict(there, moved)
    }

    /// The node at a child-index path below `root`; `None` where a slot on the way
    /// holds nothing.
    async fn at_path(
        &self,
        bus: &Connection,
        root: &Address,
        path: &[usize],
    ) -> Result<Option<Address>, Failed> {
        let mut node = root.clone();
        for index in path {
            match self.child_at(bus, &node, *index).await? {
                Some(child) => node = child,
                None => return Ok(None),
            }
        }
        Ok(Some(node))
    }

    /// The one candidate, at its place: it is the control when it answers `role`.
    async fn answers(&self, bus: &Connection, at: &Address, role: u32) -> Look {
        match self.role_of(bus, at).await {
            Ok(now) if now == role => Look::One,
            other => verdict(other.map(|_| false), Ok(false)),
        }
    }

    /// Whether the object says it is the child its place names: its parent is the
    /// node at its place's parent, at the index its place ends in.
    async fn says_it_is_at(
        &self,
        bus: &Connection,
        at: &Address,
        path: &[usize],
    ) -> Result<bool, Failed> {
        let Some((last, above)) = path.split_last() else {
            return Ok(false);
        };
        let Some(parent_there) = self.at_path(bus, &at.root(), above).await? else {
            return Ok(false);
        };
        let parent: (String, OwnedObjectPath) =
            self.property(bus, at, ACCESSIBLE, "Parent").await?;
        let index: i32 = self.accessible(bus, at, "GetIndexInParent").await?;
        let under = Address::of_child(parent).as_ref() == Some(&parent_there);
        Ok(under && usize::try_from(index).ok() == Some(*last))
    }

    async fn role_of(&self, bus: &Connection, at: &Address) -> Result<u32, Failed> {
        self.accessible(bus, at, "GetRole").await
    }

    /// Whether the object answers `role` and its own parent still lists it, at the
    /// index it says it has: present, wherever it went.
    async fn listed_elsewhere(
        &self,
        bus: &Connection,
        at: &Address,
        role: u32,
    ) -> Result<bool, Failed> {
        if self.role_of(bus, at).await? != role {
            return Ok(false);
        }
        let parent: Result<(String, OwnedObjectPath), Failed> =
            self.property(bus, at, ACCESSIBLE, "Parent").await;
        let Some(parent) = parent.ok().and_then(Address::of_child) else {
            return Ok(false);
        };
        let index: i32 = self.accessible(bus, at, "GetIndexInParent").await?;
        let Ok(index) = usize::try_from(index) else {
            return Ok(false);
        };
        Ok(self.child_at(bus, &parent, index).await?.as_ref() == Some(at))
    }
}

/// What two candidates' answers come to: each is a match, not one, or unread.
fn verdict(there: Result<bool, Failed>, moved: Result<bool, Failed>) -> Look {
    let unread = |answer: &Result<bool, Failed>| match answer {
        Err(Failed::Quit) => Some(Look::Quit),
        Err(Failed::Late | Failed::Lost) => Some(Look::Unreadable),
        _ => None,
    };
    if let Some(look) = unread(&there).or_else(|| unread(&moved)) {
        return look;
    }
    match (there == Ok(true), moved == Ok(true)) {
        (true, true) => Look::Two,
        // The object itself, where it went. Another object at its place is not it.
        (false, true) => Look::One,
        _ => Look::None,
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::time::Duration;

    use crate::ax::{Ax, Handle, Identity, Unread};
    use crate::gate::SystemClock;
    use crate::linux::atspi::Real;
    use crate::linux::fake::{self, Desktop, Fake, Life, Node, APP, PID};
    use crate::tree::{self, Walk};

    const TIMEOUT: Duration = Duration::from_millis(150);

    fn item(name: &str) -> Node {
        Node::of(43)
            .named(name)
            .at(10, 10, 80, 24)
            .acting(&[Ok("click")])
    }

    /// A window holding a list `/w/list` of these buttons, by path.
    fn listing(paths: &[&str]) -> Desktop {
        let mut nodes = vec![("/w/list", Node::of(39).named("List").holding(APP, paths))];
        nodes.extend(paths.iter().map(|path| (*path, item(path))));
        fake::one_window(&["/w/list"], nodes)
    }

    fn reader(desktop: Desktop) -> (Fake, Rc<dyn Ax>) {
        let (fake, bus) = Fake::start(desktop, TIMEOUT);
        let ax: Rc<dyn Ax> = Rc::new(Real::over(bus));
        (fake, ax)
    }

    fn walk(ax: &Rc<dyn Ax>) -> Walk {
        tree::walk(ax, PID, &SystemClock::new(), u64::MAX)
    }

    fn handle_of(found: &Walk, label: &str) -> Handle {
        found
            .nodes
            .iter()
            .find(|node| node.label.as_deref() == Some(label))
            .unwrap_or_else(|| panic!("{label} is listed"))
            .element
            .handle()
    }

    /// The list's children from now on.
    fn relist(fake: &Fake, paths: &[&str]) {
        fake.change(APP, "/w/list", |list| {
            list.children = paths
                .iter()
                .map(|path| (APP.to_string(), path.to_string()))
                .collect();
        });
    }

    // The object still at its place answers what it is now.
    #[test]
    fn a_reference_at_its_place_answers_its_facts() {
        let (fake, ax) = reader(listing(&["/w/a", "/w/b"]));
        let found = walk(&ax);
        let b = handle_of(&found, "/w/b");
        fake.change(APP, "/w/b", |b| b.name = "renamed".to_string());

        let facts = ax.identify(b).expect("found again");

        assert_eq!(facts.label.as_deref(), Some("renamed"));
    }

    // The list changed: the object moved, and another of its role took its place.
    // Both are candidates, so neither is acted on.
    #[test]
    fn a_reference_that_moved_while_another_took_its_place_is_ambiguous() {
        let mut desktop = listing(&["/w/a", "/w/b"]);
        desktop.put(APP, "/w/new", item("new"));
        let (fake, ax) = reader(desktop);
        let found = walk(&ax);
        let a = handle_of(&found, "/w/a");

        relist(&fake, &["/w/new", "/w/a", "/w/b"]);

        assert_eq!(ax.identify(a).err(), Some(Identity::Ambiguous));
    }

    // It moved and nothing of its role took its place: it is the one candidate.
    #[test]
    fn a_reference_that_moved_alone_is_found_where_it_went() {
        let mut desktop = listing(&["/w/a", "/w/b"]);
        desktop.put(APP, "/w/label", Node::of(29).named("caption"));
        let (fake, ax) = reader(desktop);
        let found = walk(&ax);
        let b = handle_of(&found, "/w/b");

        relist(&fake, &["/w/a", "/w/label", "/w/b"]);

        let facts = ax.identify(b).expect("found where it went");
        assert_eq!(facts.label.as_deref(), Some("/w/b"));
    }

    // L17-6: a destroyed GTK 3 object keeps answering, but its parent no longer
    // lists it. Another object at its place is not it.
    #[test]
    fn a_reference_its_parent_no_longer_lists_is_stale() {
        let mut desktop = listing(&["/w/a", "/w/b"]);
        desktop.put(APP, "/w/other", item("other"));
        let (fake, ax) = reader(desktop);
        let found = walk(&ax);
        let a = handle_of(&found, "/w/a");

        relist(&fake, &["/w/other", "/w/b"]);
        assert_eq!(ax.identify(a).err(), Some(Identity::Unread(Unread::Gone)));

        relist(&fake, &["/w/b"]);
        let b = handle_of(&walk(&ax), "/w/b");
        relist(&fake, &[]);
        assert_eq!(ax.identify(b).err(), Some(Identity::Unread(Unread::Gone)));
    }

    // A reference whose role changed is not the control the caller was shown.
    #[test]
    fn a_reference_that_answers_another_role_is_stale() {
        let (fake, ax) = reader(listing(&["/w/a"]));
        let found = walk(&ax);
        let a = handle_of(&found, "/w/a");

        fake.change(APP, "/w/a", |a| a.role = 7);

        assert_eq!(ax.identify(a).err(), Some(Identity::Unread(Unread::Gone)));
    }

    // A candidate that does not answer in time is asked again at most twice,
    // then its identity is unknown. Three reads of its role, no more.
    #[test]
    fn an_unreadable_candidate_is_read_three_times_then_unknown() {
        let (fake, ax) = reader(listing(&["/w/a"]));
        let found = walk(&ax);
        let a = handle_of(&found, "/w/a");
        let before = fake.sent("GetRole").len();

        fake.change(APP, "/w/a", |a| a.life = Life::Silent);

        assert_eq!(ax.identify(a).err(), Some(Identity::Unknown));
        let roles: Vec<String> = fake.sent("GetRole")[before..]
            .iter()
            .filter(|call| call.starts_with("/w/a "))
            .cloned()
            .collect();
        assert_eq!(roles.len(), 3, "{roles:?}");
    }

    // Qt answers every `GetChildAtIndex` of a table with a new object for the same
    // cell, so the object at a reference's place is never the object the walk
    // read. The object's own account of where it sits — its parent, and its index
    // there — is what finds it, and nothing else took its place.
    #[test]
    fn a_cell_of_a_table_that_makes_a_new_object_per_ask_is_found_by_where_it_says_it_is() {
        let mut desktop = listing(&["/w/a", "/w/b"]);
        desktop.put(
            APP,
            "/w/list",
            Node {
                mints: true,
                ..Node::of(39).named("List").holding(APP, &["/w/a", "/w/b"])
            },
        );
        let (fake, ax) = reader(desktop);
        let found = walk(&ax);
        let b = handle_of(&found, "/w/b");
        fake.change(APP, "/w/b", |b| b.name = "renamed".to_string());

        let facts = ax.identify(b).expect("found where it says it is");

        assert_eq!(facts.label.as_deref(), Some("renamed"));
    }

    // ServiceUnknown means the application is gone.
    #[test]
    fn a_reference_into_an_application_that_quit_is_gone() {
        let (fake, ax) = reader(listing(&["/w/a"]));
        let found = walk(&ax);
        let a = handle_of(&found, "/w/a");

        fake.quits(APP);

        assert_eq!(ax.identify(a).err(), Some(Identity::Unread(Unread::Gone)));
    }
}
