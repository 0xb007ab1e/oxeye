//! Structured-navigation primitives: classifying UI elements into navigation categories and
//! summarising a window/application's structure.
//!
//! This is the platform-agnostic **functional core** of "browse-mode" navigation — pure and
//! deterministic, unit-tested without any accessibility back-end. A platform crate reads roles
//! from the accessibility tree, calls [`classify`], and feeds the results to [`summarize`]
//! (and, later, to by-type next/previous movement built on the same categories).

/// A structural category a UI element can be navigated or summarised by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NavCategory {
    /// A section heading.
    Heading,
    /// A navigational landmark / region.
    Landmark,
    /// A hyperlink.
    Link,
    /// A push/toggle button.
    Button,
    /// An interactive form control (entry, checkbox, combo box, slider, …).
    FormField,
    /// A list.
    List,
    /// A table or grid.
    Table,
    /// An image or icon.
    Image,
}

impl NavCategory {
    /// Categories in the order a structure summary reports them.
    const ORDER: [NavCategory; 8] = [
        NavCategory::Heading,
        NavCategory::Landmark,
        NavCategory::Link,
        NavCategory::Button,
        NavCategory::FormField,
        NavCategory::List,
        NavCategory::Table,
        NavCategory::Image,
    ];

    /// The singular spoken label (e.g. "button").
    #[must_use]
    pub fn singular(self) -> &'static str {
        match self {
            NavCategory::Heading => "heading",
            NavCategory::Landmark => "landmark",
            NavCategory::Link => "link",
            NavCategory::Button => "button",
            NavCategory::FormField => "form field",
            NavCategory::List => "list",
            NavCategory::Table => "table",
            NavCategory::Image => "image",
        }
    }

    /// The plural spoken label (e.g. "buttons").
    #[must_use]
    pub fn plural(self) -> &'static str {
        match self {
            NavCategory::Heading => "headings",
            NavCategory::Landmark => "landmarks",
            NavCategory::Link => "links",
            NavCategory::Button => "buttons",
            NavCategory::FormField => "form fields",
            NavCategory::List => "lists",
            NavCategory::Table => "tables",
            NavCategory::Image => "images",
        }
    }
}

/// Classify an (AT-SPI) role name into a navigation category, if it is one oxeye surfaces.
/// Role names follow AT-SPI's `Role::name()` (e.g. `"push button"`, `"heading"`, `"link"`).
#[must_use]
pub fn classify(role: &str) -> Option<NavCategory> {
    use NavCategory::{Button, FormField, Heading, Image, Landmark, Link, List, Table};
    let category = match role {
        "heading" => Heading,
        "landmark" => Landmark,
        "link" => Link,
        "push button" | "toggle button" => Button,
        "entry" | "text" | "password text" | "spin button" | "combo box" | "check box"
        | "radio button" | "slider" => FormField,
        "list" | "list box" => List,
        "table" | "tree table" | "tree" => Table,
        "image" | "icon" => Image,
        _ => return None,
    };
    Some(category)
}

/// Summarise a window/application's structure from its elements' categories, as a spoken phrase
/// in a fixed order with pluralisation, e.g. `"3 headings, 12 buttons, 4 links"`.
///
/// Returns `None` when nothing notable is present.
#[must_use]
pub fn summarize<I>(categories: I) -> Option<String>
where
    I: IntoIterator<Item = Option<NavCategory>>,
{
    let present: Vec<NavCategory> = categories.into_iter().flatten().collect();
    let parts: Vec<String> = NavCategory::ORDER
        .iter()
        .filter_map(|&category| {
            let count = present.iter().filter(|&&c| c == category).count();
            (count > 0).then(|| {
                let label = if count == 1 {
                    category.singular()
                } else {
                    category.plural()
                };
                format!("{count} {label}")
            })
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Direction of structured (by-type) navigation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Toward the end of the document.
    Next,
    /// Toward the start of the document.
    Previous,
}

/// Find the next/previous element matching `target`, relative to `from` (the current cursor
/// position in document order), without wrapping. `from` of `None` searches from the start
/// (for `Next`) or yields nothing (for `Previous`). Returns the matching position.
#[must_use]
pub fn find(
    categories: &[Option<NavCategory>],
    from: Option<usize>,
    target: NavCategory,
    direction: Direction,
) -> Option<usize> {
    match direction {
        Direction::Next => {
            let start = from.map_or(0, |i| i + 1);
            (start..categories.len()).find(|&i| categories[i] == Some(target))
        }
        Direction::Previous => {
            let end = from.unwrap_or(0);
            (0..end).rev().find(|&i| categories[i] == Some(target))
        }
    }
}

/// A node for document-order flattening. Each node's id is its position in the slice passed to
/// [`document_order`]; `parent` is the parent's position (`None`, out-of-range, or self ⇒ a
/// root), and `index_in_parent` orders siblings.
#[derive(Clone, Copy, Debug)]
pub struct TreeNode {
    /// Position of the parent node, if any.
    pub parent: Option<usize>,
    /// This node's index among its parent's children.
    pub index_in_parent: i32,
}

/// Flatten a tree into document (depth-first, sibling-ordered) order, returning node positions.
/// Robust to malformed input: orphans become roots, cycles are broken by a visited guard, and
/// any node not reached from a root is appended so every node appears exactly once.
#[must_use]
pub fn document_order(nodes: &[TreeNode]) -> Vec<usize> {
    let n = nodes.len();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut roots: Vec<usize> = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        match node.parent {
            Some(parent) if parent < n && parent != i => children[parent].push(i),
            _ => roots.push(i),
        }
    }
    for list in &mut children {
        list.sort_by_key(|&child| nodes[child].index_in_parent);
    }
    roots.sort_by_key(|&root| nodes[root].index_in_parent);

    let mut order = Vec::with_capacity(n);
    let mut visited = vec![false; n];
    let mut stack: Vec<usize> = roots.into_iter().rev().collect();
    while let Some(node) = stack.pop() {
        if visited[node] {
            continue;
        }
        visited[node] = true;
        order.push(node);
        for &child in children[node].iter().rev() {
            if !visited[child] {
                stack.push(child);
            }
        }
    }
    // Append anything unreachable (e.g. inside a cycle) so no node is silently dropped.
    for (i, seen) in visited.iter().enumerate() {
        if !seen {
            order.push(i);
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use super::{classify, document_order, find, summarize, Direction, NavCategory, TreeNode};

    #[test]
    fn classifies_known_roles_and_ignores_others() {
        assert_eq!(classify("heading"), Some(NavCategory::Heading));
        assert_eq!(classify("push button"), Some(NavCategory::Button));
        assert_eq!(classify("entry"), Some(NavCategory::FormField));
        assert_eq!(classify("check box"), Some(NavCategory::FormField));
        assert_eq!(classify("link"), Some(NavCategory::Link));
        assert_eq!(classify("filler"), None);
        assert_eq!(classify("panel"), None);
    }

    #[test]
    fn summarize_counts_pluralizes_and_orders() {
        let cats = vec![
            Some(NavCategory::Button),
            Some(NavCategory::Heading),
            None,
            Some(NavCategory::Button),
            Some(NavCategory::Link),
        ];
        // Reported in ORDER (heading, link, button), pluralised by count.
        assert_eq!(
            summarize(cats).as_deref(),
            Some("1 heading, 1 link, 2 buttons")
        );
    }

    #[test]
    fn summarize_is_none_when_nothing_notable() {
        assert_eq!(summarize(vec![None, None]), None);
        assert_eq!(summarize(Vec::<Option<NavCategory>>::new()), None);
    }

    #[test]
    fn find_moves_next_and_previous_without_wrapping() {
        use NavCategory::{Button, Heading};
        let cats = vec![
            Some(Heading), // 0
            Some(Button),  // 1
            None,          // 2
            Some(Heading), // 3
            Some(Button),  // 4
        ];
        // Next heading from before the start, and after position 0.
        assert_eq!(find(&cats, None, Heading, Direction::Next), Some(0));
        assert_eq!(find(&cats, Some(0), Heading, Direction::Next), Some(3));
        // No heading after the last one — does not wrap.
        assert_eq!(find(&cats, Some(3), Heading, Direction::Next), None);
        // Previous button from position 4, and none before position 1.
        assert_eq!(find(&cats, Some(4), Button, Direction::Previous), Some(1));
        assert_eq!(find(&cats, Some(1), Button, Direction::Previous), None);
        // Previous from the start yields nothing.
        assert_eq!(find(&cats, None, Heading, Direction::Previous), None);
    }

    fn node(parent: Option<usize>, index_in_parent: i32) -> TreeNode {
        TreeNode {
            parent,
            index_in_parent,
        }
    }

    #[test]
    fn document_order_is_depth_first_by_sibling_index() {
        let nodes = vec![
            node(None, 0),    // 0 root
            node(Some(0), 1), // 1 second child of root
            node(Some(0), 0), // 2 first child of root
            node(Some(2), 0), // 3 child of node 2
        ];
        // DFS, siblings by index: root, then child 2 (idx 0) and its child 3, then child 1.
        assert_eq!(document_order(&nodes), vec![0, 2, 3, 1]);
    }

    #[test]
    fn document_order_handles_cycles_and_orphans() {
        // A 2-node cycle (no root) must still surface both nodes exactly once.
        let cyclic = vec![node(Some(1), 0), node(Some(0), 0)];
        let order = document_order(&cyclic);
        assert_eq!(order.len(), 2);
        assert!(order.contains(&0) && order.contains(&1));
    }
}
