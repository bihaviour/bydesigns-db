/* Section manifest for the Specs / Development Guidelines area.
   Declares the left-sidebar nav + prev/next order. Loaded before app.js. */
window.SITE_BASE = "../";
window.SITE_SECTION = "specs";
window.SECTION = {
  brand: { title: "Development Guidelines", sub: "Design specs & implementation maps" },
  groups: [
    { label: "Overview", items: [
      { file: "index.html", title: "Overview & Index" },
    ]},
    { label: "Architecture", items: [
      { file: "01-architecture-overview.html", title: "Architecture Overview" },
      { file: "02-engine-core.html",           title: "Engine Core" },
      { file: "03-storage-interface.html",     title: "Storage Interface" },
      { file: "04-object-storage-backend.html",title: "Object-Storage Backend" },
      { file: "05-local-cache.html",           title: "Local Cache" },
      { file: "06-lifecycle-controller.html",  title: "Lifecycle & Controller" },
    ]},
    { label: "Interfaces", items: [
      { file: "07-server-mode.html",   title: "Server Mode & Wire Protocol" },
      { file: "08-bun-integration.html", title: "Bun Integration" },
      { file: "20-client-runtimes.html", title: "Client Runtimes & Language SDKs" },
    ]},
    { label: "Validation", items: [
      { file: "09-benchmark-plan.html",      title: "Benchmark & Validation Plan" },
      { file: "10-hot-row-contention.html",  title: "Hot-Row Contention Strategy" },
      { file: "15-twill-bench.html",         title: "Twill Bench CLI" },
    ]},
    { label: "Operations & Planning", items: [
      { file: "11-deployment-targets.html", title: "Deployment Targets" },
      { file: "12-capabilities.html",       title: "Capabilities: Build-in vs Compose" },
      { file: "13-roadmap.html",            title: "Roadmap & Build Sequence" },
      { file: "14-tradeoffs-risks.html",    title: "Tradeoffs & Risk Register" },
      { file: "16-sql-compatibility.html",  title: "SQL Compatibility & Mapping" },
      { file: "17-row-level-security.html", title: "Row-Level Security (Proposal)" },
      { file: "18-cli-tooling.html",        title: "Scaffolding CLI & Distribution" },
      { file: "19-cli-management.html",     title: "Database Management CLI (Proposal)" },
    ]},
    { label: "Implementation Maps", items: [
      { file: "phase-1-embedded.html",            title: "Phase 1 — Embedded Library" },
      { file: "phase-2-object-storage.html",      title: "Phase 2 — Object Storage" },
      { file: "phase-3-server.html",              title: "Phase 3 — Server + pgwire" },
      { file: "phase-4-branching-lifecycle.html", title: "Phase 4 — Branching & Lifecycle" },
      { file: "phase-5-capabilities.html",        title: "Phase 5 — Capabilities: Vector Search" },
      { file: "phase-6-sql-completeness.html",    title: "Phase 6 — SQL Surface Completeness" },
    ]},
  ],
};
