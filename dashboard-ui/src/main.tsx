import { StrictMode, useCallback, useEffect, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";

// Build ids are the activity log's uniqueIdentifier (or a `sha:` content hash
// when Xcode leaves it empty) — always a string, never a row number.
type Build = { id:string; recorded_at:number; status?:string|null; total_seconds?:number|null; test_duration_seconds?:number|null; category?:string|null; project?:string|null; raw_warnings?:number; total_tests?:number; failed_tests?:number; crashed_tests?:number; scheme?:string|null; cache_hit_rate?:number|null; };
type Project = { project:string; builds:number };
type Trend = { id:string; total_seconds?:number|null; category?:string|null; recorded_at:number; project?:string|null };
type TargetRow = { name:string; seconds:number; category?:string|null; fetched_from_cache?:boolean; compiled_count?:number };
type PhaseRow = { name:string; seconds:number };
/// Per-build rows, as opposed to the averaged `FileRow`/`SwiftRow`/`DiagRow`
/// the Performance and Health tabs use. One build has a measurement, not an
/// observation count, so these carry `seconds`/`milliseconds` directly.
type BuildFileRow = { file:string; target?:string|null; seconds:number; compilations?:number|null; step_type?:string };
type BuildStepRow = { step_type:string; title:string; file?:string|null; target?:string|null; architecture?:string|null; seconds:number; started_at?:number|null; ended_at?:number|null; fetched_from_cache:boolean; executed:boolean };
type StepTotals = { total:number; executed:number; executed_seconds:number };
type BuildSwiftRow = { file:string; line:number; symbol?:string|null; kind:string; milliseconds:number; target?:string|null };
type BuildDiagRow = { fingerprint:string; severity:string; category:string; message:string; file?:string|null; line?:number|null; target?:string|null; occurrences:number };
type BuildTestRow = { suite:string; test:string; status:string; seconds?:number|null; message?:string|null; attempts?:number };
type TestTotals = { total:number; failed:number; seconds:number };
type Detail = Build & { compiled_count?:number; replayed_steps?:number; machine_id?:string|null; user?:string|null; host?:string|null; xcode_version?:string|null; platform?:string|null; architecture?:string|null; targets?:TargetRow[]; phases?:PhaseRow[]; metadata?:Record<string,string>; files?:BuildFileRow[]; swift?:BuildSwiftRow[]; diagnostics?:BuildDiagRow[]; tests?:BuildTestRow[]; test_totals?:TestTotals; steps?:BuildStepRow[]; step_totals?:StepTotals; };
type Percentiles = { builds:number; enough_history:boolean; p50?:number|null; p95?:number|null; min_seconds?:number|null; max_seconds?:number|null; avg_seconds?:number|null };
type Ranked = { name:string; observations:number; avg_seconds?:number|null; max_seconds?:number|null; cached_builds?:number };
type FileRow = { file:string; target?:string|null; observations:number; avg_seconds?:number|null; max_seconds?:number|null; compilations?:number|null };
type SwiftRow = { file:string; line:number; symbol?:string|null; kind:string; observations:number; avg_milliseconds?:number|null; max_milliseconds?:number|null };
type DiagRow = { fingerprint:string; severity?:string|null; category?:string|null; message?:string|null; file?:string|null; builds:number; occurrences?:number|null };
type FlakyRow = { suite:string; test:string; runs:number; failed:number; passed:number; flaky:boolean; retried_builds?:number };
type RegressionRow = { name:string; current_seconds:number; previous_seconds:number; delta_seconds:number; delta_percent:number; observations:number };
type PeopleRow = { user:string; builds:number; machines:number; avg_seconds?:number|null; failed:number };
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
/// A 401/403 from any panel means this backend authenticates.
const isAuthError = (message:string) => /^40[13] /.test(message);
const seconds = (n?:number|null) => n == null ? "—" : `${n < 10 ? n.toFixed(1) : n.toFixed(0)}s`;
const ms = (n?:number|null) => n == null ? "—" : `${n.toFixed(0)}ms`;
const percent = (n?:number|null) => n == null ? "—" : `${(n*100).toFixed(0)}%`;
const date = (n:number) => new Date(n*1000).toLocaleString(undefined,{month:"short",day:"numeric",hour:"2-digit",minute:"2-digit"});
const short = (s?:string|null) => s ? s.split("/").pop() || s : "—";
/// How often the overview re-fetches.
///
/// The end-to-end delay from finishing a build to seeing it is this plus the
/// watcher's scan interval plus about a second of waiting for Xcode to finish
/// writing the log. Two seconds here keeps that inside a few seconds, which is
/// short enough that a build feels like it appears on its own; the requests are
/// small reads against a local database, so the cost is trivial.
const REFRESH_MS = 2000;

function Sparkline({items}:{items:Trend[]}) { const points=items.filter(x=>x.total_seconds!=null); if(!points.length) return <div className="empty">Collect a build to start the trend.</div>; const max=Math.max(...points.map(x=>x.total_seconds!),1); const path=points.map((x,i)=>`${i?"L":"M"}${(i/(points.length-1||1))*100},${100-(x.total_seconds!/max)*82}`).join(" "); return <svg className="spark" viewBox="0 0 100 100" preserveAspectRatio="none"><path d={path} fill="none" stroke="currentColor" strokeWidth="2.5" vectorEffect="non-scaling-stroke"/><path d={`${path} L100,100 L0,100 Z`} fill="currentColor" opacity=".10"/></svg>; }
/// A backend that does not record build status (the team server, whose wire
/// format carries timings only) sends no `status` at all. Show that as unknown
/// rather than inventing a green "succeeded".
function Status({value}:{value?:string|null}) {
  if(!value) return <span className="status muted-status" title="This backend does not record build status"><i/>unknown</span>;
  // `pending_tests` is BuildLens's own, not Xcode's: the compile succeeded and
  // a test run is expected but has not reported. Shown as its own state rather
  // than rounded to success, because a build left pending is one whose tests
  // never arrived — worth seeing, not worth calling green.
  if(value==="pending_tests") return <span className="status warn" title="Compiled; waiting for test results"><i/>tests running</span>;
  const tone=value.toLowerCase().includes("fail")?"bad":value.toLowerCase().includes("run")?"warn":"good";
  return <span className={`status ${tone}`}><i/>{value}</span>;
}

/// What a build's row says about its tests.
///
/// Exists because "failed" alone is ambiguous: a compile that broke and a
/// suite that went red are different problems needing different responses, and
/// the status column cannot tell them apart. A plain build shows an em dash —
/// it ran no tests, which is not the same as running tests that all passed.
function TestCell({build}:{build:Build}) {
  if(build.status==="pending_tests") return <span className="warnpill" title="Compiled; Xcode writes results when the test run finishes">running…</span>;
  const total=build.total_tests||0;
  if(!total) return <span className="muted" title="This build ran no tests">—</span>;
  const failed=build.failed_tests||0;
  return failed>0
    ? <span className="red" title={`${failed} of ${total} tests failed`}>{failed}/{total} failed</span>
    : <span className="ok-text" title={`All ${total} tests passed`}>{total} passed</span>;
}

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

/// Inline stroke icons.
///
/// Drawn here rather than pulled from an icon package: the dashboard ships as
/// one self-contained HTML file, so a dependency that loads glyphs at runtime
/// would not survive the bundle. `currentColor` lets each tile tint its own.
const Icon = ({d,size=18}:{d:string;size?:number}) =>
  <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><path d={d}/></svg>;
const ICONS = {
  builds:"M3 5h18M3 12h18M3 19h18",
  trends:"M3 17l5-6 4 4 7-9M21 6v5h-5",
  health:"M3 12h4l2-6 4 13 2-7h6",
  clock:"M12 21a9 9 0 100-18 9 9 0 000 18zM12 7v5l3 2",
  gauge:"M12 20a8 8 0 100-16 8 8 0 000 16zM12 12l4-3",
  layers:"M12 3l9 5-9 5-9-5 9-5zM3 14l9 5 9-5",
  alert:"M12 4l9 16H3l9-16zM12 10v4M12 17.5v.5",
  file:"M14 3H7a2 2 0 00-2 2v14a2 2 0 002 2h10a2 2 0 002-2V8l-5-5zM14 3v5h5",
  cache:"M4 7c0-1.7 3.6-3 8-3s8 1.3 8 3-3.6 3-8 3-8-1.3-8-3zM4 7v10c0 1.7 3.6 3 8 3s8-1.3 8-3V7",
  calendar:"M7 3v4M17 3v4M4 9h16M5 5h14a1 1 0 011 1v13a1 1 0 01-1 1H5a1 1 0 01-1-1V6a1 1 0 011-1z",
  arrow:"M5 12h13M13 6l6 6-6 6",
  logo:"M4 18l5-7 4 4 7-10",
} as const;

/// The three views. `builds` is the default, so `#/` and an empty hash both
/// land there. `group` places each in the sidebar's labelled sections.
const TABS = [
  {id:"builds", icon:ICONS.builds, group:"Main", label:"Builds", lede:"See what changed, what slowed down, and where to look next."},
  {id:"trends", icon:ICONS.trends, group:"Analysis", label:"Performance", lede:"Where build time goes, and what is getting slower."},
  {id:"diagnostics", icon:ICONS.health, group:"Analysis", label:"Health", lede:"Warnings, failing tests, and the commits behind them."},
] as const;
type TabId = (typeof TABS)[number]["id"];

const isTab = (value:string):value is TabId => TABS.some(t=>t.id===value);

/// Hash routing keeps the dashboard a single self-contained HTML file — no
/// server-side routes to add, and `tiny_http` keeps serving one page — while
/// giving each build a real URL that can be linked, reloaded and reached with
/// the back button.
///
/// Two routes: `#/build/<id>` for one build, and `#/<tab>` for a view. A tab
/// in the URL rather than in component state means a view survives a reload
/// and can be sent to someone, which is the same reason the build detail is
/// routed rather than held in state.
function useRoute() {
  const read=()=>decodeURIComponent((location.hash.match(/^#\/build\/(.+)$/)||[])[1]||"");
  const readTab=():TabId=>{ const raw=(location.hash.match(/^#\/([a-z]+)$/)||[])[1]||""; return isTab(raw)?raw:"builds"; };
  const [tab,setTabState]=useState(readTab);
  const [buildId,setBuildId]=useState(read);
  useEffect(()=>{
    const onChange=()=>{ setBuildId(read()); setTabState(readTab()); };
    addEventListener("hashchange",onChange);
    return ()=>removeEventListener("hashchange",onChange);
  },[]);
  return {buildId, tab};
}
const openBuild=(id:string)=>{ location.hash=`#/build/${encodeURIComponent(id)}`; };
const openTab=(tab:TabId)=>{ location.hash=`#/${tab}`; };
/// Returns to the overview rather than to whatever preceded it, so closing a
/// build opened from a shared link does not leave the page blank.
const closeBuild=()=>{ if(history.length>1) history.back(); else location.hash="#/builds"; };

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

  return <div className="shell"><Sidebar tab="builds"/><main>
    <header><div><p className="eyebrow"><a className="crumb" href="#/builds">BUILDS</a> / DETAIL</p><h1>Build detail</h1><p className="lede mono">{id}</p></div><div className="header-actions"><button className="icon-button" onClick={closeBuild}>← Back to builds</button></div></header>
    {error&&<div className="error">{error}</div>}
    {!build&&!error&&<div className="panel"><div className="empty">Loading build…</div></div>}
    {build&&<>
      <section className="kpis">
        <Kpi icon={ICONS.clock} label="Duration" value={seconds(build.total_seconds)} foot={build.category||"unknown"}/>
        <Kpi icon={ICONS.file} tone="blue" label="Files compiled" value={(build.compiled_count||0).toLocaleString()} foot="This build"/>
        {/* Cache hit rate only when there is a cache to hit. Xcode never sets
            the per-step flag for its own incremental reuse — work it skips is
            absent from the log rather than marked cached — so the bit is true
            only behind a remote build cache. On a stock Xcode setup the tile
            read a permanent "0%", which looks like a diagnosis of a missing
            cache rather than the absence of one.

            What varies on those setups is how much of the logged build was
            actually re-run: Xcode restates the whole graph each time, so a
            9.7-second rebuild can list twelve thousand steps it did not
            execute. That number explains the duration; the cache one cannot. */}
        {build.cache_hit_rate
          ? <Kpi icon={ICONS.cache} tone="ok" label="Cache hit rate" value={percent(build.cache_hit_rate)} foot="Across targets"/>
          : <Kpi icon={ICONS.layers} tone="violet" label="Steps re-run"
                 value={(build.compiled_count||0).toLocaleString()}
                 foot={build.replayed_steps ? `${build.replayed_steps.toLocaleString()} reused from an earlier build` : "Everything in this build ran"}/>}
        <Kpi icon={ICONS.calendar} tone="violet" label="Recorded" value={date(build.recorded_at)} foot={build.project||"Unknown project"}/>
      </section>
      <section className="insights">
        <Section title="Targets" note="Slowest first">
          <Bars empty="No target timings for this build." rows={(build.targets||[]).slice(0,25).map(t=>({key:t.name,label:t.name,value:t.seconds,display:seconds(t.seconds),note:`${t.category||"unknown"}${t.fetched_from_cache?" · from cache":""}${t.compiled_count?` · ${t.compiled_count} files`:""}`}))}/>
        </Section>
        <Section title="Phases" note="Where this build spent its time">
          <Bars empty="No phase timings for this build." rows={(build.phases||[]).slice(0,25).map(p=>({key:p.name,label:p.name,value:p.seconds,display:seconds(p.seconds)}))}/>
        </Section>
        <Section title="Slowest files" note="Compile time in this build">
          <Bars empty="No per-file timings in this build." rows={(build.files||[]).slice(0,25).map(f=>({key:f.file,label:<span title={f.file}>{short(f.file)}</span>,value:f.seconds,display:seconds(f.seconds),note:`${f.target||"unknown target"}${(f.compilations||0)>1?` · compiled ${f.compilations}×`:""}`}))}/>
        </Section>
        <Section title="Swift type-check hotspots" note="Needs -warn-long-function-bodies">
          <Bars empty="Not enabled for this build — add -warn-long-function-bodies or -warn-long-expression-type-checking to see hotspots." rows={(build.swift||[]).slice(0,25).map(s=>({key:`${s.file}:${s.line}:${s.kind}`,label:<span title={`${s.file}:${s.line}`}>{s.symbol||`${short(s.file)}:${s.line}`}</span>,value:s.milliseconds,display:ms(s.milliseconds),note:`${s.kind.replace(/_/g," ")} · ${short(s.file)}:${s.line}`}))}/>
        </Section>
      </section>

      {/* A sequence, not a ranking: this panel answers "what ran, in what
          order", so the rows stay in timeline order however small the
          durations get. Sorting by duration would make it another slowest-N
          list, which the three panels above already are.

          Replayed steps are dimmed and labelled rather than hidden. Xcode
          re-emits them carrying their ORIGINAL durations from an earlier
          build, so showing them undimmed would put times on this page that
          were never spent in this build. */}
      {!!(build.steps||[]).length&&<section className="panel">
        <div className="panel-head"><div><h2>Build steps</h2>
          <p>{build.step_totals?`${build.step_totals.executed.toLocaleString()} ran of ${build.step_totals.total.toLocaleString()} logged · ${seconds(build.step_totals.executed_seconds)} of CPU time`:"In the order Xcode ran them"}</p>
        </div></div>
        <div className="steps">
          {(build.steps||[]).map((st,i)=>
            <div key={i} className={`step${st.executed?"":" replayed"}`}>
              <span className="step-type">{st.step_type.replace(/([A-Z])/g," $1").toLowerCase().trim()}</span>
              <span className="step-title" title={st.file||st.title}>{st.title}</span>
              <span className="step-note">{[st.target,st.architecture,st.fetched_from_cache?"cached":null,st.executed?null:"replayed"].filter(Boolean).join(" · ")}</span>
              <span className="step-secs">{seconds(st.seconds)}</span>
            </div>)}
        </div>
        {/* Says what is not shown rather than letting 500 read as the total. */}
        {!!(build.step_totals&&build.step_totals.total>(build.steps||[]).length)&&
          <p className="empty-note">Showing the first {(build.steps||[]).length.toLocaleString()} of {build.step_totals!.total.toLocaleString()} steps, in build order.</p>}
      </section>}

      {/* Errors first, because a failed build's reason is the question this
          page exists to answer. Ranked by occurrences within each severity. */}
      <section className="panel"><div className="panel-head"><div><h2>Diagnostics</h2><p>Warnings and errors reported by this build</p></div>
        {!!(build.diagnostics||[]).length&&<span className="metric">{build.errors||0} <b>errors</b> · {build.raw_warnings||0} warnings</span>}</div>
        {(build.diagnostics||[]).length
          ? <ul className="diag-list">{(build.diagnostics||[]).slice(0,25).map(d=>
              <li key={d.fingerprint}>
                <span className={`sev ${d.severity==="error"?"bad":""}`}>{d.severity}</span>
                <div><b>{d.message}</b>
                  <small>{d.file?<span className="mono" title={d.file}>{short(d.file)}{d.line?`:${d.line}`:""}</span>:"no location"}
                    {d.target?` · ${d.target}`:""} · {d.category.replace(/_/g," ")}
                    {d.occurrences>1?` · ${d.occurrences}×`:""}</small></div>
              </li>)}</ul>
          : <div className="empty">No warnings or errors recorded for this build.</div>}
      </section>

      {/* Only rendered when the build ran tests: an empty panel on every
          build-only log would read as "no tests exist" rather than "none ran". */}
      {!!build.test_totals?.total&&<section className="panel">
        <div className="panel-head"><div><h2>Tests</h2><p>Results recorded with this build</p></div>
          <span className="metric">{build.test_totals.failed
            ? <><b className="red">{build.test_totals.failed}</b> of {build.test_totals.total} failed</>
            : <>All <b>{build.test_totals.total}</b> passed</>} · {seconds(build.test_totals.seconds)}</span></div>
        <ul className="diag-list">{(build.tests||[]).slice(0,25).map(t=>
          <li key={`${t.suite}.${t.test}`}>
            {/* A test Xcode retried is neither a clean pass nor a failure: its
                status is the run that decided the build, and the retry count is
                what says it was unreliable getting there. */}
            <span className={`sev ${t.status==="failed"?"bad":(t.attempts||1)>1?"warn":"ok"}`}>{(t.attempts||1)>1?"retried":t.status}</span>
            <div><b>{t.suite}.{t.test}</b>
              <small>{seconds(t.seconds)}{(t.attempts||1)>1?` · ${t.status} after ${t.attempts} attempts`:""}{t.message?` · ${t.message}`:""}</small></div>
          </li>)}</ul>
      </section>}
      {/* Git first: "which commit was this?" is the question asked of a build
          far more often than which CPU built it, and in the flat metadata grid
          it was three rows lost among fifteen. */}
      {(() => {
        const meta = build.metadata||{};
        const git = {branch:meta["git.branch"], commit:meta["git.commit"], dirty:meta["git.dirty"]};
        if(!git.branch&&!git.commit) return null;
        return <section className="panel"><div className="panel-head"><div><h2>Git</h2><p>What was checked out when this build ran</p></div>
          {git.dirty==="true"&&<span className="sev">uncommitted changes</span>}</div>
          <div className="meta-grid">
            <div><span>Branch</span><b>{git.branch||"—"}</b></div>
            <div><span>Commit</span><b className="mono" title={git.commit}>{git.commit?git.commit.slice(0,12):"—"}</b></div>
            <div><span>Working tree</span><b>{git.dirty==null?"—":git.dirty==="true"?"Modified":"Clean"}</b></div>
          </div>
        </section>;
      })()}
      <section className="panel"><div className="panel-head"><div><h2>Environment &amp; metadata</h2><p>Git, hardware and CI context recorded with this build</p></div></div>
        <div className="meta-grid">
          {/* Only what this build actually recorded: a permanent row of em-dashes
              reads as "BuildLens lost this", not "nothing was collected". */}
          {build.xcode_version&&<div><span>Xcode</span><b>{build.xcode_version}</b></div>}
          {build.platform&&<div><span>Platform</span><b>{build.platform}</b></div>}
          {build.architecture&&<div><span>Architecture</span><b>{build.architecture}</b></div>}
          {build.machine_id&&<div><span>Machine</span><b className="mono">{build.machine_id}</b></div>}
          {/* Only present when the build was sent with --attribution identified;
              every other build has no name to show and shows no row. */}
          {build.user&&<div><span>User</span><b className="mono">{build.user}</b></div>}
          {build.host&&<div><span>Host</span><b className="mono">{build.host}</b></div>}
          {Object.entries(build.metadata||{}).map(([k,v])=><div key={k}><span>{k}</span><b className="mono">{v}</b></div>)}
        </div>
        {!Object.keys(build.metadata||{}).length&&<p className="empty-note">No collected metadata for this build. Re-collect with <code>--collect-all</code> to record git, hardware and CI context.</p>}
      </section>
    </>}
    <footer>BuildLens<span>local-first build observability</span></footer>
  </main></div>;
}

function Sidebar({tab}:{tab:TabId}) {
  // Grouped rather than flat, so the nav reads as sections once more views are
  // added. Each group header renders once, before the first tab that claims it.
  const groups = TABS.reduce<{group:string;tabs:typeof TABS[number][]}[]>((acc,t)=>{
    const last=acc.at(-1);
    if(last?.group===t.group) last.tabs.push(t); else acc.push({group:t.group,tabs:[t]});
    return acc;
  },[]);
  return <aside>
    <div className="brand"><Icon d={ICONS.logo} size={22}/><span>BuildLens</span></div>
    {groups.map(g=><div key={g.group}>
      <p className="nav-group">{g.group}</p>
      <nav>{g.tabs.map(t=>
        <button key={t.id} className={t.id===tab?"active":""} aria-current={t.id===tab?"page":undefined}
                onClick={()=>openTab(t.id)}><Icon d={t.icon}/><span>{t.label}</span></button>)}
      </nav>
    </div>)}
    <div className="side-note"><span className="pulse"/><div>Local collector<small>Read-only workspace</small></div></div></aside>;
}

function App() {
  const {buildId,tab}=useRoute();
  if(buildId) return <BuildPage id={buildId}/>;
  return <Overview tab={tab}/>;
}

function Overview({tab}:{tab:TabId}) {
  const [projects,setProjects]=useState<Project[]>([]);
  const [project,setProject]=useState(localStorage.getItem("buildlens-project")||"");
  // Auto-refresh is on unless the user turned it off; the choice persists so a
  // reload does not silently re-enable polling they opted out of.
  const [autoRefresh,setAutoRefresh]=useState(localStorage.getItem("buildlens-autorefresh")!=="off");
  const [updatedAt,setUpdatedAt]=useState(0);
  const [token,setToken]=useState(localStorage.getItem("buildlens-token")||"");
  /// Whether this backend actually wants a token.
  ///
  /// The local dashboard has no authentication, so the field is noise there —
  /// it implies a credential is expected and there is none to give. Only a
  /// backend that refuses a request (buildlens-server with BUILDLENS_TOKEN set)
  /// needs one, so the field appears when that happens, or when a token is
  /// already stored from a previous session.
  const [needsToken,setNeedsToken]=useState(!!localStorage.getItem("buildlens-token"));
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
  const [people,setPeople]=useState<PeopleRow[]>([]);
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
      ok(get<{items:PeopleRow[]}>(scope("/api/people","builds=50")),{items:[]}),
      ok(get<{items:GitRow[]}>(scope("/api/git","builds=15")),{items:[]}),
      ok(get<{items:DailyRow[]}>(scope("/api/trends/daily","limit=30")),{items:[]}),
      ok(get<{items:DiagTrendRow[]}>(scope("/api/trends/diagnostics","limit=50")),{items:[]}),
      // Polled like everything else: fetching this once on mount froze the
      // project dropdown, so a project whose first build arrived while the
      // page was open never showed up in it.
      ok(get<{items:Project[]}>("/api/projects"),{items:null as unknown as Project[]}),
    ]).then(([t,s,pc,tg,ph,fl,sw,cl,fk,rg,en,pe,gt,dy,dt,pj])=>{
      const items=t.items||[];
      const byId=new Map(items.map(x=>[x.id,x]));
      const allowed=new Set(items.map(x=>x.id));
      setTrend(items);
      setBuilds((s.builds||[]).filter(b=>!project||allowed.has(b.id)).map(b=>({...b,...(byId.get(b.id)||{})})));
      setPercentiles(pc); setTargets(tg.items||[]); setPhases(ph.items||[]); setFiles(fl.items||[]);
      setSwift(sw.items||[]); setClusters(cl.items||[]); setFlaky(fk.items||[]);
      setRegressions(rg.items||[]); setEnvironment(en.items||[]); setPeople(pe.items||[]); setGit(gt.items||[]);
      setDaily(dy.items||[]); setDiagTrend(dt.items||[]);
      // `null` means the request failed, `[]` means there are genuinely no
      // projects. Conflating them is what let a stale filter survive against
      // an empty database — the case the guard below exists for.
      const known=pj.items; setProjects(known||[]);
      // A project remembered in localStorage can outlive the builds that named
      // it: history pruned, a project renamed, or a brand new database. The
      // filter then hides every build while the dropdown does not even list
      // the value doing the hiding, so the page reads as "no builds recorded
      // for X" for a project that no longer exists. Fall back to All projects.
      if(project && known && !known.some(p=>p.project===project)){
        setProject(""); localStorage.removeItem("buildlens-project");
      }
      setStatus("ready"); setError(""); setUpdatedAt(Date.now());
    // A failed poll leaves the last good data on screen: a dropped request is
    // not a reason to replace a working dashboard with an error page.
    }).catch(e=>{
      // A refused request is what proves this backend authenticates, so the
      // token field appears exactly then rather than always.
      if(isAuthError(e.message)){
        setNeedsToken(true);
        // Drop a remembered project filter too. The reset above never runs on
        // a rejected load, so a value saved against another backend — the
        // local dashboard on :8787 shares this origin's localStorage with a
        // server on :8788 — would survive here and scope the page to a project
        // this backend has never seen. The result reads as "no builds recorded
        // for X" next to an empty dropdown, blaming the data for a filter the
        // user cannot see or clear.
        if(project){ setProject(""); localStorage.removeItem("buildlens-project"); }
      }
      if(!background){setError(e.message);setStatus("error");}
    });
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
  // Three states, not two: no builds at all, builds whose backend records no
  // verdict, and builds with verdicts. `some()` on an empty list is false, so
  // without the first case an empty database reads as "this backend does not
  // record status" — blaming the backend for having nothing to report.
  const knowsStatus=builds.some(x=>x.status!=null);
  // Tests that failed and passed inside one build, where the code, machine and
  // environment were all held constant. Stronger evidence of flakiness than
  // mixed outcomes across builds, which a source change also explains.
  const retriedTests=flaky.filter(f=>(f.retried_builds||0)>0);
  // Builds that compiled and are waiting on a test run. Normal for the ~90
  // seconds Xcode takes to write an .xcresult; a build that stays here had
  // tests that never reported, which is why the count is surfaced at all.
  const pendingTests=builds.filter(x=>x.status==="pending_tests").length;
  /// Whether a failure count can be stated at all.
  const canCountFailures=builds.length>0&&knowsStatus;
  const statusNote=!builds.length
    ? (project?`No builds recorded for ${project}`:"No builds recorded yet")
    : knowsStatus
      ? (failures?"Needs attention":"No failures in view")
      : "Not recorded by this backend";
  const view=TABS.find(t=>t.id===tab)!;
  return <div className="shell"><Sidebar tab={tab}/><main>
    <header><div><h1>{view.label}</h1><p className="lede">{view.lede}</p></div><div className="header-actions">{needsToken&&<input className="token-input" type="password" value={token} onChange={e=>{setToken(e.target.value);localStorage.setItem("buildlens-token",e.target.value)}} placeholder="Server token" aria-label="Server token" title="This backend refused a request; a buildlens-server needs its BUILDLENS_TOKEN here."/>}<select value={project} onChange={e=>{setProject(e.target.value);localStorage.setItem("buildlens-project",e.target.value)}}><option value="">All projects</option>{projects.map(p=><option key={p.project} value={p.project}>{p.project} · {p.builds}</option>)}</select><button className="icon-button" onClick={()=>load(true)} title="Refresh now">↻</button><button className={`icon-button live${autoRefresh?" on":""}`} onClick={()=>{const next=!autoRefresh;setAutoRefresh(next);localStorage.setItem("buildlens-autorefresh",next?"on":"off");}} title={autoRefresh?`Updating every ${REFRESH_MS/1000}s — click to pause`:"Auto-refresh paused — click to resume"}><i/>{autoRefresh?"Live":"Paused"}</button></div></header>
    {error&&<div className="error">{error}</div>}
    {tab==="builds"&&<><section className="kpis"><Kpi icon={ICONS.layers} tone="blue" label="Builds recorded" value={String(builds.length)} foot={project||"All projects"}/><Kpi icon={ICONS.clock} tone="" label="Average duration" value={seconds(avg)} foot="Recent saved builds"/><Kpi icon={ICONS.gauge} tone="violet" label="Median / p95" value={percentiles?.enough_history?`${seconds(percentiles.p50)} / ${seconds(percentiles.p95)}`:"—"} foot={percentiles?.enough_history?`Over ${percentiles.builds} builds`:`Needs 5+ builds (${percentiles?.builds||0})`}/><Kpi icon={ICONS.alert} tone={knowsStatus&&failures?"bad":"ok"} label="Failed builds" value={canCountFailures?String(failures):"—"} foot={statusNote} valueTone={knowsStatus&&failures?"bad":undefined}/></section>

    <section className="workspace"><div className="panel trend-panel"><div className="panel-head"><div><h2>Duration trend</h2><p>Recent builds · select a row below for the full breakdown</p></div><button className="panel-action" onClick={()=>openTab("trends")}>Open Performance <Icon d={ICONS.arrow} size={15}/></button></div><div className="chart"><Sparkline items={trend}/></div><div className="chart-foot"><span>Older</span><span className="legend"><i/> build duration · latest {seconds(latest?.total_seconds)}</span><span>Latest</span></div></div>
      <div className="panel attention"><div className="panel-head"><div><h2>Needs attention</h2><p>Signals from the current scope</p></div><button className="panel-action" onClick={()=>openTab("diagnostics")}>View Health <Icon d={ICONS.arrow} size={15}/></button></div>
        <Attention icon={ICONS.alert} count={canCountFailures?failures:"—"} label="failed builds" tone={canCountFailures?(failures?"bad":"ok"):"warn"}
                   note={canCountFailures?(failures?"Open the latest failure":"Everything is green"):"Not recorded by this backend"}/>
        <Attention icon={ICONS.trends} count={regressions.length} label="slower targets" tone={regressions.length?"warn":"ok"}
                   note={regressions.length?`${regressions[0].name} +${seconds(regressions[0].delta_seconds)}`:"No target trending slower"}/>
        <Attention icon={ICONS.health} count={flaky.filter(f=>f.flaky).length} label="flaky tests" tone={flaky.filter(f=>f.flaky).length?"bad":"ok"}
                   note={retriedTests.length?`${retriedTests.length} retried within a single build`:flaky.filter(f=>f.flaky).length?"Passing and failing across builds":"No mixed test outcomes"}/>
        {pendingTests>0&&<Attention icon={ICONS.clock} count={pendingTests} label="awaiting tests" tone="warn"
                   note="Compiled; results arrive when the test run finishes"/>}
      </div></section></>}

    {tab==="trends"&&<section className="insights">
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
      <Section title="Environment mix" note="A duration shift here is a setup change, not a code change">
        <Bars empty="No environment recorded." rows={environment.map(e=>({key:`${e.xcode_version}-${e.platform}-${e.architecture}`,label:`Xcode ${e.xcode_version} · ${e.architecture}`,value:e.builds,display:`${e.builds} builds`,note:`${e.platform} · ${e.machines} machine(s) · avg ${seconds(e.avg_seconds)}`}))}/>
      </Section>
      {/* Only the identified subset: the query excludes builds with no user, so
          this counts who opted in rather than implying it covers the team. The
          empty state says so, because an empty panel otherwise reads as "nobody
          is building" rather than "nobody has opted in". */}
      <Section title="Builds by person" note="Only builds sent with --attribution identified appear here">
        <Bars empty="No identified builds. Send with --attribution identified to break builds down by person." rows={people.map(p=>({key:p.user,label:p.user,value:p.builds,display:`${p.builds} builds`,note:`${p.machines} machine(s) · avg ${seconds(p.avg_seconds)}${p.failed?` · ${p.failed} failed`:""}`}))}/>
      </Section>
      <Section title="Day over day" note="Median and p95 per calendar day">
        <Bars empty="No daily history yet." rows={daily.map(d=>({key:d.day,label:d.day,value:d.p50||0,display:`${seconds(d.p50)} / ${seconds(d.p95)}`,note:`${d.builds} build(s) · median / p95`}))}/>
      </Section>
    </section>}

    {tab==="diagnostics"&&<section className="insights">
      <Section title="Recurring diagnostics" note="Ranked by how many builds they appear in">
        <Bars empty="No diagnostics recorded for these builds." rows={clusters.map(c=>({key:c.fingerprint,label:<span title={c.message||""}>{c.message||c.fingerprint}</span>,value:c.builds,display:`${c.builds} builds`,note:`${c.severity||"unknown"} · ${c.category||"uncategorized"}${c.file?` · ${short(c.file)}`:""}`}))}/>
      </Section>
      <Section title="Diagnostics over time" note="Warnings and errors per build">
        <Bars empty="No diagnostics recorded for these builds." rows={diagTrend.filter(d=>d.warnings+d.errors>0).map(d=>({key:d.id,label:date(d.recorded_at),value:d.warnings+d.errors,display:`${d.warnings} warn · ${d.errors} err`,note:`${d.unique_diagnostics} unique`}))}/>
      </Section>
      <Section title="Recent commits" note="What was checked out when each build ran">
        {git.length?<ul className="rows">{git.map(g=><li key={g.id}><span className="bar-label">{g.branch||"unknown branch"}{g.dirty==="true"&&<em className="dirty"> · uncommitted changes</em>}</span><span className="mono">{seconds(g.total_seconds)}</span><small className="mono">{(g.commit||"").slice(0,10)||"no commit"} · {g.category} · {date(g.recorded_at)}</small></li>)}</ul>:<div className="empty">No git context recorded. Collect with <code>--collect-git</code> to link builds to commits.</div>}
      </Section>
    </section>}

    {tab==="trends"&&regressions.length>0&&<section className="insights">
      {<Section title="Targets trending slower" note="Recent builds vs the window before them — a trend, not an environment-matched baseline">
        <ul className="rows">{regressions.map(r=><li key={r.name}><span className="bar-label">{r.name}</span><span className="mono red">+{seconds(r.delta_seconds)} ({r.delta_percent.toFixed(0)}%)</span><small>{seconds(r.previous_seconds)} → {seconds(r.current_seconds)} · {r.observations} observations</small></li>)}</ul>
      </Section>}
    </section>}

    {tab==="diagnostics"&&flaky.length>0&&<section className="insights">
      {<Section title="Failing and flaky tests" note="Retried means it failed and passed inside one build; mixed across builds is flaky; always-failing is broken">
        <ul className="rows">{flaky.map(f=><li key={`${f.suite}.${f.test}`}><span className="bar-label">{f.suite}.{f.test}</span><span className={(f.retried_builds||0)>0?"mono warnpill":f.flaky?"mono warnpill":"mono red"}>{(f.retried_builds||0)>0?"retried":f.flaky?"flaky":"failing"}</span><small>{f.failed} failed · {f.passed} passed of {f.runs} runs{(f.retried_builds||0)>0?` · retried in ${f.retried_builds} ${f.retried_builds===1?"build":"builds"}`:""}</small></li>)}</ul>
      </Section>}
    </section>}

    {tab==="builds"&&<section className="panel build-panel"><div className="panel-head table-head"><div><h2>Latest builds</h2><p>{visible.length} of {builds.length} builds · select a row to inspect its analysis</p></div><div className="filters"><input value={query} onChange={e=>setQuery(e.target.value)} placeholder="Search builds…"/><label><input type="checkbox" checked={onlyFailures} onChange={e=>setOnlyFailures(e.target.checked)}/> failures only</label></div></div><div className="table-wrap"><table><thead><tr><th>Build</th><th>Project / scheme</th><th>Duration</th><th>Category</th><th>Diagnostics</th><th>Tests</th><th>Status</th><th>Recorded</th></tr></thead><tbody>{visible.slice(0,80).map(b=><tr key={b.id} onClick={()=>openBuild(b.id)}><td><a className="build-id" href={`#/build/${encodeURIComponent(b.id)}`} onClick={e=>e.preventDefault()}>#{b.id}</a></td><td><b>{b.project||"Unknown project"}</b><small>{b.scheme||"No scheme"}</small></td><td className="mono">{seconds(b.total_seconds)}</td><td><span className="category">{b.category||"unknown"}</span></td><td className="mono diagnostics">{b.raw_warnings!=null?<span>{b.raw_warnings} warnings</span>:<span className="muted">—</span>}</td><td className="mono"><TestCell build={b}/></td><td><Status value={b.status}/></td><td className="muted">{date(b.recorded_at)}</td></tr>)}</tbody></table>{!visible.length&&<div className="empty">{builds.length?"No builds match these filters.":project?`No builds recorded for ${project}.`:"No builds recorded yet. Build in Xcode with the collector running, or run buildlens collect."}</div>}</div></section>}

    <footer>{status==="loading"?"Refreshing data…":updatedAt?`Updated ${new Date(updatedAt).toLocaleTimeString()}`:""}<span>BuildLens · local-first build observability</span></footer>
  </main></div>;
}
/// A KPI tile: accent-tinted icon, value, and a context line beneath.
///
/// `tone` colours the icon tile so four tiles in a row read as four distinct
/// metrics rather than one repeated card. `valueTone` is separate because a
/// tile can be red about its number (failed builds) while keeping its own
/// identity colour, and `undefined` must leave the value in the default ink.
function Kpi({label,value,foot,icon,tone="",valueTone=""}:{label:string;value:string;foot:string;icon:string;tone?:string;valueTone?:string}){
  return <div className={`kpi${tone?` t-${tone}`:""}`}><span className="kpi-icon"><Icon d={icon} size={19}/></span><span>{label}</span><strong className={valueTone}>{value}</strong><small title={foot}>{foot}</small></div>;
}

/// One row of the attention panel: a tinted icon chip, the count with what it
/// counts, and a line saying what to do about it.
///
/// The chip carries an icon rather than the number — the number is already in
/// the label, and repeating it made the row read as "2 · 2 failed builds".
function Attention({icon,count,label,note,tone}:{icon:string;count:React.ReactNode;label:string;note:string;tone:string}){
  return <div className={`attention-row ${tone}`}><span className="att-chip"><Icon d={icon} size={17}/></span><span className="att-text"><b>{count} {label}</b><small title={note}>{note}</small></span></div>;
}
createRoot(document.getElementById("root")!).render(<StrictMode><App/></StrictMode>);
