import fs from 'node:fs';
import path from 'node:path';
import assert from 'node:assert/strict';
import { pathToFileURL } from 'node:url';
import { spawnSync } from 'node:child_process';

const workspace = path.resolve(process.argv[2] || '.');
const results = [];
async function check(name, fn) {
  try { await fn(); results.push({name,passed:true}); }
  catch(error) { results.push({name,passed:false,error:String(error.message||error)}); }
}
const files=['index.html','styles.css','main.mjs','game.mjs','input.mjs','player.mjs','combat.mjs','enemies.mjs','collisions.mjs','renderer.mjs','ui.mjs','audio.mjs','README.md'];
await check('all delivery files exist and are nonempty',()=>{
  for(const file of files) assert.ok(fs.statSync(path.join(workspace,file)).size>30,file);
});
await check('all JavaScript modules parse',()=>{
  for(const file of files.filter(x=>x.endsWith('.mjs'))){
    const r=spawnSync(process.execPath,['--check',path.join(workspace,file)],{encoding:'utf8',timeout:10000});
    assert.equal(r.status,0,`${file}: ${r.stderr}`);
  }
});
await check('HTML wires canvas, stylesheet and module entry',()=>{
  const html=fs.readFileSync(path.join(workspace,'index.html'),'utf8');
  assert.match(html,/<canvas\b[^>]*\bid\s*=\s*["']game["']/i);
  assert.match(html,/<[^>]+\bid\s*=\s*["']ui["']/i);
  assert.match(html,/\bhref\s*=\s*["'](?:\.\/)?styles\.css["']/i);
  assert.match(html,/<script\b(?=[^>]*type\s*=\s*["']module["'])(?=[^>]*src\s*=\s*["'](?:\.\/)?main\.mjs["'])[^>]*>/i);
  assert.ok(!/(?:src|href)\s*=\s*["']https?:\/\//i.test(html),'External dependency');
});
let game,player,combat,enemies,collisions,input,audio,renderer;
await check('required module exports load in Node without DOM side effects',async()=>{
  [game,player,combat,enemies,collisions,input,audio,renderer]=await Promise.all(
    ['game','player','combat','enemies','collisions','input','audio','renderer'].map(x=>import(pathToFileURL(path.join(workspace,x+'.mjs')).href)));
  for(const key of ['createGame','startGame','togglePause','stepGame'])assert.equal(typeof game[key],'function',key);
});
const fresh=()=>{const s=game.createGame({width:960,height:600,seed:177});game.startGame(s);return s;};
const enemy=(x,y)=>({x,y,r:16,hp:1,speed:65});
const bullet=(x,y)=>({x,y,vx:0,vy:0,r:4,ttl:1,damage:1});
if(game){
  await check('title, start and initial state',()=>{
    const s=game.createGame({seed:17});assert.equal(s.mode,'title');game.startGame(s);
    assert.equal(s.mode,'playing');assert.equal(s.player.hp,3);assert.equal(s.score,0);assert.equal(s.elapsed,0);
    assert.ok(s.player.x>30&&s.player.x<s.width-30);assert.ok(s.player.y>30&&s.player.y<s.height-30);
  });
  await check('cardinal movement and diagonal normalization',()=>{
    const a=fresh(),b=fresh();const ax=a.player.x,ay=a.player.y,bx=b.player.x,by=b.player.y;
    for(let i=0;i<10;i++){player.updatePlayer(a,{moveX:1,moveY:0},0.02);player.updatePlayer(b,{moveX:1,moveY:1},0.02);}
    const da=Math.hypot(a.player.x-ax,a.player.y-ay),db=Math.hypot(b.player.x-bx,b.player.y-by);
    assert.ok(da>30);assert.ok(Math.abs(da-db)<0.5,`diagonal ${db} versus cardinal ${da}`);
  });
  await check('player remains within arena bounds',()=>{
    const s=fresh();for(let i=0;i<400;i++)player.updatePlayer(s,{moveX:-1,moveY:-1},0.05);
    assert.ok(s.player.x>=s.player.r-0.001&&s.player.y>=s.player.r-0.001);
    for(let i=0;i<500;i++)player.updatePlayer(s,{moveX:1,moveY:1},0.05);
    assert.ok(s.player.x<=s.width-s.player.r+0.001&&s.player.y<=s.height-s.player.r+0.001);
  });
  await check('projectile firing, direction and cooldown',()=>{
    const s=fresh(),px=s.player.x;const aim={fire:true,aimX:px+200,aimY:s.player.y};
    combat.updateCombat(s,aim,0.01);assert.equal(s.bullets.length,1);assert.ok(s.events.includes('shoot'));
    const b=s.bullets[0];assert.ok(b.vx>=399&&Math.abs(b.vy)<0.01);
    for(let i=0;i<5;i++)combat.updateCombat(s,aim,0.01);
    assert.equal(s.bullets.length,1,'cooldown permits excessive shots');assert.ok(b.x>px);
  });
  await check('projectiles expire',()=>{
    const s=fresh();s.bullets=[{...bullet(200,200),ttl:0.01}];combat.updateCombat(s,{},0.05);assert.equal(s.bullets.length,0);
  });
  await check('enemy spawn and pursuit',()=>{
    const s=fresh();for(let i=0;i<40;i++){s.elapsed+=0.05;enemies.updateEnemies(s,0.05);}
    assert.ok(s.enemies.length>0&&s.enemies.length<=80);
    const e=enemy(100,100);s.enemies=[e];s.spawnTimer=100;
    const before=Math.hypot(e.x-s.player.x,e.y-s.player.y);enemies.updateEnemies(s,0.05);
    assert.ok(Math.hypot(e.x-s.player.x,e.y-s.player.y)<before);
  });
  await check('same seed gives deterministic enemy state',()=>{
    const a=fresh(),b=fresh();for(let i=0;i<100;i++){a.elapsed+=0.02;b.elapsed+=0.02;enemies.updateEnemies(a,0.02);enemies.updateEnemies(b,0.02);}
    const project=s=>s.enemies.map(e=>[e.x,e.y,e.r,e.hp,e.speed]);assert.deepEqual(project(a),project(b));
  });
  await check('waves progress with elapsed time',()=>{
    const s=fresh();s.elapsed=35;enemies.updateEnemies(s,0.05);assert.ok(s.wave>=2);
  });
  await check('bullet kill gives score once and consumes bullet',()=>{
    const s=fresh();s.enemies=[enemy(200,200)];s.bullets=[bullet(200,200)];collisions.resolveCollisions(s);
    assert.equal(s.kills,1);assert.ok(s.score>=10);assert.equal(s.enemies.length,0);assert.equal(s.bullets.length,0);assert.ok(s.events.includes('kill'));
    const score=s.score;collisions.resolveCollisions(s);assert.equal(s.score,score);assert.equal(s.kills,1);
  });
  await check('contact damage respects invulnerability',()=>{
    const s=fresh();s.enemies=[enemy(s.player.x,s.player.y)];collisions.resolveCollisions(s);
    assert.equal(s.player.hp,2);assert.ok(s.player.invulnerable>=0.5);assert.ok(s.events.includes('hit'));
    s.enemies=[enemy(s.player.x,s.player.y)];collisions.resolveCollisions(s);assert.equal(s.player.hp,2);
    player.updatePlayer(s,{},2);assert.equal(s.player.invulnerable,0);
  });
  await check('health pickup heals and is consumed',()=>{
    const s=fresh();s.player.hp=1;s.pickups=[{x:s.player.x,y:s.player.y,r:10,type:'health',ttl:4}];collisions.resolveCollisions(s);
    assert.ok(s.player.hp>1&&s.player.hp<=s.player.maxHp);assert.equal(s.pickups.length,0);assert.ok(s.events.includes('pickup'));
  });
  await check('death changes mode and emits gameover',()=>{
    const s=fresh();s.player.hp=1;s.enemies=[enemy(s.player.x,s.player.y)];collisions.resolveCollisions(s);
    assert.equal(s.mode,'gameover');assert.ok(s.events.includes('gameover'));
  });
  await check('pause freezes all simulation and resumes',()=>{
    const s=fresh();game.stepGame(s,{fire:true},0.05);game.togglePause(s);assert.equal(s.mode,'paused');
    const before=JSON.stringify(s);for(let i=0;i<20;i++)game.stepGame(s,{moveX:1,fire:true},0.05);assert.equal(JSON.stringify(s),before);
    game.togglePause(s);assert.equal(s.mode,'playing');game.stepGame(s,{},0.05);assert.ok(s.elapsed>JSON.parse(before).elapsed);
  });
  await check('restart resets prior run state in place',()=>{
    const s=fresh();s.mode='gameover';s.score=500;s.elapsed=99;s.kills=20;s.player.hp=0;s.wave=9;s.bullets=[bullet(1,2)];s.enemies=[enemy(3,4)];s.pickups=[{x:1,y:2}];
    game.startGame(s);assert.equal(s.mode,'playing');assert.equal(s.score,0);assert.equal(s.elapsed,0);assert.equal(s.kills,0);assert.equal(s.player.hp,3);assert.equal(s.wave,1);
    for(const k of ['bullets','enemies','pickups'])assert.equal(s[k].length,0,k);
  });
  await check('large or invalid delta cannot corrupt or fast-forward state',()=>{
    const s=fresh();game.stepGame(s,{moveX:1},20);assert.ok(s.elapsed<=0.050001);
    for(const dt of [-1,NaN,Infinity])game.stepGame(s,{},dt);
    for(const n of [s.elapsed,s.player.x,s.player.y,s.player.hp])assert.ok(Number.isFinite(n));assert.ok(s.elapsed<=0.20001);
  });
  await check('long active simulation remains finite and bounded',()=>{
    const s=fresh();for(let i=0;i<6000;i++){
      s.player.invulnerable=10;game.stepGame(s,{moveX:Math.sin(i/90),moveY:Math.cos(i/70),fire:true,aimX:480,aimY:0},1/60);
      assert.ok(s.enemies.length<=80);assert.ok(s.bullets.length<=100);assert.ok(Number.isFinite(s.player.x)&&Number.isFinite(s.player.y)&&Number.isFinite(s.elapsed));
    }assert.ok(s.elapsed>90);assert.ok(s.wave>=2);
  });
}
if(input){
  await check('real input listeners handle keyup, blur and disposal',()=>{
    const target=new EventTarget(),canvas=new EventTarget();Object.assign(canvas,{width:960,height:600,getBoundingClientRect:()=>({left:0,top:0,width:960,height:600})});
    const controls=input.createInput(target,canvas);
    const key=(type,k,code)=>{const e=new Event(type,{cancelable:true});Object.defineProperties(e,{key:{value:k},code:{value:code}});target.dispatchEvent(e);return e;};
    key('keydown','d','KeyD');assert.ok(controls.read().moveX>0);key('keyup','d','KeyD');assert.equal(controls.read().moveX,0);
    key('keydown','ArrowLeft','ArrowLeft');key('keydown',' ','Space');assert.ok(controls.read().moveX<0&&controls.read().fire);
    target.dispatchEvent(new Event('blur'));assert.equal(controls.read().moveX,0);assert.equal(controls.read().fire,false);
    assert.equal(key('keydown','q','KeyQ').defaultPrevented,false);controls.destroy();key('keydown','d','KeyD');assert.equal(controls.read().moveX,0);
  });
}
if(audio){await check('audio gracefully degrades without AudioContext',async()=>{
  const a=audio.createAudio();await a.unlock();a.play('shoot');a.setMuted(true);a.play('hit');a.setMuted(false);a.destroy();
});}
if(renderer&&game){await check('renderer draws each lifecycle mode',()=>{
  let calls=0;const gradient={addColorStop(){}};const ctx=new Proxy({canvas:{width:960,height:600}}, {
    get(t,k){if(k in t)return t[k];if(k==='measureText')return s=>({width:s.length*8});if(String(k).startsWith('create'))return ()=>gradient;return ()=>{calls++;};},
    set(t,k,v){t[k]=v;return true;}
  });const s=fresh();for(const mode of ['title','playing','paused','gameover']){s.mode=mode;renderer.render(ctx,s);}assert.ok(calls>30);
});}
const passed=results.filter(r=>r.passed).length;
console.log(JSON.stringify({workspace,passed,failed:results.length-passed,total:results.length,results},null,2));
process.exitCode=results.every(r=>r.passed)&&results.length>=20?0:1;
