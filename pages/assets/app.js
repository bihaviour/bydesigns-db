/* =============================================================================
   twill-db — site behaviour (single source of truth).
   Injects the top site-header nav (shared across home, docs, specs, release),
   wires the theme toggle, and — when a page declares window.SECTION — builds the
   left sidebar, "on this page" TOC, prev/next, heading anchors, code-copy
   buttons, and scrollspy. Works from file:// (no fetch, no build step).
   ============================================================================= */
(function () {
  "use strict";

  var BASE = (typeof window.SITE_BASE === "string") ? window.SITE_BASE : "";
  var SECTION_KEY = window.SITE_SECTION || "";
  var REPO = "https://github.com/bihaviour/twill-db";

  // ---- Top site header (Home / Docs / Specs / Release) ----------------------
  var NAV = [
    { key: "home",    label: "Home",    href: "index.html" },
    { key: "docs",    label: "Docs",    href: "docs/index.html" },
    { key: "specs",   label: "Specs",   href: "specs/index.html" },
    { key: "release", label: "Release", href: "release/index.html" },
  ];

  function buildHeader() {
    var header = document.createElement("header");
    header.className = "site-header";
    var links = NAV.map(function (n) {
      var cur = n.key === SECTION_KEY ? " current" : "";
      return '<a class="' + n.key + cur + '" href="' + BASE + n.href + '">' + n.label + "</a>";
    }).join("");
    header.innerHTML =
      '<a class="site-brand" href="' + BASE + 'index.html">'
        + '<span class="logo">td</span><span class="name">Twill DB</span></a>'
      + '<button class="menu-btn" aria-label="Toggle menu" aria-expanded="false">☰</button>'
      + '<nav class="site-nav" id="site-nav">' + links + "</nav>"
      + '<div class="site-actions">'
        + '<a class="gh-link" href="' + REPO + '" target="_blank" rel="noopener">'
          + '<svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor" aria-hidden="true">'
          + '<path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8Z"/></svg>'
          + '<span class="gh-text">GitHub</span></a>'
        + '<button class="theme-btn" id="theme-btn" aria-label="Toggle colour theme">◐</button>'
      + "</div>";
    document.body.insertBefore(header, document.body.firstChild);

    var menuBtn = header.querySelector(".menu-btn");
    var nav = header.querySelector("#site-nav");
    if (menuBtn && nav) {
      menuBtn.addEventListener("click", function () {
        var open = nav.classList.toggle("open");
        menuBtn.setAttribute("aria-expanded", open ? "true" : "false");
      });
    }
  }
  buildHeader();

  // ---- Site-wide pre-1.0 banner ---------------------------------------------
  function buildBanner() {
    var bar = document.createElement("div");
    bar.className = "dev-banner";
    bar.innerHTML = '⚠ <strong>Pre-1.0 — active development.</strong> '
      + 'Backward compatibility is not guaranteed until 1.0.0. '
      + '<a href="' + BASE + 'release/index.html">Details →</a>';
    document.body.insertBefore(bar, document.body.firstChild);
  }
  buildBanner();

  // ---- Theme toggle (persisted) ---------------------------------------------
  function applyTheme(theme) {
    document.documentElement.setAttribute("data-theme", theme);
    var btn = document.getElementById("theme-btn");
    if (btn) btn.textContent = theme === "dark" ? "☀" : "☾";
  }
  var saved = null;
  try { saved = localStorage.getItem("bd-theme"); } catch (e) {}
  applyTheme(saved || (window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"));
  var themeBtn = document.getElementById("theme-btn");
  if (themeBtn) themeBtn.addEventListener("click", function () {
    var next = document.documentElement.getAttribute("data-theme") === "dark" ? "light" : "dark";
    applyTheme(next);
    try { localStorage.setItem("bd-theme", next); } catch (e) {}
  });

  // ---- Section sidebar / TOC / prev-next (only when window.SECTION is set) ---
  var SECTION = window.SECTION;
  function currentFile() {
    var p = location.pathname.split("/").pop();
    return (!p || p === "") ? "index.html" : p;
  }
  var here = currentFile();

  var flat = [];
  if (SECTION && SECTION.groups) {
    SECTION.groups.forEach(function (g) { g.items.forEach(function (it) { flat.push(it); }); });
  }

  var sidebar = document.getElementById("sidebar");
  if (sidebar && SECTION) {
    var brand = SECTION.brand || { title: "", sub: "" };
    var html = '<a class="brand" href="index.html">'
      + '<span class="brand-title">' + brand.title + '</span>'
      + '<span class="brand-sub">' + brand.sub + '</span></a>';
    SECTION.groups.forEach(function (g) {
      html += '<div class="nav-group"><p class="nav-group-label">' + g.label + '</p>';
      g.items.forEach(function (it) {
        var active = it.file === here ? " active" : "";
        html += '<a class="nav-link' + active + '" href="' + it.file + '">'
          + (it.id ? '<span class="nav-id">' + it.id + '</span>' : '')
          + '<span class="nav-text">' + it.title + '</span></a>';
      });
      html += "</div>";
    });
    sidebar.innerHTML = html;
  }

  // ---- Heading anchors + TOC ----
  var article = document.querySelector(".content article");
  var tocEl = document.getElementById("toc");
  var headings = [];
  if (article) {
    var hs = article.querySelectorAll("h2, h3");
    var slugCount = {};
    hs.forEach(function (h) {
      if (!h.id) {
        var base = h.textContent.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 50) || "section";
        if (slugCount[base] != null) { slugCount[base]++; base = base + "-" + slugCount[base]; }
        else { slugCount[base] = 0; }
        h.id = base;
      }
      var a = document.createElement("a");
      a.className = "heading-anchor"; a.href = "#" + h.id; a.textContent = "#"; a.setAttribute("aria-hidden", "true");
      h.appendChild(a);
      headings.push(h);
    });
  }
  if (tocEl && headings.length) {
    var t = "";
    headings.forEach(function (h) {
      var cls = h.tagName === "H3" ? "h3" : "h2";
      t += '<a class="' + cls + '" href="#' + h.id + '">' + h.firstChild.textContent + "</a>";
    });
    tocEl.innerHTML = t;
  } else if (tocEl) {
    var tocWrap = tocEl.closest(".toc"); if (tocWrap) tocWrap.style.display = "none";
  }

  // ---- Prev / next ----
  var pageNav = document.getElementById("page-nav");
  if (pageNav && flat.length) {
    var idx = flat.findIndex(function (it) { return it.file === here; });
    var out = "";
    if (idx > 0) {
      var p = flat[idx - 1];
      out += '<a class="pn-prev" href="' + p.file + '"><span class="pn-dir">← Previous</span><span class="pn-title">' + p.title + "</span></a>";
    } else { out += "<span></span>"; }
    if (idx > -1 && idx < flat.length - 1) {
      var n = flat[idx + 1];
      out += '<a class="pn-next" href="' + n.file + '"><span class="pn-dir">Next →</span><span class="pn-title">' + n.title + "</span></a>";
    }
    pageNav.innerHTML = out;
  }

  // ---- Copy buttons on code blocks ----
  document.querySelectorAll("pre").forEach(function (pre) {
    if (pre.closest(".diagram")) return;
    var btn = document.createElement("button");
    btn.className = "copy-btn"; btn.type = "button"; btn.textContent = "Copy";
    btn.addEventListener("click", function () {
      var code = pre.querySelector("code") || pre;
      var text = code.innerText;
      var done = function () { btn.textContent = "Copied"; btn.classList.add("copied"); setTimeout(function () { btn.textContent = "Copy"; btn.classList.remove("copied"); }, 1400); };
      if (navigator.clipboard && navigator.clipboard.writeText) { navigator.clipboard.writeText(text).then(done, fallback); }
      else { fallback(); }
      function fallback() {
        var ta = document.createElement("textarea"); ta.value = text; document.body.appendChild(ta); ta.select();
        try { document.execCommand("copy"); done(); } catch (e) {} document.body.removeChild(ta);
      }
    });
    pre.appendChild(btn);
  });

  // ---- "Copy as Markdown" — export the page for pasting into an AI agent ----
  // Docs pages get a header button that serialises the article body to clean
  // Markdown (headings, prose, code fences, lists, tables, callouts) so it can be
  // dropped straight into an LLM chat. Pairs with /llms.txt for crawl-time access.
  if (SECTION_KEY === "docs" && article) buildMarkdownCopy(article);

  function buildMarkdownCopy(art) {
    var header = art.querySelector(".page-header");
    var meta = header && header.querySelector(".spec-meta");
    var host = meta || header;
    if (!host) return;
    var btn = document.createElement("button");
    btn.type = "button";
    btn.className = "md-copy-btn";
    btn.title = "Copy this page as Markdown to paste into an AI agent";
    btn.setAttribute("aria-label", "Copy this page as Markdown");
    btn.innerHTML = '<span class="md-copy-ico" aria-hidden="true">⧉</span>'
      + '<span class="md-copy-label">Copy as Markdown</span>';
    if (meta && !meta.querySelector(".spec-date")) btn.style.marginLeft = "auto";
    host.appendChild(btn);

    var label = btn.querySelector(".md-copy-label");
    btn.addEventListener("click", function () {
      var md = pageToMarkdown(art);
      var done = function () {
        btn.classList.add("copied");
        if (label) label.textContent = "Copied";
        setTimeout(function () {
          btn.classList.remove("copied");
          if (label) label.textContent = "Copy as Markdown";
        }, 1500);
      };
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(md).then(done, fallback);
      } else { fallback(); }
      function fallback() {
        var ta = document.createElement("textarea"); ta.value = md;
        document.body.appendChild(ta); ta.select();
        try { document.execCommand("copy"); done(); } catch (e) {}
        document.body.removeChild(ta);
      }
    });
  }

  // ---- HTML → Markdown (powers the "Copy as Markdown" button) ----------------
  var MD_SKIP = ["page-nav", "doc-footer", "toc", "heading-anchor", "spec-meta", "md-copy-btn", "copy-btn", "nav-toggle"];
  function mdSkip(el) {
    if (el.classList) for (var i = 0; i < MD_SKIP.length; i++) { if (el.classList.contains(MD_SKIP[i])) return true; }
    var tag = el.tagName ? el.tagName.toLowerCase() : "";
    return tag === "script" || tag === "style" || tag === "button";
  }

  function mdInline(node) {
    var out = "";
    Array.prototype.forEach.call(node.childNodes, function (n) {
      if (n.nodeType === 3) { out += n.nodeValue.replace(/\s+/g, " "); return; }
      if (n.nodeType !== 1 || mdSkip(n)) return;
      var tag = n.tagName.toLowerCase();
      if (tag === "code") out += "`" + n.textContent + "`";
      else if (tag === "strong" || tag === "b") out += "**" + mdInline(n).trim() + "**";
      else if (tag === "em" || tag === "i") out += "*" + mdInline(n).trim() + "*";
      else if (tag === "br") out += "  \n";
      else if (tag === "a") {
        var txt = mdInline(n).trim(), href = n.getAttribute("href") || "";
        out += href ? "[" + txt + "](" + href + ")" : txt;
      } else out += mdInline(n);
    });
    return out;
  }

  function mdHeadingText(h) {
    var c = h.cloneNode(true), a = c.querySelector(".heading-anchor");
    if (a) a.remove();
    return c.textContent.trim();
  }

  function mdList(list, ordered, indent) {
    var md = "", i = 1;
    Array.prototype.forEach.call(list.children, function (li) {
      if (li.tagName.toLowerCase() !== "li") return;
      var marker = ordered ? (i++) + ". " : "- ";
      var clone = li.cloneNode(true);
      Array.prototype.forEach.call(clone.querySelectorAll("ul,ol"), function (x) { x.remove(); });
      md += indent + marker + mdInline(clone).trim().replace(/\s*\n\s*/g, " ") + "\n";
      Array.prototype.forEach.call(li.children, function (c) {
        var t = c.tagName.toLowerCase();
        if (t === "ul" || t === "ol") md += mdList(c, t === "ol", indent + "  ");
      });
    });
    return md + "\n";
  }

  function mdPre(pre) {
    var code = pre.querySelector("code") || pre;
    var lang = pre.getAttribute("data-lang") || "";
    return "```" + lang + "\n" + code.innerText.replace(/\s+$/, "") + "\n```\n\n";
  }

  // Escape a table cell for Markdown: backslash first (so we don't double-process
  // our own escapes), then the cell delimiter, then flatten any line breaks.
  function mdCell(el) {
    return mdInline(el).trim().replace(/\\/g, "\\\\").replace(/\|/g, "\\|").replace(/\n+/g, " ");
  }

  function mdTable(t) {
    var head = [], rows = [];
    var headRow = t.querySelector("thead tr");
    if (headRow) Array.prototype.forEach.call(headRow.children, function (c) { head.push(mdCell(c)); });
    var bodyRows = t.querySelectorAll("tbody tr");
    if (!bodyRows.length) bodyRows = t.querySelectorAll("tr");
    Array.prototype.forEach.call(bodyRows, function (tr) {
      if (tr.parentNode.tagName.toLowerCase() === "thead") return;
      var cells = [];
      Array.prototype.forEach.call(tr.children, function (c) { cells.push(mdCell(c)); });
      if (cells.length) rows.push(cells);
    });
    if (!head.length && rows.length) head = rows.shift();
    if (!head.length) return "";
    var md = "| " + head.join(" | ") + " |\n| " + head.map(function () { return "---"; }).join(" | ") + " |\n";
    rows.forEach(function (r) { md += "| " + r.join(" | ") + " |\n"; });
    return md + "\n";
  }

  function mdXrefGrid(div) {
    var md = "";
    Array.prototype.forEach.call(div.querySelectorAll("a"), function (a) {
      var title = a.querySelector(".xref-title"), desc = a.querySelector(".xref-desc");
      var href = a.getAttribute("href") || "";
      var t = title ? title.textContent.trim() : mdInline(a).trim();
      md += "- [" + t + "](" + href + ")" + (desc ? " — " + desc.textContent.trim() : "") + "\n";
    });
    return md + "\n";
  }

  function mdCallout(div) {
    var md = "";
    Array.prototype.forEach.call(div.children, function (c) {
      if (c.classList && c.classList.contains("callout-title")) md += "**" + c.textContent.trim() + "**\n";
      else md += mdInline(c).trim() + "\n";
    });
    return "> " + md.trim().replace(/\n/g, "\n> ") + "\n\n";
  }

  function mdChildren(el) {
    var md = "";
    Array.prototype.forEach.call(el.childNodes, function (n) { if (n.nodeType === 1) md += mdBlock(n); });
    return md;
  }

  function mdBlock(n) {
    if (mdSkip(n)) return "";
    switch (n.tagName.toLowerCase()) {
      case "h1": return "# " + mdHeadingText(n) + "\n\n";
      case "h2": return "## " + mdHeadingText(n) + "\n\n";
      case "h3": return "### " + mdHeadingText(n) + "\n\n";
      case "h4": return "#### " + mdHeadingText(n) + "\n\n";
      case "h5": case "h6": return "##### " + mdHeadingText(n) + "\n\n";
      case "p": var p = mdInline(n).trim(); return p ? p + "\n\n" : "";
      case "pre": return mdPre(n);
      case "ul": return mdList(n, false, "");
      case "ol": return mdList(n, true, "");
      case "blockquote": return "> " + mdInline(n).trim().replace(/\n/g, "\n> ") + "\n\n";
      case "table": return mdTable(n);
      case "hr": return "---\n\n";
      default:
        if (n.classList && n.classList.contains("callout")) return mdCallout(n);
        if (n.classList && n.classList.contains("xref-grid")) return mdXrefGrid(n);
        return mdChildren(n);
    }
  }

  function pageToMarkdown(art) {
    var md = mdChildren(art).replace(/\n{3,}/g, "\n\n").trim();
    return md + "\n\n---\n\nSource: " + location.href.split("#")[0] + "\n";
  }

  // ---- Mobile (left section) nav toggle ----
  var navToggle = document.querySelector(".nav-toggle");
  if (navToggle && sidebar) {
    navToggle.addEventListener("click", function () { sidebar.classList.toggle("open"); });
    document.addEventListener("click", function (e) {
      if (window.innerWidth <= 860 && sidebar.classList.contains("open") && !sidebar.contains(e.target) && e.target !== navToggle) {
        sidebar.classList.remove("open");
      }
    });
  }

  // ---- Scrollspy for TOC ----
  if (tocEl && headings.length) {
    var links = {};
    tocEl.querySelectorAll("a").forEach(function (a) { links[a.getAttribute("href").slice(1)] = a; });
    var spy = function () {
      var pos = window.scrollY + 110; var current = headings[0];
      for (var i = 0; i < headings.length; i++) { if (headings[i].offsetTop <= pos) current = headings[i]; }
      Object.keys(links).forEach(function (k) { links[k].classList.remove("active"); });
      if (current && links[current.id]) links[current.id].classList.add("active");
    };
    var ticking = false;
    window.addEventListener("scroll", function () { if (!ticking) { window.requestAnimationFrame(function () { spy(); ticking = false; }); ticking = true; } });
    spy();
  }
})();
