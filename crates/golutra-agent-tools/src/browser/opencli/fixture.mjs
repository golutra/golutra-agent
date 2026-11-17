// 只用于 Rust 协议回归，不连接浏览器；真实页面验收另走 live-test。
import fs from 'node:fs';
const args=process.argv.slice(2);
if(args[0]==='--version') {console.log('2.0.3');process.exit(0);}
const data=JSON.stringify(args);
fs.appendFileSync('calls.jsonl',data+'\n');
const reply=value=>console.log(JSON.stringify({ok:true,data:value}));
if(args.includes('doctor')) reply({extensionConnected:true,connectivity:{ok:true}});
else if(args[0]==='profile') reply({profiles:[{contextId:'profile-one',connected:true,default:true}]});
else if(args.at(-1)==='ambiguous' && !args.includes('--nth')) {console.log(JSON.stringify({ok:false,error:{code:'selector_ambiguous',message:'Matched 2 elements',matches_n:2}}));process.exitCode=1;}
else if(args.includes('eval') && args.at(-1)==='fail') {console.log(JSON.stringify({ok:false,error:{code:'command_lost',message:'Unknown outcome'}}));process.exitCode=75;}
else if(args.includes('eval') && args.at(-1)==='hang') {fs.writeFileSync('started','yes');setTimeout(()=>reply('late'),60000);}
else if(args.includes('screenshot')) reply('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a31cAAAAASUVORK5CYII=');
else reply({args});
