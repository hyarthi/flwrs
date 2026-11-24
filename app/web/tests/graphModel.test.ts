// Run with `npm test` (Node's test runner, which reads the TypeScript itself).
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { test } from 'node:test';
import {
  History,
  addLink,
  addNode,
  addProblem,
  applyProposal,
  asGraph,
  autoLayout,
  copyNodes,
  filesNamed,
  graphProblems,
  linkBadges,
  linkProblem,
  locate,
  pasteNodes,
  portsOf,
  proposalFor,
  removeNodes,
  renameNode,
  setNode,
  withFiles,
  withoutLocation,
  type GraphCatalog,
  type GraphValue,
} from '../src/lib/graphModel.ts';

const root = new URL('../../../plugins/cv_source/', import.meta.url);
const catalog = JSON.parse(readFileSync(new URL('schema/graph_catalog.json', root), 'utf8')) as GraphCatalog;
const example = (name: string) => JSON.parse(readFileSync(new URL(`examples/graphs/${name}.json`, root), 'utf8')).graph as GraphValue;

const small = (): GraphValue => ({
  note: 'kept',
  nodes: {
    cam: { type: 'source' },
    det: { type: 'model', model: 'a.onnx', decode: { type: 'yolo', custom: 1 }, unknown_key: [1, 2] },
    crop: { type: 'crop' },
    pose: { type: 'model', model: 'b.onnx', decode: { type: 'heatmap' }, sampling: { every_n_frames: 3 } },
  },
  links: [
    { from: 'cam.frame', to: 'det.frame' },
    { from: 'cam.frame', to: 'crop.frame' },
    { from: 'det.objects', to: 'crop.objects' },
  ],
  groups: [{ id: 'g', family: 'execution', policy: {} }],
});

test('every example graph reads and writes back byte for byte', () => {
  for (const file of readdirSync(new URL('examples/graphs/', root))) {
    const stored = example(file.replace(/\.json$/, ''));
    assert.equal(JSON.stringify(asGraph(stored)), JSON.stringify(stored));
  }
});

test('an edit keeps unknown keys and the order of what it is not about', () => {
  const g = small();
  const before = Object.keys(g.nodes);
  const { graph, id } = addNode(catalog, g, 'filter');
  assert.deepEqual(Object.keys(graph.nodes), [...before, id]);
  assert.equal(graph.note, 'kept');
  assert.deepEqual(graph.nodes.det, g.nodes.det);
  assert.deepEqual(Object.keys(removeNodes(graph, [id]).nodes), before);
  assert.equal(JSON.stringify(removeNodes(graph, [id]).nodes), JSON.stringify(g.nodes));
});

test('ports follow the settings', () => {
  const model = (extra: object) => portsOf(catalog, { type: 'model', ...extra });
  assert.deepEqual(model({ decode: { type: 'yolo' } }).inputs, [{ name: 'frame', type: 'frame' }]);
  assert.deepEqual(model({ decode: { type: 'classify' } }).outputs, [{ name: 'classes', type: 'classes' }]);
  assert.deepEqual(model({ tensor_input: { dims: 2 } }).inputs, [{ name: 'objects', type: 'objects' }]);
  assert.deepEqual(model({}).outputs, [], 'no decoder chosen yet');
  assert.deepEqual(portsOf(catalog, { type: 'emit' }).inputs, []);
  assert.deepEqual(portsOf(catalog, { type: 'emit', mode: 'frames' }).inputs, [{ name: 'objects', type: 'objects' }]);
  assert.deepEqual(portsOf(catalog, { type: 'rule', input: 'classes' }).inputs, [{ name: 'classes', type: 'classes' }]);
  assert.deepEqual(portsOf(catalog, { type: 'rule' }).inputs, [{ name: 'objects', type: 'objects' }]);
});

test('links are checked by type, and for-each is a badge', () => {
  const g = small();
  assert.equal(linkProblem(catalog, g, 'crop.regions', 'pose.frame'), null);
  assert.match(linkProblem(catalog, g, 'det.objects', 'pose.frame')!, /can’t go into/);
  assert.match(linkProblem(catalog, g, 'cam.frame', 'det.frame')!, /already linked/);
  assert.match(linkProblem(catalog, g, 'det.objects', 'det.frame')!, /itself/);
  const linked = addLink(g, 'crop.regions', 'pose.frame');
  assert.deepEqual(linkBadges(catalog, linked, linked.links![3]), ['Regions', 'for each', 'every 3 frames']);
  const cycle = { ...small(), nodes: { ...small().nodes, f: { type: 'filter' }, h: { type: 'filter' } }, links: [{ from: 'f.objects', to: 'h.objects' }] };
  assert.match(linkProblem(catalog, cycle, 'h.objects', 'f.objects')!, /cycle/);
});

test('limits, renames and removals carry through the links', () => {
  const g = small();
  assert.match(addProblem(catalog, g, 'source')!, /can have 1/);
  const renamed = renameNode(g, 'det', 'detector');
  assert.deepEqual(Object.keys(renamed.nodes), ['cam', 'detector', 'crop', 'pose']);
  assert.equal(renamed.links![2].from, 'detector.objects');
  assert.deepEqual(removeNodes(g, ['det']).links, [{ from: 'cam.frame', to: 'crop.frame' }]);
});

test('copy and paste take the links between the copied nodes', () => {
  const g = small();
  const copied = copyNodes(g, ['det', 'crop'], { det: { x: 10, y: 20 } });
  const { graph, ids, positions } = pasteNodes(catalog, g, copied, 48);
  assert.deepEqual([...ids], [['det', 'det_2'], ['crop', 'crop_2']]);
  assert.deepEqual(positions.det_2, { x: 58, y: 68 });
  assert.ok(graph.links!.some((l) => l.from === 'det_2.objects' && l.to === 'crop_2.objects'));
  const member = setNode(g, 'det', { ...g.nodes.det, groups: ['g'] });
  assert.equal(copyNodes(member, ['det'], {}).nodes.det.groups, undefined, 'a copy starts outside the groups');
  // Editor values can be proxies, which structuredClone refuses.
  const proxied = new Proxy({ type: 'filter' }, {});
  const fromProxy = copyNodes(setNode(g, 'p', proxied), ['p'], {});
  assert.deepEqual(fromProxy.nodes.p, { type: 'filter' });
  const source = pasteNodes(catalog, g, copyNodes(g, ['cam'], {}), 0);
  assert.deepEqual(source.skipped, ['cam'], 'a type at its limit');
});

test('problems: inputs not linked, and messages by their location tag', () => {
  const found = graphProblems(catalog, small());
  assert.deepEqual(found.nodes.get('pose'), ['Input frame isn’t linked.']);
  const located = locate(
    [
      'node x: configurator (run 2026): [at /graph/nodes/det] nodes.det: bad',
      '[at /graph/links/2] links[2]: bad',
      '[at /other/nodes/det] elsewhere',
      'plain',
    ],
    'graph',
  );
  assert.deepEqual(located.nodes.get('det'), ['node x: configurator (run 2026): nodes.det: bad']);
  assert.deepEqual(located.links.get(2), ['links[2]: bad']);
  assert.deepEqual(located.other, ['elsewhere', 'plain']);
  const refused = locate(['node x: config.launch_args/graph/nodes/m/decode: is required'], 'graph');
  assert.deepEqual(refused.nodes.get('m'), ['node x: config.launch_args/graph/nodes/m/decode: is required']);
  assert.equal(withoutLocation('a: [at /graph/nodes/b] c'), 'a: c');
});

test('a proposal fills only what is empty', () => {
  const node = { type: 'model', model: 'a.onnx', decode: { type: 'raw' }, labels: '' };
  const { node: next, filled } = applyProposal(node, { decode: { type: 'yolo' }, pre: { width: 640 }, labels: { model_metadata: 'names' }, type: 'x' });
  assert.deepEqual(next.decode, { type: 'raw' }, 'a set value stays');
  assert.deepEqual(next.pre, { width: 640 });
  assert.deepEqual(next.labels, { model_metadata: 'names' });
  assert.equal(next.type, 'model');
  assert.deepEqual(filled.sort(), ['labels', 'pre']);
  const kind = catalog.node_types.model;
  assert.deepEqual(proposalFor(kind, { model_info: { 'a.onnx': { proposal: { pre: 1 } } } }, 'a.onnx'), { pre: 1 });
  assert.equal(proposalFor(kind, { model_info: {} }, 'a.onnx'), undefined);
});

test('the files setting keeps its names and gains what the graph names', () => {
  assert.deepEqual(filesNamed(catalog, small()).sort(), ['a.onnx', 'b.onnx']);
  assert.deepEqual(withFiles(['labels.txt', 'a.onnx'], ['a.onnx', 'b.onnx']), ['labels.txt', 'a.onnx', 'b.onnx']);
});

test('auto layout keeps placed nodes and ignores gone ones', () => {
  const g = small();
  const out = autoLayout(g, { det: { x: 999, y: 999 }, gone: { x: 1, y: 1 } });
  assert.deepEqual(out.det, { x: 999, y: 999 });
  assert.equal(out.gone, undefined);
  assert.equal(out.cam.x, 0);
  assert.ok(out.crop.x > out.cam.x);
  const points = Object.values(out).map((p) => `${p.x},${p.y}`);
  assert.equal(new Set(points).size, points.length);
});

test('undo and redo walk the values', () => {
  const h = new History();
  const a = small();
  const b = setNode(a, 'cam', { type: 'source', x: 1 });
  h.record(a);
  assert.equal(h.undo(b), a);
  assert.equal(h.redo(a), b);
  assert.equal(new History().undo(a), null);
});
