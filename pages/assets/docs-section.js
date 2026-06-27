/* Section manifest for the Docs (user documentation) area.
   Declares the left-sidebar nav + prev/next order. Loaded before app.js. */
window.SITE_BASE = "../";
window.SITE_SECTION = "docs";
window.SECTION = {
  brand: { title: "Documentation", sub: "Use & operate Twill DB" },
  groups: [
    { label: "Get started", items: [
      { file: "index.html",           title: "Introduction" },
      { file: "architecture.html",    title: "Architecture" },
      { file: "getting-started.html", title: "Quickstart" },
      { file: "scaffolding.html",     title: "Scaffold a project (CLI)" },
    ]},
    { label: "Connect", items: [
      { file: "connect.html",            title: "Connect to your database" },
      { file: "connect-bun.html",        title: "Embedded (bun:ffi)" },
      { file: "connect-postgres.html",   title: "Postgres client" },
      { file: "connection-pooling.html", title: "Connection pooling" },
    ]},
    { label: "Connect as embedded", items: [
      { file: "connect-embedded.html",        title: "Frameworks — overview" },
      { file: "connect-embedded-bun.html",    title: "Bun (HTTP)" },
      { file: "connect-embedded-node.html",   title: "Node & frameworks" },
      { file: "connect-embedded-php.html",    title: "PHP & frameworks" },
      { file: "connect-embedded-hono.html",   title: "Hono" },
      { file: "connect-embedded-elysia.html", title: "Elysia" },
      { file: "connect-embedded-nextjs.html", title: "Next.js" },
    ]},
    { label: "Connect as server", items: [
      { file: "connect-server.html",           title: "Clients & ORMs — overview" },
      { file: "connect-server-node.html",      title: "Node / Bun" },
      { file: "connect-server-python.html",    title: "Python" },
      { file: "connect-server-go.html",        title: "Go" },
      { file: "connect-server-drizzle.html",   title: "Drizzle ORM" },
      { file: "connect-server-prisma.html",    title: "Prisma" },
      { file: "connect-server-postgrest.html", title: "PostgREST" },
    ]},
    { label: "Storage", items: [
      { file: "storage.html",      title: "Backends — overview" },
      { file: "storage-file.html", title: "file:// (local)" },
      { file: "storage-s3.html",   title: "s3:// (S3 / MinIO)" },
      { file: "storage-r2.html",   title: "r2:// (Cloudflare)" },
      { file: "storage-gs.html",   title: "gs:// (Google Cloud)" },
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
