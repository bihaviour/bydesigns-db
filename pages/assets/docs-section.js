/* Section manifest for the Docs (user documentation) area.
   Declares the left-sidebar nav + prev/next order. Loaded before app.js. */
window.SITE_BASE = "../";
window.SITE_SECTION = "docs";
window.SECTION = {
  brand: { title: "Documentation", sub: "Use & operate Twill DB" },
  groups: [
    { label: "Get started", items: [
      { file: "index.html",          title: "Introduction" },
      { file: "getting-started.html", title: "Quickstart" },
    ]},
    { label: "Connect", items: [
      { file: "connect.html",            title: "Connect to your database" },
      { file: "connect-bun.html",        title: "Connect from Bun (embedded)" },
      { file: "connect-postgres.html",   title: "Connect with a Postgres client" },
      { file: "connection-pooling.html", title: "Connection pooling" },
    ]},
    { label: "Guides", items: [
      { file: "branching.html",     title: "Branching" },
      { file: "scale-to-zero.html", title: "Scale-to-zero & lifecycle" },
      { file: "hot-row.html",       title: "Hot-row contention" },
    ]},
    { label: "Compose", items: [
      { file: "auth.html",     id: "AUTH", title: "Auth (better-auth)" },
      { file: "olap.html",     id: "OLAP", title: "Analytics (DuckDB / HTAP)" },
    ]},
    { label: "Reference", items: [
      { file: "sql-reference.html", title: "SQL reference" },
      { file: "c-abi.html",         title: "C ABI reference" },
    ]},
  ],
};
