#![allow(clippy::unwrap_used)]

use spoiler_core::{
    recording::rrweb::{Add, Mutation, Remove, SerializedNode, TextChange},
    replay::Mirror,
};

fn node(id: i64, kind: u64, tag: &str, children: Vec<SerializedNode>) -> SerializedNode {
    let mut node = SerializedNode::default();
    node.id = id;
    node.kind = kind;
    node.tag = tag.to_owned();
    node.children = children;
    node
}

fn page() -> Mirror {
    let mut mirror = Mirror::default();
    let mut text = node(6, 3, "", vec![]);
    text.text = "Open details".to_owned();
    let button = node(5, 2, "button", vec![text]);
    let item = node(4, 2, "li", vec![button]);
    mirror.reset(node(
        1,
        0,
        "",
        vec![node(2, 2, "ul", vec![item]), node(3, 2, "ul", vec![])],
    ));
    mirror
}

fn move_item(mirror: &mut Mirror, from: i64, to: i64, remove: bool) {
    mirror.apply(Mutation {
        removes: if remove {
            vec![Remove {
                parent: from,
                id: 4,
            }]
        } else {
            vec![]
        },
        adds: vec![Add {
            parent: to,
            next: None,
            // rrweb serializes mutation adds with skipChild: true.
            node: node(4, 2, "li", vec![]),
        }],
        ..Mutation::default()
    });
}

#[test]
fn move_with_remove_keeps_existing_descendants() {
    let mut mirror = page();
    move_item(&mut mirror, 2, 3, true);

    assert!(mirror.get(2).unwrap().children.is_empty());
    assert_eq!(mirror.get(3).unwrap().children, [4]);
    assert_eq!(mirror.get(4).unwrap().children, [5]);
    assert_eq!(mirror.get(5).unwrap().children, [6]);
    assert_eq!(mirror.visible_text(4), "Open details");
    assert!(mirror.is_attached(6));

    mirror.apply(Mutation {
        removes: vec![Remove { parent: 3, id: 4 }],
        ..Mutation::default()
    });
    assert!(!mirror.contains(4));
    assert!(!mirror.contains(5));
    assert!(!mirror.contains(6));
}

#[test]
fn move_without_remove_keeps_existing_descendants() {
    let mut mirror = page();
    move_item(&mut mirror, 2, 3, false);
    move_item(&mut mirror, 3, 2, false);
    mirror.apply(Mutation {
        texts: vec![TextChange {
            id: 6,
            value: "Updated details".to_owned(),
        }],
        ..Mutation::default()
    });

    assert_eq!(mirror.get(2).unwrap().children, [4]);
    assert!(mirror.get(3).unwrap().children.is_empty());
    assert_eq!(mirror.visible_text(4), "Updated details");
    assert_eq!(mirror.parent(5).unwrap().id, 4);
}

#[test]
fn move_out_of_removed_ancestor_keeps_descendants() {
    let mut mirror = page();
    mirror.apply(Mutation {
        removes: vec![Remove { parent: 1, id: 2 }],
        adds: vec![Add {
            parent: 3,
            next: None,
            node: node(5, 2, "button", vec![]),
        }],
        ..Mutation::default()
    });

    assert!(!mirror.contains(2));
    assert!(!mirror.contains(4));
    assert_eq!(mirror.get(3).unwrap().children, [5]);
    assert_eq!(mirror.get(5).unwrap().children, [6]);
    assert_eq!(mirror.visible_text(3), "Open details");
}

#[test]
fn deep_serialized_node_chain_drops_on_normal_thread_stack() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let mut root = node(0, 2, "div", vec![]);
            for id in 1..=200_000 {
                root = node(id, 2, "div", vec![root]);
            }
            drop(root);
        })
        .unwrap()
        .join()
        .unwrap();
}
