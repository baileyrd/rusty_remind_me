
const { useState, useEffect, useCallback, useRef } = React;

const API = window.location.origin + "/api";

const theme = {
  bg: "#0a0a0f", surface: "#12121a", surfaceHover: "#1a1a26", surfaceActive: "#22222e",
  border: "#2a2a3a", borderFocus: "#6366f1", text: "#e4e4ed", textSecondary: "#8888a4",
  textMuted: "#55556a", accent: "#6366f1", accentHover: "#818cf8",
  accentSubtle: "rgba(99,102,241,0.12)", danger: "#ef4444", dangerSubtle: "rgba(239,68,68,0.12)",
  success: "#22c55e", successSubtle: "rgba(34,197,94,0.12)",
  warning: "#f59e0b", warningSubtle: "rgba(245,158,11,0.12)",
  categoryColors: {
    general: "#6366f1", preference: "#f59e0b", fact: "#22c55e", project: "#06b6d4",
    person: "#ec4899", decision: "#8b5cf6", chat_import: "#64748b", observation: "#14b8a6",
  },
};
const mono = "'IBM Plex Mono', 'JetBrains Mono', monospace";
const sans = "'IBM Plex Sans', -apple-system, sans-serif";

// --- API layer ---
// The API requires a bearer token by default. The key lives in
// ~/.remind-me/api_key on the server machine (or REMIND_ME_API_KEY).
const API_KEY_STORAGE = "remind_me_api_key";
let apiKey = "";
try { apiKey = localStorage.getItem(API_KEY_STORAGE) || ""; } catch (e) { /* storage unavailable */ }

function promptForApiKey() {
  const entered = window.prompt(
    "This dashboard's API requires a key.\n\n" +
    "Find it in ~/.remind-me/api_key on the machine running remind-me-mcp\n" +
    "(or use the value of REMIND_ME_API_KEY):"
  );
  if (entered && entered.trim()) {
    apiKey = entered.trim();
    try { localStorage.setItem(API_KEY_STORAGE, apiKey); } catch (e) { /* storage unavailable */ }
    return true;
  }
  return false;
}

async function api(path, opts = {}) {
  const url = path.startsWith("http") ? path : API + path;
  const doFetch = () => fetch(url, {
    ...opts,
    headers: {
      "Content-Type": "application/json",
      ...(apiKey ? { "Authorization": "Bearer " + apiKey } : {}),
      ...(opts.headers || {}),
    },
    body: opts.body ? JSON.stringify(opts.body) : undefined,
  });
  let res = await doFetch();
  if (res.status === 401 && promptForApiKey()) {
    res = await doFetch();
  }
  return res.json();
}

function useMemoryStore() {
  const [memories, setMemories] = useState([]);
  const [stats, setStats] = useState({ total: 0, categories: {}, sources: {}, tags: {} });
  const [vitality, setVitality] = useState({ vitality_buckets: {}, active_count: 0, dormant_count: 0, vault_health_score: "0%", average_vitality: 0 });
  const [loading, setLoading] = useState(true);

  const refresh = useCallback(async (params = {}) => {
    setLoading(true);
    try {
      const qs = new URLSearchParams();
      if (params.category) qs.set("category", params.category);
      if (params.tags && params.tags.length) qs.set("tags", params.tags.join(","));
      if (params.source) qs.set("source", params.source);
      qs.set("limit", "200");
      const data = await api("/memories?" + qs.toString());
      setMemories(data.memories || []);
    } catch (e) { console.error("refresh:", e); }
    try {
      const s = await api("/stats");
      setStats(s);
    } catch (e) { console.error("stats:", e); }
    try {
      const v = await api("/vitality");
      setVitality(v);
    } catch (e) { console.error("vitality:", e); }
    setLoading(false);
  }, []);

  const search = useCallback(async (query, category, tags) => {
    if (!query.trim()) { refresh({ category, tags }); return; }
    setLoading(true);
    try {
      const qs = new URLSearchParams({ q: query, limit: "200" });
      if (category) qs.set("category", category);
      if (tags && tags.length) qs.set("tags", tags.join(","));
      const data = await api("/memories/search?" + qs.toString());
      setMemories(data.memories || []);
    } catch (e) { console.error("search:", e); }
    setLoading(false);
  }, [refresh]);

  const add = useCallback(async (mem) => {
    await api("/memories", { method: "POST", body: mem });
    refresh();
  }, [refresh]);

  const update = useCallback(async (id, updates) => {
    await api("/memories/" + id, { method: "PUT", body: updates });
    refresh();
  }, [refresh]);

  const remove = useCallback(async (id) => {
    await api("/memories/" + id, { method: "DELETE" });
    refresh();
  }, [refresh]);

  useEffect(() => { refresh(); }, [refresh]);

  return { memories, stats, vitality, loading, refresh, search, add, update, remove };
}

function useWikiStore() {
  const [pages, setPages] = useState([]);
  const [status, setStatus] = useState({ pages: 0, pending_compile: 0 });
  const [current, setCurrent] = useState(null);
  const [loading, setLoading] = useState(true);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const data = await api("/wiki");
      setPages(data.pages || []);
    } catch (e) { console.error("wiki pages:", e); }
    try {
      const s = await api("/wiki/status");
      setStatus(s);
    } catch (e) { console.error("wiki status:", e); }
    setLoading(false);
  }, []);

  const openPage = useCallback(async (slug) => {
    setLoading(true);
    try {
      const data = await api("/wiki/" + encodeURIComponent(slug));
      setCurrent(data.error ? null : data);
    } catch (e) { console.error("wiki page:", e); setCurrent(null); }
    setLoading(false);
  }, []);

  const search = useCallback(async (query) => {
    try {
      const data = await api("/wiki/search?" + new URLSearchParams({ q: query, limit: "20" }));
      return data.results || [];
    } catch (e) { console.error("wiki search:", e); return []; }
  }, []);

  useEffect(() => { refresh(); }, [refresh]);

  return { pages, status, current, setCurrent, loading, refresh, openPage, search };
}

function useEntityStore() {
  const [entities, setEntities] = useState([]);
  const [total, setTotal] = useState(0);
  const [current, setCurrent] = useState(null); // {entity, facts, memories, total_linked_memories}
  const [related, setRelated] = useState([]); // 1-hop traversal entities, self excluded
  const [loading, setLoading] = useState(true);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const data = await api("/entities?limit=100");
      setEntities(data.entities || []);
      setTotal(data.total || 0);
    } catch (e) { console.error("entities:", e); }
    setLoading(false);
  }, []);

  const openEntity = useCallback(async (name) => {
    setLoading(true);
    try {
      const profile = await api("/entity?" + new URLSearchParams({ name }));
      if (profile.error) { setCurrent(null); setRelated([]); setLoading(false); return; }
      setCurrent(profile);
      try {
        const trav = await api("/entity/traverse?" + new URLSearchParams({ name, hops: "1" }));
        setRelated(trav.error ? [] : (trav.entities || []).filter(e => e.id !== profile.entity.id));
      } catch (e) { console.error("entity traverse:", e); setRelated([]); }
    } catch (e) { console.error("entity:", e); setCurrent(null); setRelated([]); }
    setLoading(false);
  }, []);

  useEffect(() => { refresh(); }, [refresh]);

  return { entities, total, current, setCurrent, related, loading, refresh, openEntity };
}

// --- Icons ---
const Icons = {
  Search: () => React.createElement("svg", {width:16,height:16,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor",strokeWidth:2,strokeLinecap:"round"}, React.createElement("circle",{cx:11,cy:11,r:8}), React.createElement("path",{d:"m21 21-4.35-4.35"})),
  Plus: () => React.createElement("svg", {width:16,height:16,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor",strokeWidth:2,strokeLinecap:"round"}, React.createElement("path",{d:"M12 5v14M5 12h14"})),
  Trash: () => React.createElement("svg", {width:14,height:14,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor",strokeWidth:2,strokeLinecap:"round"}, React.createElement("polyline",{points:"3 6 5 6 21 6"}), React.createElement("path",{d:"M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"})),
  Edit: () => React.createElement("svg", {width:14,height:14,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor",strokeWidth:2,strokeLinecap:"round"}, React.createElement("path",{d:"M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"}), React.createElement("path",{d:"M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"})),
  Brain: () => React.createElement("svg", {width:20,height:20,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor",strokeWidth:1.5,strokeLinecap:"round"}, React.createElement("path",{d:"M12 2a7 7 0 0 0-7 7c0 3 2 5.5 4 7l3 3 3-3c2-1.5 4-4 4-7a7 7 0 0 0-7-7z"}), React.createElement("path",{d:"M12 2v10"}), React.createElement("path",{d:"M8 6c1.5 1 3 1.5 4 1.5s2.5-.5 4-1.5"})),
  Chart: () => React.createElement("svg", {width:18,height:18,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor",strokeWidth:2,strokeLinecap:"round"}, React.createElement("path",{d:"M18 20V10M12 20V4M6 20v-6"})),
  Upload: () => React.createElement("svg", {width:16,height:16,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor",strokeWidth:2,strokeLinecap:"round"}, React.createElement("path",{d:"M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"}), React.createElement("polyline",{points:"17 8 12 3 7 8"}), React.createElement("line",{x1:12,y1:3,x2:12,y2:15})),
  X: () => React.createElement("svg", {width:14,height:14,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor",strokeWidth:2,strokeLinecap:"round"}, React.createElement("path",{d:"M18 6 6 18M6 6l12 12"})),
  Copy: () => React.createElement("svg", {width:14,height:14,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor",strokeWidth:2,strokeLinecap:"round"}, React.createElement("rect",{x:9,y:9,width:13,height:13,rx:2}), React.createElement("path",{d:"M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"})),
  Tag: () => React.createElement("svg", {width:12,height:12,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor",strokeWidth:2,strokeLinecap:"round"}, React.createElement("path",{d:"M20.59 13.41l-7.17 7.17a2 2 0 0 1-2.83 0L2 12V2h10l8.59 8.59a2 2 0 0 1 0 2.82z"}), React.createElement("line",{x1:7,y1:7,x2:7.01,y2:7})),
  Check: () => React.createElement("svg", {width:14,height:14,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor",strokeWidth:2.5,strokeLinecap:"round"}, React.createElement("polyline",{points:"20 6 9 17 4 12"})),
  Database: () => React.createElement("svg", {width:16,height:16,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor",strokeWidth:2,strokeLinecap:"round"}, React.createElement("ellipse",{cx:12,cy:5,rx:9,ry:3}), React.createElement("path",{d:"M21 12c0 1.66-4 3-9 3s-9-1.34-9-3"}), React.createElement("path",{d:"M3 5v14c0 1.66 4 3 9 3s9-1.34 9-3V5"})),
  Book: () => React.createElement("svg", {width:16,height:16,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor",strokeWidth:2,strokeLinecap:"round"}, React.createElement("path",{d:"M4 19.5A2.5 2.5 0 0 1 6.5 17H20"}), React.createElement("path",{d:"M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z"})),
  Link: () => React.createElement("svg", {width:12,height:12,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor",strokeWidth:2,strokeLinecap:"round"}, React.createElement("path",{d:"M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"}), React.createElement("path",{d:"M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"})),
  Loader: () => React.createElement("svg", {width:16,height:16,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor",strokeWidth:2,strokeLinecap:"round",style:{animation:"spin 1s linear infinite"}}, React.createElement("path",{d:"M21 12a9 9 0 1 1-6.219-8.56"})),
};

const iconBtn = { background:"none", border:"none", color:theme.textSecondary, cursor:"pointer", padding:4, borderRadius:4, display:"flex", alignItems:"center", transition:"color 0.15s" };
const inputSt = { width:"100%", padding:"10px 12px", borderRadius:6, border:"1px solid "+theme.border, background:theme.bg, color:theme.text, fontSize:14, fontFamily:sans, outline:"none", transition:"border-color 0.15s", boxSizing:"border-box" };
const labelSt = { display:"block", fontSize:12, fontWeight:600, fontFamily:mono, color:theme.textSecondary, marginBottom:6, textTransform:"uppercase", letterSpacing:"0.04em" };

function CategoryBadge({category}) {
  const c = theme.categoryColors[category] || theme.accent;
  return React.createElement("span", {style:{display:"inline-flex",alignItems:"center",gap:4,padding:"2px 8px",borderRadius:4,fontSize:11,fontWeight:600,fontFamily:mono,letterSpacing:"0.04em",textTransform:"uppercase",background:c+"18",color:c,border:"1px solid "+c+"30"}}, category);
}

function TagPill({tag, onClick, removable, onRemove}) {
  return React.createElement("span", {onClick, style:{display:"inline-flex",alignItems:"center",gap:3,padding:"1px 7px",borderRadius:3,fontSize:11,fontFamily:mono,background:theme.surfaceActive,color:theme.textSecondary,border:"1px solid "+theme.border,cursor:onClick?"pointer":"default",transition:"all 0.15s"}},
    React.createElement(Icons.Tag), tag,
    removable && onRemove && React.createElement("span", {onClick:e=>{e.stopPropagation();onRemove()}, style:{cursor:"pointer",marginLeft:2,opacity:0.6}}, "\u00d7")
  );
}

function MemoryCard({memory:m, onEdit, onDelete, onTagClick, expanded, onToggle}) {
  const [copied, setCopied] = useState(false);
  const handleCopy = () => { navigator.clipboard.writeText(m.content); setCopied(true); setTimeout(()=>setCopied(false),1500); };
  const isLong = m.content.length > 200;
  const display = expanded || !isLong ? m.content : m.content.slice(0,200) + "\u2026";
  const meta = Object.entries(m.metadata||{}).filter(([k])=>k!=="import_id");

  return React.createElement("div", {style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:"16px 18px",transition:"all 0.2s"},
    onMouseEnter:e=>{e.currentTarget.style.borderColor=theme.borderFocus+"60";e.currentTarget.style.background=theme.surfaceHover},
    onMouseLeave:e=>{e.currentTarget.style.borderColor=theme.border;e.currentTarget.style.background=theme.surface}},
    // header
    React.createElement("div", {style:{display:"flex",justifyContent:"space-between",alignItems:"flex-start",marginBottom:8}},
      React.createElement("div", {style:{display:"flex",alignItems:"center",gap:8,flexWrap:"wrap"}},
        React.createElement(CategoryBadge, {category:m.category}),
        React.createElement("code", {style:{fontSize:11,color:theme.textMuted,fontFamily:mono}}, m.id),
        m.source==="chat_import" && m.metadata?.filename && React.createElement("span",{style:{fontSize:11,color:theme.textMuted,fontFamily:mono}},"\u2190 "+m.metadata.filename)
      ),
      React.createElement("div", {style:{display:"flex",gap:4}},
        React.createElement("button", {onClick:handleCopy,title:"Copy",style:iconBtn}, copied ? React.createElement(Icons.Check) : React.createElement(Icons.Copy)),
        React.createElement("button", {onClick:()=>onEdit(m),title:"Edit",style:iconBtn}, React.createElement(Icons.Edit)),
        React.createElement("button", {onClick:()=>onDelete(m.id),title:"Delete",style:{...iconBtn,color:theme.danger}}, React.createElement(Icons.Trash))
      )
    ),
    // content
    React.createElement("div", {onClick:isLong?onToggle:undefined,style:{fontFamily:sans,fontSize:14,lineHeight:1.65,color:theme.text,whiteSpace:"pre-wrap",wordBreak:"break-word",cursor:isLong?"pointer":"default"}}, display),
    isLong && React.createElement("button", {onClick:onToggle,style:{background:"none",border:"none",color:theme.accent,fontSize:12,fontFamily:mono,cursor:"pointer",padding:"4px 0",marginTop:4}}, expanded?"Show less":"Show more"),
    // tags
    React.createElement("div", {style:{display:"flex",flexWrap:"wrap",gap:4,marginTop:10}},
      (m.tags||[]).map(t => React.createElement(TagPill, {key:t, tag:t, onClick:()=>onTagClick(t)}))
    ),
    meta.length > 0 && React.createElement("div",{style:{marginTop:6,fontSize:11,color:theme.textMuted,fontFamily:mono}}, meta.map(([k,v])=>k+": "+v).join(" \u00b7 ")),
    React.createElement("div", {style:{marginTop:8,fontSize:11,color:theme.textMuted,fontFamily:mono}},
      new Date(m.created_at).toLocaleDateString("en-US",{month:"short",day:"numeric",year:"numeric"}),
      m.updated_at !== m.created_at ? " \u00b7 edited "+new Date(m.updated_at).toLocaleDateString("en-US",{month:"short",day:"numeric"}) : ""
    )
  );
}

function StatCard({label, value, color, icon}) {
  return React.createElement("div", {style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:"14px 16px",flex:"1 1 140px",minWidth:140}},
    React.createElement("div", {style:{display:"flex",alignItems:"center",gap:6,marginBottom:6}},
      React.createElement("span",{style:{color:color||theme.accent}}, icon),
      React.createElement("span",{style:{fontSize:11,color:theme.textMuted,fontFamily:mono,textTransform:"uppercase",letterSpacing:"0.06em"}}, label)
    ),
    React.createElement("div", {style:{fontSize:28,fontWeight:700,fontFamily:mono,color:theme.text,lineHeight:1}}, value)
  );
}

function BarChart({data, colorMap, preserveOrder}) {
  const max = Math.max(...Object.values(data), 1);
  const entries = preserveOrder ? Object.entries(data) : Object.entries(data).sort((a,b)=>b[1]-a[1]);
  return React.createElement("div", {style:{display:"flex",flexDirection:"column",gap:6}},
    entries.map(([label, count]) =>
      React.createElement("div", {key:label, style:{display:"flex",alignItems:"center",gap:8}},
        React.createElement("span", {style:{width:90,fontSize:12,fontFamily:mono,color:theme.textSecondary,textAlign:"right",flexShrink:0}}, label),
        React.createElement("div", {style:{flex:1,height:20,background:theme.surfaceActive,borderRadius:3,overflow:"hidden"}},
          React.createElement("div", {style:{height:"100%",width:(count/max*100)+"%",background:(colorMap&&colorMap[label])||theme.accent,borderRadius:3,transition:"width 0.4s ease",display:"flex",alignItems:"center",justifyContent:"flex-end",paddingRight:6}},
            React.createElement("span",{style:{fontSize:10,fontWeight:700,fontFamily:mono,color:"#fff"}}, count)
          )
        )
      )
    )
  );
}

// --- Wiki ---
// Lightweight rendering, not a full markdown parser: headings by leading
// '#' count, and [[Wikilink]] / [[Wikilink|alias]] spans made clickable so
// the cross-linking the wiki schema calls "the point" is actually navigable
// from the dashboard. Body text otherwise renders as-is (monospace,
// matching the rest of the app's raw-content aesthetic).
const WIKILINK_RE = /\[\[([^\[\]|]+?)(?:\|([^\[\]]+?))?\]\]/g;

function slugifyTitle(title) {
  return (title || "").trim().toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "") || "untitled";
}

function renderWikiLine(line, i, onNavigate) {
  const heading = line.match(/^(#{1,3})\s+(.*)$/);
  if (heading) {
    const size = heading[1].length === 1 ? 22 : heading[1].length === 2 ? 17 : 14;
    return React.createElement("div", {key:i, style:{fontSize:size,fontWeight:700,fontFamily:sans,letterSpacing:"-0.01em",margin:i===0?"0 0 10px":"18px 0 8px"}}, heading[2]);
  }
  const parts = [];
  let last = 0, m, idx = 0;
  WIKILINK_RE.lastIndex = 0;
  while ((m = WIKILINK_RE.exec(line))) {
    if (m.index > last) parts.push(React.createElement("span",{key:idx++}, line.slice(last, m.index)));
    const target = m[1].trim();
    const label = (m[2] || target).trim();
    parts.push(React.createElement("button",{
      key:idx++, onClick:() => onNavigate(slugifyTitle(target)),
      style:{background:"none",border:"none",padding:0,color:theme.accent,fontFamily:mono,fontWeight:600,cursor:"pointer",textDecoration:"underline",textUnderlineOffset:2,fontSize:"inherit"},
    }, label));
    last = m.index + m[0].length;
  }
  if (last < line.length) parts.push(React.createElement("span",{key:idx++}, line.slice(last)));
  if (parts.length === 0) return React.createElement("div",{key:i,style:{minHeight:8}});
  return React.createElement("div", {key:i, style:{marginBottom:2}}, parts);
}

function WikiPageBody({content, onNavigate}) {
  return React.createElement("div", {style:{fontFamily:mono,fontSize:14,color:theme.text,lineHeight:1.7}},
    content.split("\n").map((line, i) => renderWikiLine(line, i, onNavigate))
  );
}

function WikiLinkList({label, items, onNavigate}) {
  if (!items || !items.length) return null;
  return React.createElement("div", {style:{marginTop:16}},
    React.createElement("div",{style:labelSt}, label),
    React.createElement("div",{style:{display:"flex",flexWrap:"wrap",gap:6}},
      items.map(it => React.createElement("button",{
        key:it.slug, onClick:() => onNavigate(it.slug),
        style:{display:"flex",alignItems:"center",gap:4,padding:"4px 10px",borderRadius:12,border:"1px solid "+theme.border,background:theme.surface,color:theme.textSecondary,fontSize:12,fontFamily:mono,cursor:"pointer"},
      }, React.createElement(Icons.Link), it.title))
    )
  );
}

// --- Entities ---
// Mirrors WikiLinkList's shape (pill buttons that navigate on click), keyed
// by entity id/name instead of slug/title — used for the "Related Entities"
// drill-down built from a 1-hop traversal.
function EntityLinkList({label, items, onNavigate}) {
  if (!items || !items.length) return null;
  return React.createElement("div", {style:{marginTop:16}},
    React.createElement("div",{style:labelSt}, label),
    React.createElement("div",{style:{display:"flex",flexWrap:"wrap",gap:6}},
      items.map(it => React.createElement("button",{
        key:it.id, onClick:() => onNavigate(it.name),
        style:{display:"flex",alignItems:"center",gap:4,padding:"4px 10px",borderRadius:12,border:"1px solid "+theme.border,background:theme.surface,color:theme.textSecondary,fontSize:12,fontFamily:mono,cursor:"pointer"},
      }, React.createElement(Icons.Link), it.name))
    )
  );
}

function Modal({open, onClose, title, children, width}) {
  if (!open) return null;
  return React.createElement("div", {style:{position:"fixed",inset:0,zIndex:1000,display:"flex",alignItems:"center",justifyContent:"center",background:"rgba(0,0,0,0.65)",backdropFilter:"blur(4px)"}, onClick:onClose},
    React.createElement("div", {onClick:e=>e.stopPropagation(), style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:12,padding:24,width:width||480,maxWidth:"92vw",maxHeight:"85vh",overflowY:"auto",boxShadow:"0 24px 80px rgba(0,0,0,0.5)"}},
      React.createElement("div", {style:{display:"flex",justifyContent:"space-between",alignItems:"center",marginBottom:20}},
        React.createElement("h2",{style:{margin:0,fontSize:18,fontFamily:sans,fontWeight:600,color:theme.text}}, title),
        React.createElement("button",{onClick:onClose,style:{...iconBtn,color:theme.textMuted}}, React.createElement(Icons.X))
      ),
      children
    )
  );
}

function MemoryForm({initial, onSubmit, onCancel}) {
  const [content, setContent] = useState(initial?.content||"");
  const [category, setCategory] = useState(initial?.category||"general");
  const [tagInput, setTagInput] = useState("");
  const [tags, setTags] = useState(initial?.tags||[]);
  const categories = ["general","preference","fact","project","person","decision","observation"];

  const handleTagKey = e => {
    if ((e.key==="Enter"||e.key===",") && tagInput.trim()) {
      e.preventDefault();
      const t = tagInput.trim().toLowerCase().replace(/,/g,"");
      if (t && !tags.includes(t)) setTags([...tags, t]);
      setTagInput("");
    }
  };
  const handleSubmit = () => { if (content.trim()) onSubmit({content:content.trim(), category, tags}); };

  return React.createElement("div", {style:{display:"flex",flexDirection:"column",gap:16}},
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Content"),
      React.createElement("textarea",{value:content,onChange:e=>setContent(e.target.value),rows:5,placeholder:"What should I remember?",style:{...inputSt,resize:"vertical",fontFamily:sans,lineHeight:1.6},onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}})
    ),
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Category"),
      React.createElement("div",{style:{display:"flex",flexWrap:"wrap",gap:6}},
        categories.map(c => React.createElement("button",{key:c,onClick:()=>setCategory(c),style:{padding:"6px 12px",borderRadius:6,fontSize:12,fontFamily:mono,border:"1px solid "+(category===c?(theme.categoryColors[c]||theme.accent):theme.border),background:category===c?(theme.categoryColors[c]||theme.accent)+"18":"transparent",color:category===c?(theme.categoryColors[c]||theme.accent):theme.textSecondary,cursor:"pointer",transition:"all 0.15s",textTransform:"uppercase",fontWeight:category===c?600:400}}, c))
      )
    ),
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Tags"),
      tags.length>0 && React.createElement("div",{style:{display:"flex",flexWrap:"wrap",gap:4,marginBottom:8}},
        tags.map(t => React.createElement(TagPill,{key:t,tag:t,removable:true,onRemove:()=>setTags(tags.filter(x=>x!==t))}))
      ),
      React.createElement("input",{value:tagInput,onChange:e=>setTagInput(e.target.value),onKeyDown:handleTagKey,placeholder:"Type a tag and press Enter\u2026",style:inputSt,onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}})
    ),
    React.createElement("div", {style:{display:"flex",gap:8,justifyContent:"flex-end",marginTop:8}},
      React.createElement("button",{onClick:onCancel,style:{padding:"8px 16px",borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:theme.textSecondary,fontSize:13,fontFamily:mono,cursor:"pointer"}},"Cancel"),
      React.createElement("button",{onClick:handleSubmit,disabled:!content.trim(),style:{padding:"8px 20px",borderRadius:6,border:"none",background:content.trim()?theme.accent:theme.surfaceActive,color:content.trim()?"#fff":theme.textMuted,fontSize:13,fontWeight:600,fontFamily:mono,cursor:content.trim()?"pointer":"not-allowed"}}, initial?"Save Changes":"Add Memory")
    )
  );
}

function ImportForm({onComplete, onCancel}) {
  const [filePath, setFilePath] = useState("");
  const [directory, setDirectory] = useState("");
  const [mode, setMode] = useState("file"); // file or directory
  const [extractMode, setExtractMode] = useState("assistant_messages");
  const [category, setCategory] = useState("chat_import");
  const [tagInput, setTagInput] = useState("");
  const [tags, setTags] = useState([]);
  const [importing, setImporting] = useState(false);
  const [result, setResult] = useState(null);
  const [error, setError] = useState(null);

  const extractModes = [
    {value:"assistant_messages", label:"Assistant messages", desc:"Only Claude/AI responses (best for knowledge base)"},
    {value:"user_messages", label:"User messages", desc:"Only your messages"},
    {value:"all_messages", label:"All messages", desc:"Both sides, prefixed with role"},
    {value:"conversations", label:"Full conversations", desc:"Entire conversations as single memories"},
  ];

  const handleTagKey = e => {
    if ((e.key==="Enter"||e.key===",") && tagInput.trim()) {
      e.preventDefault();
      const t = tagInput.trim().toLowerCase().replace(/,/g,"");
      if (t && !tags.includes(t)) setTags([...tags, t]);
      setTagInput("");
    }
  };

  const handleImport = async () => {
    const path = mode === "file" ? filePath.trim() : directory.trim();
    if (!path) return;
    setImporting(true); setError(null); setResult(null);
    try {
      const body = { file_path: path, category, tags, extract_mode: extractMode };
      const data = await api("/import", { method: "POST", body });
      if (data.error) { setError(data.error); }
      else { setResult(data); onComplete(); }
    } catch (e) { setError("Import failed: " + e.message); }
    setImporting(false);
  };

  return React.createElement("div", {style:{display:"flex",flexDirection:"column",gap:16}},
    // Mode toggle
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Import Type"),
      React.createElement("div",{style:{display:"flex",gap:6}},
        [["file","Single File"],["directory","Directory"]].map(([v,l])=>
          React.createElement("button",{key:v,onClick:()=>setMode(v),style:{padding:"6px 14px",borderRadius:6,fontSize:12,fontFamily:mono,border:"1px solid "+(mode===v?theme.accent:theme.border),background:mode===v?theme.accentSubtle:"transparent",color:mode===v?theme.accent:theme.textSecondary,cursor:"pointer",fontWeight:mode===v?600:400}},l)
        )
      )
    ),
    // Path input
    React.createElement("div", null,
      React.createElement("label",{style:labelSt}, mode==="file" ? "File Path" : "Directory Path"),
      React.createElement("input",{
        value: mode==="file" ? filePath : directory,
        onChange: e => mode==="file" ? setFilePath(e.target.value) : setDirectory(e.target.value),
        placeholder: mode==="file"
          ? "~/Downloads/claude-export/conversations.json"
          : "~/Downloads/claude-export/",
        style: inputSt,
        onFocus:e=>{e.target.style.borderColor=theme.borderFocus},
        onBlur:e=>{e.target.style.borderColor=theme.border},
      }),
      React.createElement("div",{style:{fontSize:11,color:theme.textMuted,fontFamily:mono,marginTop:4}},
        mode==="file"
          ? "Supports .json, .jsonl, .md, .txt files"
          : "Will scan for all supported files" + " (recursively)"
      )
    ),
    // Extract mode
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Extract Mode"),
      React.createElement("div",{style:{display:"flex",flexDirection:"column",gap:4}},
        extractModes.map(em =>
          React.createElement("button",{key:em.value,onClick:()=>setExtractMode(em.value),style:{display:"flex",flexDirection:"column",alignItems:"flex-start",padding:"8px 12px",borderRadius:6,border:"1px solid "+(extractMode===em.value?theme.accent:theme.border),background:extractMode===em.value?theme.accentSubtle:"transparent",cursor:"pointer",transition:"all 0.15s"}},
            React.createElement("span",{style:{fontSize:13,fontFamily:mono,fontWeight:extractMode===em.value?600:400,color:extractMode===em.value?theme.accent:theme.text}}, em.label),
            React.createElement("span",{style:{fontSize:11,color:theme.textMuted,fontFamily:sans,marginTop:2}}, em.desc)
          )
        )
      )
    ),
    // Category
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Category"),
      React.createElement("input",{value:category,onChange:e=>setCategory(e.target.value),style:inputSt,
        onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}})
    ),
    // Tags
    React.createElement("div", null,
      React.createElement("label",{style:labelSt},"Tags (applied to all imported memories)"),
      tags.length > 0 && React.createElement("div",{style:{display:"flex",flexWrap:"wrap",gap:4,marginBottom:8}},
        tags.map(t => React.createElement(TagPill,{key:t,tag:t,removable:true,onRemove:()=>setTags(tags.filter(x=>x!==t))}))
      ),
      React.createElement("input",{value:tagInput,onChange:e=>setTagInput(e.target.value),onKeyDown:handleTagKey,placeholder:"Type a tag and press Enter\u2026",style:inputSt,
        onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}})
    ),
    // Error
    error && React.createElement("div",{style:{padding:"10px 14px",borderRadius:6,background:theme.dangerSubtle,border:"1px solid "+theme.danger+"40",color:theme.danger,fontSize:13,fontFamily:mono}}, error),
    // Result
    result && React.createElement("div",{style:{padding:"10px 14px",borderRadius:6,background:theme.successSubtle,border:"1px solid "+theme.success+"40",color:theme.success,fontSize:13,fontFamily:mono}},
      result.status === "ok"
        ? "\u2713 Imported "+result.memories_created+" memories from "+result.file
        : result.status === "skipped"
          ? "Skipped: "+result.reason + (result.file ? " ("+result.file+")" : "")
          : result.files_processed
            ? "\u2713 Processed "+result.files_processed+" files: "+result.total_memories_created+" memories created, "+result.skipped+" skipped"
            : JSON.stringify(result)
    ),
    // Actions
    React.createElement("div", {style:{display:"flex",gap:8,justifyContent:"flex-end",marginTop:8}},
      React.createElement("button",{onClick:onCancel,style:{padding:"8px 16px",borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:theme.textSecondary,fontSize:13,fontFamily:mono,cursor:"pointer"}},"Close"),
      React.createElement("button",{onClick:handleImport,disabled:importing||!(mode==="file"?filePath.trim():directory.trim()),style:{padding:"8px 20px",borderRadius:6,border:"none",background:importing||!(mode==="file"?filePath.trim():directory.trim())?theme.surfaceActive:theme.accent,color:importing||!(mode==="file"?filePath.trim():directory.trim())?theme.textMuted:"#fff",fontSize:13,fontWeight:600,fontFamily:mono,cursor:importing?"wait":"pointer",display:"flex",alignItems:"center",gap:6}},
        importing && React.createElement(Icons.Loader),
        importing ? "Importing\u2026" : "Import"
      )
    )
  );
}

function App() {
  const store = useMemoryStore();
  const wikiStore = useWikiStore();
  const entityStore = useEntityStore();
  const [view, setView] = useState("browse");
  const [searchQuery, setSearchQuery] = useState("");
  const [filterCategory, setFilterCategory] = useState("");
  const [filterTags, setFilterTags] = useState([]);
  const [showAddModal, setShowAddModal] = useState(false);
  const [showImportModal, setShowImportModal] = useState(false);
  const [editMemory, setEditMemory] = useState(null);
  const [deleteConfirm, setDeleteConfirm] = useState(null);
  const [expandedIds, setExpandedIds] = useState(new Set());
  const [wikiQuery, setWikiQuery] = useState("");
  const [wikiSearchResults, setWikiSearchResults] = useState(null);
  const [entityQuery, setEntityQuery] = useState("");
  const searchRef = useRef(null);
  const debounceRef = useRef(null);
  const wikiDebounceRef = useRef(null);

  useEffect(() => {
    const h = e => { if ((e.metaKey||e.ctrlKey)&&e.key==="k") { e.preventDefault(); searchRef.current?.focus(); } };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, []);

  // Debounced search
  useEffect(() => {
    clearTimeout(debounceRef.current);
    debounceRef.current = setTimeout(() => {
      if (searchQuery.trim()) {
        store.search(searchQuery, filterCategory||null, filterTags.length?filterTags:null);
      } else {
        store.refresh({ category: filterCategory||undefined, tags: filterTags.length?filterTags:undefined });
      }
    }, 250);
  }, [searchQuery, filterCategory, filterTags]);

  // Debounced wiki search — a filter over the page list, mirroring the
  // Browse search box; null means "show every page" (no query typed).
  useEffect(() => {
    clearTimeout(wikiDebounceRef.current);
    wikiDebounceRef.current = setTimeout(async () => {
      if (wikiQuery.trim()) {
        setWikiSearchResults(await wikiStore.search(wikiQuery));
      } else {
        setWikiSearchResults(null);
      }
    }, 250);
  }, [wikiQuery]);

  const handleAdd = async data => { await store.add(data); setShowAddModal(false); };
  const handleEdit = async data => { if (editMemory) { await store.update(editMemory.id, data); setEditMemory(null); } };
  const handleDelete = async id => { await store.remove(id); setDeleteConfirm(null); };
  const toggleExpand = id => setExpandedIds(prev => { const n=new Set(prev); n.has(id)?n.delete(id):n.add(id); return n; });
  const handleTagClick = tag => { if (!filterTags.includes(tag)) setFilterTags([...filterTags, tag]); };
  const handleWikiNavigate = slug => { wikiStore.openPage(slug); setWikiQuery(""); setWikiSearchResults(null); };
  const handleEntityNavigate = name => { entityStore.openEntity(name); setEntityQuery(""); };

  const stats = store.stats;
  const vitality = store.vitality;
  const allCategories = Object.keys(stats.categories||{});
  const allTags = Object.keys(stats.tags||{}).sort((a,b)=>(stats.tags[b]||0)-(stats.tags[a]||0));
  const entityQueryLower = entityQuery.trim().toLowerCase();
  const filteredEntities = entityQueryLower
    ? entityStore.entities.filter(e =>
        e.name.toLowerCase().includes(entityQueryLower) ||
        (e.aliases||[]).some(a => a.toLowerCase().includes(entityQueryLower)))
    : entityStore.entities;

  return React.createElement("div", {style:{minHeight:"100vh",background:theme.bg,color:theme.text,fontFamily:sans}},
    React.createElement("style",null,"@keyframes spin{to{transform:rotate(360deg)}}"),
    // Header
    React.createElement("header", {style:{borderBottom:"1px solid "+theme.border,padding:"16px 24px",display:"flex",alignItems:"center",justifyContent:"space-between",position:"sticky",top:0,zIndex:100,background:theme.bg+"e6",backdropFilter:"blur(12px)"}},
      React.createElement("div",{style:{display:"flex",alignItems:"center",gap:10}},
        React.createElement("div",{style:{width:36,height:36,borderRadius:8,background:"linear-gradient(135deg,"+theme.accent+",#a855f7)",display:"flex",alignItems:"center",justifyContent:"center"}}, React.createElement(Icons.Brain)),
        React.createElement("div",null,
          React.createElement("h1",{style:{margin:0,fontSize:18,fontWeight:700,fontFamily:sans,letterSpacing:"-0.02em"}},"Remind Me"),
          React.createElement("span",{style:{fontSize:11,color:theme.textMuted,fontFamily:mono}}, (stats.total||0)+" memories \u00b7 "+((stats.db_path||"").replace(/.*\//,"~/"))),
        )
      ),
      React.createElement("div",{style:{display:"flex",alignItems:"center",gap:8}},
        store.loading && React.createElement("span",{style:{color:theme.textMuted}}, React.createElement(Icons.Loader)),
        React.createElement("div",{style:{display:"flex",background:theme.surface,borderRadius:6,border:"1px solid "+theme.border,overflow:"hidden"}},
          [["browse","Browse"],["stats","Stats"],["wiki","Wiki"],["entities","Entities"]].map(([v,l])=>React.createElement("button",{key:v,onClick:()=>setView(v),style:{padding:"6px 14px",border:"none",fontSize:12,fontFamily:mono,fontWeight:500,cursor:"pointer",background:view===v?theme.accent:"transparent",color:view===v?"#fff":theme.textSecondary,transition:"all 0.15s"}},
            l, v==="wiki" && wikiStore.status.pending_compile>0 && React.createElement("span",{style:{marginLeft:6,padding:"1px 6px",borderRadius:8,background:view===v?"rgba(255,255,255,0.25)":theme.warningSubtle,color:view===v?"#fff":theme.warning,fontSize:10,fontWeight:700}}, wikiStore.status.pending_compile)
          ))
        ),
        React.createElement("button",{onClick:()=>setShowImportModal(true),style:{display:"flex",alignItems:"center",gap:6,padding:"8px 14px",borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:theme.textSecondary,fontSize:13,fontWeight:500,fontFamily:mono,cursor:"pointer",transition:"all 0.15s"},onMouseEnter:e=>{e.currentTarget.style.borderColor=theme.accent;e.currentTarget.style.color=theme.text},onMouseLeave:e=>{e.currentTarget.style.borderColor=theme.border;e.currentTarget.style.color=theme.textSecondary}}, React.createElement(Icons.Upload), " Import"),
        React.createElement("button",{onClick:()=>setShowAddModal(true),style:{display:"flex",alignItems:"center",gap:6,padding:"8px 14px",borderRadius:6,border:"none",background:theme.accent,color:"#fff",fontSize:13,fontWeight:600,fontFamily:mono,cursor:"pointer"}}, React.createElement(Icons.Plus), " Add")
      )
    ),
    // Body
    React.createElement("div",{style:{display:"flex",maxWidth:1200,margin:"0 auto"}},
      // Sidebar
      view==="browse" && React.createElement("aside",{style:{width:220,borderRight:"1px solid "+theme.border,padding:"20px 16px",flexShrink:0,position:"sticky",top:69,height:"calc(100vh - 69px)",overflowY:"auto"}},
        React.createElement("div",{style:{marginBottom:20}},
          React.createElement("div",{style:{...labelSt,marginBottom:10}},"Categories"),
          React.createElement("button",{onClick:()=>setFilterCategory(""),style:{display:"block",width:"100%",textAlign:"left",padding:"6px 10px",borderRadius:5,border:"none",background:!filterCategory?theme.accentSubtle:"transparent",color:!filterCategory?theme.accent:theme.textSecondary,fontSize:13,fontFamily:sans,cursor:"pointer",fontWeight:!filterCategory?600:400,marginBottom:2}}, "All ("+(stats.total||0)+")"),
          allCategories.map(cat=>React.createElement("button",{key:cat,onClick:()=>setFilterCategory(filterCategory===cat?"":cat),style:{display:"flex",alignItems:"center",justifyContent:"space-between",width:"100%",textAlign:"left",padding:"6px 10px",borderRadius:5,border:"none",background:filterCategory===cat?(theme.categoryColors[cat]||theme.accent)+"18":"transparent",color:filterCategory===cat?(theme.categoryColors[cat]||theme.accent):theme.textSecondary,fontSize:13,fontFamily:sans,cursor:"pointer",fontWeight:filterCategory===cat?600:400,marginBottom:2}},
            React.createElement("span",null,cat),
            React.createElement("span",{style:{fontSize:11,fontFamily:mono,opacity:0.7}}, stats.categories[cat])
          ))
        ),
        React.createElement("div",null,
          React.createElement("div",{style:{...labelSt,marginBottom:10}},"Popular Tags"),
          React.createElement("div",{style:{display:"flex",flexWrap:"wrap",gap:4}},
            allTags.slice(0,15).map(t=>React.createElement(TagPill,{key:t,tag:t,onClick:()=>handleTagClick(t)}))
          )
        )
      ),
      view==="wiki" && React.createElement("aside",{style:{width:240,borderRight:"1px solid "+theme.border,padding:"20px 16px",flexShrink:0,position:"sticky",top:69,height:"calc(100vh - 69px)",overflowY:"auto"}},
        React.createElement("div",{style:{position:"relative",marginBottom:16}},
          React.createElement("div",{style:{position:"absolute",left:10,top:"50%",transform:"translateY(-50%)",color:theme.textMuted}}, React.createElement(Icons.Search)),
          React.createElement("input",{value:wikiQuery,onChange:e=>setWikiQuery(e.target.value),placeholder:"Search wiki…",style:{...inputSt,paddingLeft:32,fontSize:13,padding:"7px 10px 7px 32px"}})
        ),
        wikiStore.status.pending_compile>0 && React.createElement("div",{style:{padding:"6px 10px",borderRadius:5,background:theme.warningSubtle,color:theme.warning,fontSize:11,fontFamily:mono,marginBottom:14}},
          wikiStore.status.pending_compile+" raw "+(wikiStore.status.pending_compile===1?"memory":"memories")+" not yet compiled"
        ),
        React.createElement("div",{style:{...labelSt,marginBottom:10}}, (wikiSearchResults?wikiSearchResults.length:wikiStore.pages.length)+" page(s)"),
        React.createElement("div",{style:{display:"flex",flexDirection:"column",gap:2}},
          (wikiSearchResults || wikiStore.pages).map(p=>React.createElement("button",{
            key:p.slug, onClick:()=>handleWikiNavigate(p.slug),
            style:{display:"block",width:"100%",textAlign:"left",padding:"7px 10px",borderRadius:5,border:"none",background:wikiStore.current&&wikiStore.current.slug===p.slug?theme.accentSubtle:"transparent",color:wikiStore.current&&wikiStore.current.slug===p.slug?theme.accent:theme.textSecondary,fontSize:13,fontFamily:sans,cursor:"pointer",fontWeight:wikiStore.current&&wikiStore.current.slug===p.slug?600:400},
          },
            React.createElement("div",null, p.title),
            p.summary && React.createElement("div",{style:{fontSize:11,color:theme.textMuted,fontFamily:sans,marginTop:1,overflow:"hidden",textOverflow:"ellipsis",whiteSpace:"nowrap"}}, p.snippet ? React.createElement("span",{dangerouslySetInnerHTML:{__html:p.snippet.replace(/\[/g,"<b>").replace(/\]/g,"</b>")}}) : p.summary)
          )),
          !wikiStore.loading && (wikiSearchResults||wikiStore.pages).length===0 && React.createElement("div",{style:{fontSize:12,color:theme.textMuted,fontFamily:sans,padding:"8px 10px"}}, wikiQuery ? "No matches." : "The wiki is empty. Ask Claude to run remind_me_wiki_compile.")
        )
      ),
      view==="entities" && React.createElement("aside",{style:{width:240,borderRight:"1px solid "+theme.border,padding:"20px 16px",flexShrink:0,position:"sticky",top:69,height:"calc(100vh - 69px)",overflowY:"auto"}},
        React.createElement("div",{style:{position:"relative",marginBottom:16}},
          React.createElement("div",{style:{position:"absolute",left:10,top:"50%",transform:"translateY(-50%)",color:theme.textMuted}}, React.createElement(Icons.Search)),
          React.createElement("input",{value:entityQuery,onChange:e=>setEntityQuery(e.target.value),placeholder:"Filter entities…",style:{...inputSt,paddingLeft:32,fontSize:13,padding:"7px 10px 7px 32px"}})
        ),
        React.createElement("div",{style:{...labelSt,marginBottom:10}}, filteredEntities.length+" of "+entityStore.total+" entit"+(entityStore.total===1?"y":"ies")),
        React.createElement("div",{style:{display:"flex",flexDirection:"column",gap:2}},
          filteredEntities.map(e=>React.createElement("button",{
            key:e.id, onClick:()=>handleEntityNavigate(e.name),
            style:{display:"flex",alignItems:"center",justifyContent:"space-between",width:"100%",textAlign:"left",padding:"7px 10px",borderRadius:5,border:"none",background:entityStore.current&&entityStore.current.entity.id===e.id?theme.accentSubtle:"transparent",color:entityStore.current&&entityStore.current.entity.id===e.id?theme.accent:theme.textSecondary,fontSize:13,fontFamily:sans,cursor:"pointer",fontWeight:entityStore.current&&entityStore.current.entity.id===e.id?600:400},
          },
            React.createElement("span",null, e.name),
            React.createElement("span",{style:{fontSize:11,fontFamily:mono,opacity:0.7,flexShrink:0,marginLeft:8}}, e.mention_count)
          )),
          !entityStore.loading && filteredEntities.length===0 && React.createElement("div",{style:{fontSize:12,color:theme.textMuted,fontFamily:sans,padding:"8px 10px"}},
            entityQuery ? "No matches." : "No entities yet. They're created automatically when Claude extracts facts via remind_me_decompose or remind_me_annotate."
          )
        )
      ),
      // Main
      React.createElement("main",{style:{flex:1,padding:"20px 24px",minWidth:0}},
        view==="browse" ? React.createElement(React.Fragment,null,
          // Search
          React.createElement("div",{style:{position:"relative",marginBottom:16}},
            React.createElement("div",{style:{position:"absolute",left:12,top:"50%",transform:"translateY(-50%)",color:theme.textMuted}}, React.createElement(Icons.Search)),
            React.createElement("input",{ref:searchRef,value:searchQuery,onChange:e=>setSearchQuery(e.target.value),placeholder:"Search memories\u2026 (\u2318K)",style:{...inputSt,paddingLeft:36,background:theme.surface,fontSize:15},onFocus:e=>{e.target.style.borderColor=theme.borderFocus},onBlur:e=>{e.target.style.borderColor=theme.border}})
          ),
          // Active tag filters
          filterTags.length>0 && React.createElement("div",{style:{display:"flex",alignItems:"center",gap:6,marginBottom:12,flexWrap:"wrap"}},
            React.createElement("span",{style:{fontSize:12,color:theme.textMuted,fontFamily:mono}},"Filtered by:"),
            filterTags.map(t=>React.createElement(TagPill,{key:t,tag:t,removable:true,onRemove:()=>setFilterTags(filterTags.filter(x=>x!==t))})),
            React.createElement("button",{onClick:()=>setFilterTags([]),style:{background:"none",border:"none",color:theme.accent,fontSize:12,fontFamily:mono,cursor:"pointer"}},"Clear all")
          ),
          React.createElement("div",{style:{fontSize:12,color:theme.textMuted,fontFamily:mono,marginBottom:12}}, store.memories.length+" "+(store.memories.length===1?"memory":"memories")+(searchQuery||filterCategory||filterTags.length?" matching filters":"")),
          // Cards
          React.createElement("div",{style:{display:"flex",flexDirection:"column",gap:10}},
            store.memories.map(m=>React.createElement(MemoryCard,{key:m.id,memory:m,onEdit:setEditMemory,onDelete:setDeleteConfirm,onTagClick:handleTagClick,expanded:expandedIds.has(m.id),onToggle:()=>toggleExpand(m.id)})),
            store.memories.length===0 && !store.loading && React.createElement("div",{style:{textAlign:"center",padding:"60px 20px",color:theme.textMuted}},
              React.createElement("div",{style:{fontSize:40,marginBottom:12}},"\u2205"),
              React.createElement("div",{style:{fontSize:15,marginBottom:6}},"No memories found"),
              React.createElement("div",{style:{fontSize:13}},"Try adjusting your search or filters")
            )
          )
        ) :
        view==="wiki" ?
        // Wiki view
        (wikiStore.current ? React.createElement("div",null,
          React.createElement("div",{style:{display:"flex",justifyContent:"space-between",alignItems:"flex-start",marginBottom:12}},
            React.createElement("span",{style:{fontSize:11,color:theme.textMuted,fontFamily:mono}}, "Updated "+(wikiStore.current.updated_at||"").replace("T"," ").slice(0,16)),
            React.createElement("button",{onClick:()=>wikiStore.setCurrent(null),style:{background:"none",border:"none",color:theme.textSecondary,fontSize:12,fontFamily:mono,cursor:"pointer",display:"flex",alignItems:"center",gap:4}}, React.createElement(Icons.X), " Close")
          ),
          React.createElement("div",{style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:"20px 24px"}},
            React.createElement(WikiPageBody,{content:wikiStore.current.content, onNavigate:handleWikiNavigate}),
            React.createElement(WikiLinkList,{label:"Links",items:wikiStore.current.links,onNavigate:handleWikiNavigate}),
            React.createElement(WikiLinkList,{label:"Backlinks",items:wikiStore.current.backlinks,onNavigate:handleWikiNavigate})
          )
        ) : React.createElement("div",{style:{textAlign:"center",padding:"80px 20px",color:theme.textMuted}},
          React.createElement("div",{style:{color:theme.textMuted,marginBottom:12,display:"flex",justifyContent:"center"}}, React.createElement(Icons.Book)),
          React.createElement("div",{style:{fontSize:15,marginBottom:6}}, wikiStore.pages.length===0 ? "The wiki is empty" : "Select a page"),
          React.createElement("div",{style:{fontSize:13}}, wikiStore.pages.length===0 ? "Ask Claude to run remind_me_wiki_compile to synthesise one from your memories." : "Pick a page from the list on the left.")
        )) :
        view==="entities" ?
        // Entity detail view
        (entityStore.current ? React.createElement("div",null,
          React.createElement("div",{style:{display:"flex",justifyContent:"space-between",alignItems:"flex-start",marginBottom:12}},
            React.createElement("div",null,
              React.createElement("h2",{style:{margin:0,fontFamily:sans,fontWeight:700,fontSize:20,letterSpacing:"-0.02em"}}, entityStore.current.entity.name),
              entityStore.current.entity.kind && React.createElement("span",{style:{fontSize:11,color:theme.textMuted,fontFamily:mono}}, entityStore.current.entity.kind)
            ),
            React.createElement("button",{onClick:()=>entityStore.setCurrent(null),style:{background:"none",border:"none",color:theme.textSecondary,fontSize:12,fontFamily:mono,cursor:"pointer",display:"flex",alignItems:"center",gap:4}}, React.createElement(Icons.X), " Close")
          ),
          (entityStore.current.entity.aliases||[]).length>0 && React.createElement("div",{style:{marginBottom:12,display:"flex",flexWrap:"wrap",gap:4}},
            entityStore.current.entity.aliases.map(a=>React.createElement(TagPill,{key:a,tag:a}))
          ),
          React.createElement("div",{style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:"20px 24px"}},
            React.createElement("div",{style:labelSt}, "Facts ("+entityStore.current.facts.length+")"),
            entityStore.current.facts.length===0 ?
              React.createElement("div",{style:{fontSize:13,color:theme.textMuted,fontFamily:sans,marginBottom:16}},"No structured facts yet.") :
              React.createElement("div",{style:{display:"flex",flexDirection:"column",gap:8,marginBottom:16}},
                entityStore.current.facts.map(f=>React.createElement("div",{key:f.id,style:{fontFamily:mono,fontSize:13,color:theme.text,padding:"8px 10px",background:theme.surfaceActive,borderRadius:5}},
                  f.subject && f.predicate && f.object ?
                    React.createElement("span",null, React.createElement("b",null,f.subject)," ",f.predicate," ",React.createElement("b",null,f.object)) :
                    f.content
                ))
              ),
            React.createElement("div",{style:labelSt}, "Linked Memories ("+entityStore.current.total_linked_memories+")"),
            entityStore.current.memories.length===0 ?
              React.createElement("div",{style:{fontSize:13,color:theme.textMuted,fontFamily:sans}},"No linked memories.") :
              React.createElement("div",{style:{display:"flex",flexDirection:"column",gap:8}},
                entityStore.current.memories.map(m=>React.createElement("div",{key:m.id,style:{fontSize:13,fontFamily:sans,color:theme.text,padding:"8px 10px",background:theme.surfaceActive,borderRadius:5,lineHeight:1.5}},
                  React.createElement(CategoryBadge,{category:m.category}),
                  React.createElement("div",{style:{marginTop:4}}, m.content_snippet)
                ))
              ),
            React.createElement(EntityLinkList,{label:"Related Entities",items:entityStore.related,onNavigate:handleEntityNavigate})
          )
        ) : React.createElement("div",{style:{textAlign:"center",padding:"80px 20px",color:theme.textMuted}},
          React.createElement("div",{style:{color:theme.textMuted,marginBottom:12,display:"flex",justifyContent:"center"}}, React.createElement(Icons.Brain)),
          React.createElement("div",{style:{fontSize:15,marginBottom:6}}, entityStore.entities.length===0 ? "No entities yet" : "Select an entity"),
          React.createElement("div",{style:{fontSize:13}}, entityStore.entities.length===0 ? "Entities are created automatically when Claude extracts facts via remind_me_decompose or remind_me_annotate." : "Pick an entity from the list on the left.")
        )) :
        // Stats view
        React.createElement("div",null,
          React.createElement("h2",{style:{fontFamily:sans,fontWeight:700,fontSize:22,marginBottom:20,letterSpacing:"-0.02em"}},"Memory Statistics"),
          React.createElement("div",{style:{display:"flex",gap:12,marginBottom:24,flexWrap:"wrap"}},
            React.createElement(StatCard,{label:"Total Memories",value:stats.total||0,color:theme.accent,icon:React.createElement(Icons.Database)}),
            React.createElement(StatCard,{label:"Categories",value:Object.keys(stats.categories||{}).length,color:"#22c55e",icon:React.createElement(Icons.Chart)}),
            React.createElement(StatCard,{label:"Unique Tags",value:Object.keys(stats.tags||{}).length,color:"#f59e0b",icon:React.createElement(Icons.Tag)}),
            React.createElement(StatCard,{label:"Sources",value:Object.keys(stats.sources||{}).length,color:"#06b6d4",icon:React.createElement(Icons.Upload)})
          ),
          React.createElement("div",{style:{display:"grid",gridTemplateColumns:"1fr 1fr",gap:16}},
            React.createElement("div",{style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:20}},
              React.createElement("h3",{style:{fontFamily:mono,fontSize:13,fontWeight:600,color:theme.textSecondary,marginBottom:16,textTransform:"uppercase",letterSpacing:"0.04em"}},"By Category"),
              React.createElement(BarChart,{data:stats.categories||{},colorMap:theme.categoryColors})
            ),
            React.createElement("div",{style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:20}},
              React.createElement("h3",{style:{fontFamily:mono,fontSize:13,fontWeight:600,color:theme.textSecondary,marginBottom:16,textTransform:"uppercase",letterSpacing:"0.04em"}},"By Source"),
              React.createElement(BarChart,{data:stats.sources||{},colorMap:{manual:theme.accent,chat_import:"#64748b"}})
            )
          ),
          React.createElement("div",{style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:20,marginTop:16}},
            React.createElement("div",{style:{display:"flex",justifyContent:"space-between",alignItems:"baseline",marginBottom:16}},
              React.createElement("h3",{style:{fontFamily:mono,fontSize:13,fontWeight:600,color:theme.textSecondary,textTransform:"uppercase",letterSpacing:"0.04em"}},"Vitality Distribution"),
              React.createElement("span",{style:{fontFamily:mono,fontSize:12,color:theme.textMuted}}, "Vault health "+(vitality.vault_health_score||"0%")+" · "+(vitality.active_count||0)+" active · "+(vitality.dormant_count||0)+" dormant")
            ),
            React.createElement(BarChart,{data:vitality.vitality_buckets||{},preserveOrder:true,colorMap:{"0.00-0.05":theme.danger,"0.05-0.25":"#f59e0b","0.25-0.50":"#eab308","0.50-0.75":"#84cc16","0.75+":"#22c55e"}})
          ),
          React.createElement("div",{style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:20,marginTop:16}},
            React.createElement("h3",{style:{fontFamily:mono,fontSize:13,fontWeight:600,color:theme.textSecondary,marginBottom:16,textTransform:"uppercase",letterSpacing:"0.04em"}},"Top Tags"),
            React.createElement(BarChart,{data:Object.fromEntries(Object.entries(stats.tags||{}).sort((a,b)=>b[1]-a[1]).slice(0,10))})
          ),
          React.createElement("div",{style:{background:theme.surface,border:"1px solid "+theme.border,borderRadius:8,padding:20,marginTop:16}},
            React.createElement("h3",{style:{fontFamily:mono,fontSize:13,fontWeight:600,color:theme.textSecondary,marginBottom:12,textTransform:"uppercase",letterSpacing:"0.04em"}},"Server Info"),
            React.createElement("div",{style:{fontFamily:mono,fontSize:13,color:theme.textSecondary,lineHeight:2}},
              React.createElement("div",null, React.createElement("span",{style:{color:theme.textMuted}},"Database: "), React.createElement("code",{style:{color:theme.text}}, stats.db_path||"~/.remind-me/memory.db")),
              React.createElement("div",null, React.createElement("span",{style:{color:theme.textMuted}},"Size: "), React.createElement("code",{style:{color:theme.text}}, (stats.db_size_mb||0)+" MB")),
              React.createElement("div",null, React.createElement("span",{style:{color:theme.textMuted}},"Search engine: "), React.createElement("code",{style:{color:theme.text}}, "SQLite FTS5")),
              React.createElement("div",null, React.createElement("span",{style:{color:theme.textMuted}},"API: "), React.createElement("code",{style:{color:theme.text}}, window.location.origin))
            )
          )
        )
      )
    ),
    // Modals
    React.createElement(Modal,{open:showAddModal,onClose:()=>setShowAddModal(false),title:"Add Memory",width:520},
      React.createElement(MemoryForm,{onSubmit:handleAdd,onCancel:()=>setShowAddModal(false)})
    ),
    React.createElement(Modal,{open:showImportModal,onClose:()=>setShowImportModal(false),title:"Import Chat History",width:560},
      React.createElement(ImportForm,{onComplete:()=>store.refresh(),onCancel:()=>setShowImportModal(false)})
    ),
    React.createElement(Modal,{open:!!editMemory,onClose:()=>setEditMemory(null),title:"Edit Memory",width:520},
      editMemory && React.createElement(MemoryForm,{initial:editMemory,onSubmit:handleEdit,onCancel:()=>setEditMemory(null)})
    ),
    React.createElement(Modal,{open:!!deleteConfirm,onClose:()=>setDeleteConfirm(null),title:"Delete Memory",width:400},
      React.createElement("p",{style:{color:theme.textSecondary,fontFamily:sans,fontSize:14,lineHeight:1.6}}, "Are you sure you want to permanently delete memory ", React.createElement("code",{style:{fontFamily:mono,color:theme.text}},deleteConfirm), "? This cannot be undone."),
      React.createElement("div",{style:{display:"flex",gap:8,justifyContent:"flex-end",marginTop:20}},
        React.createElement("button",{onClick:()=>setDeleteConfirm(null),style:{padding:"8px 16px",borderRadius:6,border:"1px solid "+theme.border,background:"transparent",color:theme.textSecondary,fontSize:13,fontFamily:mono,cursor:"pointer"}},"Cancel"),
        React.createElement("button",{onClick:()=>handleDelete(deleteConfirm),style:{padding:"8px 20px",borderRadius:6,border:"none",background:theme.danger,color:"#fff",fontSize:13,fontWeight:600,fontFamily:mono,cursor:"pointer"}},"Delete")
      )
    )
  );
}

ReactDOM.createRoot(document.getElementById("root")).render(React.createElement(App));
