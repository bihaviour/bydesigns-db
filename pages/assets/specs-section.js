/* Section manifest for the Specs / Development Guidelines area.
   Declares the left-sidebar nav + prev/next order. Loaded before app.js. */
window.SITE_BASE = "../";
window.SITE_SECTION = "specs";
window.SECTION = {
  brand: { title: "Development Guidelines", sub: "Design specs & implementation maps" },
  groups: [
    { label: "Overview", items: [
      { file: "index.html", id: "OV", title: "Overview & Index" },
    ]},
    { label: "Architecture", items: [
      { file: "01-architecture-overview.html", id: "ARCH",  title: "Architecture Overview" },
      { file: "02-engine-core.html",           id: "ENG",   title: "Engine Core" },
      { file: "03-storage-interface.html",     id: "STOR",  title: "Storage Interface" },
      { file: "04-object-storage-backend.html",id: "OBJ",   title: "Object-Storage Backend" },
      { file: "05-local-cache.html",           id: "CACHE", title: "Local Cache" },
      { file: "06-lifecycle-controller.html",  id: "CTL",   title: "Lifecycle & Controller" },
    ]},
    { label: "Interfaces", items: [
      { file: "07-server-mode.html",   id: "SRV", title: "Server Mode & Wire Protocol" },
      { file: "08-bun-integration.html", id: "BUN", title: "Bun Integration" },
    ]},
    { label: "Validation", items: [
      { file: "09-benchmark-plan.html",      id: "BENCH", title: "Benchmark & Validation Plan" },
      { file: "10-hot-row-contention.html",  id: "HOT",   title: "Hot-Row Contention Strategy" },
    ]},
    { label: "Operations & Planning", items: [
      { file: "11-deployment-targets.html", id: "DEPLOY", title: "Deployment Targets" },
      { file: "12-capabilities.html",       id: "CAP",    title: "Capabilities: Build-in vs Compose" },
      { file: "13-roadmap.html",            id: "ROAD",   title: "Roadmap & Build Sequence" },
      { file: "14-tradeoffs-risks.html",    id: "RISK",   title: "Tradeoffs & Risk Register" },
    ]},
    { label: "Implementation Maps", items: [
      { file: "phase-1-embedded.html",            id: "P1", title: "Phase 1 — Embedded Library" },
      { file: "phase-2-object-storage.html",      id: "P2", title: "Phase 2 — Object Storage" },
      { file: "phase-3-server.html",              id: "P3", title: "Phase 3 — Server + pgwire" },
      { file: "phase-4-branching-lifecycle.html", id: "P4", title: "Phase 4 — Branching & Lifecycle" },
      { file: "phase-5-capabilities.html",        id: "P5", title: "Phase 5 — Capabilities: Vector Search" },
    ]},
  ],
};
