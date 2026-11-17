// 验证 npm 启动器把随包 OpenCLI/Node 的真实路径传给原生程序，不依赖全局 PATH。
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
const exec=promisify(execFile);

test('npm launcher routes bundled OpenCLI and preserves literal arguments',{skip:process.platform==='win32'},async()=>{
  const root=await fs.mkdtemp(path.join(os.tmpdir(),'golutra-npm-opencli-'));
  try {
    await fs.cp(new URL('../../npm/agent',import.meta.url),root,{recursive:true});
    const platform=`@golutra/agent-${process.platform}-${process.arch}`;
    const native=path.join(root,'node_modules',platform);
    await fs.mkdir(path.join(native,'vendor/bin'),{recursive:true});
    const version=JSON.parse(await fs.readFile(path.join(root,'package.json'),'utf8')).version;
    await fs.writeFile(path.join(native,'package.json'),JSON.stringify({name:platform,version}));
    const binary=path.join(native,'vendor/bin/golutra-agent');
    await fs.writeFile(binary,`#!${process.execPath}\nconsole.log(JSON.stringify({opencli:process.env.GOLUTRA_AGENT_OPENCLI_EXECUTABLE,node:process.env.GOLUTRA_AGENT_OPENCLI_NODE,args:process.argv.slice(2)}));\n`);
    await fs.chmod(binary,0o700);
    const opencli=path.join(root,'node_modules/@jackwener/opencli');
    await fs.mkdir(path.join(opencli,'dist/src'),{recursive:true});
    await fs.writeFile(path.join(opencli,'package.json'),JSON.stringify({name:'@jackwener/opencli',exports:'./dist/src/main.js',type:'module'}));
    await fs.writeFile(path.join(opencli,'dist/src/main.js'),'');
    const env={...process.env};delete env.GOLUTRA_AGENT_OPENCLI_EXECUTABLE;
    const result=await exec(process.execPath,[path.join(root,'bin/golutra-agent.js'),'literal --profile work'],{env});
    const output=JSON.parse(result.stdout);
    assert.equal(await fs.realpath(output.opencli),await fs.realpath(path.join(opencli,'dist/src/main.js')));
    assert.equal(output.node,process.execPath);assert.deepEqual(output.args,['literal --profile work']);
  } finally {await fs.rm(root,{recursive:true,force:true});}
});
