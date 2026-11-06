/* Loom docs: the thread, and the little things every page does.
 *
 * A thread comes down from the woven band at the top of the page and runs
 * through the whole chapter: it strings a bead on at each section, dives
 * under every sample through the grommets on its edge and comes out below,
 * and ties off at the end. The part you've read is dyed; the rest is still
 * raw yarn. It's one SVG behind the text, drawn from where things actually
 * are on the page, and drawn again when they move (a sample that runs grows).
 *
 * Also: the chapter menu on narrow screens, the "Elsewhere" tabs (which
 * remember the language you picked), and the beads in the sidebar for the
 * chapters you've visited. Nothing here is needed to read the pages.
 */
(function () {
  "use strict";

  const SVG = "http://www.w3.org/2000/svg";
  const DYES = ["#ee7a5f", "#f3bd55", "#b7c56f", "#8fb6d8", "#ec86ad", "#79c6b0", "#a7a1ec", "#f59d57"];
  const reduced = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  function store(key, value) {
    try { localStorage.setItem(key, value); } catch (_) { /* fine without */ }
  }
  function stored(key) {
    try { return localStorage.getItem(key); } catch (_) { return null; }
  }

  function svg(tag, attrs, parent) {
    const e = document.createElementNS(SVG, tag);
    for (const k in attrs) e.setAttribute(k, attrs[k]);
    if (parent) parent.appendChild(e);
    return e;
  }

  // ------------------------------------------------------------ the thread

  const main = document.querySelector("main.weave");
  let layer = null;
  let samples = []; // [length along the path, y] pairs, for the dye
  let paths = null;

  // A smooth curve through the points (Catmull-Rom, as cubic Béziers).
  function smooth(pts) {
    if (pts.length < 2) return "";
    let d = `M${pts[0].x.toFixed(1)},${pts[0].y.toFixed(1)}`;
    for (let i = 0; i < pts.length - 1; i++) {
      const p0 = pts[i - 1] || pts[i];
      const p1 = pts[i];
      const p2 = pts[i + 1];
      const p3 = pts[i + 2] || p2;
      const t = p1.tension != null ? p1.tension : 0.5;
      const c1x = p1.x + ((p2.x - p0.x) / 6) * t * 2;
      const c1y = p1.y + ((p2.y - p0.y) / 6) * t * 2;
      const c2x = p2.x - ((p3.x - p1.x) / 6) * t * 2;
      const c2y = p2.y - ((p3.y - p1.y) / 6) * t * 2;
      d += ` C${c1x.toFixed(1)},${c1y.toFixed(1)} ${c2x.toFixed(1)},${c2y.toFixed(1)} ${p2.x.toFixed(1)},${p2.y.toFixed(1)}`;
    }
    return d;
  }

  function draw() {
    if (!main || document.body.classList.contains("no-thread")) return;
    const box = main.getBoundingClientRect();
    const width = main.clientWidth;
    const height = main.scrollHeight;
    const gutter = parseFloat(getComputedStyle(main).paddingLeft) || 60;
    const gx = Math.max(10, gutter * 0.44);
    const sway = Math.min(18, gutter * 0.3);
    const rel = (e) => {
      const r = e.getBoundingClientRect();
      return { top: r.top - box.top, bottom: r.bottom - box.top, left: r.left - box.left, right: r.right - box.left };
    };

    // Where the thread must pass, top to bottom.
    const pts = [{ x: gx, y: -8 }];
    const beads = [];
    const grommets = [];
    let section = 0;
    let lastY = -8;
    let side = 1;
    const meander = (toY) => {
      // gentle swings in the gutter between two fixed points
      const span = toY - lastY;
      const steps = Math.floor(span / 150);
      for (let i = 1; i <= steps; i++) {
        pts.push({ x: gx + side * sway, y: lastY + (span * i) / (steps + 1) });
        side = -side;
      }
    };
    for (const child of main.children) {
      if (child === layer || child.offsetParent === null) continue;
      const r = rel(child);
      if (child.matches("h2")) {
        const y = r.top + Math.min(r.bottom - r.top, 44) / 2;
        meander(y - 30);
        pts.push({ x: gx, y: y - 24, tension: 0.4 });
        pts.push({ x: gx, y });
        pts.push({ x: gx, y: y + 24, tension: 0.4 });
        beads.push({ x: gx, y, n: ++section });
        lastY = y + 24;
      } else if (child.matches(".sample, .compare")) {
        const inY = r.top + 29;
        const outY = r.bottom - 29;
        if (outY - inY < 30) continue;
        meander(inY - 50);
        pts.push({ x: gx + side * sway * 0.4, y: inY - 34 });
        pts.push({ x: r.left, y: inY, tension: 0.35 });
        // under the card, out of sight
        pts.push({ x: r.left + 34, y: inY + 30, tension: 0.4 });
        pts.push({ x: r.left + 34, y: outY - 30, tension: 0.4 });
        pts.push({ x: r.left, y: outY, tension: 0.35 });
        pts.push({ x: gx - side * sway * 0.4, y: outY + 34 });
        grommets.push(inY, outY);
        lastY = outY + 34;
      } else if (child.matches(".pager")) {
        const y = r.top + 26;
        meander(y - 40);
        pts.push({ x: gx, y });
        lastY = y;
      }
    }
    // on to the bottom of the page, where it's tied off
    const endY = Math.max(lastY + 40, height - 46);
    if (lastY < height - 60) {
      meander(endY);
      pts.push({ x: gx, y: endY });
    }
    const end = pts[pts.length - 1];

    if (!layer) {
      layer = svg("svg", { class: "thread", "aria-hidden": "true", focusable: "false" });
      main.insertBefore(layer, main.firstChild);
    }
    layer.textContent = "";
    layer.setAttribute("width", width);
    layer.setAttribute("height", height);
    layer.setAttribute("viewBox", `0 0 ${width} ${height}`);

    const defs = svg("defs", {}, layer);
    const grad = svg("linearGradient", { id: "dye", gradientUnits: "userSpaceOnUse", x1: 0, y1: 0, x2: 0, y2: height }, defs);
    const stops = Math.max(2, Math.round(height / 900) + 2);
    for (let i = 0; i < stops; i++) {
      svg("stop", { offset: (i / (stops - 1)).toFixed(3), "stop-color": DYES[i % DYES.length] }, grad);
    }

    const d = smooth(pts);
    const raw = svg("path", { class: "yarn-raw", d }, layer);
    const dyed = svg("path", { class: "yarn-dyed", d, stroke: "url(#dye)" }, layer);
    const ply = svg("path", { class: "yarn-ply", d }, layer);
    const length = raw.getTotalLength();
    dyed.style.strokeDasharray = `${length} ${length}`;
    ply.style.strokeDasharray = "2 5";

    // the knot at the end
    svg("circle", { class: "knot", cx: end.x, cy: end.y, r: 4.5 }, layer);
    svg("path", {
      d: `M${end.x},${end.y} q -9,6 -6,15 M${end.x},${end.y} q 8,7 9,15`,
      fill: "none", stroke: "url(#dye)", "stroke-width": 2.4, "stroke-linecap": "round",
    }, layer);

    // beads, one per section, over the thread (smaller where the gutter is thin)
    const r = gutter < 40 ? 9 : 12.5;
    for (const b of beads) {
      const dye = DYES[(b.n - 1) % DYES.length];
      svg("circle", { class: "bead", cx: b.x, cy: b.y, r, fill: dye, stroke: "rgba(255,240,220,0.55)" }, layer);
      svg("circle", { cx: b.x - r * 0.32, cy: b.y - r * 0.36, r: r * 0.24, fill: "rgba(255,255,255,0.35)" }, layer);
      const t = svg("text", { class: "bead-num", x: b.x, y: b.y + 0.5, fill: "#1b1511" }, layer);
      if (r < 12) t.style.fontSize = "8.5px";
      t.textContent = String(b.n);
    }

    const needle = svg("g", { class: "needle" }, layer);
    svg("circle", { r: 5.5, fill: "#fff2d6" }, needle);
    svg("circle", { r: 2.4, fill: "#ee7a5f" }, needle);

    // where along the thread each height is, to dye it as far as you've read
    samples = [];
    const step = Math.max(6, length / 1500);
    for (let l = 0; l <= length; l += step) {
      samples.push([l, raw.getPointAtLength(l).y]);
    }
    samples.push([length, raw.getPointAtLength(length).y]);
    paths = { dyed, needle, length };
    dye();
  }

  // Dyes the thread down to a little past the middle of the window.
  function dye() {
    if (!paths || !main) return;
    const box = main.getBoundingClientRect();
    const target = window.innerHeight * 0.62 - box.top;
    let lo = 0;
    let hi = samples.length - 1;
    // the last sample at or above the target height
    let best = 0;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (samples[mid][1] <= target) {
        best = mid;
        lo = mid + 1;
      } else {
        hi = mid - 1;
      }
    }
    // the thread's heights aren't quite monotonic (it loops at grommets), so
    // walk forward past any that still fit
    while (best + 1 < samples.length && samples[best + 1][1] <= target) best++;
    const at = target <= samples[0][1] ? 0 : samples[best][0];
    paths.dyed.style.strokeDashoffset = String(paths.length - at);
    const p = paths.dyed.getPointAtLength(at);
    paths.needle.setAttribute("transform", `translate(${p.x.toFixed(1)},${p.y.toFixed(1)})`);
    paths.needle.style.opacity = at <= 0 || at >= paths.length ? "0" : "1";
  }

  let redrawTimer = null;
  function redrawSoon() {
    clearTimeout(redrawTimer);
    redrawTimer = setTimeout(draw, 80);
  }

  let dyeQueued = false;
  function dyeSoon() {
    if (dyeQueued) return;
    dyeQueued = true;
    requestAnimationFrame(() => {
      dyeQueued = false;
      dye();
    });
  }

  // ------------------------------------------------------------ page bits

  function menu() {
    const toggle = document.querySelector(".menu-toggle");
    if (!toggle) return;
    toggle.addEventListener("click", (e) => {
      e.stopPropagation();
      const open = document.body.classList.toggle("menu-open");
      toggle.setAttribute("aria-expanded", String(open));
    });
    document.addEventListener("click", (e) => {
      if (document.body.classList.contains("menu-open") && !e.target.closest("nav.sidebar")) {
        document.body.classList.remove("menu-open");
        toggle.setAttribute("aria-expanded", "false");
      }
    });
  }

  // "Elsewhere" boxes: one tab per language, the one you last picked first.
  function tabs() {
    const boxes = document.querySelectorAll(".compare");
    const pick = (box, lang) => {
      const panels = box.querySelectorAll(".tab-panel");
      let found = false;
      for (const p of panels) if (p.dataset.lang === lang) found = true;
      if (!found) lang = panels[0] && panels[0].dataset.lang;
      for (const p of panels) p.hidden = p.dataset.lang !== lang;
      for (const b of box.querySelectorAll(".tabs button")) b.setAttribute("aria-selected", String(b.dataset.lang === lang));
    };
    for (const box of boxes) {
      const bar = box.querySelector(".tabs");
      if (!bar) continue;
      for (const p of box.querySelectorAll(".tab-panel")) {
        const b = document.createElement("button");
        b.type = "button";
        b.setAttribute("role", "tab");
        b.dataset.lang = p.dataset.lang;
        b.textContent = p.dataset.label || p.dataset.lang;
        b.addEventListener("click", () => {
          store("loom.lang", p.dataset.lang);
          for (const other of boxes) pick(other, p.dataset.lang);
          redrawSoon();
        });
        bar.appendChild(b);
      }
      box.classList.add("has-tabs");
      pick(box, stored("loom.lang"));
    }
  }

  function visited() {
    const here = document.body.dataset.chapter;
    let seen = [];
    try { seen = JSON.parse(stored("loom.visited") || "[]"); } catch (_) { seen = []; }
    if (here && !seen.includes(here)) {
      seen.push(here);
      store("loom.visited", JSON.stringify(seen));
    }
    for (const a of document.querySelectorAll("nav.sidebar a[data-chapter]")) {
      if (seen.includes(a.dataset.chapter)) a.classList.add("visited");
    }
  }

  function init() {
    document.documentElement.classList.remove("no-js");
    menu();
    tabs();
    visited();
    if (!main) return;
    draw();
    window.addEventListener("scroll", dyeSoon, { passive: true });
    window.addEventListener("resize", redrawSoon);
    if (document.fonts && document.fonts.ready) document.fonts.ready.then(redrawSoon);
    if (window.ResizeObserver) new ResizeObserver(redrawSoon).observe(main);
    window.addEventListener("load", redrawSoon);
    if (reduced && paths) paths.needle.style.display = "none";
  }

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init);
  else init();
})();
