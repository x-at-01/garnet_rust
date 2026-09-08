#!/usr/bin/env -S bun
import { readFileSync, existsSync } from 'node:fs';
import { resolve, join } from 'node:path';

const ROOT_DIR = process.cwd();
const GARNET_DIR = resolve(ROOT_DIR, '../garnet');

// 1. 读取 wedb_resp/src/cmd.rs 中的所有 RespCommand 枚举
const cmdRsContent = readFileSync(join(ROOT_DIR, 'wedb_resp/src/cmd.rs'), 'utf-8');

// 提取 pub enum RespCommand 里的变体
const enumRegex = /pub\s+enum\s+RespCommand\s*\{([\s\S]*?)\n\}/;
const enumMatch = cmdRsContent.match(enumRegex);
if (!enumMatch) {
  console.error('Could not find RespCommand enum in wedb_resp/src/cmd.rs');
  process.exit(1);
}

const enumBody = enumMatch[1];
const respCommands = new Map(); // UpperName -> EnumVariantName
for (const line of enumBody.split('\n')) {
  const trimmed = line.trim();
  if (!trimmed || trimmed.startsWith('//') || trimmed.startsWith('#')) continue;
  const match = trimmed.match(/^([A-Za-z0-9_]+)\s*(?:=\s*[^,]+)?,?/);
  if (match) {
    const variant = match[1];
    if (variant === 'None') continue;
    respCommands.set(variant.toUpperCase(), variant);
  }
}

// 2. 读取 wedb_server/src/dispatcher.rs 中的已分发命令
const dispatcherContent = readFileSync(join(ROOT_DIR, 'wedb_server/src/dispatcher.rs'), 'utf-8');

// 提取 execute_single_command 和 handle_ 辅助方法中 match 到的 RespCommand::...
const dispatchedCommands = new Set();
const dispatchMatchRegex = /RespCommand::([A-Za-z0-9_]+)/g;
let m;
while ((m = dispatchMatchRegex.exec(dispatcherContent)) !== null) {
  const variant = m[1];
  dispatchedCommands.add(variant.toUpperCase());
}

// 3. 读取 Garnet RespCommand.cs 中的命令
let garnetCommands = new Set();
const garnetCmdFile = join(GARNET_DIR, 'libs/server/Resp/Parser/RespCommand.cs');
if (existsSync(garnetCmdFile)) {
  const garnetContent = readFileSync(garnetCmdFile, 'utf-8');
  const garnetEnumMatch = garnetContent.match(/public\s+enum\s+RespCommand\s*:\s*ushort\s*\{([\s\S]*?)\n\s*\}/);
  if (garnetEnumMatch) {
    for (const line of garnetEnumMatch[1].split('\n')) {
      const trimmed = line.trim();
      if (!trimmed || trimmed.startsWith('//') || trimmed.startsWith('#')) continue;
      const match = trimmed.match(/^([A-Za-z0-9_]+)\s*(?:=\s*[^,]+)?,?/);
      if (match) {
        const name = match[1];
        if (name !== 'NONE') {
          garnetCommands.add(name.toUpperCase());
        }
      }
    }
  }
}

// 4. 比对分析
const allKnown = new Set([...respCommands.keys(), ...garnetCommands]);
const implemented = [];
const missingInDispatcher = [];
const missingInWedbResp = [];

for (const cmd of allKnown) {
  const inGarnet = garnetCommands.has(cmd);
  const inWedbResp = respCommands.has(cmd);
  const inDispatcher = dispatchedCommands.has(cmd);

  if (inDispatcher) {
    implemented.push({ cmd, inGarnet, inWedbResp });
  } else {
    if (inWedbResp) {
      missingInDispatcher.push({ cmd, variant: respCommands.get(cmd), inGarnet });
    } else {
      missingInWedbResp.push({ cmd, inGarnet });
    }
  }
}

// 按功能分类未迁移的命令
const categories = {
  '阻塞列表命令 (Blocking List)': ['BLPOP', 'BRPOP', 'BLMOVE', 'BLMPOP', 'BRPOPLPUSH'],
  '阻塞有序集合命令 (Blocking ZSet)': ['BZMPOP', 'BZPOPMAX', 'BZPOPMIN'],
  '向量检索命令 (Vector / HNSW)': [],
  '位图与统计 (Bitmap & Stats)': [],
  '地理位置命令 (Geo)': [],
  '流与发布订阅 (Streams & PubSub)': [],
  '事务与脚本 (Txn & Scripting)': [],
  '其它未归类命令 (Others)': [],
};

for (const item of missingInDispatcher) {
  const c = item.cmd;
  let categorized = false;
  if (categories['阻塞列表命令 (Blocking List)'].includes(c)) {
    categorized = true;
  } else if (categories['阻塞有序集合命令 (Blocking ZSet)'].includes(c)) {
    categorized = true;
  } else if (c.startsWith('VECTOR') || c.startsWith('VLS') || c.startsWith('VADD') || c.startsWith('VSIM')) {
    categories['向量检索命令 (Vector / HNSW)'].push(c);
    categorized = true;
  } else if (c.startsWith('GEO')) {
    categories['地理位置命令 (Geo)'].push(c);
    categorized = true;
  } else if (c.startsWith('X') || c.includes('STREAM')) {
    categories['流与发布订阅 (Streams & PubSub)'].push(c);
    categorized = true;
  } else {
    categories['其它未归类命令 (Others)'].push(c);
  }
}

console.log('====================================================');
console.log(`Garnet 总定义命令数: ${garnetCommands.size}`);
console.log(`wedb_resp 已定义命令数: ${respCommands.size}`);
console.log(`wedb_server 已分发处理命令数: ${implemented.length}`);
console.log(`已在 wedb_resp 定义但未在 wedb_server 分发的命令数: ${missingInDispatcher.length}`);
console.log(`在 Garnet 中存在但在 wedb_resp 中缺失的命令数: ${missingInWedbResp.length}`);
console.log('====================================================\n');

console.log('### 未分发/未迁移命令详细分类：\n');
for (const [cat, list] of Object.entries(categories)) {
  if (list.length > 0) {
    console.log(`#### ${cat} (${list.length} 个)`);
    console.log(list.join(', '));
    console.log('');
  }
}

if (missingInWedbResp.length > 0) {
  console.log('#### 在 Garnet 存在但在 wedb_resp 完全未定义的命令：');
  console.log(missingInWedbResp.map(x => x.cmd).join(', '));
  console.log('');
}
