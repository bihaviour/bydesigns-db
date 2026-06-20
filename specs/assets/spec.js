/* =============================================================================
   Serverless OLTP Engine — Specification site behaviour.
   Single source of truth for navigation. Builds the sidebar, TOC, prev/next,
   heading anchors, code-copy buttons, theme toggle, and scrollspy.
   Works from file:// (no fetch, no build step).
   ============================================================================= */
(function () {
  "use strict";

  // ---- The canonical document manifest (the ONLY place page order is defined) ----
  var GROUPS = [
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
  ];

  function currentFile() {
    var p = location.pathname.split("/").pop();
    return (!p || p === "") ? "index.html" : p;
  }
  var here = currentFile();

  // Flat ordering for prev/next
  var flat = [];
  GROUPS.forEach(function (g) { g.items.forEach(function (it) { flat.push(it); }); });

  // ---- Build sidebar ----
  var sidebar = document.getElementById("sidebar");
  if (sidebar) {
    var html = '<a class="brand" href="index.html">'
      + '<span class="brand-title">Serverless OLTP Engine</span>'
      + '<span class="brand-sub">Development Specification</span></a>';
    GROUPS.forEach(function (g) {
      html += '<div class="nav-group"><p class="nav-group-label">' + g.label + '</p>';
      g.items.forEach(function (it) {
        var active = it.file === here ? " active" : "";
        html += '<a class="nav-link' + active + '" href="' + it.file + '">'
          + '<span class="nav-id">' + it.id + '</span>'
          + '<span class="nav-text">' + it.title + '</span></a>';
      });
      html += "</div>";
    });
    html += '<div class="controls">'
      + '<button class="icon-btn" id="theme-btn" aria-label="Toggle colour theme"><span id="theme-icon">◐</span><span id="theme-label">Theme</span></button>'
      + '</div>';
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
  if (pageNav) {
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

  // ---- Theme toggle (persisted) ----
  function applyTheme(theme) {
    document.documentElement.setAttribute("data-theme", theme);
    var icon = document.getElementById("theme-icon");
    var label = document.getElementById("theme-label");
    if (icon) icon.textContent = theme === "dark" ? "☀" : "☾";
    if (label) label.textContent = theme === "dark" ? "Light" : "Dark";
  }
  var saved = null;
  try { saved = localStorage.getItem("spec-theme"); } catch (e) {}
  applyTheme(saved || (window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"));
  var themeBtn = document.getElementById("theme-btn");
  if (themeBtn) themeBtn.addEventListener("click", function () {
    var next = document.documentElement.getAttribute("data-theme") === "dark" ? "light" : "dark";
    applyTheme(next);
    try { localStorage.setItem("spec-theme", next); } catch (e) {}
  });

  // ---- Mobile nav toggle ----
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
      var pos = window.scrollY + 96; var current = headings[0];
      for (var i = 0; i < headings.length; i++) { if (headings[i].offsetTop <= pos) current = headings[i]; }
      Object.keys(links).forEach(function (k) { links[k].classList.remove("active"); });
      if (current && links[current.id]) links[current.id].classList.add("active");
    };
    var ticking = false;
    window.addEventListener("scroll", function () { if (!ticking) { window.requestAnimationFrame(function () { spy(); ticking = false; }); ticking = true; } });
    spy();
  }
})();
