/* Section manifest for the Docs (user documentation) area.
   Declares the left-sidebar nav + prev/next order. Loaded before app.js. */
window.SITE_BASE = "../";
window.SITE_SECTION = "docs";
window.SECTION = {
  brand: { title: "Documentation", sub: "Use & operate Twill DB" },
  groups: [
    { label: "Get started", items: [
      { file: "index.html",          id: "OV", title: "Introduction" },
      { file: "getting-started.html", id: "QS", title: "Quickstart" },
    ]},
    { label: "Connect", items: [
      { file: "connect.html",            id: "CONN", title: "Connect to your database" },
      { file: "connect-bun.html",        id: "BUN",  title: "Connect from Bun (embedded)" },
      { file: "connect-postgres.html",   id: "PG",   title: "Connect with a Postgres client" },
      { file: "connection-pooling.html", id: "POOL", title: "Connection pooling" },
    ]},
    { label: "Guides", items: [
      { file: "branching.html",     id: "BR", title: "Branching" },
      { file: "scale-to-zero.html", id: "S0", title: "Scale-to-zero & lifecycle" },
    ]},
    { label: "Reference", items: [
      { file: "sql-reference.html", id: "SQL", title: "SQL reference" },
      { file: "c-abi.html",         id: "ABI", title: "C ABI reference" },
    ]},
  ],
};
