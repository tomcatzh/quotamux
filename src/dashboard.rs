pub const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <title>QuotaMux</title>
  <style>
    :root{color-scheme:dark;--bg:#0c0e12;--panel:#151922;--line:#2a3140;--muted:#8e99ab;--text:#eef2f8;--accent:#7ce3b1;--warn:#ffca6a;--bad:#ff7d83}
    *{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--text);font:14px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace}
    main{max-width:1180px;margin:auto;padding:28px 20px 60px}header{display:flex;justify-content:space-between;align-items:end;margin-bottom:24px}h1{font-size:24px;margin:0}h2{font-size:14px;text-transform:uppercase;letter-spacing:.08em;color:var(--muted);margin:28px 0 10px}.muted{color:var(--muted)}
    .grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:10px}.card{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:14px}.value{font-size:22px;margin-top:6px}.good{color:var(--accent)}.warn{color:var(--warn)}.bad{color:var(--bad)}
    table{width:100%;border-collapse:collapse;background:var(--panel);border:1px solid var(--line)}th,td{text-align:left;padding:9px 10px;border-bottom:1px solid var(--line);white-space:nowrap}th{color:var(--muted);font-weight:500}tbody tr:last-child td{border-bottom:0}.scroll{overflow:auto}code{color:var(--accent)}
  </style>
</head>
<body><main>
  <header><div><h1>QuotaMux</h1><div class="muted">DeepSeek V4 Flash 0731 gateway</div></div><div id="updated" class="muted">loading…</div></header>
  <section class="grid" id="cards"></section>
  <h2>Providers</h2><div class="scroll"><table><thead><tr><th>Provider</th><th>Attempts</th><th>Success</th><th>Errors</th><th>Cache hit</th><th>Cost estimate</th></tr></thead><tbody id="providers"></tbody></table></div>
  <h2>Recent requests</h2><div class="scroll"><table><thead><tr><th>Time</th><th>Protocol</th><th>Provider</th><th>Status</th><th>Fallback</th><th>Latency</th><th>Tokens</th></tr></thead><tbody id="requests"></tbody></table></div>
  <h2>Active alerts</h2><div class="scroll"><table><thead><tr><th>Provider</th><th>Class</th><th>Last seen</th><th>Next probe</th></tr></thead><tbody id="alerts"></tbody></table></div>
</main><script>
const esc=v=>String(v??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const n=v=>Number(v||0).toLocaleString(); const pct=(a,b)=>b?`${(100*a/b).toFixed(1)}%`:'—';
async function refresh(){
 const [status,stats,recent]=await Promise.all(['/api/status','/api/stats','/api/requests?limit=50'].map(u=>fetch(u).then(r=>r.json())));
 document.querySelector('#updated').textContent=new Date().toLocaleTimeString();
 const cards=[['Active provider',status.active_provider],['Go circuit',status.circuit.mode],['Requests',n(stats.requests.total)],['Logical errors',pct(stats.requests.errors,stats.requests.total)],['Fallbacks',n(stats.requests.fallbacks)],['Output tokens',n(stats.requests.output_tokens)],['Traffic',`${(stats.requests.bytes/1048576).toFixed(2)} MiB`],['DeepSeek balance',status.deepseek_balance?.display||'unknown']];
 document.querySelector('#cards').innerHTML=cards.map(([k,v])=>`<div class="card"><div class="muted">${esc(k)}</div><div class="value">${esc(v)}</div></div>`).join('');
 document.querySelector('#providers').innerHTML=Object.entries(stats.providers).map(([name,p])=>`<tr><td>${esc(name)}</td><td>${n(p.attempts)}</td><td>${n(p.successes)}</td><td>${n(p.errors)}</td><td>${pct(p.cache_hit_tokens,p.cache_hit_tokens+p.cache_miss_tokens)} <span class="muted">(${pct(p.cache_reported_attempts,p.attempts)} coverage)</span></td><td>$${Number(p.cost_usd||0).toFixed(6)}</td></tr>`).join('');
 document.querySelector('#requests').innerHTML=recent.requests.map(r=>`<tr><td>${new Date(r.started_at_ms).toLocaleTimeString()}</td><td>${esc(r.protocol)}</td><td>${esc(r.provider||'—')}</td><td>${r.status}</td><td>${r.fallback?'yes':'no'}</td><td>${r.total_ms} ms</td><td>${n(r.usage.total_tokens)}</td></tr>`).join('');
 document.querySelector('#alerts').innerHTML=(status.alerts||[]).filter(a=>a.active).map(a=>`<tr><td>${esc(a.provider)}</td><td class="bad">${esc(a.class)}</td><td>${new Date(a.last_seen_ms).toLocaleString()}</td><td>${a.next_probe_at_ms?new Date(a.next_probe_at_ms).toLocaleString():'—'}</td></tr>`).join('')||'<tr><td colspan="4" class="muted">No active alerts</td></tr>';
}
refresh().catch(e=>document.querySelector('#updated').textContent=e.message);setInterval(refresh,5000);
</script></body></html>"#;
