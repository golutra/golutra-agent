# ORBIT BREAKER: shared game contract (benchmark v2)

Build a polished, genuinely playable top-down space survival shooter using browser-native ES modules and Canvas. Zero packages/CDNs/remote assets. Serve this directory with python3 -m http.server. This file is supplied infrastructure, not generated implementation; preserve it.

## Player-visible requirements
- 960x600 logical arena with responsive scaling; dark space/starfield, distinct ship/enemies/projectiles, health, score, wave and time.
- Title screen with instructions and an obvious Start button; WASD/arrows move, mouse aims, Space or held mouse fires. Prevent game keys scrolling the page.
- Pause/resume with P or Escape and a visible button; restart from game over; mute button.
- Enemies spawn at edges, pursue the player and get harder over time. Shooting, enemy death, damage, score and health pickups work.
- Three starting HP, brief damage invulnerability, bounded movement, losing all HP ends the run. A new run resets score/timer/entities/health.
- Procedural WebAudio effects after user interaction, with working mute and graceful lack-of-audio fallback.
- Ship/projectile/enemy contrast, hit feedback and restrained particles; no placeholder panels. README includes controls and launch instructions.
- Main animation uses delta seconds, caps long frames, freezes simulation while paused, clears held input on blur. No network or backend is needed to play.
- The mute button immediately toggles between Mute and Unmute, with aria-pressed matching the mute state, including while paused or on the title screen. Pass main's muted preference through the state supplied to UI.update. The pause button reads Resume while paused. Visible buttons and the title overlay must fit a 420x840 viewport. DOM owns interactive overlays/HUD; avoid drawing a second overlapping copy in Canvas.

## Exactly eight child assignments and file ownership
1. input: input.mjs
2. player: player.mjs
3. combat: combat.mjs
4. enemies: enemies.mjs
5. collisions: collisions.mjs
6. renderer: renderer.mjs
7. ui: ui.mjs and styles.css
8. audio: audio.mjs
Parent owns game.mjs, main.mjs, index.html and README.md, establishes integration, and may make necessary integration corrections after children settle. Each child reads this contract and implements only its owned files. Independent module boundaries below are frozen for both benchmark arms. Nested delegation is unnecessary.

## State and API (implement, do not stub)
Parent game.mjs exports createGame(options={}), startGame(state), togglePause(state), stepGame(state,input={},dt=1/60).
createGame({width=960,height=600,seed=1}) returns:
{ width,height,mode:'title',elapsed:0,score:0,kills:0,wave:1,rngSeed:seed,
  player,bullets:[],enemies:[],pickups:[],particles:[],events:[],spawnTimer:0 }
player: {x,y,r:14,speed:260,hp:3,maxHp:3,cooldown:0,invulnerable:0}.
Bullet: {x,y,vx,vy,r,ttl,damage:1}. Enemy: {x,y,r,hp,speed}. Pickup: {x,y,r,type:'health',ttl}.
events is an array of strings (shoot, hit, kill, pickup, gameover) reset each simulation step.
startGame mutates the same object, resets all transient data and enters 'playing'.
togglePause swaps 'playing'/'paused', otherwise leaves mode unchanged.
stepGame runs only in 'playing'; clamp valid dt to [0,0.05], advance elapsed, call updatePlayer, updateCombat, updateEnemies, resolveCollisions in order. Negative/nonfinite dt must not corrupt state.
Input is {moveX:0,moveY:0,aimX:...,aimY:...,fire:false}; absent fields mean neutral.

input.mjs exports createInput(target,canvas) -> {read(),destroy()}. Target is a browser Window/EventTarget. Listen keydown/up, pointer movement/down/up and blur; read returns current input in logical canvas coordinates. Support WASD/arrows and Space; normalize diagonal in player module. Prevent only recognized game keys. All listeners removed on destroy; blur clears movement/fire. Main handles pause/restart and UI buttons.

player.mjs exports createPlayer(width,height), updatePlayer(state,input,dt). Initial player centered. Normalize diagonal movement, clamp center within [r,width-r]/[r,height-r], and decrement invulnerable toward zero.

combat.mjs exports updateCombat(state,input,dt). On fire and cooldown<=0 emit a projectile aimed at aimX/aimY (default upwards), then enforce at least 0.12 sec cooldown. Bullets move at >=400 logical pixels/sec, use dt, expire within 3 sec or outside arena. Decrement cooldown each step, append shoot event on actual shots.

enemies.mjs exports updateEnemies(state,dt). Spawn enemies within 2 seconds of starting, at edges separated from player; pursue current player using speed*dt. Raise wave by elapsed survival time (>=2 by 35 sec). Deterministic RNG from state.rngSeed, not Math.random for game-affecting choices. Bound enemy count at 80.

collisions.mjs exports resolveCollisions(state). Circle overlap: bullets damage enemies and are consumed; dead enemies increment kills and score (>=10), append kill. Contact deals 1 HP when invulnerable<=0, grants >=0.5 sec invulnerability, appends hit. Periodically drop health on enemy kills. Touching health pickup increases HP to maxHp and consumes pickup. HP<=0 sets mode='gameover' and appends gameover. Never repeatedly count the same dead enemy.

renderer.mjs exports render(ctx,state). Draw the entire arena with appropriate visual feedback; no global DOM side effects at import time. It must work in title/playing/paused/gameover, at logical state.width/state.height.

ui.mjs exports mountUI(root,handlers) -> {update(state),destroy()}. handlers: {start,pause,restart,mute}. Build the game's HUD and overlays inside root only; buttons have data-action=start|pause|restart|mute and readable labels. update shows health/score/wave/time and correct overlays for mode; avoid rebuilding all DOM every frame. root is a separate div from the canvas. styles.css may style parent elements #app, #game, #ui, body. Mute state belongs to main.

audio.mjs exports createAudio() -> {unlock(),play(eventName),setMuted(boolean),destroy()}. No eager browser globals/AudioContext usage during module import; unlock after interaction, all methods safe when AudioContext absent.

Parent index.html includes a canvas #game width=960 height=600, a separate #ui, links styles.css and module main.mjs. main connects the APIs and animation. For acceptance only, expose window.__orbitSnapshot() returning a fresh JSON-safe copy of {mode,elapsed,score,kills,wave,player,bullets,enemies}; it must reflect the real running game and never control it.

## Validation
The caller uses an independent external behavioral verifier outside this workspace. You may run:
node ../verify.mjs .
It checks game logic, start/pause/reset, movement, shooting/cooldown, collisions/health, waves, input cleanup, DOM entry wiring, and sane bounded simulation. Fix actual product failures, preserving this contract and external verifier. Run the verifier before final response. Report real limitations honestly.
