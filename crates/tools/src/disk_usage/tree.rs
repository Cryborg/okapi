//! The tree of the directories an analysis met, and what they hold: each
//! node keeps the total of its whole subtree, kept up to date as directories
//! are read, so that a view of a directory costs only its children.

use std::ffi::OsString;
use std::io;

use okapi_platform::FileIdentity;

/// A directory of the tree, by its place in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(u32);

impl NodeId {
    /// The directory the analysis started from.
    pub const ROOT: Self = Self(0);

    fn index(self) -> usize {
        self.0 as usize
    }
}

/// What something takes, files counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Size {
    /// On disk: less than `apparent` for a sparse file, more for a small one
    /// (a whole block).
    pub allocated: u64,
    /// As its length says.
    pub apparent: u64,
    pub files: u64,
}

impl Size {
    pub(crate) const fn add(&mut self, other: Self) {
        self.allocated = self.allocated.saturating_add(other.allocated);
        self.apparent = self.apparent.saturating_add(other.apparent);
        self.files = self.files.saturating_add(other.files);
    }
}

/// Where a directory stands in the analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    /// Not read yet, or never: the analysis was stopped before it.
    Pending,
    Read,
    /// It could not be opened or listed.
    Unreadable(io::ErrorKind),
    /// It was no longer the directory listed when it was opened (replaced,
    /// or mounted over).
    Changed,
    /// Removed between its listing and its opening (`NotFound`; removed
    /// once opened, it reads as an empty folder): it holds
    /// nothing any more, and that is no doubt (a cache always loses some).
    /// One replaced by a file or a link is [`Self::Changed`].
    Gone,
    /// The root of another mount: never entered, it adds nothing.
    OtherMount,
}

impl NodeState {
    /// Whether what it holds is unknown, so that the sizes above it are
    /// partial.
    const fn is_doubt(self) -> bool {
        matches!(self, Self::Pending | Self::Unreadable(_) | Self::Changed)
    }
}

/// One directory of the tree.
#[derive(Debug)]
struct Node {
    name: OsString,
    /// What it was when listed: a directory opened again must still be it.
    /// `None` for a mount never read.
    identity: Option<FileIdentity>,
    parent: Option<NodeId>,
    state: NodeState,
    /// Some entry of it could not be read: its size misses that one.
    incomplete: bool,
    /// What its whole subtree takes, as far as read.
    total: Size,
    /// How many nodes of its subtree, itself included, are a doubt or
    /// incomplete: its total is partial while any is.
    doubts: u32,
    /// Its subdirectories, made in one batch: `first .. first + count`.
    first_child: u32,
    children: u32,
}

/// A directory as shown: its name, what it holds, and whether that is all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: NodeId,
    pub name: OsString,
    pub state: NodeState,
    pub total: Size,
    /// Some of what it holds could not be read, or not yet.
    pub partial: bool,
}

/// A directory and its subdirectories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub directory: Entry,
    pub children: Vec<Entry>,
}

/// A subdirectory met while reading a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Child {
    pub(crate) name: OsString,
    pub(crate) identity: Option<FileIdentity>,
    /// The root of another mount, never entered.
    pub(crate) other_mount: bool,
}

/// The directories met, the root first; a child always comes after its
/// parent.
#[derive(Debug)]
pub(crate) struct Tree {
    nodes: Vec<Node>,
}

impl Tree {
    /// A tree of the root alone, not read yet.
    pub(crate) fn new(root_name: OsString, root_identity: FileIdentity) -> Self {
        Self {
            nodes: vec![Node {
                name: root_name,
                identity: Some(root_identity),
                parent: None,
                state: NodeState::Pending,
                incomplete: false,
                total: Size::default(),
                doubts: 1,
                first_child: 0,
                children: 0,
            }],
        }
    }

    fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.index())
    }

    /// The node `id` and each of its ancestors, up to the root.
    fn lineage(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        std::iter::successors(Some(id), |id| self.node(*id).and_then(|node| node.parent))
    }

    /// Adds `change` to the doubts of `id` and of each ancestor. A count
    /// that would go below zero is a fault: said, and left at one, so that
    /// the sizes above it stay partial rather than seem whole.
    fn add_doubts(&mut self, id: NodeId, change: i64) {
        let lineage: Vec<NodeId> = self.lineage(id).collect();
        for ancestor in lineage {
            let node = &mut self.nodes[ancestor.index()];
            node.doubts = u32::try_from(i64::from(node.doubts) + change).unwrap_or_else(|_| {
                tracing::error!(?ancestor, change, "doubts counted wrong: kept partial");
                1
            });
        }
    }

    /// Records the directory `id` read: what it holds itself (`own`: its
    /// files and its own blocks), whether an entry of it could not be read,
    /// and its subdirectories, which wait to be read. What it holds is added
    /// to it and to each ancestor. The new subdirectories, in the order
    /// given; none when `id` was read already.
    pub(crate) fn record(
        &mut self,
        id: NodeId,
        own: Size,
        incomplete: bool,
        children: Vec<Child>,
    ) -> Vec<NodeId> {
        if self
            .node(id)
            .is_none_or(|node| node.state != NodeState::Pending)
        {
            tracing::error!(?id, "a directory recorded twice: kept as first read");
            return Vec::new();
        }
        let first = self.nodes.len();
        let (Ok(first_child), Ok(count), Ok(_)) = (
            u32::try_from(first),
            u32::try_from(children.len()),
            u32::try_from(first + children.len()),
        ) else {
            // Four billion directories: this one stays pending, a doubt.
            tracing::error!(?id, "too many directories: one left unread");
            return Vec::new();
        };
        let pending = children.iter().filter(|child| !child.other_mount).count();
        self.nodes.extend(children.into_iter().map(|child| Node {
            name: child.name,
            identity: child.identity,
            parent: Some(id),
            state: if child.other_mount {
                NodeState::OtherMount
            } else {
                NodeState::Pending
            },
            incomplete: false,
            total: Size::default(),
            doubts: u32::from(!child.other_mount),
            first_child: 0,
            children: 0,
        }));
        let node = &mut self.nodes[id.index()];
        node.state = NodeState::Read;
        node.incomplete = incomplete;
        node.first_child = first_child;
        node.children = count;
        let lineage: Vec<NodeId> = self.lineage(id).collect();
        for ancestor in lineage {
            self.nodes[ancestor.index()].total.add(own);
        }
        // Its own doubt gives way to its pending subdirectories, and stays
        // when it is incomplete.
        let pending = i64::try_from(pending).unwrap_or(i64::MAX);
        self.add_doubts(id, pending - i64::from(!incomplete));
        (first_child..first_child + count).map(NodeId).collect()
    }

    /// Records the directory `id` as not read: `state` says why. Its doubt
    /// stays, unless it is no doubt (gone).
    pub(crate) fn fail(&mut self, id: NodeId, state: NodeState) {
        let Some(node) = self.nodes.get_mut(id.index()) else {
            return;
        };
        if node.state != NodeState::Pending || matches!(state, NodeState::Pending | NodeState::Read)
        {
            return;
        }
        node.state = state;
        if !state.is_doubt() {
            self.add_doubts(id, -1);
        }
    }

    fn entry(&self, id: NodeId) -> Option<Entry> {
        let node = self.node(id)?;
        Some(Entry {
            id,
            name: node.name.clone(),
            state: node.state,
            total: node.total,
            partial: node.doubts > 0,
        })
    }

    /// The directory `id` and its subdirectories.
    pub(crate) fn view(&self, id: NodeId) -> Option<View> {
        let node = self.node(id)?;
        let first = node.first_child;
        let children = (first..first.saturating_add(node.children))
            .filter_map(|index| self.entry(NodeId(index)))
            .collect();
        Some(View {
            directory: self.entry(id)?,
            children,
        })
    }

    /// The names and identities from the root (excluded) down to `id`, when
    /// `id` was read: the way to open it again.
    pub(crate) fn read_lineage(&self, id: NodeId) -> Option<Vec<(OsString, FileIdentity)>> {
        if self.node(id)?.state != NodeState::Read {
            return None;
        }
        let mut lineage = self
            .lineage(id)
            .filter(|id| *id != NodeId::ROOT)
            .map(|id| {
                let node = self.node(id)?;
                Some((node.name.clone(), node.identity?))
            })
            .collect::<Option<Vec<_>>>()?;
        lineage.reverse();
        Some(lineage)
    }

    /// The names of the subdirectories of `id`.
    pub(crate) fn child_names(&self, id: NodeId) -> Vec<OsString> {
        let Some(node) = self.node(id) else {
            return Vec::new();
        };
        let first = node.first_child;
        (first..first.saturating_add(node.children))
            .filter_map(|index| self.node(NodeId(index)))
            .map(|child| child.name.clone())
            .collect()
    }

    /// The names from the root (excluded) down to `id`.
    pub(crate) fn names(&self, id: NodeId) -> Option<Vec<OsString>> {
        self.node(id)?;
        let mut names: Vec<OsString> = self
            .lineage(id)
            .filter(|id| *id != NodeId::ROOT)
            .filter_map(|id| self.node(id).map(|node| node.name.clone()))
            .collect();
        names.reverse();
        Some(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(allocated: u64, files: u64) -> Size {
        Size {
            allocated,
            apparent: allocated / 2,
            files,
        }
    }

    fn directory(name: &str) -> Child {
        Child {
            name: name.into(),
            identity: Some(FileIdentity::new(1, u64::from(name.as_bytes()[0]))),
            other_mount: false,
        }
    }

    fn tree() -> Tree {
        Tree::new("root".into(), FileIdentity::new(1, 1))
    }

    fn total(tree: &Tree, id: NodeId) -> (u64, u64, bool) {
        let entry = tree.entry(id).unwrap();
        (entry.total.allocated, entry.total.files, entry.partial)
    }

    #[test]
    fn what_a_directory_holds_rises_to_the_root_and_its_doubts_settle() {
        let mut tree = tree();
        assert_eq!(total(&tree, NodeId::ROOT), (0, 0, true), "not read yet");
        let [a, b] = tree
            .record(
                NodeId::ROOT,
                size(4, 1),
                false,
                vec![directory("a"), directory("b")],
            )
            .try_into()
            .unwrap();
        assert_eq!(total(&tree, NodeId::ROOT), (4, 1, true), "two left");
        let [c] = tree
            .record(a, size(30, 3), false, vec![directory("c")])
            .try_into()
            .unwrap();
        tree.record(b, size(200, 2), false, Vec::new());
        assert_eq!(total(&tree, NodeId::ROOT), (234, 6, true), "c left");
        assert_eq!(total(&tree, b), (200, 2, false));
        tree.record(c, size(1000, 7), false, Vec::new());
        assert_eq!(total(&tree, NodeId::ROOT), (1234, 13, false));
        assert_eq!(total(&tree, a), (1030, 10, false));
        assert_eq!(tree.names(c).unwrap(), ["a", "c"]);
        assert_eq!(tree.names(NodeId::ROOT).unwrap(), Vec::<OsString>::new());
        let view = tree.view(NodeId::ROOT).unwrap();
        let names: Vec<&OsString> = view.children.iter().map(|child| &child.name).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(
            tree.record(c, size(1, 1), false, Vec::new()),
            Vec::<NodeId>::new(),
            "read once"
        );
        assert_eq!(total(&tree, NodeId::ROOT), (1234, 13, false));
    }

    #[test]
    fn a_doubt_below_keeps_every_ancestor_partial() {
        let mut tree = tree();
        let [a, b] = tree
            .record(
                NodeId::ROOT,
                size(0, 0),
                false,
                vec![directory("a"), directory("b")],
            )
            .try_into()
            .unwrap();
        let [c] = tree
            .record(a, size(0, 0), true, vec![directory("c")])
            .try_into()
            .unwrap();
        tree.fail(b, NodeState::Unreadable(io::ErrorKind::PermissionDenied));
        tree.record(c, size(1, 1), false, Vec::new());
        assert_eq!(total(&tree, c), (1, 1, false));
        assert!(total(&tree, a).2, "an entry of a unread");
        assert!(total(&tree, b).2, "b unread");
        assert_eq!(tree.entry(NodeId::ROOT).unwrap().total.files, 1);
        assert!(total(&tree, NodeId::ROOT).2);
        assert_eq!(
            tree.entry(b).unwrap().state,
            NodeState::Unreadable(io::ErrorKind::PermissionDenied)
        );
    }

    #[test]
    fn another_mount_is_no_doubt_and_never_read() {
        let mut tree = tree();
        let [mounted] = tree
            .record(
                NodeId::ROOT,
                size(8, 1),
                false,
                vec![Child {
                    name: "mounted".into(),
                    identity: None,
                    other_mount: true,
                }],
            )
            .try_into()
            .unwrap();
        assert_eq!(total(&tree, NodeId::ROOT), (8, 1, false));
        assert_eq!(tree.entry(mounted).unwrap().state, NodeState::OtherMount);
        assert_eq!(tree.child_names(NodeId::ROOT), ["mounted"]);
        assert!(
            !tree.entry(mounted).unwrap().partial,
            "nothing of it is awaited"
        );
        assert_eq!(
            tree.record(mounted, size(9, 9), false, Vec::new()),
            Vec::<NodeId>::new()
        );
        tree.fail(mounted, NodeState::Changed);
        assert_eq!(tree.entry(mounted).unwrap().state, NodeState::OtherMount);
        assert_eq!(total(&tree, NodeId::ROOT), (8, 1, false));
    }

    #[test]
    fn a_folder_gone_before_its_reading_is_no_doubt() {
        let mut tree = tree();
        let [gone, kept] = tree
            .record(
                NodeId::ROOT,
                size(2, 1),
                false,
                vec![directory("gone"), directory("kept")],
            )
            .try_into()
            .unwrap();
        tree.fail(gone, NodeState::Gone);
        tree.record(kept, size(3, 1), false, Vec::new());
        assert_eq!(total(&tree, NodeId::ROOT), (5, 2, false));
        assert_eq!(tree.entry(gone).unwrap().state, NodeState::Gone);
        tree.fail(gone, NodeState::Changed);
        assert_eq!(tree.entry(gone).unwrap().state, NodeState::Gone, "once");
        assert_eq!(total(&tree, NodeId::ROOT), (5, 2, false));
    }

    #[test]
    fn only_a_folder_read_is_opened_again_by_its_lineage() {
        let mut tree = tree();
        let [a] = tree
            .record(NodeId::ROOT, size(0, 0), false, vec![directory("a")])
            .try_into()
            .unwrap();
        assert_eq!(tree.read_lineage(a), None, "not read yet");
        let [b] = tree
            .record(a, size(0, 0), false, vec![directory("b")])
            .try_into()
            .unwrap();
        tree.record(b, size(0, 0), false, Vec::new());
        let lineage = tree.read_lineage(b).unwrap();
        let names: Vec<&OsString> = lineage.iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(lineage[1].1, FileIdentity::new(1, u64::from(b'b')));
        assert_eq!(tree.read_lineage(NodeId::ROOT), Some(Vec::new()));
        assert_eq!(tree.child_names(a), ["b"]);
    }
}
