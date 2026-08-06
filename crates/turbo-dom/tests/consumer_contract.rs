//! CONSUMER CONTRACT — the public `rtdom` surface that in-process Rust embedders rely on.
//!
//! turbo-test and turbo-surf embed this crate as their DOM engine and bind it to a V8/napi
//! JS boundary. They marshal handles as plain integers and read numeric `nodeType`/namespace
//! ids, so a few things are load-bearing PUBLIC contract, not internal detail:
//!
//!   * `Handle` round-trips through `raw()` / `from_raw()` (a handle sent to JS as a number
//!     and handed back must resolve to the same node).
//!   * `node_type_id()` / `namespace_id()` return the stable DOM numbers.
//!   * the read / mutation / serialize / cascade / `NodeRef` / `DocumentExt` methods below
//!     keep their names and shapes.
//!
//! This file is an INTEGRATION test: it compiles as an EXTERNAL crate, so it sees exactly what
//! a consumer sees — only the public API. If a change renames, removes, or alters the signature
//! of anything here it stops compiling (or an assert fails) and CI goes red BEFORE the break
//! ships to turbo-test / turbo-surf. That is the point: no silent breaking change to the consumer
//! contract. When you intentionally change the contract, update this file AND the consumers in
//! the same release.
//!
//! (Consumers alias the crate to `turbo_dom_parser` via a Cargo `package` rename; from inside the
//! crate's own test suite it's just `turbo_dom`.)

use turbo_dom::rtdom::cascade::{computed_style, get_property_value};
use turbo_dom::rtdom::node_ref::DocumentExt;
use turbo_dom::rtdom::serialize::{serialize_inner, serialize_outer};
use turbo_dom::rtdom::tree::{Handle, Namespace, Tree};
use turbo_dom::rtdom::{Dom, Event};
use turbo_dom::rtdom::NodeRef;

/// Depth-first search for the first text node — exercises `children()` + `node_type_id()`.
fn find_text(t: &Tree, h: Handle, out: &mut Option<Handle>) {
    if out.is_none() && t.node_type_id(h) == 3 {
        *out = Some(h);
    }
    for c in t.children(h) {
        find_text(t, c, out);
    }
}

#[test]
fn handle_round_trips_through_raw_and_from_raw() {
    // A handle marshaled to JS as a u32/f64 and handed back must reconstruct the same node.
    let tree = Tree::parse("<div id=a><span>x</span></div>");
    let a = tree.query_selector("#a").expect("found #a");
    let raw: u32 = a.raw();
    assert_eq!(Handle::from_raw(raw), a);
    assert_eq!(tree.get_attribute(Handle::from_raw(raw), "id"), Some("a"));
    // f64 marshaling path (v8::Number): consumers do `h.raw() as f64` out, `x as u32` back.
    let as_f64 = f64::from(a.raw());
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let from_f64 = Handle::from_raw(as_f64 as u32);
    assert_eq!(from_f64, a);
}

#[test]
fn numeric_node_type_and_namespace_ids_are_stable() {
    let tree = Tree::parse("<div id=e></div><svg id=s></svg>");
    let e = tree.query_selector("#e").unwrap();
    let s = tree.query_selector("#s").unwrap();
    assert_eq!(tree.node_type_id(e), 1, "element nodeType == 1");
    assert_eq!(tree.namespace_id(e), 0, "HTML namespace == 0");
    assert_eq!(tree.namespace_id(s), 1, "SVG namespace == 1");

    // a text node reports nodeType 3 (consumers branch on the number at the JS boundary)
    let t = Tree::parse("hello");
    let mut text: Option<Handle> = None;
    find_text(&t, t.root(), &mut text);
    assert_eq!(t.node_type_id(text.expect("a text node")), 3, "text nodeType == 3");
}

#[test]
fn core_read_surface_is_present() {
    let tree = Tree::parse("<ul id=list><li class=item>a</li><li class=item>b</li></ul>");
    let root: Handle = tree.root();
    let list = tree.get_element_by_id("list").expect("getElementById");
    assert_eq!(tree.tag_name(list).as_deref(), Some("UL"));
    assert_eq!(tree.local_name(list), Some("ul"));
    assert_eq!(tree.get_attribute(list, "id"), Some("list"));
    assert!(tree.has_attribute(list, "id"));
    assert_eq!(tree.query_selector_all(".item").len(), 2);
    let first = tree.query_selector("li.item").unwrap();
    assert!(tree.matches(first, "li.item"));
    assert_eq!(tree.children(list).len(), 2);
    assert!(!tree.text_content(list).is_empty());
    // attributes() -> owned (name, value) pairs (consumers enumerate them across the boundary)
    let _attrs: Vec<(String, String)> = tree.attributes(list);
    assert!(tree.node_type_id(root) == 9 || tree.node_type_id(root) == 11);
}

#[test]
fn mutation_surface_is_present() {
    let mut tree = Tree::parse("<div id=host></div>");
    let host = tree.get_element_by_id("host").unwrap();
    let child = tree.create_element("span");
    tree.append_child(host, child);
    tree.set_attribute(child, "data-x", "1");
    assert_eq!(tree.get_attribute(child, "data-x"), Some("1"));
    tree.remove_attribute(child, "data-x");
    assert_eq!(tree.get_attribute(child, "data-x"), None);
    let txt = tree.create_text_node("hi");
    tree.append_child(child, txt);
    tree.set_text_content(child, "bye");
    tree.remove_child(host, child);
    assert_eq!(tree.children(host).len(), 0);

    // namespaced creation: consumers classify a namespace URI to a number, then build the
    // element via `Namespace::from_id` (the public inverse of `namespace_id`). Round-trips.
    let svg = tree.create_element_ns("svg", Namespace::from_id(1));
    tree.append_child(host, svg);
    assert_eq!(tree.namespace_id(svg), 1);
    assert_eq!(Namespace::from_id(0), Namespace::Html);
    assert_eq!(Namespace::from_id(2), Namespace::MathMl);
}

#[test]
fn serialize_cascade_noderef_documentext_surface() {
    let tree = Tree::parse("<div id=a style=\"color: red\"><b>hi</b></div>");
    let a = tree.query_selector("#a").unwrap();
    // serialize
    assert!(serialize_inner(&tree, a).contains("<b>"));
    assert!(serialize_outer(&tree, a).contains("id=\"a\""));
    // cascade
    let cs = computed_style(&tree, a);
    assert_eq!(get_property_value(&cs, "color"), "rgb(255, 0, 0)");
    // NodeRef
    let nref: NodeRef<'_> = NodeRef::new(&tree, a);
    assert_eq!(nref.handle(), a);
    let b = tree.query_selector("b").unwrap();
    assert_eq!(NodeRef::new(&tree, b).parent().map(|p| p.handle()), Some(a));
    // DocumentExt (document()/node()/query()/query_all() live on Tree via the trait)
    assert_eq!(tree.document().handle(), tree.root());
    assert_eq!(tree.node(a).handle(), a);
    assert!(tree.query("#a").is_some());
    assert_eq!(tree.query_all(".zzz").len(), 0);
}

#[test]
fn event_dispatch_surface_is_present() {
    // Consumers that drive the DOM from Rust (rather than their own JS event layer) go
    // through `Dom` + `Event`. `target`/`current_target` are `Option<Handle>`: `None`
    // before dispatch, `Some(node)` from `dispatch_event` onward. They are NOT a sentinel
    // handle — `Handle(0)` is the document root, so a sentinel would read as "targets the
    // document" before dispatch.
    let mut dom = Dom::parse("<div id=outer><button id=b>x</button></div>");
    let button = dom.tree.get_element_by_id("b").expect("#b");
    let mut ev = Event::new("click", true, true);
    assert_eq!(ev.target, None);
    assert_eq!(ev.current_target, None);
    let id = dom.add_event_listener(button, "click", false, false, Box::new(|_, e| e.prevent_default()));
    let not_prevented = dom.dispatch_event(button, &mut ev);
    assert_eq!(ev.target, Some(button));
    assert!(!not_prevented, "dispatch_event returns !defaultPrevented");
    assert!(ev.default_prevented());
    dom.remove_event_listener(button, id);
}

#[test]
fn subtree_replacement_detaches_the_old_children() {
    // A handle retained into a replaced subtree must report NO parent — walking `parent()`
    // upward from it terminates instead of climbing back into the live tree.
    let mut tree = Tree::parse("<div id=a><span id=s>old</span></div>");
    let a = tree.query_selector("#a").unwrap();
    let s = tree.query_selector("#s").unwrap();
    tree.set_inner_html(a, "<em>new</em>");
    assert_eq!(tree.parent(s), None);
    let mut tree = Tree::parse("<div id=a><span id=s>old</span></div>");
    let a = tree.query_selector("#a").unwrap();
    let s = tree.query_selector("#s").unwrap();
    tree.set_text_content(a, "new");
    assert_eq!(tree.parent(s), None);
}
