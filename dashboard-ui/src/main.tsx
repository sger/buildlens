import { StrictMode, useCallback, useEffect, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";

// Build ids are the activity log's uniqueIdentifier (or a `sha:` content hash
// when Xcode leaves it empty) — always a string, never a row number.
type Build = { id:string; recorded_at:number; status?:string|null; total_seconds?:number|null; test_duration_seconds?:number|null; category?:string|null; project?:string|null; raw_warnings?:number; failed_tests?:number; crashed_tests?:number; scheme?:string|null; cache_hit_rate?:number|null; };
type Project = { project:string; builds:number };
type Trend = { id:string; total_seconds?:number|null; category?:string|null; recorded_at:number; project?:string|null };
type TargetRow = { name:string; seconds:number; category?:string|null; fetched_from_cache?:boolean; compiled_count?:number };
type PhaseRow = { name:string; seconds:number };
type Detail = Build & { compiled_count?:number; machine_id?:string|null; xcode_version?:string|null; platform?:string|null; architecture?:string|null; targets?:TargetRow[]; phases?:PhaseRow[]; metadata?:Record<string,string>; };
type Percentiles = { builds:number; enough_history:boolean; p50?:number|null; p95?:number|null; min_seconds?:number|null; max_seconds?:number|null; avg_seconds?:number|null };
type Ranked = { name:string; observations:number; avg_seconds?:number|null; max_seconds?:number|null; cached_builds?:number };
type FileRow = { file:string; target?:string|null; observations:number; avg_seconds?:number|null; max_seconds?:number|null; compilations?:number|null };
type SwiftRow = { file:string; line:number; symbol?:string|null; kind:string; observations:number; avg_milliseconds?:number|null; max_milliseconds?:number|null };
type DiagRow = { fingerprint:string; severity?:string|null; category?:string|null; message?:string|null; file?:string|null; builds:number; occurrences?:number|null };
type FlakyRow = { suite:string; test:string; runs:number; failed:number; passed:number; flaky:boolean };
type RegressionRow = { name:string; current_seconds:number; previous_seconds:number; delta_seconds:number; delta_percent:number; observations:number };
type EnvRow = { xcode_version:string; platform:string; architecture:string; builds:number; machines:number; avg_seconds?:number|null };
type GitRow = { id:string; recorded_at:number; total_seconds:number; category:string; branch?:string|null; commit?:string|null; dirty?:string|null };
type DailyRow = { day:string; builds:number; p50?:number|null; p95?:number|null };
type DiagTrendRow = { id:string; recorded_at:number; warnings:number; errors:number; unique_diagnostics:number };

/// Fetches JSON, attaching the team-server bearer token when one is set.
///
/// The local dashboard has no authentication — the token is only for pointing
/// this UI at `buildlens-server`, which does. It lives in localStorage, so it
/// is readable by any script on this origin; that is acceptable for a token
/// that only grants read access to build timings, but it is not a password.
const get = async <T,>(url:string):Promise<T> => { const token=localStorage.getItem("buildlens-token")||""; const r=await fetch(url,token?{headers:{Authorization:`Bearer ${token}`}}:undefined); if(!r.ok) throw Error(`${r.status} ${url}`); return r.json(); };
const seconds = (n?:number|null) => n == null ? "—" : `${n < 10 ? n.toFixed(1) : n.toFixed(0)}s`;
const ms = (n?:number|null) => n == null ? "—" : `${n.toFixed(0)}ms`;
const percent = (n?:number|null) => n == null ? "—" : `${(n*100).toFixed(0)}%`;
const date = (n:number) => new Date(n*1000).toLocaleString(undefined,{month:"short",day:"numeric",hour:"2-digit",minute:"2-digit"});
const short = (s?:string|null) => s ? s.split("/").pop() || s : "—";
/// How often the overview re-fetches. A build takes seconds to collect, so a
/// few seconds of staleness is invisible while the request cost stays trivial.
const REFRESH_MS = 5000;

function Sparkline({items}:{items:Trend[]}) { const points=items.filter(x=>x.total_seconds!=null); if(!points.length) return <div className="empty">Collect a build to start the trend.</div>; const max=Math.max(...points.map(x=>x.total_seconds!),1); const path=points.map((x,i)=>`${i?"L":"M"}${(i/(points.length-1||1))*100},${100-(x.total_seconds!/max)*82}`).join(" "); return <svg className="spark" viewBox="0 0 100 100" preserveAspectRatio="none"><path d={path} fill="none" stroke="currentColor" strokeWidth="2.5" vectorEffect="non-scaling-stroke"/><path d={`${path} L100,100 L0,100 Z`} fill="currentColor" opacity=".10"/></svg>; }
/// A backend that does not record build status (the team server, whose wire
/// format carries timings only) sends no `status` at all. Show that as unknown
/// rather than inventing a green "succeeded".
function Status({value}:{value?:string|null}) { if(!value) return <span className="status muted-status" title="This backend does not record build status"><i/>unknown</span>; const tone=value.toLowerCase().includes("fail")?"bad":value.toLowerCase().includes("run")?"warn":"good"; return <span className={`status ${tone}`}><i/>{value}</span>; }

/// A bar-ranked list. Widths are relative to the largest row, so the shape of
/// the distribution is readable without axes.
function Bars({rows,empty}:{rows:{key:string;label:React.ReactNode;note?:React.ReactNode;value:number;display:string}[];empty:string}) {
  if(!rows.length) return <div className="empty">{empty}</div>;
  const max=Math.max(...rows.map(r=>r.value),0.0001);
  return <ul className="bars">{rows.map(r=><li key={r.key}><div className="bar-head"><span className="bar-label" title={typeof r.label==="string"?r.label:undefined}>{r.label}</span><span className="bar-value mono">{r.display}</span></div><div className="bar-track"><i style={{width:`${Math.max((r.value/max)*100,2)}%`}}/></div>{r.note&&<small>{r.note}</small>}</li>)}</ul>;
}

function Section({title,note,children}:{title:string;note?:string;children:React.ReactNode}) {
  return <div className="panel insight"><div className="panel-head"><div><h2>{title}</h2>{note&&<p>{note}</p>}</div></div>{children}</div>;
}

/// Hash routing keeps the dashboard a single self-contained HTML file — no
/// server-side routes to add, and `tiny_http` keeps serving one page — while
/// giving each build a real URL that can be linked, reloaded and reached with
/// the back button. `#/build/<id>` is the only route; everything else is the
/// overview.
function useRoute() {
  const read=()=>decodeURIComponent((location.hash.match(/^#\/build\/(.+)$/)||[])[1]||"");
  const [buildId,setBuildId]=useState(read);
  useEffect(()=>{
    const onChange=()=>setBuildId(read());
    addEventListener("hashchange",onChange);
    return ()=>removeEventListener("hashchange",onChange);
  },[]);
  return buildId;
}
const openBuild=(id:string)=>{ location.hash=`#/build/${encodeURIComponent(id)}`; };
const closeBuild=()=>{ if(history.length>1) history.back(); else location.hash=""; };

/// The build detail page. Fetches on its own so a reloaded or shared
/// `#/build/<id>` URL works without the overview having run first.
function BuildPage({id}:{id:string}) {
  const [build,setBuild]=useState<Detail|null>(null);
  const [error,setError]=useState("");
  useEffect(()=>{
    setBuild(null); setError("");
    scrollTo(0,0);
    get<Detail>(`/api/build/${encodeURIComponent(id)}`).then(setBuild).catch(e=>setError(e.message));
  },[id]);

  return <div className="shell"><Sidebar/><main>
    <header><div><p className="eyebrow"><a className="crumb" href="#">BUILDS</a> / DETAIL</p><h1>Build detail</h1><p className="lede mono">{id}</p></div><div className="header-actions"><button className="icon-button" onClick={closeBuild}>← Back to builds</button></div></header>
    {error&&<div className="error">{error}</div>}
    {!build&&!error&&<div className="panel"><div className="empty">Loading build…</div></div>}
    {build&&<>
      <section className="kpis">
        <Kpi label="Duration" value={seconds(build.total_seconds)} foot={build.category||"unknown"}/>
        <Kpi label="Files compiled" value={(build.compiled_count||0).toLocaleString()} foot="This build"/>
        <Kpi label="Cache hit rate" value={percent(build.cache_hit_rate)} foot="Across targets"/>
        <Kpi label="Recorded" value={date(build.recorded_at)} foot={build.project||"Unknown project"}/>
      </section>
      <section className="insights">
        <Section title="Targets" note="Slowest first">
          <Bars empty="No target timings for this build." rows={(build.targets||[]).slice(0,25).map(t=>({key:t.name,label:t.name,value:t.seconds,display:seconds(t.seconds),note:`${t.category||"unknown"}${t.fetched_from_cache?" · from cache":""}${t.compiled_count?` · ${t.compiled_count} files`:""}`}))}/>
        </Section>
        <Section title="Phases" note="Where this build spent its time">
          <Bars empty="No phase timings for this build." rows={(build.phases||[]).slice(0,25).map(p=>({key:p.name,label:p.name,value:p.seconds,display:seconds(p.seconds)}))}/>
        </Section>
      </section>
      <section className="panel"><div className="panel-head"><div><h2>Environment &amp; metadata</h2><p>Git, hardware and CI context recorded with this build</p></div></div>
        <div className="meta-grid">
          <div><span>Xcode</span><b>{build.xcode_version||"—"}</b></div>
          <div><span>Platform</span><b>{build.platform||"—"}</b></div>
          <div><span>Architecture</span><b>{build.architecture||"—"}</b></div>
          <div><span>Machine</span><b className="mono">{build.machine_id||"—"}</b></div>
          {Object.entries(build.metadata||{}).map(([k,v])=><div key={k}><span>{k}</span><b className="mono">{v}</b></div>)}
        </div>
        {!Object.keys(build.metadata||{}).length&&<p className="empty-note">No collected metadata for this build. Re-collect with <code>--collect-all</code> to record git, hardware and CI context.</p>}
      </section>
    </>}
    <footer>BuildLens<span>local-first build observability</span></footer>
  </main></div>;
}

function Sidebar() {
  return <aside><div className="brand"><span className="brandmark">B</span><span>BuildLens</span></div><nav><button className="active">▦ <span>Builds</span></button><button>⌁ <span>Trends</span></button><button>◌ <span>Diagnostics</span></button></nav><div className="side-note"><span className="pulse"/> Local collector<br/><small>Read-only workspace</small></div></aside>;
}

function App() {
  const routedBuild=useRoute();
  if(routedBuild) return <BuildPage id={routedBuild}/>;
  return <Overview/>;
}

function Overview() {
  const [projects,setProjects]=useState<Project[]>([]);
  const [project,setProject]=useState(localStorage.getItem("buildlens-project")||"");
  // Auto-refresh is on unless the user turned it off; the choice persists so a
  // reload does not silently re-enable polling they opted out of.
  const [autoRefresh,setAutoRefresh]=useState(localStorage.getItem("buildlens-autorefresh")!=="off");
  const [updatedAt,setUpdatedAt]=useState(0);
  const [token,setToken]=useState(localStorage.getItem("buildlens-token")||"");
  const [status,setStatus]=useState("loading");
  const [trend,setTrend]=useState<Trend[]>([]);
  const [builds,setBuilds]=useState<Build[]>([]);
  const [query,setQuery]=useState("");
  const [onlyFailures,setOnlyFailures]=useState(false);
  const [error,setError]=useState("");
  const [percentiles,setPercentiles]=useState<Percentiles|null>(null);
  const [targets,setTargets]=useState<Ranked[]>([]);
  const [phases,setPhases]=useState<Ranked[]>([]);
  const [files,setFiles]=useState<FileRow[]>([]);
  const [swift,setSwift]=useState<SwiftRow[]>([]);
  const [clusters,setClusters]=useState<DiagRow[]>([]);
  const [flaky,setFlaky]=useState<FlakyRow[]>([]);
  const [regressions,setRegressions]=useState<RegressionRow[]>([]);
  const [environment,setEnvironment]=useState<EnvRow[]>([]);
  const [git,setGit]=useState<GitRow[]>([]);
  const [daily,setDaily]=useState<DailyRow[]>([]);
  const [diagTrend,setDiagTrend]=useState<DiagTrendRow[]>([]);

  // Every project-scoped request goes through here, so a new panel cannot
  // forget the filter and quietly mix one project's numbers into another's.
  const scope=useCallback((path:string,extra="")=>`${path}?${extra}${extra?"&":""}${project?`project=${encodeURIComponent(project)}`:""}`,[project]);


  // `background` distinguishes the poll from a first load or a project change:
  // a poll must not blank the page to a spinner every few seconds.
  const load=useCallback((background=false)=>{
    if(!background) setStatus("loading");
    const ok=<T,>(p:Promise<T>,fallback:T)=>p.catch(()=>fallback);
    return Promise.all([
      get<{items:Trend[]}>(scope("/api/trends/duration","limit=80")),
      get<{builds:Build[]}>("/api/summary"),
      ok(get<Percentiles>(scope("/api/trends/percentiles","limit=100")),null as any),
      ok(get<{items:Ranked[]}>(scope("/api/trends/targets","builds=30&top=8")),{items:[]}),
      ok(get<{items:Ranked[]}>(scope("/api/trends/phases","builds=30&top=8")),{items:[]}),
      ok(get<{items:FileRow[]}>(scope("/api/files/slowest","builds=30&limit=10")),{items:[]}),
      ok(get<{items:SwiftRow[]}>(scope("/api/swift/slowest","builds=30&limit=10")),{items:[]}),
      ok(get<{items:DiagRow[]}>(scope("/api/diagnostics/clusters","builds=30&limit=8")),{items:[]}),
      ok(get<{items:FlakyRow[]}>(scope("/api/tests/flaky","builds=30&limit=8")),{items:[]}),
      ok(get<{items:RegressionRow[]}>(scope("/api/regressions","builds=10&limit=8")),{items:[]}),
      ok(get<{items:EnvRow[]}>(scope("/api/environment","builds=50")),{items:[]}),
      ok(get<{items:GitRow[]}>(scope("/api/git","builds=15")),{items:[]}),
      ok(get<{items:DailyRow[]}>(scope("/api/trends/daily","limit=30")),{items:[]}),
      ok(get<{items:DiagTrendRow[]}>(scope("/api/trends/diagnostics","limit=50")),{items:[]}),
      // Polled like everything else: fetching this once on mount froze the
      // project dropdown, so a project whose first build arrived while the
      // page was open never showed up in it.
      ok(get<{items:Project[]}>("/api/projects"),{items:[]}),
    ]).then(([t,s,pc,tg,ph,fl,sw,cl,fk,rg,en,gt,dy,dt,pj])=>{
      const items=t.items||[];
      const byId=new Map(items.map(x=>[x.id,x]));
      const allowed=new Set(items.map(x=>x.id));
      setTrend(items);
      setBuilds((s.builds||[]).filter(b=>!project||allowed.has(b.id)).map(b=>({...b,...(byId.get(b.id)||{})})));
      setPercentiles(pc); setTargets(tg.items||[]); setPhases(ph.items||[]); setFiles(fl.items||[]);
      setSwift(sw.items||[]); setClusters(cl.items||[]); setFlaky(fk.items||[]);
      setRegressions(rg.items||[]); setEnvironment(en.items||[]); setGit(gt.items||[]);
      setDaily(dy.items||[]); setDiagTrend(dt.items||[]); setProjects(pj.items||[]);
      setStatus("ready"); setError(""); setUpdatedAt(Date.now());
    // A failed poll leaves the last good data on screen: a dropped request is
    // not a reason to replace a working dashboard with an error page.
    }).catch(e=>{ if(!background){setError(e.message);setStatus("error");} });
  },[project,scope]);

  useEffect(()=>{ load(); },[load]);

  // Poll so a build collected while this page is open shows up on its own.
  // Paused when the tab is hidden — a background tab polling forever is pure
  // waste, and it refreshes on return anyway.
  useEffect(()=>{
    if(!autoRefresh) return;
    let timer=0;
    const tick=()=>{ if(!document.hidden) load(true); };
    const start=()=>{ timer=window.setInterval(tick,REFRESH_MS); };
    const onVisible=()=>{ if(!document.hidden) load(true); };
    start();
    document.addEventListener("visibilitychange",onVisible);
    return ()=>{ window.clearInterval(timer); document.removeEventListener("visibilitychange",onVisible); };
  },[autoRefresh,load]);

  const visible=useMemo(()=>builds.filter(b=>(!onlyFailures||(b.status||"").toLowerCase().includes("fail"))&&(!query||`${b.id} ${b.project||""} ${b.scheme||""} ${b.status||""}`.toLowerCase().includes(query.toLowerCase()))),[builds,query,onlyFailures]);
  const recorded=trend.filter(x=>x.total_seconds!=null);
  const avg=recorded.length?recorded.reduce((a,x)=>a+x.total_seconds!,0)/recorded.length:null;
  const latest=recorded.at(-1);
  const failures=builds.filter(x=>(x.status||"").toLowerCase().includes("fail")).length;
  // Neither backend reports per-build status or warning counts today, so the
  // tiles that depend on them render "—" instead of a misleading zero.
  const knowsStatus=builds.some(x=>x.status!=null);
  return <div className="shell"><Sidebar/><main>
    <header><div><p className="eyebrow">BUILD INTELLIGENCE / OVERVIEW</p><h1>Builds</h1><p className="lede">See what changed, what slowed down, and where to look next.</p></div><div className="header-actions"><input className="token-input" type="password" value={token} onChange={e=>{setToken(e.target.value);localStorage.setItem("buildlens-token",e.target.value)}} placeholder="Server token" aria-label="Server token"/><select value={project} onChange={e=>{setProject(e.target.value);localStorage.setItem("buildlens-project",e.target.value)}}><option value="">All projects</option>{projects.map(p=><option key={p.project} value={p.project}>{p.project} · {p.builds}</option>)}</select><button className="icon-button" onClick={()=>load(true)} title="Refresh now">↻</button><button className={`icon-button live${autoRefresh?" on":""}`} onClick={()=>{const next=!autoRefresh;setAutoRefresh(next);localStorage.setItem("buildlens-autorefresh",next?"on":"off");}} title={autoRefresh?`Updating every ${REFRESH_MS/1000}s — click to pause`:"Auto-refresh paused — click to resume"}><i/>{autoRefresh?"Live":"Paused"}</button></div></header>
    {error&&<div className="error">{error}</div>}
    <section className="kpis"><Kpi label="Builds recorded" value={String(builds.length)} foot={project||"All projects"}/><Kpi label="Average duration" value={seconds(avg)} foot="Recent saved builds"/><Kpi label="Median / p95" value={percentiles?.enough_history?`${seconds(percentiles.p50)} / ${seconds(percentiles.p95)}`:"—"} foot={percentiles?.enough_history?`Over ${percentiles.builds} builds`:`Needs 5+ builds (${percentiles?.builds||0})`}/><Kpi label="Failed builds" value={knowsStatus?String(failures):"—"} foot={knowsStatus?(failures?"Needs attention":"No failures in view"):"Not recorded by this backend"} tone={knowsStatus&&failures?"bad":undefined}/></section>

    <section className="workspace"><div className="panel trend-panel"><div className="panel-head"><div><h2>Duration trend</h2><p>Recent builds · select a row below for the full breakdown</p></div><span className="metric">Latest <b>{seconds(latest?.total_seconds)}</b></span></div><div className="chart"><Sparkline items={trend}/></div><div className="chart-foot"><span>Older</span><span className="legend"><i/> build duration</span><span>Latest</span></div></div>
      <div className="panel attention"><div className="panel-head"><div><h2>Needs attention</h2><p>Signals from the current scope</p></div></div>
        <div className="attention-row"><strong className={knowsStatus?(failures?"red":"green"):""}>{knowsStatus?failures:"—"}</strong><span>failed builds<br/><small>{knowsStatus?(failures?"Open the latest failure":"Everything is green"):"Not recorded by this backend"}</small></span></div>
        <div className="attention-row"><strong className={regressions.length?"red":"green"}>{regressions.length}</strong><span>slower targets<br/><small>{regressions.length?`${regressions[0].name} +${seconds(regressions[0].delta_seconds)}`:"No target trending slower"}</small></span></div>
        <div className="attention-row"><strong className={flaky.filter(f=>f.flaky).length?"red":"green"}>{flaky.filter(f=>f.flaky).length}</strong><span>flaky tests<br/><small>{flaky.filter(f=>f.flaky).length?"Passing and failing across builds":"No mixed test outcomes"}</small></span></div>
      </div></section>

    <section className="insights">
      <Section title="Slowest targets" note="Averaged across recent builds">
        <Bars empty="No target timings recorded yet." rows={targets.map(t=>({key:t.name,label:t.name,value:t.avg_seconds||0,display:seconds(t.avg_seconds),note:`${t.observations} builds · peak ${seconds(t.max_seconds)}${t.cached_builds?` · ${t.cached_builds} cached`:""}`}))}/>
      </Section>
      <Section title="Time by phase" note="Where the build spends its time">
        <Bars empty="No phase timings recorded yet." rows={phases.map(p=>({key:p.name,label:p.name,value:p.avg_seconds||0,display:seconds(p.avg_seconds),note:`${p.observations} builds · peak ${seconds(p.max_seconds)}`}))}/>
      </Section>
      <Section title="Slowest files" note="Per-file compile time">
        <Bars empty="No per-file timings in these builds." rows={files.map(f=>({key:f.file,label:<span title={f.file}>{short(f.file)}</span>,value:f.avg_seconds||0,display:seconds(f.avg_seconds),note:`${f.target||"unknown target"}${(f.compilations||0)>f.observations?` · compiled ${f.compilations}×`:""}`}))}/>
      </Section>
      <Section title="Swift type-check hotspots" note="Needs -warn-long-function-bodies">
        <Bars empty="Not enabled for these builds — add -warn-long-function-bodies or -warn-long-expression-type-checking to see hotspots." rows={swift.map(s=>({key:`${s.file}:${s.line}:${s.kind}`,label:<span title={`${s.file}:${s.line}`}>{s.symbol||`${short(s.file)}:${s.line}`}</span>,value:s.avg_milliseconds||0,display:ms(s.avg_milliseconds),note:`${s.kind.replace("_"," ")} · ${short(s.file)}:${s.line}`}))}/>
      </Section>
      <Section title="Recurring diagnostics" note="Ranked by how many builds they appear in">
        <Bars empty="No diagnostics recorded for these builds." rows={clusters.map(c=>({key:c.fingerprint,label:<span title={c.message||""}>{c.message||c.fingerprint}</span>,value:c.builds,display:`${c.builds} builds`,note:`${c.severity||"unknown"} · ${c.category||"uncategorized"}${c.file?` · ${short(c.file)}`:""}`}))}/>
      </Section>
      <Section title="Environment mix" note="A duration shift here is a setup change, not a code change">
        <Bars empty="No environment recorded." rows={environment.map(e=>({key:`${e.xcode_version}-${e.platform}-${e.architecture}`,label:`Xcode ${e.xcode_version} · ${e.architecture}`,value:e.builds,display:`${e.builds} builds`,note:`${e.platform} · ${e.machines} machine(s) · avg ${seconds(e.avg_seconds)}`}))}/>
      </Section>
      <Section title="Day over day" note="Median and p95 per calendar day">
        <Bars empty="No daily history yet." rows={daily.map(d=>({key:d.day,label:d.day,value:d.p50||0,display:`${seconds(d.p50)} / ${seconds(d.p95)}`,note:`${d.builds} build(s) · median / p95`}))}/>
      </Section>
      <Section title="Diagnostics over time" note="Warnings and errors per build">
        <Bars empty="No diagnostics recorded for these builds." rows={diagTrend.filter(d=>d.warnings+d.errors>0).map(d=>({key:d.id,label:date(d.recorded_at),value:d.warnings+d.errors,display:`${d.warnings} warn · ${d.errors} err`,note:`${d.unique_diagnostics} unique`}))}/>
      </Section>
      <Section title="Recent commits" note="What was checked out when each build ran">
        {git.length?<ul className="rows">{git.map(g=><li key={g.id}><span className="bar-label">{g.branch||"unknown branch"}{g.dirty==="true"&&<em className="dirty"> · uncommitted changes</em>}</span><span className="mono">{seconds(g.total_seconds)}</span><small className="mono">{(g.commit||"").slice(0,10)||"no commit"} · {g.category} · {date(g.recorded_at)}</small></li>)}</ul>:<div className="empty">No git context recorded. Collect with <code>--collect-git</code> to link builds to commits.</div>}
      </Section>
    </section>

    {(regressions.length>0||flaky.length>0)&&<section className="insights">
      {regressions.length>0&&<Section title="Targets trending slower" note="Recent builds vs the window before them — a trend, not an environment-matched baseline">
        <ul className="rows">{regressions.map(r=><li key={r.name}><span className="bar-label">{r.name}</span><span className="mono red">+{seconds(r.delta_seconds)} ({r.delta_percent.toFixed(0)}%)</span><small>{seconds(r.previous_seconds)} → {seconds(r.current_seconds)} · {r.observations} observations</small></li>)}</ul>
      </Section>}
      {flaky.length>0&&<Section title="Failing and flaky tests" note="Mixed pass/fail is flaky; always-failing is broken">
        <ul className="rows">{flaky.map(f=><li key={`${f.suite}.${f.test}`}><span className="bar-label">{f.suite}.{f.test}</span><span className={f.flaky?"mono warnpill":"mono red"}>{f.flaky?"flaky":"failing"}</span><small>{f.failed} failed · {f.passed} passed of {f.runs} runs</small></li>)}</ul>
      </Section>}
    </section>}

    <section className="panel build-panel"><div className="panel-head table-head"><div><h2>Latest builds</h2><p>{visible.length} of {builds.length} builds · select a row to inspect its analysis</p></div><div className="filters"><input value={query} onChange={e=>setQuery(e.target.value)} placeholder="Search builds…"/><label><input type="checkbox" checked={onlyFailures} onChange={e=>setOnlyFailures(e.target.checked)}/> failures only</label></div></div><div className="table-wrap"><table><thead><tr><th>Build</th><th>Project / scheme</th><th>Duration</th><th>Category</th><th>Diagnostics</th><th>Status</th><th>Recorded</th></tr></thead><tbody>{visible.slice(0,80).map(b=><tr key={b.id} onClick={()=>openBuild(b.id)}><td><a className="build-id" href={`#/build/${encodeURIComponent(b.id)}`} onClick={e=>e.preventDefault()}>#{b.id}</a></td><td><b>{b.project||"Unknown project"}</b><small>{b.scheme||"No scheme"}</small></td><td className="mono">{seconds(b.total_seconds)}</td><td><span className="category">{b.category||"unknown"}</span></td><td className="mono diagnostics">{b.raw_warnings!=null?<span>{b.raw_warnings} warnings</span>:<span className="muted">—</span>}{(b.failed_tests||0)>0&&<span className="red"> · {b.failed_tests} failed tests</span>}</td><td><Status value={b.status}/></td><td className="muted">{date(b.recorded_at)}</td></tr>)}</tbody></table>{!visible.length&&<div className="empty">No builds match these filters.</div>}</div></section>

    <footer>{status==="loading"?"Refreshing data…":updatedAt?`Updated ${new Date(updatedAt).toLocaleTimeString()}`:""}<span>BuildLens · local-first build observability</span></footer>
  </main></div>;
}
function Kpi({label,value,foot,tone=""}:{label:string;value:string;foot:string;tone?:string}){return <div className="kpi"><span>{label}</span><strong className={tone}>{value}</strong><small>{foot}</small></div>}
createRoot(document.getElementById("root")!).render(<StrictMode><App/></StrictMode>);
