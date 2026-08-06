// Phase 3, test strategy #3 + #4: liveness/identity property tests turning each
// known happy-dom failure class into a regression test, plus the invariant
// "fallback audit" — lazy partial reads must never differ from full eager reads.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { createEnvironment } from '../src/runtime/index.mjs';

const require = createRequire(import.meta.url);
const { JSDOM } = require('jsdom');

const canon = (node) => {
  const attrs = Array.from(node.attributes).map((a) => `${a.name}=${a.value}`).sort().join(',');
  let kids = '';
  node.childNodes.forEach((c) => {
    if (c.nodeType === 1) kids += canon(c);
    else if (c.nodeType === 3 && c.data.trim() !== '') kids += `"${c.data.trim()}"`;
  });
  return `${node.localName}[${attrs}]{${kids}}`;
};

// --- known happy-dom failure classes, as regressions (vs jsdom oracle) ---

test('adoption agency algorithm: <a><p></a></p>', () => {
  const html = '<!doctype html><body><a><p></a></p></body>';
  const fast = createEnvironment(html).document;
  const jdom = new JSDOM(html).window.document;
  assert.equal(canon(fast.body), canon(jdom.body));
});

test('table foster-parenting: stray text fostered BEFORE the table (WHATWG spec)', () => {
  // Spec: non-whitespace chars in "in table" mode foster-parent immediately BEFORE
  // the table. turbo-dom (html5ever) matches html5lib-tests here. (This jsdom build
  // places the text after the table — turbo-dom sides with the spec, not jsdom.)
  const html = '<!doctype html><body><table>oops<tr><td>x</td></tr></table></body>';
  const fast = createEnvironment(html).document;
  const kids = Array.from(fast.body.childNodes).filter((n) => n.nodeType === 1 || (n.nodeType === 3 && n.data.trim() !== ''));
  assert.equal(kids[0].nodeType, 3);
  assert.ok(kids[0].data.includes('oops'), 'fostered text first');
  assert.equal(kids[1].localName, 'table', 'table follows the fostered text');
});

test('mis-nested formatting reparented identically to jsdom', () => {
  const html = '<!doctype html><body><b>1<i>2</b>3</i></body>';
  const fast = createEnvironment(html).document;
  const jdom = new JSDOM(html).window.document;
  assert.equal(canon(fast.body), canon(jdom.body));
});

// --- liveness / identity invariants ---

test('childNodes read → mutate → reflected (no stale snapshot)', () => {
  const { document } = createEnvironment('<!doctype html><body><ul><li>a</li></ul></body>');
  const ul = document.querySelector('ul');
  const cn = ul.childNodes;
  assert.equal(cn.length, 1);
  ul.appendChild(document.createElement('li'));
  ul.appendChild(document.createElement('li'));
  assert.equal(cn.length, 3); // same live object reflects both mutations
});

test('node read twice is === (identity memoization)', () => {
  const { document } = createEnvironment('<!doctype html><body><div id="x"><span></span></div></body>');
  assert.equal(document.getElementById('x'), document.querySelector('#x'));
  const s1 = document.querySelector('#x span');
  const s2 = document.querySelector('#x').firstElementChild;
  assert.equal(s1, s2);
});

test('WeakMap keyed by a node survives mutations around it', () => {
  const { document } = createEnvironment('<!doctype html><body><div id="x"></div></body>');
  const x = document.getElementById('x');
  const wm = new WeakMap(); wm.set(x, 42);
  for (let i = 0; i < 10; i++) x.appendChild(document.createElement('b'));
  document.body.insertBefore(document.createElement('p'), x);
  assert.equal(wm.get(document.getElementById('x')), 42);
});

// --- invariant fallback audit: lazy partial reads ≡ full eager reads ---

test('lazy partial read of a deep node === the node from a full traversal', () => {
  const html = '<!doctype html><body><div><section><article><p id="deep">hit</p></article></section></div></body>';
  const { document } = createEnvironment(html);
  // partial path: jump straight to the deep node (inflates only the ancestor chain)
  const viaId = document.getElementById('deep');
  // full path: walk everything first, then locate the same node
  const all = Array.from(document.getElementsByTagName('*'));
  const viaWalk = all.find((e) => e.getAttribute('id') === 'deep');
  assert.equal(viaId, viaWalk, 'partial and full inflation must yield the SAME handle');
  assert.equal(viaId.textContent, 'hit');
});

test('fallback audit: lazy DOM canon === full-eager canon across messy inputs (0 divergences)', () => {
  const inputs = [
    '<table><tr><td>a<td>b</table>',
    '<p>1<p>2<p>3',
    '<ul><li>a<li>b<li>c',
    '<a><div><p></a>',
    '<select><div></div><option>x</select>',
    '<b><i><u>x</b>y</i>z',
  ];
  let divergences = 0;
  for (const frag of inputs) {
    const html = `<!doctype html><body>${frag}</body>`;
    // "lazy": canon walks on demand. "eager": force full inflation first, then canon.
    const lazy = createEnvironment(html).document;
    const lazyCanon = canon(lazy.body);

    const eagerEnv = createEnvironment(html);
    Array.from(eagerEnv.document.getElementsByTagName('*')).forEach((e) => void e.textContent);
    const eagerCanon = canon(eagerEnv.document.body);

    if (lazyCanon !== eagerCanon) divergences++;
  }
  assert.equal(divergences, 0, 'lazy inflation never diverges from eager — no fallback needed');
});

test('query/getElementById caches invalidate on mutation', () => {
  const { document } = createEnvironment('<!doctype html><body><ul><li>a</li></ul></body>');
  const ul = document.querySelector('ul');
  assert.equal(document.querySelectorAll('li').length, 1);   // caches
  ul.appendChild(document.createElement('li'));               // mutate → bump version
  assert.equal(document.querySelectorAll('li').length, 2);   // fresh, not stale
  // getElementById invalidates on add + attribute change + remove
  const x = document.createElement('div'); x.id = 'x'; ul.appendChild(x);
  assert.equal(document.getElementById('x'), x);
  x.setAttribute('id', 'y');
  assert.equal(document.getElementById('x'), null);
  assert.equal(document.getElementById('y'), x);
  x.remove();
  assert.equal(document.getElementById('y'), null);
  // innerHTML/textContent also invalidate
  ul.innerHTML = '<li class="z"></li><li class="z"></li>';
  assert.equal(document.querySelectorAll('.z').length, 2);
  ul.textContent = '';
  assert.equal(document.querySelectorAll('.z').length, 0);
});

test('Element.attributes is a live NamedNodeMap (React 19 releaseSingletonInstance terminates)', () => {
  const { document } = createEnvironment('<!doctype html><body></body>');
  const el = document.createElement('div');
  el.setAttribute('a', '1');
  el.setAttribute('b', '2');
  el.setAttribute('c', '3');

  // captured reference is live: reflects removals on the element
  const attrs = el.attributes;
  assert.equal(attrs.length, 3);
  el.removeAttribute('a');
  assert.equal(attrs.length, 2, 'captured map shrinks when element attr removed');

  // EXACT React 19 releaseSingletonInstance loop must terminate
  let n = 0;
  while (attrs.length) { el.removeAttributeNode(attrs[0]); if (++n > 100) break; }
  assert.ok(n <= 2, `loop terminated in ${n} passes, not 100+`);
  assert.equal(el.attributes.length, 0);

  // identity is stable (spec: same NamedNodeMap object)
  assert.equal(el.attributes, el.attributes);

  // NamedNodeMap named accessors mutate through the owner element
  el.setAttribute('x', '1');
  const map = el.attributes;
  assert.equal(map.getNamedItem('x').value, '1');
  assert.equal(map.getNamedItemNS(null, 'x').value, '1');
  assert.equal(map.removeNamedItem('x').name, 'x');
  assert.equal(el.hasAttribute('x'), false);
  el.setAttribute('y', '2');
  assert.equal(map.removeNamedItemNS(null, 'y').name, 'y');
  assert.equal(el.hasAttribute('y'), false);
  map.setNamedItem({ name: 'z', value: '9' });
  map.setNamedItemNS({ name: 'w', value: '8' });
  assert.equal(el.getAttribute('z'), '9');
  assert.equal(el.getAttribute('w'), '8');
  assert.equal(String(el.attributes), '[object NamedNodeMap]');
});

// A subtree replaced wholesale (innerHTML / textContent / replaceChildren) must leave its
// old children DETACHED — a reference held into the replaced subtree that walks parentNode
// upward has to terminate, not climb back into the live tree. jsdom is the oracle.
// (rtdom parity: crates/turbo-dom/src/rtdom/tree.rs `orphan_children`.)
test('replaced children are detached (parentNode cleared), not left pointing at the old parent', () => {
  const html = '<!doctype html><body><div id="a"><span id="s"><b id="deep">x</b></span></div></body>';

  for (const [name, replace] of [
    ['innerHTML', (el) => { el.innerHTML = '<em>new</em>'; }],
    ['textContent', (el) => { el.textContent = 'new'; }],
    ['textContent=""', (el) => { el.textContent = ''; }],
    ['replaceChildren', (el) => { el.replaceChildren(); }],
  ]) {
    const doc = createEnvironment(html).document;
    const jdoc = new JSDOM(html).window.document;
    for (const d of [doc, jdoc]) {
      const a = d.getElementById('a');
      const s = d.getElementById('s');
      const deep = d.getElementById('deep');
      a.querySelectorAll('*'); // force inflation of the subtree before it is replaced
      replace(a);
      assert.equal(s.parentNode, null, `${name}: orphan reports no parent (${d === doc ? 'turbo' : 'jsdom'})`);
      assert.equal(Array.from(a.childNodes).includes(s), false, `${name}: not a child anymore`);
      // the detached subtree itself is intact — only the top link is cut
      assert.equal(deep.parentNode, s, `${name}: inner structure survives`);
      assert.equal(s.contains(deep), true);
    }
  }
});

test('replaceChildren detaches the outgoing children on a document fragment too', () => {
  const doc = createEnvironment('<!doctype html><body></body>').document;
  const frag = doc.createDocumentFragment();
  const kid = doc.createElement('p');
  frag.appendChild(kid);
  assert.equal(kid.parentNode, frag);
  frag.replaceChildren(doc.createElement('b'));
  assert.equal(kid.parentNode, null);
  assert.equal(frag.childNodes.length, 1);
});
