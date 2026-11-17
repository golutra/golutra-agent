// 协议测试替身：只模拟 SDK 生命周期，不作为真实浏览器验收证据。
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const directory = path.dirname(fileURLToPath(import.meta.url));
const dbPath = path.join(directory, 'spaces.json');
const read = () => fs.existsSync(dbPath) ? JSON.parse(fs.readFileSync(dbPath, 'utf8')) : { next: 1, spaces: [] };
const write = db => fs.writeFileSync(dbPath, JSON.stringify(db));
if (process.argv.includes('skill')) {
  console.log('Fixture SDK guide. '.repeat(1000));
  if (!fs.existsSync(path.join(directory, 'incomplete-guide'))) console.log('[ego-browser:skill] end of guide fixture-1');
  process.exit(0);
}
const failure = (code) => { const error = new Error(code); error.code = code; throw error; };
globalThis.ego = { getBrowserVersion: async () => 'fixture-1' };
globalThis.listTaskSpaces = async () => read().spaces;
const handle = id => ({
  spaceId: id,
  page: label => ({
    label,
    async goto(url) { const db = read(); const space=db.spaces.find(s => s.id === id); if (!(label in space.pages)) failure('EGO_PAGE_NOT_FOUND'); space.url=url; space.pages[label]=url; write(db); },
    async url() { return read().spaces.find(s => s.id === id).pages[label]; },
    async reload() { if (fs.existsSync(path.join(directory,'reload-fails'))) failure('EGO_PAGE_UNRESPONSIVE'); const db=read(); db.reloads=(db.reloads||0)+1; write(db); },
    async snapshot(options = {}) { return JSON.stringify({ ...read().spaces.find(s => s.id === id), snapshotOptions: options }); },
    async screenshot({ path }) {
      fs.writeFileSync(path, Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a8l8AAAAASUVORK5CYII=', 'base64'));
      return path;
    },
  }),
  async newPage() { const db=read(),space=db.spaces.find(s=>s.id===id),label='p'+(++space.nextLabel);space.pages[label]='about:blank';write(db);return handle(id).page(label); },
  async pages() { return Object.keys(read().spaces.find(s=>s.id===id).pages).map(label=>handle(id).page(label)); },
  async tabs() {
    const space=read().spaces.find(s=>s.id===id);
    return [...Object.keys(space.pages).map(label=>({ label, page:handle(id).page(label), openedBy:'agent' })),
      ...(space.unmanaged||[]).map(tab=>({ page:{url:tab.url}, openedBy:tab.openedBy }))];
  },
  async adopt(page) {
    const db=read(),space=db.spaces.find(s=>s.id===id);
    const index=space.unmanaged.findIndex(tab=>tab.url===page.url);
    if(index<0) failure('EGO_PAGE_NOT_FOUND');
    const label='p'+(++space.nextLabel);
    space.pages[label]=space.unmanaged.splice(index,1)[0].url; write(db);
    return handle(id).page(label);
  },
  async handOff() { const db = read(); const space=db.spaces.find(s => s.id === id); space.control = 'user'; space.ownership='agentDelegatedToUser'; write(db); },
  async finish({ keep }) {
    const db=read(),space=db.spaces.find(s=>s.id===id),labels=Object.keys(space.pages);
    const kept=keep==='all'?labels:keep;
    if(kept.some(label=>!labels.includes(label))) failure('EGO_PAGE_NOT_FOUND');
    const unmanaged=space.unmanaged||[];
    const result={spaceId:id,closedSpace:kept.length===0&&unmanaged.length===0,keptManagedLabels:kept,closedManagedLabels:labels.filter(label=>!kept.includes(label)),preservedUnmanagedCount:unmanaged.length};
    db.lastKeep=keep;db.lastFinish=result;
    if(result.closedSpace)db.spaces=db.spaces.filter(s=>s.id!==id);
    else {space.control='ended';space.ownership='user';space.unmanaged=[...unmanaged,...kept.map(label=>({url:space.pages[label],openedBy:'agent'}))];space.pages={};}
    write(db);
    return fs.existsSync(path.join(directory,'missing-finish-receipt'))?undefined:result;
  },
});
globalThis.taskSpace = async value => {
  const db = read();
  let space = db.spaces.find(s => typeof value === 'number' ? s.id === value : s.name === value);
  if (!space) {
    if (typeof value === 'number') failure('EGO_SPACE_MISSING');
    space = { id: db.next++, name: value, ownership: 'agent', control: 'agent', url: 'about:blank', pages:{p1:'about:blank'}, nextLabel:1 };
    db.spaces.push(space); write(db);
  }
  if (space.control === 'user') failure('EGO_TASK_SPACE_USER_IN_CONTROL');
  if (space.control === 'ended') failure('EGO_TASK_SPACE_INACTIVE');
  return handle(space.id);
};
globalThis.takeOverTaskSpace = async id => {
  const db = read();
  const space = db.spaces.find(s => s.id === id);
  if (!space) failure('EGO_SPACE_MISSING');
  if (space.control === 'ended') failure('EGO_TASK_SPACE_INACTIVE');
  space.control = 'agent'; space.ownership='agent'; write(db);
  return handle(id);
};
globalThis.claimTaskSpace = async id => {
  const db=read(),space=db.spaces.find(s=>s.id===id);
  if(!space) failure('EGO_SPACE_MISSING');
  if(space.ownership!=='user') failure('EGO_TASK_SPACE_USER_IN_CONTROL');
  space.control='agent';space.ownership='agent';db.claims=(db.claims||0)+1;write(db);
  return handle(id);
};
let source = '';
for await (const chunk of process.stdin) source += chunk;
const AsyncFunction = Object.getPrototypeOf(async function() {}).constructor;
await new AsyncFunction(source)();
