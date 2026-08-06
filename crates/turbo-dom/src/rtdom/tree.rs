//! Rust-native DOM tree (pure Rust API, no JS boundary) — Phase-0 core of the
//! pivoted runtime (`RUST_PORT_PLAN` §7 verdict: dual-runtime, Rust consumer gets
//! a native API). Mirrors the JS runtime's load-bearing model:
//!   * immutable parse buffer (`core::Soa`) is the read-only source of truth,
//!   * COW overlay: a node only allocates owned state when it is mutated,
//!   * handle (`u32` node index) is stable; identity is the handle itself,
//!   * `version` counter bumped on EVERY mutation = the cache-invalidation key.
//!
//! Reads come straight off the buffer (zero alloc) unless an overlay exists.

use crate::core::{self, Soa};
use smol_str::SmolStr;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use std::cell::RefCell;

/// A DOM node type. `#[repr(u8)]` with the standard `nodeType` numeric values, so
/// it round-trips the `SoA`'s compact `u8` column by a plain cast — no lookup table.
/// It enumerates exactly the node types the html5ever core can emit; nothing else
/// is representable, so a `match` over a node's type is exhaustive and compiler-checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NodeType {
    Element = 1,
    Text = 3,
    ProcessingInstruction = 7,
    Comment = 8,
    Document = 9,
    Doctype = 10,
    Fragment = 11,
}

impl NodeType {
    /// Read a node type back from the `SoA`'s untyped `u8` column. Total over every
    /// value the parser core writes (see the node-type constants in `core`).
    fn from_u8(v: u8) -> NodeType {
        match v {
            1 => NodeType::Element,
            3 => NodeType::Text,
            7 => NodeType::ProcessingInstruction,
            8 => NodeType::Comment,
            9 => NodeType::Document,
            10 => NodeType::Doctype,
            11 => NodeType::Fragment,
            _ => unreachable!("SoA node_type column only holds DOM node-type values"),
        }
    }
}

/// An element's namespace — the only three an HTML parser produces. The `SoA` stores
/// it as `0`/`1`/`2`; this is the typed view at the API surface (`as u8` to store).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Namespace {
    Html = 0,
    Svg = 1,
    MathMl = 2,
}

impl Namespace {
    fn from_u8(v: u8) -> Namespace {
        match v {
            1 => Namespace::Svg,
            2 => Namespace::MathMl,
            _ => Namespace::Html,
        }
    }

    /// Build a `Namespace` from its NUMBER (0 HTML, 1 SVG, 2 `MathML`; anything else HTML) —
    /// the public inverse of [`Tree::namespace_id`]. FFI consumers that classify a namespace
    /// URI to a small int can pass it straight to [`Tree::create_element_ns`] without
    /// matching the enum themselves. Locked by `tests/consumer_contract.rs`.
    #[inline]
    #[must_use]
    pub const fn from_id(id: u8) -> Namespace {
        match id {
            1 => Namespace::Svg,
            2 => Namespace::MathMl,
            _ => Namespace::Html,
        }
    }
}

/// A stable node identity within one [`Tree`]. Opaque: internally it indexes the
/// parse buffer (or, at/after `buf_len`, the created-node list), but callers treat
/// it as a token — obtain one from the tree, hand it back. Newtyped rather than a
/// bare `u32` so an arbitrary integer, or a handle belonging to a *different* tree,
/// can't be silently passed where a node of *this* tree is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Handle(pub(crate) u32);

impl Handle {
    /// As a `usize` index into the parse buffer's columns.
    #[inline]
    fn idx(self) -> usize {
        self.0 as usize
    }
    /// The raw `u32` a `Handle` wraps (buffer offset, or `new_nodes` slot once
    /// `>= buf_len`). PUBLIC and stable: FFI consumers (turbo-test, turbo-surf) store
    /// handles as plain integers across the JS/napi boundary and must round-trip them
    /// through `raw()` / [`Handle::from_raw`]. Locked by `tests/consumer_contract.rs`.
    #[inline]
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }
    /// Reconstruct a `Handle` from a raw `u32` previously obtained via [`Handle::raw`]
    /// (e.g. a handle marshaled to JS as a number and handed back). The inverse of
    /// `raw()`. No validation — an out-of-range value simply won't resolve to a node,
    /// exactly as a bogus index did under the old `u32`-alias `Handle`.
    #[inline]
    #[must_use]
    pub const fn from_raw(raw: u32) -> Handle {
        Handle(raw)
    }
}

/// A node created at runtime (not read from the parse buffer). Each variant carries
/// exactly the data its node type needs — a text node cannot hold a namespace, an
/// element always has one — so the old `(node_type, name, ns)` triple's invalid
/// combinations (a `Text` with an `ns`, an element with no name) are unrepresentable.
enum NewNode {
    Element { name: SmolStr, ns: Namespace },
    Text(SmolStr),
    Comment(SmolStr),
    Fragment,
}

impl NewNode {
    fn node_type(&self) -> NodeType {
        match self {
            NewNode::Element { .. } => NodeType::Element,
            NewNode::Text(_) => NodeType::Text,
            NewNode::Comment(_) => NodeType::Comment,
            NewNode::Fragment => NodeType::Fragment,
        }
    }
    fn namespace(&self) -> Namespace {
        match self {
            NewNode::Element { ns, .. } => *ns,
            _ => Namespace::Html,
        }
    }
    /// The element's local name, if this is an element.
    fn element_name(&self) -> Option<&str> {
        match self {
            NewNode::Element { name, .. } => Some(name),
            _ => None,
        }
    }
    /// The character data, if this is a text or comment node.
    fn char_data(&self) -> Option<&str> {
        match self {
            NewNode::Text(s) | NewNode::Comment(s) => Some(s),
            _ => None,
        }
    }
}

/// Per-node overlay child list. `SmallVec` inlines up to 8 child handles
/// (32 bytes) before spilling to the heap — most elements have few children,
/// so mutation churn no longer heap-allocs a `Vec` per mutated node.
type ChildList = SmallVec<[Handle; 8]>;
/// Per-node overlay attribute list. Inlines up to 2 `(name, value)` pairs
/// (covers the common id/class case) before spilling to the heap.
type AttrList = SmallVec<[(SmolStr, SmolStr); 2]>;

/// A COW mutation overlay keyed by [`Handle`], split by the structural fact that
/// created-node handles are **dense**: `push_node` assigns them `buf_len + i`
/// monotonically (parallel to the `new_nodes` arena), while buffer nodes are the
/// sparse ones that only sometimes get mutated. So:
///   * a **created** node (`idx >= buf_len`) lives in a `Vec<Option<T>>` indexed by
///     `idx - buf_len` — O(1), no hashing, no rehash, and zero memory waste (every
///     created node already exists in `new_nodes`, so the slot is one it "owns").
///   * a **buffer** node (`idx < buf_len`) lives in an `FxHashMap` (only the sparse
///     subset of buffer nodes that are ever mutated allocate an entry).
///
/// The split is encapsulated ENTIRELY inside these methods — call sites never branch
/// on `idx < buf_len`. A created node structurally always resolves to its dense slot;
/// the only way to reach a slot is through this API, so "created node → dense, buffer
/// node → map" can't be gotten wrong at a call site.
struct Overlay<T> {
    /// Indexed by `handle.idx() - buf_len`. Grows with `None` as created handles
    /// appear; an absent (out-of-bounds) or `None` slot = no overlay for that node.
    dense: Vec<Option<T>>,
    /// Overrides for buffer nodes (`idx < buf_len`), the sparsely-mutated subset.
    map: FxHashMap<Handle, T>,
    /// The created-node boundary: handles `< buf_len` are buffer nodes (→ `map`),
    /// `>= buf_len` are created nodes (→ `dense[idx - buf_len]`).
    buf_len: u32,
}

impl<T> Overlay<T> {
    fn new(buf_len: u32) -> Overlay<T> {
        Overlay { dense: Vec::new(), map: FxHashMap::default(), buf_len }
    }

    /// `Some(handle.idx() - buf_len)` for a created node, `None` for a buffer node.
    #[inline]
    fn dense_idx(&self, h: Handle) -> Option<usize> {
        if h.raw() >= self.buf_len {
            Some((h.raw() - self.buf_len) as usize)
        } else {
            None
        }
    }

    #[inline]
    fn get(&self, h: Handle) -> Option<&T> {
        match self.dense_idx(h) {
            Some(i) => self.dense.get(i).and_then(Option::as_ref),
            None => self.map.get(&h),
        }
    }

    #[inline]
    fn get_mut(&mut self, h: Handle) -> Option<&mut T> {
        match self.dense_idx(h) {
            Some(i) => self.dense.get_mut(i).and_then(Option::as_mut),
            None => self.map.get_mut(&h),
        }
    }

    #[inline]
    fn contains(&self, h: Handle) -> bool {
        match self.dense_idx(h) {
            Some(i) => matches!(self.dense.get(i), Some(Some(_))),
            None => self.map.contains_key(&h),
        }
    }

    /// Grow the dense Vec so index `i` is addressable (filling with `None`).
    #[inline]
    fn ensure_dense_slot(&mut self, i: usize) {
        if i >= self.dense.len() {
            self.dense.resize_with(i + 1, || None);
        }
    }

    #[inline]
    fn insert(&mut self, h: Handle, v: T) {
        match self.dense_idx(h) {
            Some(i) => {
                self.ensure_dense_slot(i);
                self.dense[i] = Some(v);
            }
            None => {
                self.map.insert(h, v);
            }
        }
    }
}

pub struct Tree {
    buf: Soa,
    buf_len: u32,
    new_nodes: Vec<NewNode>,
    // --- COW overlays (present only for mutated/created nodes) ---
    // Hybrid `Overlay<T>`: created nodes (dense handles) live in a `Vec`, sparse
    // buffer-node overrides in a map — see `Overlay`. `Some(parent)` = re-parented;
    // `None` = detached. No `-1` sentinel — a detached node is `None`, never a handle
    // that happens to read as negative.
    parent_ov: Overlay<Option<Handle>>,
    children_ov: Overlay<ChildList>,
    attrs_ov: Overlay<AttrList>,
    text_ov: Overlay<SmolStr>,
    /// host → shadow-root handle, and shadow-root → host (the two shadow maps).
    shadow_root_of_host: FxHashMap<Handle, Handle>,
    host_of_shadow_root: FxHashMap<Handle, Handle>,
    pub version: u64,
    /// version-keyed query result cache (mirrors JS cachedQSA / Document.__version).
    pub(crate) qcache: RefCell<QueryCache>,
    /// selector-string → parsed selector-list cache. A `matches()` driven over an
    /// element loop re-parses the SAME selector per element; this memoizes the parse
    /// (shared `Rc` so a hit is a pointer bump). Version-independent — a parsed
    /// selector doesn't change when the tree mutates. Bounded like `qcache`.
    pub(crate) parse_cache: RefCell<FxHashMap<String, std::rc::Rc<[crate::rtdom::query::Complex]>>>,
    /// version-keyed computed-style cache (mirrors JS `__computedStyle`).
    pub(crate) css_cache: RefCell<crate::rtdom::cascade::CssCache>,
    /// Mutation-record buffer. `None` until an observer calls `start_recording`
    /// (the no-observer hot path stays zero-alloc, mirroring JS notifyMutation gating).
    mutation_log: Option<Vec<crate::rtdom::mutations::MutationRecord>>,
    /// Live IDL boolean form-state properties (checked/selected/disabled/required/
    /// read-only), settable independently of the HTML attribute — the Rust analogue of
    /// the JS runtime's `__checked`/`__selected` overlays that React/interaction set
    /// without touching the attribute. Absent ⇒ fall back to the attribute (see the
    /// form-state pseudo-classes in query.rs). Empty on the parse/no-interaction path.
    form_props: FxHashMap<(Handle, FormProp), bool>,
}

/// The boolean IDL form-state properties whose selector semantics can diverge from the
/// HTML attribute once a consumer sets the live property (mirrors the JS getters:
/// `checked`/`selected` read a live overlay first, the rest read the attribute but the
/// `:disabled`/`:required`/`:read-only` matchers also honor the live property).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FormProp {
    Checked,
    Selected,
    Disabled,
    Required,
    ReadOnly,
}

#[derive(Default)]
pub(crate) struct QueryCache {
    pub version: u64,
    /// Shared so a cache hit is a pointer bump, not a Vec copy (repeated qSA is hot).
    pub map: FxHashMap<String, std::rc::Rc<[Handle]>>,
}

impl Tree {
    #[must_use]
    pub fn parse(html: &str) -> Tree {
        let buf = core::parse_html_soa(html);
        let buf_len = buf.node_type.len() as u32;
        Tree {
            buf,
            buf_len,
            new_nodes: Vec::new(),
            parent_ov: Overlay::new(buf_len),
            children_ov: Overlay::new(buf_len),
            attrs_ov: Overlay::new(buf_len),
            text_ov: Overlay::new(buf_len),
            shadow_root_of_host: FxHashMap::default(),
            host_of_shadow_root: FxHashMap::default(),
            version: 0,
            qcache: RefCell::new(QueryCache::default()),
            parse_cache: RefCell::new(FxHashMap::default()),
            css_cache: RefCell::new(crate::rtdom::cascade::CssCache::default()),
            mutation_log: None,
            form_props: FxHashMap::default(),
        }
    }

    #[inline]
    fn is_new(&self, h: Handle) -> bool {
        h.raw() >= self.buf_len
    }
    #[inline]
    fn new_ref(&self, h: Handle) -> &NewNode {
        &self.new_nodes[(h.raw() - self.buf_len) as usize]
    }

    pub fn node_count(&self) -> u32 {
        self.buf_len + self.new_nodes.len() as u32
    }

    /// The document root (node 0 in the `SoA`).
    pub fn root(&self) -> Handle {
        Handle(0)
    }

    /// Iterate every node handle in the tree, in index order (buffer nodes first,
    /// then runtime-created nodes). The safe substitute for a raw `0..node_count()`
    /// scan now that a `Handle` is opaque.
    pub fn handles(&self) -> impl Iterator<Item = Handle> {
        (0..self.node_count()).map(Handle)
    }

    // ----------------------------------------------------------------- reads
    pub fn node_type(&self, h: Handle) -> NodeType {
        if self.is_new(h) {
            self.new_ref(h).node_type()
        } else {
            NodeType::from_u8(self.buf.node_type[h.idx()])
        }
    }

    pub fn namespace(&self, h: Handle) -> Namespace {
        if self.is_new(h) {
            self.new_ref(h).namespace()
        } else {
            Namespace::from_u8(self.buf.ns[h.idx()])
        }
    }

    /// The DOM `nodeType` NUMBER for `h` (element = 1, text = 3, PI = 7, comment = 8,
    /// document = 9, doctype = 10, fragment = 11). PUBLIC, stable FFI accessor: consumers
    /// marshal `nodeType` to JS as a number, so they need the integer, not the
    /// [`NodeType`] enum (which stays the internal, type-safe representation). Locked by
    /// `tests/consumer_contract.rs`.
    #[inline]
    #[must_use]
    pub fn node_type_id(&self, h: Handle) -> u8 {
        self.node_type(h) as u8
    }

    /// The namespace as a NUMBER (HTML = 0, SVG = 1, `MathML` = 2). PUBLIC, stable FFI
    /// accessor mirroring [`Tree::node_type_id`]: consumers branch on `namespace_id(h)
    /// == 0` (is-HTML) at the JS/napi boundary. Locked by `tests/consumer_contract.rs`.
    #[inline]
    #[must_use]
    pub fn namespace_id(&self, h: Handle) -> u8 {
        self.namespace(h) as u8
    }

    /// localName (lowercase as parsed). None for non-elements.
    pub fn local_name(&self, h: Handle) -> Option<&str> {
        if self.node_type(h) != NodeType::Element {
            return None;
        }
        if self.is_new(h) {
            self.new_ref(h).element_name()
        } else {
            Some(&self.buf.tag_names[self.buf.tag_id[h.idx()] as usize])
        }
    }

    /// tagName: uppercased for HTML-namespace elements (DOM contract), as-is otherwise.
    pub fn tag_name(&self, h: Handle) -> Option<String> {
        let ln = self.local_name(h)?;
        if self.namespace(h) == Namespace::Html {
            Some(ln.to_ascii_uppercase())
        } else {
            Some(ln.to_string())
        }
    }

    pub fn parent(&self, h: Handle) -> Option<Handle> {
        if let Some(&p) = self.parent_ov.get(h) {
            return p;
        }
        if self.is_new(h) {
            return None; // detached unless set via overlay
        }
        let p = self.buf.parent[h.idx()];
        if p < 0 {
            None
        } else {
            Some(Handle(p as u32))
        }
    }

    /// Live child list. Reads overlay if the node was structurally mutated,
    /// else walks the buffer first-child/next-sib chain (zero overlay alloc).
    pub fn children(&self, h: Handle) -> Vec<Handle> {
        if let Some(c) = self.children_ov.get(h) {
            return c.to_vec();
        }
        if self.is_new(h) {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut c = self.buf.first_child[h.idx()];
        while c >= 0 {
            out.push(Handle(c as u32));
            c = self.buf.next_sib[c as usize];
        }
        out
    }

    /// Visit each child handle WITHOUT allocating a `Vec` — walks the child overlay
    /// if the node was structurally mutated, else the buffer first-child/next-sib
    /// chain straight off the `SoA`. Hot-path alternative to `children()` (which
    /// clones a `Vec<Handle>`) for callers that only iterate. Mirrors `for_each_attr`.
    pub fn for_each_child(&self, h: Handle, mut f: impl FnMut(Handle)) {
        if let Some(c) = self.children_ov.get(h) {
            for &child in c {
                f(child);
            }
            return;
        }
        if self.is_new(h) {
            return;
        }
        let mut c = self.buf.first_child[h.idx()];
        while c >= 0 {
            f(Handle(c as u32));
            c = self.buf.next_sib[c as usize];
        }
    }

    pub fn first_child(&self, h: Handle) -> Option<Handle> {
        self.children(h).first().copied()
    }
    pub fn last_child(&self, h: Handle) -> Option<Handle> {
        self.children(h).last().copied()
    }

    pub fn next_sibling(&self, h: Handle) -> Option<Handle> {
        let p = self.parent(h)?;
        let sibs = self.children(p);
        let i = sibs.iter().position(|&x| x == h)?;
        sibs.get(i + 1).copied()
    }
    pub fn previous_sibling(&self, h: Handle) -> Option<Handle> {
        let p = self.parent(h)?;
        let sibs = self.children(p);
        let i = sibs.iter().position(|&x| x == h)?;
        if i == 0 {
            None
        } else {
            sibs.get(i - 1).copied()
        }
    }

    // ----------------------------------------------------------------- attrs
    /// getAttribute. Reads overlay if attrs were mutated, else the buffer slice
    /// (lazy — no owned Vec built for unmutated nodes).
    pub fn get_attribute(&self, h: Handle, name: &str) -> Option<&str> {
        if let Some(ov) = self.attrs_ov.get(h) {
            return ov.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
        }
        if self.is_new(h) {
            return None;
        }
        let i = h.idx();
        let start = self.buf.attr_start[i];
        if start < 0 {
            return None;
        }
        let start = start as usize;
        let count = self.buf.attr_count[i] as usize;
        for j in start..start + count {
            let nid = self.buf.attr_name_id[j] as usize;
            if self.buf.attr_names[nid] == name {
                return Some(&self.buf.attr_values[self.buf.attr_value_id[j] as usize]);
            }
        }
        None
    }

    pub fn has_attribute(&self, h: Handle, name: &str) -> bool {
        self.get_attribute(h, name).is_some()
    }

    /// All (name, value) pairs for a node (owned copy).
    pub fn attributes(&self, h: Handle) -> Vec<(String, String)> {
        if let Some(ov) = self.attrs_ov.get(h) {
            return ov.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect();
        }
        if self.is_new(h) {
            return Vec::new();
        }
        let i = h.idx();
        let start = self.buf.attr_start[i];
        if start < 0 {
            return Vec::new();
        }
        let start = start as usize;
        let count = self.buf.attr_count[i] as usize;
        let mut out = Vec::with_capacity(count);
        for j in start..start + count {
            let nid = self.buf.attr_name_id[j] as usize;
            let vid = self.buf.attr_value_id[j] as usize;
            out.push((self.buf.attr_names[nid].clone(), self.buf.attr_values[vid].clone()));
        }
        out
    }

    /// Visit each (name, value) pair WITHOUT allocating — yields borrowed `&str`
    /// straight from the overlay or the `SoA` tables. Hot-path alternative to
    /// `attributes()` (which clones a `Vec<(String, String)>`).
    pub fn for_each_attr(&self, h: Handle, mut f: impl FnMut(&str, &str)) {
        if let Some(ov) = self.attrs_ov.get(h) {
            for (n, v) in ov {
                f(n, v);
            }
            return;
        }
        if self.is_new(h) {
            return;
        }
        let i = h.idx();
        let start = self.buf.attr_start[i];
        if start < 0 {
            return;
        }
        let start = start as usize;
        let count = self.buf.attr_count[i] as usize;
        for j in start..start + count {
            let nid = self.buf.attr_name_id[j] as usize;
            let vid = self.buf.attr_value_id[j] as usize;
            f(&self.buf.attr_names[nid], &self.buf.attr_values[vid]);
        }
    }

    /// Like `for_each_attr` but also yields the foreign-content attr `prefix`
    /// ("xlink"/"xml"/"xmlns", else ""). Needed by the html5lib dump. Overlay
    /// (created/mutated) attrs have no stored prefix → "".
    pub fn for_each_attr_full(&self, h: Handle, mut f: impl FnMut(&str, &str, &str)) {
        if let Some(ov) = self.attrs_ov.get(h) {
            for (n, v) in ov {
                f("", n, v);
            }
            return;
        }
        if self.is_new(h) {
            return;
        }
        let i = h.idx();
        let start = self.buf.attr_start[i];
        if start < 0 {
            return;
        }
        let start = start as usize;
        let count = self.buf.attr_count[i] as usize;
        for j in start..start + count {
            let prefix = &self.buf.attr_prefixes[self.buf.attr_prefix_id[j] as usize];
            let nid = self.buf.attr_name_id[j] as usize;
            let vid = self.buf.attr_value_id[j] as usize;
            f(prefix, &self.buf.attr_names[nid], &self.buf.attr_values[vid]);
        }
    }

    /// The interned tag/label name for an element OR document-fragment node (the
    /// `<template>` synthetic `content` fragment carries the label "content").
    /// None for nodes without a tag id (text/comment/doctype/document).
    pub fn node_label(&self, h: Handle) -> Option<&str> {
        let nt = self.node_type(h);
        if nt != NodeType::Element && nt != NodeType::Fragment {
            return None;
        }
        if self.is_new(h) {
            return if nt == NodeType::Element { self.new_ref(h).element_name() } else { None };
        }
        Some(&self.buf.tag_names[self.buf.tag_id[h.idx()] as usize])
    }

    /// DOCTYPE name / public id / system id (buffer-backed doctype nodes). The
    /// `SoA` stores all three as pooled strings (public/system "" when absent).
    pub fn doctype_name(&self, h: Handle) -> Option<&str> {
        self.doctype_field(h, &self.buf.text_id)
    }
    pub fn doctype_public_id(&self, h: Handle) -> Option<&str> {
        self.doctype_field(h, &self.buf.pub_id)
    }
    pub fn doctype_system_id(&self, h: Handle) -> Option<&str> {
        self.doctype_field(h, &self.buf.sys_id)
    }
    fn doctype_field<'a>(&'a self, h: Handle, col: &[i32]) -> Option<&'a str> {
        if self.node_type(h) != NodeType::Doctype || self.is_new(h) {
            return None;
        }
        let id = col[h.idx()];
        Some(self.buf.strings.get(id as usize).map_or("", |s| s.as_str()))
    }

    // ----------------------------------------------------------------- text
    pub fn node_value(&self, h: Handle) -> Option<String> {
        let nt = self.node_type(h);
        if nt != NodeType::Text && nt != NodeType::Comment {
            return None;
        }
        if let Some(t) = self.text_ov.get(h) {
            return Some(t.to_string());
        }
        if self.is_new(h) {
            return self.new_ref(h).char_data().map(std::string::ToString::to_string);
        }
        // buffer text/comment nodes always have text_id >= 0 (core pools every
        // string); .get + default keeps this total and coverable.
        let tid = self.buf.text_id[h.idx()];
        Some(self.buf.strings.get(tid as usize).cloned().unwrap_or_default())
    }

    /// textContent: concatenated descendant text.
    pub fn text_content(&self, h: Handle) -> String {
        let nt = self.node_type(h);
        if nt == NodeType::Text || nt == NodeType::Comment {
            return self.node_value(h).unwrap_or_default();
        }
        let mut out = String::new();
        self.collect_text(h, &mut out);
        out
    }
    fn collect_text(&self, h: Handle, out: &mut String) {
        for c in self.children(h) {
            // Comment and Fragment share an empty body but are skipped for distinct,
            // separately-documented reasons — keep them as separate, commented arms.
            #[allow(clippy::match_same_arms)]
            match self.node_type(c) {
                NodeType::Text => out.push_str(&self.node_value(c).unwrap_or_default()),
                NodeType::Comment => {}
                // A <template>'s content is stored as a synthetic `content` fragment
                // child, but in the DOM it lives in `template.content`, NOT among the
                // element's child nodes — so it must not contribute to textContent
                // (`template.textContent === ""`). It is the only fragment that ever
                // appears in a child list here. Serialization still walks it directly.
                NodeType::Fragment => {}
                _ => self.collect_text(c, out),
            }
        }
    }

    // ------------------------------------------------------------- mutations
    fn bump(&mut self) {
        self.version += 1;
    }

    /// Record a `childList` mutation IF recording is on (an observer is attached).
    /// Takes PRIMITIVES and early-returns BEFORE allocating the added/removed `Vec`s
    /// or the record — mirroring JS `notifyMutation` (dom.mjs:1501), which builds the
    /// `[added]`/`[removed]` arrays only after the no-observer early-return. The
    /// version bump is the CALLER's job and is always done unconditionally, so the
    /// no-observer path stays zero-alloc while caches still invalidate.
    fn record_child_list(&mut self, parent: Handle, added: Option<Handle>, removed: Option<Handle>) {
        if !self.is_recording() {
            return;
        }
        let rec = crate::rtdom::mutations::MutationRecord::child_list(
            parent,
            added.into_iter().collect(),
            removed.into_iter().collect(),
        );
        self.mutation_log.as_mut().unwrap().push(rec);
    }

    /// Like `record_child_list` but for the batch case (`textContent` replacing a
    /// whole child list): only takes ownership of the already-built `Vec`s — which
    /// the caller produced for the actual mutation anyway — when recording.
    fn record_child_list_batch(&mut self, parent: Handle, added: Vec<Handle>, removed: Vec<Handle>) {
        if !self.is_recording() {
            return;
        }
        let rec = crate::rtdom::mutations::MutationRecord::child_list(parent, added, removed);
        self.mutation_log.as_mut().unwrap().push(rec);
    }

    /// Record an `attributes` mutation IF recording is on. Takes the name as `&str`
    /// and only allocates the owned `String` (via `MutationRecord::attributes`) after
    /// the early-return — so a no-observer `set_attribute`/`remove_attribute` never
    /// builds the record's name string.
    fn record_attributes(&mut self, target: Handle, name: &str, old_value: Option<String>) {
        if !self.is_recording() {
            return;
        }
        let rec = crate::rtdom::mutations::MutationRecord::attributes(target, name, old_value);
        self.mutation_log.as_mut().unwrap().push(rec);
    }

    /// Record a `characterData` mutation IF recording is on.
    fn record_character_data(&mut self, target: Handle, old_value: Option<String>) {
        if !self.is_recording() {
            return;
        }
        let rec = crate::rtdom::mutations::MutationRecord::character_data(target, old_value);
        self.mutation_log.as_mut().unwrap().push(rec);
    }

    /// Begin buffering mutation records (called by `MutationObserver::observe`).
    pub fn start_recording(&mut self) {
        self.mutation_log.get_or_insert_with(Vec::new);
    }
    /// Stop buffering and drop any pending records.
    pub fn stop_recording(&mut self) {
        self.mutation_log = None;
    }
    pub fn is_recording(&self) -> bool {
        self.mutation_log.is_some()
    }
    /// Drain the buffered records (keeps recording on). Empty if not recording.
    pub fn take_mutation_records(&mut self) -> Vec<crate::rtdom::mutations::MutationRecord> {
        self.mutation_log.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /// Ensure a node's attr overlay exists (copy buffer attrs in on first write).
    fn ensure_attrs(&mut self, h: Handle) -> &mut AttrList {
        if !self.attrs_ov.contains(h) {
            let init: AttrList =
                self.attributes(h).into_iter().map(|(n, v)| (n.into(), v.into())).collect();
            self.attrs_ov.insert(h, init);
        }
        self.attrs_ov.get_mut(h).unwrap()
    }

    pub fn set_attribute(&mut self, h: Handle, name: &str, value: &str) {
        let old = if self.is_recording() {
            self.get_attribute(h, name).map(std::string::ToString::to_string)
        } else {
            None
        };
        let ov = self.ensure_attrs(h);
        if let Some(slot) = ov.iter_mut().find(|(n, _)| n == name) {
            slot.1 = value.into();
        } else {
            ov.push((name.into(), value.into()));
        }
        self.bump();
        self.record_attributes(h, name, old);
    }

    pub fn remove_attribute(&mut self, h: Handle, name: &str) {
        let old = if self.is_recording() {
            self.get_attribute(h, name).map(std::string::ToString::to_string)
        } else {
            None
        };
        let ov = self.ensure_attrs(h);
        ov.retain(|(n, _)| n != name);
        self.bump();
        self.record_attributes(h, name, old);
    }

    /// Set a live IDL boolean form-state property (e.g. `FormProp::Checked`) on `h`,
    /// independently of the HTML attribute — the Rust analogue of assigning `el.checked`
    /// in the JS runtime. Form-state pseudo-classes (`:checked`/`:selected`/`:disabled`/
    /// `:required`/`:read-only`) read this first and fall back to the attribute when
    /// unset. Bumps the version so cached queries re-evaluate.
    pub fn set_form_property(&mut self, h: Handle, prop: FormProp, value: bool) {
        self.form_props.insert((h, prop), value);
        self.bump();
    }

    /// Clear a live form-state property override, reverting the pseudo-class to reading
    /// the HTML attribute. Bumps the version.
    pub fn clear_form_property(&mut self, h: Handle, prop: FormProp) {
        if self.form_props.remove(&(h, prop)).is_some() {
            self.bump();
        }
    }

    /// The live override for a form-state property, or `None` when unset (⇒ the matcher
    /// reads the HTML attribute).
    #[must_use]
    pub(crate) fn form_property(&self, h: Handle, prop: FormProp) -> Option<bool> {
        self.form_props.get(&(h, prop)).copied()
    }

    /// Ensure a node's child overlay exists (copy buffer children in on first mutate).
    fn ensure_children(&mut self, h: Handle) -> &mut ChildList {
        if !self.children_ov.contains(h) {
            let init: ChildList = self.children(h).into();
            self.children_ov.insert(h, init);
        }
        self.children_ov.get_mut(h).unwrap()
    }

    fn detach(&mut self, child: Handle) {
        if let Some(p) = self.parent(child) {
            let list = self.ensure_children(p);
            if let Some(pos) = list.iter().position(|&x| x == child) {
                list.remove(pos);
            }
        }
    }

    /// Detach every current child of `h`, for the callers that overwrite the whole child
    /// list at once (`textContent` / `innerHTML` setters). Each prior child's `parent_ov`
    /// becomes `None`, so a consumer still holding a handle into the replaced subtree sees
    /// a detached node instead of a stale parent pointer that climbs back into the live
    /// tree. `remove_child` already does this on the one-at-a-time path.
    ///
    /// Allocation-free on the two shapes that matter: an existing overlay is moved out
    /// (`SmallVec`, inline up to 8), and an untouched buffer-backed node walks the `SoA`
    /// sibling chain directly rather than materializing `children()`.
    fn orphan_children(&mut self, h: Handle) {
        if let Some(list) = self.children_ov.get_mut(h) {
            // Leaves an empty overlay entry behind — every caller overwrites it immediately.
            let prior = std::mem::take(list);
            for &c in &prior {
                self.parent_ov.insert(c, None);
            }
            return;
        }
        if self.is_new(h) {
            return;
        }
        let mut c = self.buf.first_child[h.idx()];
        while c >= 0 {
            let next = self.buf.next_sib[c as usize];
            self.parent_ov.insert(Handle(c as u32), None);
            c = next;
        }
    }

    pub fn append_child(&mut self, parent: Handle, child: Handle) {
        self.detach(child);
        self.ensure_children(parent).push(child);
        self.parent_ov.insert(child, Some(parent));
        self.bump();
        self.record_child_list(parent, Some(child), None);
    }

    pub fn insert_before(&mut self, parent: Handle, child: Handle, reference: Option<Handle>) {
        self.detach(child);
        let list = self.ensure_children(parent);
        match reference.and_then(|r| list.iter().position(|&x| x == r)) {
            Some(idx) => list.insert(idx, child),
            None => list.push(child),
        }
        self.parent_ov.insert(child, Some(parent));
        self.bump();
        self.record_child_list(parent, Some(child), None);
    }

    pub fn remove_child(&mut self, parent: Handle, child: Handle) {
        let list = self.ensure_children(parent);
        if let Some(pos) = list.iter().position(|&x| x == child) {
            list.remove(pos);
        }
        self.parent_ov.insert(child, None);
        self.bump();
        self.record_child_list(parent, None, Some(child));
    }

    pub fn set_text_content(&mut self, h: Handle, text: &str) {
        let nt = self.node_type(h);
        if nt == NodeType::Text || nt == NodeType::Comment {
            let old = if self.is_recording() { self.node_value(h) } else { None };
            self.text_ov.insert(h, text.into());
            self.bump();
            self.record_character_data(h, old);
        } else {
            // `removed` (the prior child list) is needed ONLY for a mutation record;
            // the overlay overwrite below ignores it. So snapshot it solely when
            // recording, and avoid cloning `added` on the no-observer path. Snapshot
            // BEFORE orphaning — `orphan_children` moves the child list out.
            let removed = if self.is_recording() { self.children(h) } else { Vec::new() };
            // The prior children leave the tree: clear their parent pointers so none of
            // them keeps reporting `parent() == h` after the overwrite below.
            self.orphan_children(h);
            // replace children with a single text node — but per the DOM spec, the EMPTY string
            // produces NO child node (the element ends up empty). React's createRoot clears the
            // container via `textContent = ''` and relies on it leaving zero children.
            let added = if text.is_empty() {
                Vec::new()
            } else {
                let t = self.create_text_node(text);
                self.parent_ov.insert(t, Some(h));
                vec![t]
            };
            if self.is_recording() {
                self.children_ov.insert(h, added.clone().into());
                self.bump();
                self.record_child_list_batch(h, added, removed);
            } else {
                self.children_ov.insert(h, added.into());
                self.bump();
            }
        }
    }

    // ---- CharacterData mutation (text/comment nodes) -------------------------------------------
    // The DOM's CharacterData methods, operating on the node's text. Offsets/counts are in chars
    // (BMP-correct; matches JS string indexing for the ASCII/typed-text these drive). Records a
    // characterData mutation so a MutationObserver (Lexical's editor model) sees the edit.
    /// Set the text of a text/comment node directly (`CharacterData.data` / `nodeValue` setter).
    pub fn set_node_value(&mut self, h: Handle, text: &str) {
        let nt = self.node_type(h);
        if nt != NodeType::Text && nt != NodeType::Comment {
            return;
        }
        let old = if self.is_recording() { self.node_value(h) } else { None };
        self.text_ov.insert(h, text.into());
        self.bump();
        self.record_character_data(h, old);
    }
    pub fn insert_data(&mut self, h: Handle, offset: usize, data: &str) {
        let chars: Vec<char> = self.node_value(h).unwrap_or_default().chars().collect();
        let off = offset.min(chars.len());
        let mut out: String = chars[..off].iter().collect();
        out.push_str(data);
        out.extend(chars[off..].iter());
        self.set_node_value(h, &out);
    }
    pub fn delete_data(&mut self, h: Handle, offset: usize, count: usize) {
        let chars: Vec<char> = self.node_value(h).unwrap_or_default().chars().collect();
        let off = offset.min(chars.len());
        let end = off.saturating_add(count).min(chars.len());
        let mut out: String = chars[..off].iter().collect();
        out.extend(chars[end..].iter());
        self.set_node_value(h, &out);
    }
    pub fn append_data(&mut self, h: Handle, data: &str) {
        let mut s = self.node_value(h).unwrap_or_default();
        s.push_str(data);
        self.set_node_value(h, &s);
    }
    pub fn replace_data(&mut self, h: Handle, offset: usize, count: usize, data: &str) {
        let chars: Vec<char> = self.node_value(h).unwrap_or_default().chars().collect();
        let off = offset.min(chars.len());
        let end = off.saturating_add(count).min(chars.len());
        let mut out: String = chars[..off].iter().collect();
        out.push_str(data);
        out.extend(chars[end..].iter());
        self.set_node_value(h, &out);
    }
    pub fn substring_data(&self, h: Handle, offset: usize, count: usize) -> String {
        let chars: Vec<char> = self.node_value(h).unwrap_or_default().chars().collect();
        let off = offset.min(chars.len());
        let end = off.saturating_add(count).min(chars.len());
        chars[off..end].iter().collect()
    }
    /// Split a text node at `offset`: truncate this node to the head, create a sibling holding the
    /// tail (inserted right after), and return the new node's handle.
    pub fn split_text(&mut self, h: Handle, offset: usize) -> Handle {
        let chars: Vec<char> = self.node_value(h).unwrap_or_default().chars().collect();
        let off = offset.min(chars.len());
        let head: String = chars[..off].iter().collect();
        let tail: String = chars[off..].iter().collect();
        self.set_node_value(h, &head);
        let new = self.create_text_node(&tail);
        if let Some(p) = self.parent(h) {
            let next = self.next_sibling(h);
            self.insert_before(p, new, next);
        }
        new
    }

    // ------------------------------------------------------------- creation
    fn push_node(&mut self, node: NewNode) -> Handle {
        let h = Handle(self.node_count());
        self.new_nodes.push(node);
        h
    }

    pub fn create_element(&mut self, tag: &str) -> Handle {
        let h = self.push_node(NewNode::Element { name: tag.to_ascii_lowercase().into(), ns: Namespace::Html });
        self.children_ov.insert(h, ChildList::new());
        self.attrs_ov.insert(h, AttrList::new());
        self.bump();
        h
    }

    pub fn create_element_ns(&mut self, tag: &str, ns: Namespace) -> Handle {
        let h = self.push_node(NewNode::Element { name: tag.into(), ns });
        self.children_ov.insert(h, ChildList::new());
        self.attrs_ov.insert(h, AttrList::new());
        self.bump();
        h
    }

    pub fn create_text_node(&mut self, data: &str) -> Handle {
        let h = self.push_node(NewNode::Text(data.into()));
        self.bump();
        h
    }

    pub fn create_comment(&mut self, data: &str) -> Handle {
        let h = self.push_node(NewNode::Comment(data.into()));
        self.bump();
        h
    }

    /// Parse `html` as a fragment in the context of element `h` and replace its
    /// children (innerHTML setter). Imported nodes become owned (new) nodes.
    pub fn set_inner_html(&mut self, h: Handle, html: &str) {
        let ctx = self.local_name(h).unwrap_or("body").to_string();
        let frag = core::parse_html_fragment_context(html, &ctx);
        // The prior children are replaced wholesale — detach them first so a retained
        // handle into the old subtree reports no parent rather than pointing back at `h`.
        self.orphan_children(h);
        let mut kids = Vec::with_capacity(frag.children.len());
        for c in &frag.children {
            kids.push(self.import_node(c));
        }
        for &k in &kids {
            self.parent_ov.insert(k, Some(h));
        }
        self.children_ov.insert(h, kids.into());
        self.bump();
    }

    // ---- ChildNode / ParentNode manipulation (spec methods) -----------------------------------
    /// Insert `nodes` (in order) immediately before `h` within `h`'s parent. No-op if detached.
    pub fn before(&mut self, h: Handle, nodes: &[Handle]) {
        let Some(p) = self.parent(h) else { return };
        for &n in nodes {
            self.insert_before(p, n, Some(h));
        }
    }
    /// The "viable next sibling" used by `after`/`replaceWith`: `h`'s first following
    /// sibling that is NOT itself in `nodes`. Per the DOM spec these methods anchor the
    /// insertion at this node, so a following sibling that is being moved by the call is
    /// skipped over (using it as the anchor would corrupt the order once it's detached).
    fn viable_next_sibling(&self, h: Handle, nodes: &[Handle]) -> Option<Handle> {
        let p = self.parent(h)?;
        let sibs = self.children(p);
        let i = sibs.iter().position(|&x| x == h)?;
        sibs[i + 1..].iter().copied().find(|s| !nodes.contains(s))
    }
    /// Insert `nodes` (in order) immediately after `h` within `h`'s parent.
    pub fn after(&mut self, h: Handle, nodes: &[Handle]) {
        let Some(p) = self.parent(h) else { return };
        let next = self.viable_next_sibling(h, nodes);
        for &n in nodes {
            self.insert_before(p, n, next);
        }
    }
    /// Replace `h` with `nodes` (in order) in `h`'s parent.
    pub fn replace_with(&mut self, h: Handle, nodes: &[Handle]) {
        let Some(p) = self.parent(h) else { return };
        let next = self.viable_next_sibling(h, nodes);
        // Insert before the viable next sibling so order is preserved, then remove h. (If a
        // node in `nodes` is h itself it is first detached by insert_before — harmless; spec
        // dedups similarly.)
        for &n in nodes {
            if n != h {
                self.insert_before(p, n, next);
            }
        }
        self.remove_child(p, h);
    }
    /// Replace all children of `h` with `nodes` (in order).
    pub fn replace_children(&mut self, h: Handle, nodes: &[Handle]) {
        for c in self.children(h) {
            self.remove_child(h, c);
        }
        for &n in nodes {
            self.append_child(h, n);
        }
    }
    /// `insertAdjacentElement(position, element)` — position is beforebegin | afterbegin | beforeend
    /// | afterend (case-insensitive). Returns true if inserted.
    pub fn insert_adjacent_element(&mut self, h: Handle, position: &str, el: Handle) -> bool {
        match position.to_ascii_lowercase().as_str() {
            "beforebegin" => { if self.parent(h).is_some() { self.before(h, &[el]); true } else { false } }
            "afterend" => { if self.parent(h).is_some() { self.after(h, &[el]); true } else { false } }
            "afterbegin" => { let first = self.first_child(h); self.insert_before(h, el, first); true }
            "beforeend" => { self.append_child(h, el); true }
            _ => false,
        }
    }
    /// `insertAdjacentHTML(position, html)` — parse `html` as a fragment (in `h`'s context for
    /// beforeend/afterbegin, else the parent's) and insert the resulting nodes at `position`.
    pub fn insert_adjacent_html(&mut self, h: Handle, position: &str, html: &str) {
        let pos = position.to_ascii_lowercase();
        let ctx_h = match pos.as_str() {
            "beforebegin" | "afterend" => self.parent(h).unwrap_or(h),
            _ => h,
        };
        let ctx = self.local_name(ctx_h).unwrap_or("body").to_string();
        let frag = core::parse_html_fragment_context(html, &ctx);
        let kids: Vec<Handle> = frag.children.iter().map(|c| self.import_node(c)).collect();
        match pos.as_str() {
            "beforebegin" => self.before(h, &kids),
            "afterend" => self.after(h, &kids),
            "afterbegin" => { let first = self.first_child(h); for &k in &kids { self.insert_before(h, k, first); } }
            _ => for &k in &kids { self.append_child(h, k); }, // beforeend (default)
        }
    }
    /// `toggleAttribute(name, force?)` — add when absent (or force=Some(true)), remove when present
    /// (or force=Some(false)). Returns whether the attribute is present afterward.
    pub fn toggle_attribute(&mut self, h: Handle, name: &str, force: Option<bool>) -> bool {
        let present = self.has_attribute(h, name);
        let want = force.unwrap_or(!present);
        if want {
            if !present { self.set_attribute(h, name, ""); }
            true
        } else {
            if present { self.remove_attribute(h, name); }
            false
        }
    }
    /// `getAttributeNS(namespace, localName)` — our attrs are keyed by qualified name; match the
    /// local name directly, then any `prefix:localName` form (covers SVG `xlink:href` etc.).
    pub fn get_attribute_ns(&self, h: Handle, _ns: Option<&str>, local: &str) -> Option<String> {
        if let Some(v) = self.get_attribute(h, local) { return Some(v.to_string()); }
        let suffix = format!(":{local}");
        for (n, v) in self.attributes(h) {
            if n == local || n.ends_with(&suffix) { return Some(v); }
        }
        None
    }

    /// Recursively import a parsed `core::Node` as owned nodes. Returns its handle.
    fn import_node(&mut self, n: &core::Node) -> Handle {
        match NodeType::from_u8(n.node_type) {
            NodeType::Element => {
                let ns = ns_id(&n.namespace);
                let h = self.create_element_ns(&n.name, ns);
                for a in &n.attrs {
                    self.set_attribute(h, &a.name, &a.value);
                }
                let mut kids = Vec::with_capacity(n.children.len());
                for c in &n.children {
                    kids.push(self.import_node(c));
                }
                for &k in &kids {
                    self.parent_ov.insert(k, Some(h));
                }
                self.children_ov.insert(h, kids.into());
                h
            }
            NodeType::Comment => self.create_comment(&n.value),
            _ => self.create_text_node(&n.value),
        }
    }

    /// Force every buffer-backed node into the owned overlay (attrs + children).
    /// Defeats laziness on purpose — only for the lazy-vs-eager A/B bench. After
    /// this, reads go through the `HashMap` overlay instead of the zero-alloc buffer.
    pub fn force_inflate_all(&mut self) {
        for h in (0..self.buf_len).map(Handle) {
            if self.node_type(h) == NodeType::Element {
                self.ensure_attrs(h);
            }
            self.ensure_children(h);
        }
    }

    // --------------------------------------------------------------- shadow DOM
    /// Attach a shadow root to `host` and return its handle (a FRAGMENT node).
    /// The shadow root is NOT a light child of the host — `children(host)` never
    /// includes it (encapsulation is free: queries over the light tree skip it).
    /// Populate it via `set_inner_html(shadow_root, ...)` or `append_child`.
    pub fn attach_shadow(&mut self, host: Handle) -> Handle {
        let sr = self.push_node(NewNode::Fragment);
        self.children_ov.insert(sr, ChildList::new());
        self.shadow_root_of_host.insert(host, sr);
        self.host_of_shadow_root.insert(sr, host);
        self.bump();
        sr
    }

    pub fn shadow_root(&self, host: Handle) -> Option<Handle> {
        self.shadow_root_of_host.get(&host).copied()
    }
    pub fn shadow_host(&self, shadow_root: Handle) -> Option<Handle> {
        self.host_of_shadow_root.get(&shadow_root).copied()
    }
    pub fn is_shadow_root(&self, h: Handle) -> bool {
        self.host_of_shadow_root.contains_key(&h)
    }

    /// The shadow root that contains `h`, if any (walk to the top of `h`'s tree;
    /// a node inside a shadow tree tops out at the shadow-root fragment).
    pub fn shadow_root_of(&self, h: Handle) -> Option<Handle> {
        let mut top = h;
        while let Some(p) = self.parent(top) {
            top = p;
        }
        if self.is_shadow_root(top) {
            Some(top)
        } else {
            None
        }
    }

    /// DFS document-order descendants of `root` (excluding `root`).
    pub fn descendants(&self, root: Handle) -> Vec<Handle> {
        let mut out = Vec::new();
        let mut stack: Vec<Handle> = self.children(root).into_iter().rev().collect();
        while let Some(h) = stack.pop() {
            out.push(h);
            for &c in self.children(h).iter().rev() {
                stack.push(c);
            }
        }
        out
    }

    /// Short-circuiting DFS over `root`'s descendant ELEMENTS (excluding `root`):
    /// returns true as soon as `pred` does, WITHOUT materializing a descendant
    /// `Vec`. Children are pushed alloc-free (push then reverse the just-added
    /// segment) so traversal stays document-order. Mirrors the JS `someDescendant`;
    /// the alloc-free alternative to `descendants(h).into_iter().any(...)`.
    /// `root` itself (when an element) or any of its descendants satisfies `pred`.
    pub(crate) fn some_self_or_descendant(
        &self,
        root: Handle,
        mut pred: impl FnMut(Handle) -> bool,
    ) -> bool {
        if self.node_type(root) == NodeType::Element && pred(root) {
            return true;
        }
        self.some_descendant(root, pred)
    }

    pub(crate) fn some_descendant(&self, root: Handle, mut pred: impl FnMut(Handle) -> bool) -> bool {
        let mut stack: Vec<Handle> = Vec::new();
        let base = stack.len();
        self.for_each_child(root, |c| stack.push(c));
        stack[base..].reverse();
        while let Some(h) = stack.pop() {
            if self.node_type(h) == NodeType::Element && pred(h) {
                return true;
            }
            let base = stack.len();
            self.for_each_child(h, |c| stack.push(c));
            stack[base..].reverse();
        }
        false
    }

    /// Light-DOM nodes assigned to `slot` (a `<slot>` inside a shadow tree):
    /// the host's light element children whose `slot` attr equals the slot's
    /// `name` (default "" ↔ unnamed slot). Mirrors `assignedNodes`.
    pub fn assigned_nodes(&self, slot: Handle) -> Vec<Handle> {
        // shadow_root_of yields a real shadow root (is_shadow_root ⇒ host exists),
        // so the only reachable miss is "slot not in any shadow tree".
        let Some(host) = self.shadow_root_of(slot).and_then(|sr| self.shadow_host(sr)) else { return Vec::new() };
        let slot_name = self.get_attribute(slot, "name").unwrap_or("");
        self.children(host)
            .into_iter()
            .filter(|&c| self.node_type(c) == NodeType::Element)
            .filter(|&c| self.get_attribute(c, "slot").unwrap_or("") == slot_name)
            .collect()
    }
}

fn ns_id(namespace: &str) -> Namespace {
    match namespace {
        "svg" => Namespace::Svg,
        "math" => Namespace::MathMl,
        _ => Namespace::Html,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(html: &str) -> Tree {
        Tree::parse(html)
    }

    #[test]
    fn reads_structure_from_buffer() {
        let tree = t("<div id=a><span class=x>hi</span><p>yo</p></div>");
        // find the div
        let mut div = None;
        for h in tree.handles() {
            if tree.local_name(h) == Some("div") {
                div = Some(h);
            }
        }
        let div = div.unwrap();
        assert_eq!(tree.tag_name(div).as_deref(), Some("DIV"));
        assert_eq!(tree.get_attribute(div, "id"), Some("a"));
        let kids = tree.children(div);
        assert_eq!(kids.len(), 2);
        assert_eq!(tree.local_name(kids[0]), Some("span"));
        assert_eq!(tree.local_name(kids[1]), Some("p"));
        assert_eq!(tree.text_content(div), "hiyo");
    }

    #[test]
    fn set_attribute_is_cow_and_bumps_version() {
        let mut tree = t("<div id=a></div>");
        let div = tree.handles().find(|&h| tree.local_name(h) == Some("div")).unwrap();
        let v0 = tree.version;
        tree.set_attribute(div, "class", "card");
        assert!(tree.version > v0);
        assert_eq!(tree.get_attribute(div, "class"), Some("card"));
        // original buffer attr still readable through overlay
        assert_eq!(tree.get_attribute(div, "id"), Some("a"));
    }

    #[test]
    fn append_and_remove_child() {
        let mut tree = t("<div></div>");
        let div = tree.handles().find(|&h| tree.local_name(h) == Some("div")).unwrap();
        let span = tree.create_element("span");
        tree.append_child(div, span);
        assert_eq!(tree.children(div), vec![span]);
        assert_eq!(tree.parent(span), Some(div));
        assert_eq!(tree.tag_name(span).as_deref(), Some("SPAN"));
        tree.remove_child(div, span);
        assert!(tree.children(div).is_empty());
        assert_eq!(tree.parent(span), None);
    }

    #[test]
    fn create_text_and_set_text_content() {
        let mut tree = t("<p>old</p>");
        let p = tree.handles().find(|&h| tree.local_name(h) == Some("p")).unwrap();
        tree.set_text_content(p, "new");
        assert_eq!(tree.text_content(p), "new");
    }

    #[test]
    fn dump_accessors_doctype_prefix_label() {
        // doctype name + public/system ids
        let dt = t("<!DOCTYPE html PUBLIC \"-//W3C//DTD\" \"sys.dtd\"><html></html>");
        let doctype = dt.handles().find(|&h| dt.node_type(h) == NodeType::Doctype).unwrap();
        assert_eq!(dt.doctype_name(doctype), Some("html"));
        assert_eq!(dt.doctype_public_id(doctype), Some("-//W3C//DTD"));
        assert_eq!(dt.doctype_system_id(doctype), Some("sys.dtd"));
        // plain doctype: empty ids
        let dt2 = t("<!DOCTYPE html><html></html>");
        let d2 = dt2.handles().find(|&h| dt2.node_type(h) == NodeType::Doctype).unwrap();
        assert_eq!(dt2.doctype_public_id(d2), Some(""));
        // doctype accessors return None for non-doctype + new nodes
        let html = dt2.get_elements_by_tag_name("html")[0];
        assert_eq!(dt2.doctype_name(html), None);
        let mut m = t("<div></div>");
        let txt = m.create_text_node("x");
        assert_eq!(m.doctype_name(txt), None);

        // node_label: element, template-content fragment, and None for text
        let tpl = t("<template><i></i></template>");
        let frag = tpl.handles().find(|&h| tpl.node_type(h) == NodeType::Fragment).unwrap();
        assert_eq!(tpl.node_label(frag), Some("content"));
        let i = tpl.handles().find(|&h| tpl.local_name(h) == Some("i")).unwrap();
        assert_eq!(tpl.node_label(i), Some("i"));
        let txt2 = tpl.handles().find(|&h| tpl.node_type(h) == NodeType::Text);
        if let Some(tx) = txt2 {
            assert_eq!(tpl.node_label(tx), None);
        }
        // node_label for a created (is_new) element + a created fragment-less text
        let fresh = m.create_element("span");
        assert_eq!(m.node_label(fresh), Some("span"));
        assert_eq!(m.node_label(txt), None);

        // for_each_attr_full yields foreign-content prefix from the buffer
        let svg = t("<svg><a xlink:href=\"u\" id=\"k\"></a></svg>");
        let a = svg.handles().find(|&h| svg.local_name(h) == Some("a")).unwrap();
        let mut got = Vec::new();
        svg.for_each_attr_full(a, |p, n, v| got.push(format!("{p}|{n}={v}")));
        assert!(got.contains(&"xlink|href=u".to_string()));
        assert!(got.contains(&"|id=k".to_string()));
        // overlay + is_new paths → empty prefix / no calls
        let mut o = t("<b></b>");
        let b = o.handles().find(|&h| o.local_name(h) == Some("b")).unwrap();
        o.set_attribute(b, "class", "c");
        let mut ov = Vec::new();
        o.for_each_attr_full(b, |p, n, v| ov.push(format!("{p}|{n}={v}")));
        assert_eq!(ov, vec!["|class=c".to_string()]);
        let fresh_txt = o.create_text_node("z");
        o.for_each_attr_full(fresh_txt, |_, _, _| panic!("text has no attrs"));
    }

    #[test]
    fn for_each_attr_borrows_both_sources() {
        let tree = t("<a href=/x data-k=v>hi</a>");
        let a = tree.handles().find(|&h| tree.local_name(h) == Some("a")).unwrap();
        let mut buf = Vec::new();
        tree.for_each_attr(a, |n, v| buf.push(format!("{n}={v}")));
        assert_eq!(buf, vec!["href=/x".to_string(), "data-k=v".to_string()]);
        // overlay source after a mutation
        let mut t2 = t("<b>x</b>");
        let b = t2.handles().find(|&h| t2.local_name(h) == Some("b")).unwrap();
        t2.set_attribute(b, "class", "y");
        let mut ov = Vec::new();
        t2.for_each_attr(b, |n, v| ov.push(format!("{n}={v}")));
        assert_eq!(ov, vec!["class=y".to_string()]);
        // buffer element with NO attributes (start < 0) → no calls
        let t3 = t("<i>x</i>");
        let i = t3.handles().find(|&h| t3.local_name(h) == Some("i")).unwrap();
        let mut count = 0;
        t3.for_each_attr(i, |_, _| count += 1);
        assert_eq!(count, 0);
        // created text node (is_new, no attrs overlay) → hits the is_new return
        let mut t4 = t("<div></div>");
        let txt = t4.create_text_node("hello");
        t4.for_each_attr(txt, |_, _| count += 1);
        assert_eq!(count, 0);
    }

    #[test]
    fn some_descendant_short_circuits_over_elements() {
        let tree = t("<div id=root><section><span class=hit>x</span><b>y</b></section></div>");
        let root = tree.handles().find(|&h| tree.local_name(h) == Some("div")).unwrap();
        // finds a descendant element matching the predicate
        assert!(tree.some_descendant(root, |d| tree.get_attribute(d, "class") == Some("hit")));
        // no matching descendant → false
        assert!(!tree.some_descendant(root, |d| tree.local_name(d) == Some("nope")));
        // visits ONLY elements: a text node is never passed to the predicate
        let mut saw_non_element = false;
        tree.some_descendant(root, |d| {
            if tree.node_type(d) != NodeType::Element {
                saw_non_element = true;
            }
            false
        });
        assert!(!saw_non_element);
        // short-circuit: stops at the first match (predicate call count bounded)
        let mut calls = 0;
        let found = tree.some_descendant(root, |d| {
            calls += 1;
            tree.local_name(d) == Some("section")
        });
        assert!(found);
        assert_eq!(calls, 1, "section is the first descendant element → exactly one call");
        // a node with no descendants → false, no calls
        let empty = tree.handles().find(|&h| tree.local_name(h) == Some("b")).unwrap();
        let mut ec = 0;
        assert!(!tree.some_descendant(empty, |_| { ec += 1; true }));
        assert_eq!(ec, 0);
    }

    #[test]
    fn for_each_child_walks_buffer_overlay_and_new() {
        // buffer chain (first_child/next_sib): matches children() exactly
        let tree = t("<ul><li>1</li><li>2</li><li>3</li></ul>");
        let ul = tree.handles().find(|&h| tree.local_name(h) == Some("ul")).unwrap();
        let mut walked = Vec::new();
        tree.for_each_child(ul, |c| walked.push(c));
        assert_eq!(walked, tree.children(ul));
        // overlay source after a structural mutation
        let mut t2 = t("<div><a></a></div>");
        let div = t2.handles().find(|&h| t2.local_name(h) == Some("div")).unwrap();
        let b = t2.create_element("b");
        t2.append_child(div, b);
        let mut ov = Vec::new();
        t2.for_each_child(div, |c| ov.push(c));
        assert_eq!(ov, t2.children(div));
        // is_new node with no overlay (a created text node) → no calls
        let txt = t2.create_text_node("x");
        let mut count = 0;
        t2.for_each_child(txt, |_| count += 1);
        assert_eq!(count, 0);
    }

    #[test]
    fn set_inner_html_imports_fragment() {
        let mut tree = t("<div></div>");
        let div = tree.handles().find(|&h| tree.local_name(h) == Some("div")).unwrap();
        tree.set_inner_html(div, "<span class=a>hi</span><b>x</b>");
        let kids = tree.children(div);
        assert_eq!(kids.len(), 2);
        assert_eq!(tree.local_name(kids[0]), Some("span"));
        assert_eq!(tree.get_attribute(kids[0], "class"), Some("a"));
        assert_eq!(tree.local_name(kids[1]), Some("b"));
        assert_eq!(tree.text_content(div), "hix");
        assert_eq!(tree.parent(kids[0]), Some(div));
    }

    #[test]
    fn shadow_root_encapsulation_and_slots() {
        let mut tree = t("<my-card><span slot=title>Hi</span><p>light</p></my-card>");
        let host = tree.handles().find(|&h| tree.local_name(h) == Some("my-card")).unwrap();
        let sr = tree.attach_shadow(host);
        tree.set_inner_html(sr, "<style>:host{color:red}</style><slot name=title></slot><slot></slot>");
        // encapsulation: shadow content is NOT in the light tree
        assert!(tree.shadow_root_of_host.contains_key(&host));
        assert_eq!(tree.query_selector_all("slot").len(), 0); // light query skips shadow
        // shadow_root_of resolves for shadow-internal nodes
        let slots = tree.descendants(sr).into_iter().filter(|&h| tree.local_name(h) == Some("slot")).collect::<Vec<_>>();
        assert_eq!(slots.len(), 2);
        assert_eq!(tree.shadow_root_of(slots[0]), Some(sr));
        assert_eq!(tree.shadow_host(sr), Some(host));
        // assignedNodes: the named slot gets the span[slot=title]; default slot gets <p>
        let named = slots.iter().find(|&&s| tree.get_attribute(s, "name") == Some("title")).copied().unwrap();
        let default = slots.iter().find(|&&s| tree.get_attribute(s, "name").is_none()).copied().unwrap();
        let an = tree.assigned_nodes(named);
        assert_eq!(an.len(), 1);
        assert_eq!(tree.local_name(an[0]), Some("span"));
        assert_eq!(tree.assigned_nodes(default).len(), 1); // the <p>
    }

    #[test]
    fn siblings() {
        let tree = t("<ul><li>1</li><li>2</li><li>3</li></ul>");
        let lis: Vec<_> = tree.handles().filter(|&h| tree.local_name(h) == Some("li")).collect();
        assert_eq!(tree.next_sibling(lis[0]), Some(lis[1]));
        assert_eq!(tree.previous_sibling(lis[2]), Some(lis[1]));
        assert_eq!(tree.next_sibling(lis[2]), None);
    }

    #[test]
    fn first_and_last_child() {
        let mut tree = t("<ul><li>1</li><li>2</li><li>3</li></ul>");
        let ul = tree.handles().find(|&h| tree.local_name(h) == Some("ul")).unwrap();
        let kids = tree.children(ul);
        assert_eq!(tree.first_child(ul), Some(kids[0]));
        assert_eq!(tree.last_child(ul), Some(kids[2]));
        // previous_sibling of the first child is None (index 0 branch)
        assert_eq!(tree.previous_sibling(kids[0]), None);
        // empty element: no first/last child
        let empty = tree.create_element("div");
        assert_eq!(tree.first_child(empty), None);
        assert_eq!(tree.last_child(empty), None);
    }

    #[test]
    fn create_element_ns_keeps_case_and_namespace() {
        let mut tree = t("<div></div>");
        // SVG-namespace element: mixed-case local name preserved,
        // tag_name returns as-is (non-HTML namespace branch).
        let g = tree.create_element_ns("linearGradient", Namespace::Svg);
        assert_eq!(tree.namespace(g), Namespace::Svg);
        assert_eq!(tree.local_name(g), Some("linearGradient"));
        assert_eq!(tree.tag_name(g).as_deref(), Some("linearGradient"));
        // math namespace
        let m = tree.create_element_ns("mi", Namespace::MathMl);
        assert_eq!(tree.namespace(m), Namespace::MathMl);
        assert_eq!(tree.tag_name(m).as_deref(), Some("mi"));
    }

    #[test]
    fn new_node_reads_are_detached_and_empty() {
        let mut tree = t("<div></div>");
        let span = tree.create_element("span");
        // a freshly created (new) node with no parent overlay → detached
        assert_eq!(tree.parent(span), None);
        // get_attribute / has_attribute on an attr-less new node
        assert_eq!(tree.get_attribute(span, "id"), None);
        assert!(!tree.has_attribute(span, "id"));
        tree.set_attribute(span, "id", "x");
        assert!(tree.has_attribute(span, "id"));
        // attributes() on a brand-new text node (is_new, no attrs_ov) → empty
        let txt = tree.create_text_node("hi");
        assert!(tree.attributes(txt).is_empty());
        // get_attribute on a new node lacking an attr overlay (text node) → None
        assert_eq!(tree.get_attribute(txt, "foo"), None);
    }

    #[test]
    fn node_value_text_comment_and_none() {
        let mut tree = t("<p>hi</p><!--c-->");
        // node_value on an element → None (not text/comment)
        let p = tree.handles().find(|&h| tree.local_name(h) == Some("p")).unwrap();
        assert_eq!(tree.node_value(p), None);
        // freshly created text node reads its data via is_new branch
        let txt = tree.create_text_node("hello");
        assert_eq!(tree.node_value(txt).as_deref(), Some("hello"));
        // a comment node from the buffer
        let comment = tree.handles().find(|&h| tree.node_type(h) == NodeType::Comment).unwrap();
        assert_eq!(tree.node_value(comment).as_deref(), Some("c"));
        // text_content of a comment goes through the text/comment branch
        assert_eq!(tree.text_content(comment), "c");
        // text_ov branch: mutate text content of the text node, read back
        tree.set_text_content(txt, "changed");
        assert_eq!(tree.node_value(txt).as_deref(), Some("changed"));
    }

    #[test]
    fn empty_buffer_text_node_value() {
        // An empty text node (text_id < 0) → node_value is empty string.
        let tree = t("<p></p>");
        // collect_text skips comments: a comment inside an element is ignored.
        let p = tree.handles().find(|&h| tree.local_name(h) == Some("p")).unwrap();
        assert_eq!(tree.text_content(p), "");
    }

    #[test]
    fn text_content_skips_comments() {
        // The NodeType::Comment arm of collect_text is exercised: comment text is NOT
        // concatenated into an element's text_content.
        let tree = t("<div>a<!--ignored-->b</div>");
        let div = tree.handles().find(|&h| tree.local_name(h) == Some("div")).unwrap();
        assert_eq!(tree.text_content(div), "ab");
    }

    #[test]
    fn set_attribute_updates_existing_slot() {
        let mut tree = t("<div id=a></div>");
        let div = tree.handles().find(|&h| tree.local_name(h) == Some("div")).unwrap();
        // overwrite an existing attribute (the find-and-update slot branch)
        tree.set_attribute(div, "id", "b");
        assert_eq!(tree.get_attribute(div, "id"), Some("b"));
        assert_eq!(tree.attributes(div).len(), 1);
    }

    #[test]
    fn remove_attribute_works() {
        let mut tree = t("<div id=a class=x></div>");
        let div = tree.handles().find(|&h| tree.local_name(h) == Some("div")).unwrap();
        let v0 = tree.version;
        tree.remove_attribute(div, "id");
        assert!(tree.version > v0);
        assert!(!tree.has_attribute(div, "id"));
        assert_eq!(tree.get_attribute(div, "class"), Some("x"));
    }

    #[test]
    fn insert_before_with_reference_and_append() {
        let mut tree = t("<ul><li>1</li><li>2</li></ul>");
        let ul = tree.handles().find(|&h| tree.local_name(h) == Some("ul")).unwrap();
        let kids = tree.children(ul);
        let (a, b) = (kids[0], kids[1]);
        // insert before a reference node (the Some(idx) branch)
        let x = tree.create_element("li");
        tree.insert_before(ul, x, Some(b));
        assert_eq!(tree.children(ul), vec![a, x, b]);
        assert_eq!(tree.parent(x), Some(ul));
        // insert with None reference → append (the None branch)
        let y = tree.create_element("li");
        tree.insert_before(ul, y, None);
        assert_eq!(tree.children(ul), vec![a, x, b, y]);
        // detach-with-parent: re-inserting x moves it (detach removes from parent)
        tree.insert_before(ul, x, Some(a));
        assert_eq!(tree.children(ul), vec![x, a, b, y]);
        // reference that is not a child → falls through to append
        let z = tree.create_element("li");
        tree.insert_before(ul, z, Some(Handle(9999)));
        assert_eq!(tree.children(ul).last().copied(), Some(z));
    }

    #[test]
    fn set_text_content_on_element_replaces_children() {
        let mut tree = t("<div><span>old</span></div>");
        let div = tree.handles().find(|&h| tree.local_name(h) == Some("div")).unwrap();
        // element branch: replaced with a single text node
        tree.set_text_content(div, "new");
        assert_eq!(tree.text_content(div), "new");
        let kids = tree.children(div);
        assert_eq!(kids.len(), 1);
        assert_eq!(tree.node_type(kids[0]), NodeType::Text);
        assert_eq!(tree.parent(kids[0]), Some(div));
    }

    // A subtree replaced wholesale must leave its old children DETACHED: a consumer that
    // still holds a handle into the replaced subtree and walks `parent()` upward has to
    // terminate at `None`, not climb back into the live tree (issue #4).
    #[test]
    fn set_text_content_orphans_the_replaced_children() {
        let mut tree = t("<div><span>old</span></div>");
        let div = tree.handles().find(|&h| tree.local_name(h) == Some("div")).unwrap();
        let span = tree.handles().find(|&h| tree.local_name(h) == Some("span")).unwrap();
        let inner = tree.children(span)[0];
        assert_eq!(tree.parent(span), Some(div));
        // buffer-backed node with no child overlay yet -> the `SoA` sibling-chain walk
        tree.set_text_content(div, "new");
        assert_eq!(tree.parent(span), None, "replaced child is detached");
        assert!(!tree.children(div).contains(&span));
        // the detached subtree itself stays intact — only its top link is cut
        assert_eq!(tree.parent(inner), Some(span));
        // and the empty-string (React container clear) path orphans too
        let mut tree = t("<div><span>old</span></div>");
        let div = tree.handles().find(|&h| tree.local_name(h) == Some("div")).unwrap();
        let span = tree.handles().find(|&h| tree.local_name(h) == Some("span")).unwrap();
        tree.set_text_content(div, "");
        assert!(tree.children(div).is_empty());
        assert_eq!(tree.parent(span), None);
    }

    #[test]
    fn set_inner_html_orphans_the_replaced_children() {
        let mut tree = t("<div></div>");
        let div = tree.handles().find(|&h| tree.local_name(h) == Some("div")).unwrap();
        // append first so `div` has a child OVERLAY — the other `orphan_children` branch
        let kid = tree.create_element("p");
        tree.append_child(div, kid);
        assert_eq!(tree.parent(kid), Some(div));
        tree.set_inner_html(div, "<b>x</b>");
        assert_eq!(tree.parent(kid), None, "replaced child is detached");
        assert_eq!(tree.children(div).len(), 1);
        assert_eq!(tree.parent(tree.children(div)[0]), Some(div));
    }

    #[test]
    fn orphan_children_skips_a_created_node_with_no_child_overlay() {
        // Defensive branch: a CREATED node without a child overlay (a text node — only
        // created elements get one) has no `SoA` slot, so the chain walk must be skipped
        // rather than indexing the buffer out of range.
        let mut tree = t("<div></div>");
        let txt = tree.create_text_node("x");
        tree.set_inner_html(txt, "<b>y</b>");
        assert_eq!(tree.children(txt).len(), 1);
    }

    #[test]
    fn create_comment_node() {
        let mut tree = t("<div></div>");
        let v0 = tree.version;
        let c = tree.create_comment("note");
        assert!(tree.version > v0);
        assert_eq!(tree.node_type(c), NodeType::Comment);
        assert_eq!(tree.node_value(c).as_deref(), Some("note"));
    }

    #[test]
    fn set_inner_html_imports_comment_and_svg() {
        let mut tree = t("<div></div>");
        let div = tree.handles().find(|&h| tree.local_name(h) == Some("div")).unwrap();
        // imports an element, a comment (NodeType::Comment arm), and an svg element (ns).
        tree.set_inner_html(div, "<p>hi</p><!--c--><svg><circle r=5></circle></svg>");
        let kids = tree.children(div);
        // p, comment, svg
        let comment = kids.iter().copied().find(|&k| tree.node_type(k) == NodeType::Comment).unwrap();
        assert_eq!(tree.node_value(comment).as_deref(), Some("c"));
        let svg = kids.iter().copied().find(|&k| tree.local_name(k) == Some("svg")).unwrap();
        assert_eq!(tree.namespace(svg), Namespace::Svg);
        // svg child carries the svg namespace too
        let circle = tree.children(svg).into_iter().find(|&k| tree.local_name(k) == Some("circle")).unwrap();
        assert_eq!(tree.namespace(circle), Namespace::Svg);
    }

    #[test]
    fn force_inflate_all_preserves_reads() {
        let mut tree = t("<div id=a><span class=x>hi</span></div>");
        let div = tree.handles().find(|&h| tree.local_name(h) == Some("div")).unwrap();
        tree.force_inflate_all();
        // after inflation reads go through overlays but stay correct
        assert_eq!(tree.get_attribute(div, "id"), Some("a"));
        let kids = tree.children(div);
        assert_eq!(tree.local_name(kids[0]), Some("span"));
        assert_eq!(tree.get_attribute(kids[0], "class"), Some("x"));
        assert_eq!(tree.text_content(div), "hi");
    }

    #[test]
    fn template_content_excluded_from_text_content() {
        // A <template>'s parsed content lives in a separate `content` document fragment,
        // NOT among the element's child nodes. Per the DOM, `template.textContent` is ""
        // and an ancestor's textContent does not include the template's content. (The JS
        // runtime mirrors this by excluding the synthetic content fragment from its
        // child iteration.)
        let tree = t("<div>before<template><p>hidden</p></template>after</div>");
        let div = tree.handles().find(|&h| tree.local_name(h) == Some("div")).unwrap();
        assert_eq!(tree.text_content(div), "beforeafter");
        let tpl = tree.handles().find(|&h| tree.local_name(h) == Some("template")).unwrap();
        assert_eq!(tree.text_content(tpl), "");
    }

    #[test]
    fn after_and_replace_with_nodes_containing_next_sibling() {
        // DOM spec: ChildNode.after()/replaceWith() insert before the "viable next
        // sibling" — the first following sibling NOT in the argument list. When the
        // immediate next sibling IS one of the inserted nodes, it must be skipped.
        //
        // <a><b><c> ; a.after(b, x) → a, b, x, c
        let mut tree = t("<div><a></a><b></b><c></c></div>");
        let a = tree.handles().find(|&h| tree.local_name(h) == Some("a")).unwrap();
        let b = tree.handles().find(|&h| tree.local_name(h) == Some("b")).unwrap();
        let x = tree.create_element("x");
        tree.after(a, &[b, x]);
        let div = tree.handles().find(|&h| tree.local_name(h) == Some("div")).unwrap();
        let names: Vec<_> = tree.children(div).iter().map(|&h| tree.local_name(h).unwrap().to_string()).collect();
        assert_eq!(names, vec!["a", "b", "x", "c"], "after() must skip a next sibling that is itself being inserted");

        // <a><b><c> ; a.replaceWith(b, x) → b, x, c
        let mut tree2 = t("<div><a></a><b></b><c></c></div>");
        let a2 = tree2.handles().find(|&h| tree2.local_name(h) == Some("a")).unwrap();
        let b2 = tree2.handles().find(|&h| tree2.local_name(h) == Some("b")).unwrap();
        let x2 = tree2.create_element("x");
        tree2.replace_with(a2, &[b2, x2]);
        let div2 = tree2.handles().find(|&h| tree2.local_name(h) == Some("div")).unwrap();
        let names2: Vec<_> = tree2.children(div2).iter().map(|&h| tree2.local_name(h).unwrap().to_string()).collect();
        assert_eq!(names2, vec!["b", "x", "c"], "replaceWith() must skip a next sibling that is itself being inserted");
    }

    #[test]
    fn assigned_nodes_without_shadow_is_empty() {
        let tree = t("<div><span>x</span></div>");
        // a node not inside any shadow tree → shadow_root_of None → empty
        let span = tree.handles().find(|&h| tree.local_name(h) == Some("span")).unwrap();
        assert!(tree.assigned_nodes(span).is_empty());
    }
}
